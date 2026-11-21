// SPDX-License-Identifier: Apache-2.0

//! Retention: sweeping the artifact store, keeping everything the store's
//! runs and gates may still read.
//!
//! The sweep fails closed. Besides what the caller keeps, it keeps every
//! blob whose digest appears, in either case, anywhere in a string at any
//! depth: as `artifact://sha256/<hex>`, `sha256:<hex>`, the store's path
//! form `sha256/<2 hex>/<62 hex>`, bare, within a longer hex string, with
//! JSON-escaped or percent-encoded slashes, or inside markdown or JSON
//! carried as text. It looks for digests the store holds, in:
//!
//! - every run's input, and its result or error once it has one;
//! - every step's recorded output or error, in every run in the store,
//!   finished or not: a run's pinned definitions (its snapshot) and the
//!   results its steps hand on by reference among them;
//! - every gate's pinned evidence, open or resolved, read from the gate
//!   store itself.
//!
//! It also keeps anything written within the grace period before it began
//! reading these, however long the reading and the wait for the store's
//! lock take, and anything a store holds as it begins reading or as it
//! sweeps: an exec step holds the files its command handed over from before
//! they are stored until its turn is checkpointed. What it cannot see: any
//! other producer that stores content more than a grace period before the
//! step that refers to it is checkpointed. And what lives in the source tree
//! (eval cases, cassettes, reports) refers to artifacts no run records, so
//! only the caller's keep set covers it.
//!
//! Nothing sweeps on its own: a caller decides when.

use atomic_core::artifact::{ArtifactRef, Grace, LocalStore, Swept};
use atomic_durable::{DurableRuntime, Durare};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};

/// How many runs a scan reads at a time.
const PAGE: i64 = 100;

/// What a sweep keeps, as read from the store's runs and gates at a time.
#[derive(Clone, Debug)]
pub struct Kept {
    /// The artifacts to keep.
    pub artifacts: BTreeSet<ArtifactRef>,
    /// When the reading began: content written within a grace period before
    /// it is kept too.
    pub read_at: std::time::SystemTime,
}

/// Read what a sweep of `store` keeps: `keep`, everything held as the
/// reading begins, and everything [`referenced`] finds, with the time the
/// reading began.
///
/// The holds are read first: a step lets its hold go once it has recorded
/// the reference, which the reading of the runs may already have passed,
/// so the holds the sweep reads later under the store's lock may no longer
/// show it.
pub async fn read_kept(
    store: &LocalStore,
    rt: &Durare,
    keep: &BTreeSet<ArtifactRef>,
) -> anyhow::Result<Kept> {
    read_kept_then(store, rt, keep, || {}).await
}

/// [`read_kept`], calling `between` once the holds are read and before the
/// runs are, as a step that records its reference and lets its hold go in
/// that window would: for tests.
#[doc(hidden)]
pub async fn read_kept_then(
    store: &LocalStore,
    rt: &Durare,
    keep: &BTreeSet<ArtifactRef>,
    between: impl FnOnce(),
) -> anyhow::Result<Kept> {
    let read_at = std::time::SystemTime::now();
    let mut artifacts = keep.clone();
    let held = held_now(store).await?;
    between();
    artifacts.extend(held);
    artifacts.extend(referenced(store, rt).await?);
    Ok(Kept { artifacts, read_at })
}

/// What every store holds now, read off the async workers.
async fn held_now(store: &LocalStore) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let store = store.clone();
    Ok(tokio::task::spawn_blocking(move || store.held()).await??)
}

