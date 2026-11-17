// SPDX-License-Identifier: Apache-2.0

//! A step started inside another step's body is refused.
//!
//! atomic composes an atom's turn, its reviewers and each repair round as
//! sibling steps in workflow code, never as steps inside steps. It relies on
//! durare refusing a nested durable call outright, rather than recording a
//! checkpoint at a position a replay would not reach the same way.

use durare::{
    DurableContext, DurableEngine, Error, ErrorCode, InMemoryProvider, Result, SqliteProvider,
    StateProvider, WorkflowOptions,
};
use std::sync::Arc;

async fn check_on(provider: Arc<dyn StateProvider>) -> Result<()> {
    let mut engine = DurableEngine::new(provider).await?;
    engine.register("nests", |ctx: DurableContext, (): ()| async move {
        let inner_ctx = ctx.clone();
        let outer: Result<i64> = ctx
            .step("outer", move || async move {
                // The nested call: refused before it claims a position.
                inner_ctx
                    .step("inner", || async { Ok::<_, Error>(1_i64) })
                    .await
            })
            .await;
        let after = ctx
            .step("after", || async { Ok::<_, Error>(2_i64) })
            .await?;
        let code = outer.as_ref().err().map(|e| e.code());
        Ok((code.map(|c| format!("{c:?}")), after))
    });
    engine.launch().await?;

    let (code, after): (Option<String>, i64) = engine
        .start("nests", (), WorkflowOptions::with_id("nests-1"))
        .await?
        .result()
        .await?;
    assert_eq!(
        code.as_deref(),
        Some(format!("{:?}", ErrorCode::NestedDurableCall).as_str())
    );
    assert_eq!(after, 2);

    // Only the two sibling steps are recorded, at consecutive positions: the
    // refused inner call took no position and left no checkpoint.
    let steps: Vec<(i32, String)> = engine
        .get_workflow_steps("nests-1")
        .await?
        .into_iter()
        .map(|s| (s.step_id, s.name))
        .collect();
    assert_eq!(steps, [(0, "outer".to_string()), (1, "after".to_string())]);
    engine.shutdown(std::time::Duration::from_secs(5)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_step_inside_a_step_is_refused_in_memory() -> Result<()> {
    check_on(Arc::new(InMemoryProvider::new())).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_step_inside_a_step_is_refused_on_sqlite() -> Result<()> {
    let db = std::env::temp_dir().join(format!("atomic-nested-steps-{}.db", std::process::id()));
    let url = format!("sqlite://{}?mode=rwc", db.display());
    let result = check_on(Arc::new(SqliteProvider::connect(&url).await?)).await;
    let _ = std::fs::remove_file(&db);
    result
}
