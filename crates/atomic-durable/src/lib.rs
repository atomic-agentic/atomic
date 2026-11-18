// SPDX-License-Identifier: Apache-2.0

//! The `DurableRuntime` trait and its adapter.
//!
//! No other atomic crate names the durable-execution engine; workflows reach it
//! only through this crate.

pub mod gate;
pub mod publication;
pub mod runtime;

/// A named point where a crash test can kill the process: with the
/// `failpoints` feature, when the `ATOMIC_FAILPOINT` environment variable
/// equals `name`, the process aborts here, as if killed. Without the feature
/// it does nothing.
pub fn failpoint(name: &str) {
    #[cfg(feature = "failpoints")]
    if std::env::var("ATOMIC_FAILPOINT").is_ok_and(|f| f == name) {
        eprintln!("failpoint {name}: aborting");
        std::process::abort();
    }
    #[cfg(not(feature = "failpoints"))]
    let _ = name;
}
pub mod store;

pub use publication::{
    Acceptance, Candidate, Precondition, Proposal, Published, Write, execute_candidate,
    publish_candidate, review_candidate, run_atom,
};
pub use runtime::{
    Database, DurableError, DurableRuntime, Durare, DurareContext, ENGINE_IDENTITY_VARIABLE,
    IdentityError, Param, Result, Row, RunStatus, StepInfo, StepRecord, ThreadRecord, Tx, TxFuture,
    WorkflowContext, executor_identity,
};

/// Whether this build runs the PostgreSQL variants of the tests (the
/// `postgres-tests` feature), for test suites in other crates, which cannot
/// see this crate's features. With it on, a missing database fails them.
pub const POSTGRES_TESTS: bool = cfg!(feature = "postgres-tests");

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
