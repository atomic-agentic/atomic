// SPDX-License-Identifier: Apache-2.0

//! Retention: sweeping the artifact store while keeping what gates pin.
//!
//! A gate's evidence is content in the artifact store, named by digest in
//! the gate store. A decision, and a run's recovery, may need it for as long
//! as the gate is in the store, open or resolved, so a sweep keeps every
//! digest a gate pins, whatever the caller asks to keep besides. Nothing
//! sweeps on its own: a caller decides when, and what else is referenced.

use atomic_core::artifact::{LocalStore, Swept};
use atomic_durable::DurableRuntime;
use std::collections::BTreeSet;
use std::time::Duration;

/// Sweep `store`, keeping `keep`, every digest a gate in `rt`'s store pins,
/// and anything written within `grace` (see [`LocalStore::sweep`]).
pub async fn sweep<R: DurableRuntime>(
    store: &LocalStore,
    rt: &R,
    keep: &BTreeSet<String>,
    grace: Duration,
) -> anyhow::Result<Swept> {
    let mut all = keep.clone();
    all.extend(atomic_durable::gate::pinned_evidence(rt).await?);
    let store = store.clone();
    Ok(tokio::task::spawn_blocking(move || store.sweep(&all, grace)).await??)
}
