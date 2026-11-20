// SPDX-License-Identifier: Apache-2.0

//! Retention: sweeping the artifact store, keeping everything the store's
//! runs and gates may still read.
//!
//! The sweep fails closed. Besides what the caller keeps, it keeps:
//!
//! - every artifact any run has recorded: each run's pinned definitions
//!   (its snapshot) and every artifact reference in any step's recorded
//!   output, for every run in the store, finished or not, since a later
//!   step, a gate or a recovery may read it;
//! - every digest a gate in the store pins as evidence, open or resolved,
//!   for as long as the gate's decision and its run's recovery may need it;
//! - anything written within the grace period.
//!
//! Nothing sweeps on its own: a caller decides when.

use atomic_core::artifact::{ArtifactRef, Grace, LocalStore, Swept};
use atomic_durable::{DurableRuntime, Durare};
use serde_json::Value;
use std::collections::BTreeSet;

/// Every artifact reference in `value`: a string `artifact://sha256/<hex>`,
/// or a gate's `sha256:<hex>`, at any depth.
fn references(value: &Value, into: &mut BTreeSet<ArtifactRef>) {
    match value {
        Value::String(s) => {
            if let Ok(a) = s.parse::<ArtifactRef>() {
                into.insert(a);
            } else if let Some(Ok(a)) = s.strip_prefix("sha256:").map(ArtifactRef::from_digest) {
                into.insert(a);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| references(v, into)),
        Value::Object(map) => map.values().for_each(|v| references(v, into)),
        _ => {}
    }
}

/// What the store's runs and gates refer to: [`recorded`] and [`pinned`].
pub async fn referenced(rt: &Durare) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let mut found = recorded(rt).await?;
    found.extend(pinned(rt).await?);
    Ok(found)
}

/// Every artifact reference in any step output any run in the store has
/// recorded, finished or not.
pub async fn recorded(rt: &Durare) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let mut found = BTreeSet::new();
    for run in rt.every_run().await? {
        for step in rt.steps(&run.id).await? {
            if let Some(output) = &step.output {
                references(output, &mut found);
            }
        }
    }
    Ok(found)
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
/// anything written within `grace` (see [`LocalStore::sweep`]).
pub async fn sweep(
    store: &LocalStore,
    rt: &Durare,
    keep: &BTreeSet<ArtifactRef>,
    grace: Grace,
) -> anyhow::Result<Swept> {
    let mut all = keep.clone();
    all.extend(referenced(rt).await?);
    let store = store.clone();
    Ok(tokio::task::spawn_blocking(move || store.sweep(&all, grace)).await??)
}
