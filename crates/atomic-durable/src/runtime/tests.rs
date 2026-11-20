// SPDX-License-Identifier: Apache-2.0

//! The trait's own contract, written against [`DurableRuntime`] alone so any
//! engine behind it must pass it, and run on durare over SQLite and (with the
//! `postgres-tests` feature) PostgreSQL:
//!
//! - a step's body runs once; after a crash its recorded output is replayed;
//! - a transaction step commits its writes with its checkpoint, or nothing;
//! - an interrupted execution records nothing and stays pending;
//! - a failure is recorded as the workflow's outcome;
//! - of two executions of one run that race to record the same step, the
//!   one that loses records nothing, and the run ends with the winner's
//!   outcome;
//! - steps are recorded as siblings, in order, and a step inside a step is
//!   refused;
//! - starting a workflow id twice runs it once;
//! - a message sent to a workflow is received once, replayed after a crash,
//!   and a second send with the same key delivers nothing; a receive with
//!   nothing to take times out;
//! - a control transaction outside any workflow commits its writes, or
//!   nothing.

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

/// An engine on a schema of its own, which the returned guard drops.
#[cfg(feature = "postgres-tests")]
async fn postgres() -> (Durare, crate::testing::TestSchema) {
    let schema = crate::testing::schema("rt").await;
    let rt = Durare::postgres(schema.url(), schema.name())
        .await
        .expect("postgres engine");
    (rt, schema)
}

/// Each backend's engine, and what must outlive it.
mod backends {
    use super::Durare;

    pub(super) async fn sqlite() -> (Durare, ()) {
        (super::sqlite().await, ())
    }

