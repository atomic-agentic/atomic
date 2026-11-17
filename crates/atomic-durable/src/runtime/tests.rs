// SPDX-License-Identifier: Apache-2.0

//! The trait's own contract, written against [`DurableRuntime`] alone so any
//! engine behind it must pass it, and run on durare over SQLite and (with the
//! `postgres-tests` feature) PostgreSQL:
//!
//! - a step's body runs once; after a crash its recorded output is replayed;
//! - a transaction step commits its writes with its checkpoint, or nothing;
//! - an interrupted execution records nothing and stays pending;
//! - a failure is recorded as the workflow's outcome;
//! - steps are recorded as siblings, in order, and a step inside a step is
//!   refused;
//! - starting a workflow id twice runs it once.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn unique(tag: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{tag}_{}_{n}_{nanos}", std::process::id())
}

async fn sqlite() -> Durare {
    let path = std::env::temp_dir().join(format!("{}.db", unique("atomic_runtime")));
    Durare::sqlite(&path).await.expect("sqlite engine")
}

#[cfg(feature = "postgres-tests")]
async fn postgres() -> Durare {
    // A suite that cannot reach its backend fails; it never skips.
    let url = std::env::var("ATOMIC_TEST_DATABASE_URL")
        .expect("the postgres-tests feature needs ATOMIC_TEST_DATABASE_URL");
    Durare::postgres(&url, &unique("atomic_rt"))
        .await
        .expect("postgres engine")
}