/// Every blob of `present` that `text` may name, in either case: any 64 hex
/// digits within it, however much hex surrounds them, and the store's own
/// path form, `sha256/<2 hex>/<62 hex>`. Slashes may be JSON-escaped (`\/`)
/// or percent-encoded (`%2F`). That finds `artifact://sha256/<hex>`,
/// `sha256:<hex>`, a bare digest and one inside a longer hex string; only a
/// digest the store holds is kept, so unrelated hashes cost nothing.
fn named_in(text: &str, present: &HashSet<String>, into: &mut BTreeSet<ArtifactRef>) {
    let text = text
        .to_ascii_lowercase()
        .replace("\\/", "/")
        .replace("%2f", "/");
    let bytes = text.as_bytes();
    let mut keep = |digest: &str| {
        if present.contains(digest)
            && let Ok(a) = ArtifactRef::from_digest(digest)
        {
            into.insert(a);
        }
    };
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_hexdigit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        let run = &text[start..i];
        for at in 0..run.len().saturating_sub(63) {
            keep(&run[at..at + 64]);
        }
        // The path form: two hex digits, a slash, then the other 62.
        if run.len() >= 62
            && start >= 3
            && bytes[start - 1] == b'/'
            && bytes[start - 2].is_ascii_hexdigit()
            && bytes[start - 3].is_ascii_hexdigit()
            && (start == 3 || !bytes[start - 4].is_ascii_hexdigit())
        {
            keep(&format!("{}{}", &text[start - 3..start - 1], &run[..62]));
        }
    }
}

/// Every blob of `present` named in any string in `value`, at any depth,
/// keys included.
fn references(value: &Value, present: &HashSet<String>, into: &mut BTreeSet<ArtifactRef>) {
    match value {
        Value::String(s) => named_in(s, present, into),
        Value::Array(items) => items.iter().for_each(|v| references(v, present, into)),
        Value::Object(map) => map.iter().for_each(|(k, v)| {
            named_in(k, present, into);
            references(v, present, into);
        }),
        _ => {}
    }
}

/// What the store's runs and gates refer to: [`recorded`] and [`pinned`].
pub async fn referenced(store: &LocalStore, rt: &Durare) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let mut found = recorded(store, rt).await?;
    found.extend(pinned(rt).await?);
    Ok(found)
}

/// Every blob in `store` any run in the store names, finished or not: in its
/// input, its result or error, and every step's recorded output or error.
/// Runs are read a page at a time, in order of creation and id.
pub async fn recorded(store: &LocalStore, rt: &Durare) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let present: HashSet<String> = {
        let store = store.clone();
        tokio::task::spawn_blocking(move || store.stored())
            .await??
            .into_iter()
            .map(|a| a.digest().to_owned())
            .collect()
    };
    let mut found = BTreeSet::new();
    let mut after = None;
    loop {
        let page = rt.runs_after(after.as_ref(), PAGE).await?;
        for (_, run) in &page {
            references(&run.input, &present, &mut found);
            if let Some(output) = &run.output {
                references(output, &present, &mut found);
            }
            if let Some(error) = &run.error {
                named_in(error, &present, &mut found);
            }
            for step in rt.steps(&run.record.id).await? {
                if let Some(output) = &step.output {
                    references(output, &present, &mut found);
                }
                if let Some(error) = &step.error {
                    named_in(error, &present, &mut found);
                }
            }
        }
        match page.last() {
            Some((key, _)) if page.len() as i64 == PAGE => after = Some(key.clone()),
            _ => return Ok(found),
        }
    }
}

/// Every digest a gate in the store pins as evidence, open or resolved. The
/// gate store keeps these apart from the run's own records, which a run's
/// gate step may not repeat, so they are read from it directly.
pub async fn pinned(rt: &Durare) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let mut found = BTreeSet::new();
    for pin in atomic_durable::gate::pinned_evidence(rt).await? {
        let digest = pin
            .strip_prefix("sha256:")
            .ok_or_else(|| anyhow::anyhow!("a gate pins {pin}, which is not a content digest"))?;
        found.insert(ArtifactRef::from_digest(digest)?);
    }
    Ok(found)
}

/// Sweep `store`, keeping `keep`, everything [`referenced`] finds, and
/// anything written within `grace` before it began reading the store's runs
/// (see [`read_kept`] and [`LocalStore::sweep`]).
pub async fn sweep(
    store: &LocalStore,
    rt: &Durare,
    keep: &BTreeSet<ArtifactRef>,
    grace: Grace,
) -> anyhow::Result<Swept> {
    let kept = read_kept(store, rt, keep).await?;
    let store = store.clone();
    Ok(
        tokio::task::spawn_blocking(move || store.sweep(&kept.artifacts, grace, kept.read_at))
            .await??,
    )
}
