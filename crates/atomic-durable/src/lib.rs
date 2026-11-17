// SPDX-License-Identifier: Apache-2.0

//! The `DurableRuntime` trait and its adapter.
//!
//! No other atomic crate names the durable-execution engine; workflows reach it
//! only through this crate.

mod publication;

use std::path::Path;
use std::sync::Arc;

/// A failure reported by the durable-execution engine.
#[derive(Debug, thiserror::Error)]
#[error("durable store: {0}")]
pub struct DurableError(String);

impl From<durare::Error> for DurableError {
    fn from(e: durare::Error) -> Self {
        Self(e.to_string())
    }
}

/// Open (or create) the durable store at `database` and run one trivial
/// workflow through it. Returns how many probe workflows the store has run,
/// this one included. A diagnostic: it proves the engine links and works in
/// this process.
pub async fn probe(database: &Path) -> Result<usize, DurableError> {
    let url = format!("sqlite://{}?mode=rwc", database.display());
    let provider = Arc::new(durare::SqliteProvider::connect(&url).await?);
    let mut engine = durare::DurableEngine::new(provider).await?;
    engine.register(
        "atomic.probe",
        |ctx: durare::DurableContext, (): ()| async move {
            ctx.step("probe", || async { Ok::<_, durare::Error>(1_i64) })
                .await
        },
    );
    engine.launch().await?;
    engine
        .start::<_, i64>("atomic.probe", (), durare::WorkflowOptions::default())
        .await?
        .result()
        .await?;
    let runs = engine
        .list_workflows(&durare::ListFilter {
            name: vec!["atomic.probe".into()],
            ..durare::ListFilter::default()
        })
        .await?
        .len();
    engine.shutdown(std::time::Duration::from_secs(5)).await?;
    Ok(runs)
}