/// Wait until `id` is no longer running in this process: pending with no
/// progress for a moment, or finished.
async fn settle<R: DurableRuntime>(rt: &R, id: &str) -> Option<RunStatus> {
    let until = Instant::now() + Duration::from_secs(10);
    let mut last = None;
    loop {
        let status = rt.status(id).await.expect("status");
        if matches!(status, Some(RunStatus::Succeeded | RunStatus::Failed(_))) {
            return status;
        }
        let steps = rt.steps(id).await.expect("steps").len();
        if last == Some(steps) || Instant::now() > until {
            return status;
        }
        last = Some(steps);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn a_step_runs_once_and_is_replayed_after_a_crash<R: DurableRuntime>(mut rt: R) {
    let bodies = Arc::new(AtomicUsize::new(0));
    let executions = Arc::new(AtomicUsize::new(0));
    let (b, e) = (bodies.clone(), executions.clone());
    rt.register("once", move |ctx: R::Context, (): ()| {
        let (b, e) = (b.clone(), e.clone());
        async move {
            let v: i64 = ctx
                .step("work", move |_| async move {
                    b.fetch_add(1, Ordering::SeqCst);
                    Ok(42)
                })
                .await?;
            if e.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("crash after the step's checkpoint");
            }
            Ok(v)
        }
    });
    rt.launch().await.expect("launch");
    let id = unique("once");
    rt.start("once", &id, ()).await.expect("start");
    assert!(
        rt.result::<i64>(&id).await.is_err(),
        "the first execution crashed"
    );
    rt.recover().await.expect("recover");
    assert_eq!(rt.result::<i64>(&id).await, Ok(42));
    assert_eq!(bodies.load(Ordering::SeqCst), 1, "the step body ran once");
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn a_transaction_commits_its_writes_with_its_checkpoint_or_nothing<R: DurableRuntime>(
    mut rt: R,
) {
    let table = unique("t");
    let t = table.clone();
    rt.register("write", move |ctx: R::Context, fail: bool| {
        let t = t.clone();
        async move {
            ctx.transaction("insert", move |mut tx| {
                let t = t.clone();
                Box::pin(async move {
                    tx.execute(
                        &format!("CREATE TABLE IF NOT EXISTS {t} (v TEXT NOT NULL)"),
                        &[],
                    )
                    .await?;
                    tx.execute(
                        &format!("INSERT INTO {t} (v) VALUES (?)"),
                        &[Param::Text("a".into())],
                    )
                    .await?;
                    if fail {
                        return Err(DurableError::Failed("after the write".into()));
                    }
                    Ok(())
                })
            })
            .await
        }
    });
    let t = table.clone();
    rt.register("count", move |ctx: R::Context, (): ()| {
        let t = t.clone();
        async move {
            ctx.transaction("count", move |mut tx| {
                let t = t.clone();
                Box::pin(async move {
                    tx.execute(
                        &format!("CREATE TABLE IF NOT EXISTS {t} (v TEXT NOT NULL)"),
                        &[],
                    )
                    .await?;
                    tx.query_one(&format!("SELECT COUNT(*) AS n FROM {t}"), &[])
                        .await?
                        .int("n")
                })
            })
            .await
        }
    });
    rt.launch().await.expect("launch");
    let failed = unique("fail");
    rt.start("write", &failed, true).await.expect("start");
    assert_eq!(
        rt.result::<()>(&failed).await,
        Err(DurableError::Failed("after the write".into()))
    );
    assert_eq!(
        count_rows(&rt).await,
        0,
        "a failed transaction writes nothing"
    );
    let ok = unique("ok");
    rt.start("write", &ok, false).await.expect("start");
    assert_eq!(rt.result::<()>(&ok).await, Ok(()));
    assert_eq!(count_rows(&rt).await, 1);
    let names: Vec<String> = rt
        .steps(&ok)
        .await
        .expect("steps")
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["insert"]);
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn count_rows<R: DurableRuntime>(rt: &R) -> i64 {
    let id = unique("count");
    rt.start("count", &id, ()).await.expect("start");
    rt.result::<i64>(&id).await.expect("count")
}

async fn an_interrupted_execution_records_nothing_and_stays_pending<R: DurableRuntime>(mut rt: R) {
    rt.register("interrupted", |ctx: R::Context, (): ()| async move {
        ctx.step("before", |_| async { Ok(1_i64) }).await?;
        ctx.transaction("refused", |_tx| {
            Box::pin(async { Err::<(), _>(DurableError::Interrupted("lost the run".into())) })
        })
        .await
    });
    rt.launch().await.expect("launch");
    let id = unique("interrupted");
    rt.start("interrupted", &id, ()).await.expect("start");
    assert_eq!(settle(&rt, &id).await, Some(RunStatus::Pending));
    let names: Vec<String> = rt
        .steps(&id)
        .await
        .expect("steps")
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(
        names,
        ["before"],
        "nothing recorded for the interrupted step"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn a_failure_is_recorded_as_the_outcome<R: DurableRuntime>(mut rt: R) {
    rt.register("fails", |_ctx: R::Context, (): ()| async move {
        Err::<(), _>(DurableError::Failed("boom".into()))
    });
    rt.launch().await.expect("launch");
    let id = unique("fails");
    rt.start("fails", &id, ()).await.expect("start");
    assert_eq!(
        rt.result::<()>(&id).await,
        Err(DurableError::Failed("boom".into()))
    );
    assert!(matches!(rt.status(&id).await, Ok(Some(RunStatus::Failed(m))) if m.contains("boom")));
    assert_eq!(rt.status(&unique("missing")).await, Ok(None));
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn steps_are_siblings_in_order_and_cannot_nest<R: DurableRuntime>(mut rt: R) {
    rt.register("siblings", |ctx: R::Context, (): ()| async move {
        let inner = ctx.clone();
        let a: (i32, String) = ctx
            .step(
                "a",
                |info| async move { Ok((info.step_id, info.workflow_id)) },
            )
            .await?;
        let nested: String = ctx
            .step("b", move |_| async move {
                // A step inside a step is refused, not run.
                match inner.step("inner", |_| async { Ok(0_i64) }).await {
                    Err(DurableError::Nested(_)) => Ok("refused".to_owned()),
                    other => Ok(format!("{other:?}")),
                }
            })
            .await?;
        let c: (i32, String) = ctx
            .step(
                "c",
                |info| async move { Ok((info.step_id, info.workflow_id)) },
            )
            .await?;
        Ok((a.0, nested, c.0, a.1 == ctx.workflow_id()))
    });
    rt.launch().await.expect("launch");
    let id = unique("siblings");
    rt.start("siblings", &id, ()).await.expect("start");
    let (a, nested, c, same) = rt
        .result::<(i32, String, i32, bool)>(&id)
        .await
        .expect("runs");
    assert_eq!(nested, "refused");
    assert!(a < c && same, "positions {a} < {c}, one workflow");
    let names: Vec<String> = rt
        .steps(&id)
        .await
        .expect("steps")
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["a", "b", "c"]);
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn a_workflow_id_started_twice_runs_once<R: DurableRuntime>(mut rt: R) {
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    rt.register("idempotent", move |ctx: R::Context, (): ()| {
        let r = r.clone();
        async move {
            ctx.step("count", move |_| async move {
                Ok(r.fetch_add(1, Ordering::SeqCst) as i64)
            })
            .await
        }
    });
    rt.launch().await.expect("launch");
    let id = unique("twice");
    rt.start("idempotent", &id, ()).await.expect("start");
    rt.start("idempotent", &id, ()).await.expect("start again");
    assert_eq!(rt.result::<i64>(&id).await, Ok(0));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

macro_rules! on_backend {
    ($backend:ident, $($name:ident),+ $(,)?) => {
        mod $backend {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn $name() {
                    super::$name(super::$backend().await).await
                }
            )+
        }
    };
}

on_backend!(
    sqlite,
    a_step_runs_once_and_is_replayed_after_a_crash,
    a_transaction_commits_its_writes_with_its_checkpoint_or_nothing,
    an_interrupted_execution_records_nothing_and_stays_pending,
    a_failure_is_recorded_as_the_outcome,
    steps_are_siblings_in_order_and_cannot_nest,
    a_workflow_id_started_twice_runs_once,
);

#[cfg(feature = "postgres-tests")]
on_backend!(
    postgres,
    a_step_runs_once_and_is_replayed_after_a_crash,
    a_transaction_commits_its_writes_with_its_checkpoint_or_nothing,
    an_interrupted_execution_records_nothing_and_stays_pending,
    a_failure_is_recorded_as_the_outcome,
    steps_are_siblings_in_order_and_cannot_nest,
    a_workflow_id_started_twice_runs_once,
);
