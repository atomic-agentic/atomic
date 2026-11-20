// SPDX-License-Identifier: Apache-2.0

//! Retention: sweeping the artifact store, keeping everything the store's
//! runs and gates may still read.
//!
//! The sweep fails closed. Besides what the caller keeps, it keeps every
//! artifact named, as `artifact://sha256/<64 hex>` or `sha256:<64 hex>`
//! anywhere in a string (a whole value, or inside markdown or JSON carried as
//! text), at any depth, in:
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

/// Every artifact named in `text`: `artifact://sha256/<64 hex>` or
/// `sha256:<64 hex>`, as the whole string or within it.
fn named_in(text: &str, into: &mut BTreeSet<ArtifactRef>) {
    for marker in ["artifact://sha256/", "sha256:"] {
        for (at, _) in text.match_indices(marker) {
            let rest = &text[at + marker.len()..];
            let hex = rest
                .bytes()
                .take_while(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                .count();
            if hex >= 64
                && let Ok(a) = ArtifactRef::from_digest(&rest[..64])
            {
                into.insert(a);
            }
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
/// read a page at a time.
pub async fn recorded(rt: &Durare) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let mut found = BTreeSet::new();
    let mut offset = 0;
    loop {
        let page = rt.runs_page(offset, PAGE).await?;
        for run in &page {
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
        if (page.len() as i64) < PAGE {
            return Ok(found);
        }
        offset += PAGE;
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
/// (see [`LocalStore::sweep`]).
pub async fn sweep(
    store: &LocalStore,
    rt: &Durare,
    keep: &BTreeSet<ArtifactRef>,
    grace: Grace,
) -> anyhow::Result<Swept> {
    let read_at = std::time::SystemTime::now();
    let mut all = keep.clone();
    all.extend(referenced(rt).await?);
    let store = store.clone();
    Ok(tokio::task::spawn_blocking(move || store.sweep(&all, grace, read_at)).await??)
}