    #[cfg(feature = "postgres-tests")]
    pub(super) async fn postgres() -> (Durare, crate::testing::TestSchema) {
        super::postgres().await
    }
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

async fn positions_follow_construction_not_polling_order<R: DurableRuntime>(mut rt: R) {
    rt.register("positions", |ctx: R::Context, (): ()| async move {
        let a = ctx.step("a", |s| async move { Ok(s.step_id) });
        let b = ctx.step("b", |s| async move { Ok(s.step_id) });
        let t = ctx.transaction("t", |_tx| Box::pin(async { Ok(0_i32) }));
        // Awaited in the other order: positions must not follow.
        let t_done = t.await?;
        let b = b.await?;
        let a = a.await?;
        Ok((a, b, t_done))
    });
    rt.launch().await.expect("launch");
    let id = unique("positions");
    rt.start("positions", &id, ()).await.expect("start");
    let (a, b, _): (i32, i32, i32) = rt.result(&id).await.expect("runs");
    assert!(a < b, "constructed a before b, but a={a}, b={b}");
    let steps: Vec<(i32, String)> = rt
        .steps(&id)
        .await
        .expect("steps")
        .into_iter()
        .map(|s| (s.step_id, s.name))
        .collect();
    let mut by_position = steps.clone();
    by_position.sort();
    let names: Vec<&str> = by_position.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(
        names,
        ["a", "b", "t"],
        "positions in construction order: {steps:?}"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn a_message_is_received_once_and_replayed_after_a_crash<R: DurableRuntime>(mut rt: R) {
    let executions = Arc::new(AtomicUsize::new(0));
    let e = executions.clone();
    rt.register("receive", move |ctx: R::Context, (): ()| {
        let e = e.clone();
        async move {
            let first: Option<String> = ctx.recv("topic", Duration::from_secs(30)).await?;
            if e.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("crash after the receive's checkpoint");
            }
            // The second send carried the same key, so nothing else waits.
            let second: Option<String> = ctx.recv("topic", Duration::from_millis(300)).await?;
            Ok((first, second))
        }
    });
    rt.launch().await.expect("launch");
    let id = unique("receive");
    rt.start("receive", &id, ()).await.expect("start");
    for _ in 0..2 {
        rt.send(&id, "topic", "hello", "key-1").await.expect("send");
    }
    assert!(
        rt.result::<(Option<String>, Option<String>)>(&id)
            .await
            .is_err(),
        "the first execution crashed"
    );
    rt.recover().await.expect("recover");
    assert_eq!(
        rt.result::<(Option<String>, Option<String>)>(&id).await,
        Ok((Some("hello".to_owned()), None)),
        "the message taken before the crash is replayed, and taken once"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn a_receive_times_out_at_its_deadline<R: DurableRuntime>(mut rt: R) {
    rt.register("timeout", |ctx: R::Context, (): ()| async move {
        let got: Option<String> = ctx.recv("nothing", Duration::from_millis(500)).await?;
        Ok(got)
    });
    rt.launch().await.expect("launch");
    let id = unique("timeout");
    let started = Instant::now();
    rt.start("timeout", &id, ()).await.expect("start");
    assert_eq!(rt.result::<Option<String>>(&id).await, Ok(None));
    assert!(started.elapsed() >= Duration::from_millis(500));
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn a_control_transaction_commits_its_writes_or_nothing<R: DurableRuntime>(rt: R) {
    let table = unique("c");
    let write = |fail: bool| {
        let t = table.clone();
        rt.control_transaction(move |mut tx| {
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
    };
    write(false).await.expect("commits");
    assert_eq!(
        write(true).await,
        Err(DurableError::Failed("after the write".into()))
    );
    let t = table.clone();
    let n = rt
        .control_transaction(move |mut tx| {
            let t = t.clone();
            Box::pin(async move {
                tx.query_one(
                    &format!("SELECT COUNT(*) AS n FROM {t} WHERE v = ?"),
                    &[Param::Text("a".into())],
                )
                .await?
                .int("n")
            })
        })
        .await
        .expect("count");
    assert_eq!(n, 1, "the failed transaction wrote nothing");
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn control_authorization_is_rechecked_on_workflow_replay<R: DurableRuntime>(mut rt: R) {
    let executions = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let (counter, fresh) = (executions.clone(), reads.clone());
    rt.register("control-replay", move |ctx: R::Context, (): ()| {
        let (counter, fresh) = (counter.clone(), fresh.clone());
        async move {
            let current = ctx
                .control_transaction(move |mut tx| {
                    let fresh = fresh.clone();
                    Box::pin(async move {
                        assert_eq!(
                            tx.query_one("SELECT 42 AS value", &[])
                                .await?
                                .int("value")?,
                            42
                        );
                        Ok(fresh.fetch_add(1, Ordering::SeqCst))
                    })
                })
                .await?;
            let saved = ctx
                .step("checkpoint", move |_| async move { Ok(current) })
                .await?;
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(DurableError::Interrupted("replay authorization".into()));
            }
            Ok((current, saved))
        }
    });
    rt.launch().await.expect("launch");
    let run = unique("control-replay");
    rt.start("control-replay", &run, ()).await.expect("start");
    assert!(matches!(
        rt.result::<(usize, usize)>(&run).await,
        Err(DurableError::Interrupted(_))
    ));
    rt.recover().await.expect("recover");
    assert_eq!(
        rt.result::<(usize, usize)>(&run).await.expect("replayed"),
        (1, 0)
    );
    assert_eq!(
        rt.steps(&run).await.expect("steps").len(),
        1,
        "control reads do not consume durable operation positions"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn the_loser_of_a_checkpoint_race_takes_the_winners_outcome<R: DurableRuntime>(mut rt: R) {
    let entered = Arc::new(AtomicUsize::new(0));
    let meet = Arc::new(tokio::sync::Barrier::new(2));
    let lost = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let (e, m, l, r) = (entered.clone(), meet.clone(), lost.clone(), release.clone());
    rt.register("raced", move |ctx: R::Context, (): ()| {
        let (entered, meet, lost, release) = (e.clone(), m.clone(), l.clone(), r.clone());
        async move {
            let contested = ctx
                .step("contested", move |_| {
                    let (entered, meet) = (entered.clone(), meet.clone());
                    async move {
                        // The first execution stops short, which schedules
                        // its settlement; the next two meet here, so both
                        // have run the body before either records it.
                        if entered.fetch_add(1, Ordering::SeqCst) == 0 {
                            return Err(DurableError::Interrupted("stopped short".into()));
                        }
                        let met =
                            tokio::time::timeout(Duration::from_secs(20), meet.wait()).await;
                        Ok(met.is_ok())
                    }
                })
                .await;
            // The execution that lost the race to record the step.
            if matches!(&contested, Err(e) if !matches!(e, DurableError::Interrupted(m) if m == "stopped short"))
            {
                lost.fetch_add(1, Ordering::SeqCst);
            }
            let met = contested?;
            // The winner goes on only once the test has seen what the loser
            // did with its loss.
            ctx.step("after", move |_| {
                let release = release.clone();
                async move {
                    let _ = tokio::time::timeout(Duration::from_secs(20), release.notified()).await;
                    Ok(())
                }
            })
            .await?;
            Ok(met)
        }
    });
    rt.launch().await.expect("launch");
    let id = unique("raced");
    rt.start("raced", &id, ()).await.expect("start");
    // The settlement durare scheduled is in the step; a recovery now runs
    // the run a second time, alongside it.
    let until = Instant::now() + Duration::from_secs(20);
    while entered.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < until, "the settlement never ran the step");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    rt.recover().await.expect("recover");
    while lost.load(Ordering::SeqCst) < 1 {
        assert!(Instant::now() < until, "neither execution lost the race");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // While the winner waits, the loser has lost: it must record nothing.
    // Recording takes it milliseconds; three seconds is ample.
    let watch = Instant::now() + Duration::from_secs(3);
    while Instant::now() < watch {
        let status = rt.status(&id).await.expect("status");
        assert_eq!(
            status,
            Some(RunStatus::Pending),
            "the execution that lost the race recorded an outcome"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    release.notify_one();
    let until = Instant::now() + Duration::from_secs(60);
    let status = loop {
        let status = rt.status(&id).await.expect("status");
        if matches!(status, Some(RunStatus::Succeeded | RunStatus::Failed(_)))
            || Instant::now() > until
        {
            break status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(entered.load(Ordering::SeqCst), 3, "two executions raced");
    assert_eq!(
        status,
        Some(RunStatus::Succeeded),
        "the run takes the outcome of the execution that recorded the step"
    );
    let steps = rt.steps(&id).await.expect("steps");
    assert_eq!(steps.len(), 2, "{steps:?}");
    assert_eq!(
        steps[0].output,
        Some(serde_json::json!(true)),
        "the winner met the loser"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

macro_rules! on_backend {
    ($backend:ident, $($name:ident),+ $(,)?) => {
        mod $backend {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn $name() {
                    // The schema, if any, outlives the engine.
                    let (rt, _schema) = super::backends::$backend().await;
                    super::$name(rt).await
                }
            )+
        }
    };
}

on_backend!(
    sqlite,
    control_authorization_is_rechecked_on_workflow_replay,
    positions_follow_construction_not_polling_order,
    a_step_runs_once_and_is_replayed_after_a_crash,
    a_transaction_commits_its_writes_with_its_checkpoint_or_nothing,
    an_interrupted_execution_records_nothing_and_stays_pending,
    a_failure_is_recorded_as_the_outcome,
    the_loser_of_a_checkpoint_race_takes_the_winners_outcome,
    steps_are_siblings_in_order_and_cannot_nest,
    a_workflow_id_started_twice_runs_once,
    a_message_is_received_once_and_replayed_after_a_crash,
    a_receive_times_out_at_its_deadline,
    a_control_transaction_commits_its_writes_or_nothing,
);

#[cfg(feature = "postgres-tests")]
on_backend!(
    postgres,
    control_authorization_is_rechecked_on_workflow_replay,
    positions_follow_construction_not_polling_order,
    a_step_runs_once_and_is_replayed_after_a_crash,
    a_transaction_commits_its_writes_with_its_checkpoint_or_nothing,
    an_interrupted_execution_records_nothing_and_stays_pending,
    a_failure_is_recorded_as_the_outcome,
    the_loser_of_a_checkpoint_race_takes_the_winners_outcome,
    steps_are_siblings_in_order_and_cannot_nest,
    a_workflow_id_started_twice_runs_once,
    a_message_is_received_once_and_replayed_after_a_crash,
    a_receive_times_out_at_its_deadline,
    a_control_transaction_commits_its_writes_or_nothing,
);

/// A serialization conflict raised once in a transaction step is retried by
/// the engine, not recorded as the step's failure.
#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_postgres_serialization_conflict_is_retried() {
    let (mut rt, _schema) = postgres().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let a = attempts.clone();
    rt.register("conflict", move |ctx: DurareContext, (): ()| {
        let a = a.clone();
        async move {
            ctx.transaction("setup", |mut tx| {
                Box::pin(async move {
                    tx.execute("CREATE SEQUENCE IF NOT EXISTS conflict_attempt", &[])
                        .await?;
                    Ok(())
                })
            })
            .await?;
            ctx.transaction("retried", move |mut tx| {
                a.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    tx.execute(
                        "DO $$ BEGIN IF nextval('conflict_attempt') = 1 THEN \
                         RAISE EXCEPTION 'synthetic conflict' USING ERRCODE = '40001'; END IF; END $$",
                        &[],
                    )
                    .await?;
                    Ok(42_i64)
                })
            })
            .await
        }
    });
    rt.launch().await.expect("launch");
    let id = unique("conflict");
    rt.start("conflict", &id, ()).await.expect("start");
    assert_eq!(rt.result::<i64>(&id).await, Ok(42));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "one conflict, one retry"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

/// A store written by a newer atomic is refused before the engine opens it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_store_is_refused_untouched() {
    let path = std::env::temp_dir().join(format!("{}.db", unique("newer")));
    crate::store::open_sqlite(&path)
        .await
        .expect("a current store");
    let url = format!("sqlite://{}", path.display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("pool");
    sqlx::query("UPDATE atomic_store_version SET version = 999")
        .execute(&pool)
        .await
        .expect("mark newer");
    let tables = |pool: sqlx::SqlitePool| async move {
        let n: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'")
            .fetch_one(&pool)
            .await
            .expect("count");
        n.0
    };
    let before = tables(pool.clone()).await;
    let opened = Durare::sqlite(&path).await;
    assert!(
        matches!(&opened, Err(DurableError::Engine(m)) if m.contains("999")),
        "refused: {:?}",
        opened.as_ref().err()
    );
    assert_eq!(
        tables(pool.clone()).await,
        before,
        "nothing was created in it"
    );
    pool.close().await;
}

/// On PostgreSQL too, a schema written by a newer atomic is refused before
/// anything is created in it.
#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_postgres_store_is_refused_untouched() {
    let guard = crate::testing::schema("newer").await;
    let (url, schema) = (guard.url().to_owned(), guard.name().to_owned());
    let rt = Durare::postgres(&url, &schema)
        .await
        .expect("a current store");
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    let pool = sqlx::PgPool::connect(&url).await.expect("pool");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {schema}.atomic_store_version SET version = 999"
    )))
    .execute(&pool)
    .await
    .expect("mark newer");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TABLE {schema}.atomic_threads"
    )))
    .execute(&pool)
    .await
    .expect("drop a table the runtime would create");
    let tables = |pool: sqlx::PgPool, schema: String| async move {
        let n: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = $1",
        )
        .bind(schema)
        .fetch_one(&pool)
        .await
        .expect("count");
        n.0
    };
    let before = tables(pool.clone(), schema.clone()).await;
    let opened = Durare::postgres(&url, &schema).await;
    assert!(
        matches!(&opened, Err(DurableError::Engine(m)) if m.contains("999")),
        "refused: {:?}",
        opened.as_ref().err()
    );
    assert_eq!(
        tables(pool.clone(), schema).await,
        before,
        "nothing was created"
    );
    pool.close().await;
}
