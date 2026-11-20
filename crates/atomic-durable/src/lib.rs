// SPDX-License-Identifier: Apache-2.0

//! The `DurableRuntime` trait and its adapter.
//!
//! No other atomic crate names the durable-execution engine; workflows reach it
//! only through this crate.

pub mod attempt;
pub mod failpoints;
pub mod gate;
pub mod publication;
pub mod runtime;
pub mod store;
#[cfg(feature = "postgres-tests")]
pub mod testing;
pub mod usage;

pub use failpoints::failpoint;
pub use publication::{
    Acceptance, Candidate, Park, Precondition, Proposal, Published, Write, execute_candidate,
    execute_candidate_at, parks, publish_candidate, release, review_candidate, run_atom,
    run_atom_at,
};
pub use runtime::{
    DEFAULT_VERSION, Database, DurableError, DurableRuntime, Durare, DurareContext,
    ENGINE_IDENTITY_VARIABLE, ENGINE_VERSION_VARIABLE, IdentityError, Param, RESUME_QUEUES, Result,
    Row, RunContents, RunRecord, RunStatus, StepInfo, StepRecord, ThreadRecord, Tx, TxFuture,
    WorkflowContext, executor_identity,
};

use std::path::Path;
use std::sync::Arc;

impl From<durare::Error> for DurableError {
    fn from(e: durare::Error) -> Self {
        runtime::from_durare(e)
    }
}

/// Open (or create) the durable store at `database` and run one trivial
/// workflow through it. Returns how many probe workflows the store has run,
/// this one included. A diagnostic: it proves the engine links and works in
/// this process.
pub async fn probe(database: &Path) -> std::result::Result<usize, DurableError> {
    store::open_sqlite(database)
        .await
        .map_err(|e| DurableError::Engine(e.to_string()))?;
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
