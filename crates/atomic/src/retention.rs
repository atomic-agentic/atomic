// SPDX-License-Identifier: Apache-2.0

//! Retention: sweeping the artifact store, keeping everything the store's
//! runs and gates may still read.
//!
//! The sweep fails closed. Besides what the caller keeps, it keeps every
//! artifact whose digest appears, as a run of 64 hex digits in either case,
//! anywhere in a string at any depth (as `artifact://sha256/<hex>`,
//! `sha256:<hex>`, with JSON-escaped slashes, bare, or inside markdown or
//! JSON carried as text), in:
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
//! lock take. What it cannot see: a producer that stores content more than
//! a grace period before the step that refers to it is checkpointed. And
//! what lives in the source tree (eval cases, cassettes, reports) refers to
//! artifacts no run records, so only the caller's keep set covers it.
//!
//! Nothing sweeps on its own: a caller decides when.

use atomic_core::artifact::{ArtifactRef, Grace, LocalStore, Swept};
use atomic_durable::{DurableRuntime, Durare};
use serde_json::Value;
use std::collections::BTreeSet;

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

/// Read what a sweep keeps: `keep` and everything [`referenced`] finds,
/// with the time the reading began.
pub async fn read_kept(rt: &Durare, keep: &BTreeSet<ArtifactRef>) -> anyhow::Result<Kept> {
    let read_at = std::time::SystemTime::now();
    let mut artifacts = keep.clone();
    artifacts.extend(referenced(rt).await?);
    Ok(Kept { artifacts, read_at })
}

/// Every artifact `text` may name: each run of exactly 64 hex digits in
/// it, in either case, whatever surrounds it. That finds
/// `artifact://sha256/<hex>` and `sha256:<hex>` whole, inside longer text,
/// with JSON-escaped slashes or upper-case hex, and a bare digest too. It
/// keeps more than is named, which only keeps more.
fn named_in(text: &str, into: &mut BTreeSet<ArtifactRef>) {
    let bytes = text.as_bytes();
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
        if i - start == 64
            && let Ok(a) = ArtifactRef::from_digest(&text[start..i].to_ascii_lowercase())
        {
            into.insert(a);
        }
    }
}

/// Every artifact named in any string in `value`, at any depth, keys
/// included.
fn references(value: &Value, into: &mut BTreeSet<ArtifactRef>) {
    match value {
        Value::String(s) => named_in(s, into),
        Value::Array(items) => items.iter().for_each(|v| references(v, into)),
        Value::Object(map) => map.iter().for_each(|(k, v)| {
            named_in(k, into);
            references(v, into);
        }),
        _ => {}
    }
}

/// What the store's runs and gates refer to: [`recorded`] and [`pinned`].
pub async fn referenced(rt: &Durare) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let mut found = recorded(rt).await?;
    found.extend(pinned(rt).await?);
    Ok(found)
}

/// Every artifact any run in the store names, finished or not: in its input,
/// its result or error, and every step's recorded output or error. Runs are
/// read a page at a time, in order of creation and id.
pub async fn recorded(rt: &Durare) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let mut found = BTreeSet::new();
    let mut after = None;
    loop {
        let page = rt.runs_after(after.as_ref(), PAGE).await?;
        for (_, run) in &page {
            references(&run.input, &mut found);
            if let Some(output) = &run.output {
                references(output, &mut found);
            }
            if let Some(error) = &run.error {
                named_in(error, &mut found);
            }
            for step in rt.steps(&run.record.id).await? {
                if let Some(output) = &step.output {
                    references(output, &mut found);
                }
                if let Some(error) = &step.error {
                    named_in(error, &mut found);
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
    let kept = read_kept(rt, keep).await?;
    let store = store.clone();
    Ok(
        tokio::task::spawn_blocking(move || store.sweep(&kept.artifacts, grace, kept.read_at))
            .await??,
    )
}
