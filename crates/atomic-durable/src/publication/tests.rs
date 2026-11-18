// SPDX-License-Identifier: Apache-2.0

//! The adapter's contract, on SQLite and (with the `postgres-tests` feature)
//! Postgres:
//!
//! - a crash after the candidate checkpoint and before publication reuses the
//!   candidate, with no second author call;
//! - publication commits the result and every write together, or nothing;
//! - publishing the same candidate twice writes once;
//! - a publication whose precondition fails commits nothing, records no
//!   checkpoint and interrupts the execution;
//! - a publication and a takeover that race are ordered by the fence's row
//!   lock: a takeover waits for a publication already past its fence, and a
//!   publication that meets a takeover in progress is refused once the
//!   takeover commits.
//!
//! The races interleave on PostgreSQL only. On SQLite every write
//! transaction begins IMMEDIATE and so runs alone: a publication and a
//! takeover cannot interleave, and holding one open to stage the race makes
//! durare's own reads fail with SQLITE_BUSY instead of waiting. SQLite runs
//! the same two orderings one after the other, with the same assertions.

use super::*;
use crate::runtime::{DurableRuntime, Durare, DurareContext, RunStatus};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// Where a test holds a publication inside its transaction, just after its
/// fences: (signalled when it gets there, waited on before it goes on).
type Pause = Arc<(Notify, Notify)>;

/// Pause points by workflow id; a candidate's id starts with its workflow's.
static PAUSES: LazyLock<Mutex<HashMap<String, Pause>>> = LazyLock::new(Mutex::default);

fn pause_after_fences(workflow_id: &str) -> Pause {
    let pause: Pause = Arc::default();
    PAUSES
        .lock()
        .expect("pauses")
        .insert(format!("{workflow_id}:"), pause.clone());
    pause
}

/// Called by the publication transaction after its fences hold.
pub(super) async fn after_fences(candidate_id: &str) {
    let pause = PAUSES.lock().ok().and_then(|p| {
        p.iter()
            .find(|(prefix, _)| candidate_id.starts_with(prefix.as_str()))
            .map(|(_, pause)| pause.clone())
    });
    if let Some(pause) = pause {
        pause.0.notify_one();
        pause.1.notified().await;
    }
}

/// The fence an execution that owns run-1 at epoch 1 publishes behind.
const FENCE: &str = "UPDATE run_epochs SET epoch = epoch WHERE epoch = ? AND run = ?";
/// A fence that cannot run at all.
const BROKEN: &str = "UPDATE no_such_table SET x = 1 WHERE ? = ?";
/// A read of the same condition: true, but it locks nothing.
const READ: &str =
    "SELECT CASE WHEN epoch = ? THEN 1 ELSE 0 END AS ok FROM run_epochs WHERE run = ?";

/// A name no other test in this process uses. The clock alone is not enough:
/// macOS reports it in microseconds, and parallel tests start together.
fn unique(tag: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{tag}_{}_{n}_{nanos}", std::process::id())
}

async fn sqlite() -> Result<Durare> {
    let db = std::env::temp_dir().join(format!("{}.db", unique("atomic_publication")));
    Durare::sqlite(&db).await
}

#[cfg(feature = "postgres-tests")]
async fn postgres() -> Result<Durare> {
    // A suite that cannot reach its backend fails; it never skips.
    let url = std::env::var("ATOMIC_TEST_DATABASE_URL")
        .expect("the postgres-tests feature needs ATOMIC_TEST_DATABASE_URL");
    Durare::postgres(&url, &unique("atomic_pub")).await
}

/// Rows in the tables these tests write to, read through a transaction.
async fn counts(engine: &Durare) -> Result<(i64, i64)> {
    let id = unique("audit");
    engine.start("audit", &id, ()).await?;
    engine.result(&id).await
}

fn register_common(engine: &mut Durare) {
    engine.register("setup", |ctx: DurareContext, epoch: i64| async move {
        ctx.transaction("tables", move |mut tx| {
            Box::pin(async move {
                tx.execute(
                    "CREATE TABLE IF NOT EXISTS items (name TEXT NOT NULL)",
                    &[],
                )
                .await?;
                tx.execute(
                    "CREATE TABLE IF NOT EXISTS run_epochs (run TEXT PRIMARY KEY, epoch BIGINT NOT NULL)",
                    &[],
                )
                .await?;
                tx.execute(
                    "CREATE TABLE IF NOT EXISTS atomic_publications (
                         publication_key TEXT PRIMARY KEY,
                         digest TEXT NOT NULL,
                         output TEXT NOT NULL
                     )",
                    &[],
                )
                .await?;
                tx.execute("DELETE FROM run_epochs", &[]).await?;
                tx.execute(
                    "INSERT INTO run_epochs (run, epoch) VALUES ('run-1', ?)",
                    &[Param::Int(epoch)],
                )
                .await?;
                Ok(())
            })
        })
        .await
    });
    engine.register("audit", |ctx: DurareContext, (): ()| async move {
        ctx.transaction("count", |mut tx| {
            Box::pin(async move {
                let items = tx.query_one("SELECT COUNT(*) AS n FROM items", &[]).await?;
                let pubs = tx
                    .query_one("SELECT COUNT(*) AS n FROM atomic_publications", &[])
                    .await?;
                Ok((items.int("n")?, pubs.int("n")?))
            })
        })
        .await
    });
}

/// Wait until execution `id` has checkpointed its candidate and had its
/// publication refused: interrupted, not finished, with no publish checkpoint.
async fn assert_refused(engine: &Durare, id: &str) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let steps = engine.steps(id).await?;
        if steps.iter().any(|s| s.name == "turn") || Instant::now() > until {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let infos = engine.steps(id).await?;
    let steps: Vec<String> = infos.into_iter().map(|s| s.name).collect();
    assert_eq!(steps, ["turn"], "no checkpoint for the refused publication");
    let status = engine.status(id).await?;
    assert_eq!(
        status,
        Some(RunStatus::Pending),
        "interrupted, not finished"
    );
    Ok(())
}

async fn setup(engine: &Durare, epoch: i64) -> Result<()> {
    let id = unique("setup");
    engine.start("setup", &id, epoch).await?;
    engine.result(&id).await
}

/// Start workflow `name` as `id` and wait for its output.
async fn run<I, O>(engine: &Durare, name: &str, id: &str, input: I) -> Result<O>
where
    I: serde::Serialize + Send,
    O: DeserializeOwned + Send,
{
    engine.start(name, id, input).await?;
    engine.result(id).await
}

fn insert(name: Option<&str>) -> Write {
    Write {
        sql: "INSERT INTO items (name) VALUES (?)".into(),
        params: vec![name.map_or(Value::Null, |n| json!(n))],
    }
}

// ---------------------------------------------------------------------------

async fn candidate_survives_a_crash_before_publication(mut engine: Durare) -> Result<()> {
    let author_calls = Arc::new(AtomicUsize::new(0));
    let body_runs = Arc::new(AtomicUsize::new(0));
    register_common(&mut engine);
    let (calls, runs) = (author_calls.clone(), body_runs.clone());
    engine.register("atom", move |ctx: DurareContext, (): ()| {
        let (calls, runs) = (calls.clone(), runs.clone());
        async move {
            let candidate = execute_candidate(&ctx, "turn", move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Proposal {
                    output: json!({"answer": 42}),
                    metadata: json!({}),
                    writes: vec![insert(Some("a")), insert(Some("b"))],
                })
            })
            .await?;
            if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                // The engine treats a panic in a workflow body as a crash: the run
                // stays PENDING with its checkpoints, and recovery re-dispatches it.
                panic!("crash after the candidate checkpoint, before publication");
            }
            publish_candidate(
                &ctx,
                "publish",
                &candidate,
                &review_candidate(&candidate),
                Vec::new(),
            )
            .await
        }
    });
    engine.launch().await?;
    setup(&engine, 1).await?;

    engine.start("atom", "atom-1", ()).await?;
    assert!(
        engine.result::<Value>("atom-1").await.is_err(),
        "the first execution crashed"
    );
    // Nothing reached the tables before publication.
    assert_eq!(counts(&engine).await?, (0, 0));

    engine.recover().await?;
    let output: Value = engine.result("atom-1").await?;
    assert_eq!(output, json!({"answer": 42}));
    assert_eq!(
        author_calls.load(Ordering::SeqCst),
        1,
        "no second author call"
    );
    assert_eq!(body_runs.load(Ordering::SeqCst), 2);
    assert_eq!(counts(&engine).await?, (2, 1));
    engine.shutdown(Duration::from_secs(5)).await
}

async fn publication_is_all_or_nothing(mut engine: Durare) -> Result<()> {
    register_common(&mut engine);
    engine.register(
        "atom",
        |ctx: DurareContext, writes: Vec<Write>| async move {
            let candidate = execute_candidate(&ctx, "turn", move || async move {
                Ok(Proposal {
                    output: json!("done"),
                    metadata: json!({}),
                    writes,
                })
            })
            .await?;
            let once = publish_candidate(
                &ctx,
                "publish",
                &candidate,
                &review_candidate(&candidate),
                Vec::new(),
            )
            .await?;
            // The same candidate again: the publication key returns what was
            // published, with no second set of writes.
            let twice = publish_candidate(
                &ctx,
                "publish-again",
                &candidate,
                &review_candidate(&candidate),
                Vec::new(),
            )
            .await?;
            assert_eq!(once, twice);
            Ok(once)
        },
    );
    engine.launch().await?;
    setup(&engine, 1).await?;

    // The second write violates NOT NULL, so neither write, nor the
    // publication, commits.
    let failed = run::<_, Value>(
        &engine,
        "atom",
        "atom-bad",
        vec![insert(Some("a")), insert(None)],
    )
    .await;
    assert!(failed.is_err(), "a failing write fails the publication");
    assert_eq!(counts(&engine).await?, (0, 0));

    let ok: Value = run(
        &engine,
        "atom",
        "atom-good",
        vec![insert(Some("a")), insert(Some("b"))],
    )
    .await?;
    assert_eq!(ok, json!("done"));
    assert_eq!(
        counts(&engine).await?,
        (2, 1),
        "both writes once, one publication"
    );
    engine.shutdown(Duration::from_secs(5)).await
}

/// `epoch_now` is the run's epoch when publication happens; the execution
/// expects 1 and publishes behind `fence`.
async fn a_failed_precondition_commits_nothing(
    mut engine: Durare,
    epoch_now: i64,
    fence: &'static str,
) -> Result<bool> {
    register_common(&mut engine);
    engine.register("atom", move |ctx: DurareContext, (): ()| async move {
        let candidate = execute_candidate(&ctx, "turn", || async {
            Ok(Proposal {
                output: json!("published"),
                metadata: json!({}),
                writes: vec![insert(Some("a"))],
            })
        })
        .await?;
        // This execution believes it owns run-1 at epoch 1.
        let fence = Precondition {
            what: "run-1 is still at epoch 1".into(),
            sql: fence.into(),
            params: vec![json!(1), json!("run-1")],
        };
        publish_candidate(
            &ctx,
            "publish",
            &candidate,
            &review_candidate(&candidate),
            vec![fence],
        )
        .await
    });
    engine.launch().await?;
    // Another owner may have taken the run over and moved it to a new epoch.
    setup(&engine, epoch_now).await?;

    let id = unique("fenced");
    engine.start("atom", &id, ()).await?;
    let published = if epoch_now == 1 && fence == FENCE {
        assert_eq!(engine.result::<Value>(&id).await?, json!("published"));
        true
    } else {
        // The execution is interrupted, not failed.
        assert_refused(&engine, &id).await?;
        false
    };
    let rows = counts(&engine).await?;
    engine.shutdown(Duration::from_secs(5)).await?;
    if published {
        assert_eq!(rows, (1, 1));
    } else {
        assert_eq!(rows, (0, 0), "a refused publication writes nothing");
    }
    Ok(published)
}

/// Register the racing publication and the takeover. The takeover moves run-1
/// to epoch 2 and returns how many publications it then sees; with `hold` it
/// signals `taken` after its update and commits only once `release` fires.
fn register_race(engine: &mut Durare, taken: Arc<Notify>, release: Arc<Notify>) {
    engine.register("racer", |ctx: DurareContext, (): ()| async move {
        let candidate = execute_candidate(&ctx, "turn", || async {
            Ok(Proposal {
                output: json!("published"),
                metadata: json!({}),
                writes: vec![insert(Some("a"))],
            })
        })
        .await?;
        let fence = Precondition {
            what: "run-1 is still at epoch 1".into(),
            sql: FENCE.into(),
            params: vec![json!(1), json!("run-1")],
        };
        publish_candidate(
            &ctx,
            "publish",
            &candidate,
            &review_candidate(&candidate),
            vec![fence],
        )
        .await
    });
    engine.register("takeover", move |ctx: DurareContext, hold: bool| {
        let (taken, release) = (taken.clone(), release.clone());
        async move {
            ctx.transaction("take", move |mut tx| {
                let (taken, release) = (taken.clone(), release.clone());
                Box::pin(async move {
                    tx.execute("UPDATE run_epochs SET epoch = 2 WHERE run = 'run-1'", &[])
                        .await?;
                    if hold {
                        taken.notify_one();
                        release.notified().await;
                    }
                    let n = tx
                        .query_one("SELECT COUNT(*) AS n FROM atomic_publications", &[])
                        .await?;
                    n.int("n")
                })
            })
            .await
        }
    });
}

async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// A publication already past its fence holds the ownership row: a takeover
/// waits for it to commit, and then sees the publication. Without
/// `interleave`, the takeover simply runs after the publication.
async fn a_takeover_waits_for_a_fenced_publication(
    mut engine: Durare,
    interleave: bool,
) -> Result<()> {
    register_common(&mut engine);
    register_race(&mut engine, Arc::default(), Arc::default());
    engine.launch().await?;
    let engine = Arc::new(engine);
    setup(&engine, 1).await?;

    let id = unique("race_publish_first");
    let pause = interleave.then(|| pause_after_fences(&id));
    engine.start("racer", &id, ()).await?;
    match &pause {
        Some(pause) => {
            within("the publication to pass its fence", pause.0.notified()).await;
        }
        None => {
            let out: Value = within("the publication", engine.result(&id)).await?;
            assert_eq!(out, json!("published"));
        }
    }

    let takeover_id = unique("takeover");
    engine.start("takeover", &takeover_id, false).await?;
    let waiter = engine.clone();
    let takeover = tokio::spawn(async move { waiter.result::<i64>(&takeover_id).await });
    if let Some(pause) = pause {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !takeover.is_finished(),
            "the takeover must wait while a fenced publication is open"
        );
        pause.1.notify_one();
        let out: Value = within("the publication", engine.result(&id)).await?;
        assert_eq!(out, json!("published"));
    }
    let seen = within("the takeover", takeover)
        .await
        .map_err(|e| DurableError::Engine(e.to_string()))??;
    assert_eq!(seen, 1, "the takeover is ordered after the publication");
    assert_eq!(counts(&engine).await?, (1, 1));
    engine.shutdown(Duration::from_secs(5)).await
}

/// A takeover that has moved the epoch but not yet committed holds the row: a
/// publication waits at its fence, finds the new epoch once the takeover
/// commits, and is refused with nothing written. Without `interleave`, the
/// publication simply runs after the takeover.
async fn a_publication_meeting_a_takeover_is_refused(
    mut engine: Durare,
    interleave: bool,
) -> Result<()> {
    let (taken, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    register_common(&mut engine);
    register_race(&mut engine, taken.clone(), release.clone());
    engine.launch().await?;
    setup(&engine, 1).await?;

    let takeover = unique("takeover");
    engine.start("takeover", &takeover, interleave).await?;
    let id = unique("race_takeover_first");
    if interleave {
        within("the takeover to update the epoch", taken.notified()).await;
        engine.start("racer", &id, ()).await?;
        // Let the publication reach its fence and wait there.
        tokio::time::sleep(Duration::from_millis(500)).await;
        release.notify_one();
        assert_eq!(
            within("the takeover", engine.result::<i64>(&takeover)).await?,
            0
        );
    } else {
        assert_eq!(
            within("the takeover", engine.result::<i64>(&takeover)).await?,
            0
        );
        engine.start("racer", &id, ()).await?;
    }
    assert_refused(&engine, &id).await?;
    assert_eq!(
        counts(&engine).await?,
        (0, 0),
        "the stale owner wrote nothing"
    );
    engine.shutdown(Duration::from_secs(5)).await
}

/// An acceptance that does not accept this exact candidate publishes nothing,
/// and a later step of the same workflow sees nothing of it.
async fn an_unaccepted_candidate_is_never_published(mut engine: Durare) -> Result<()> {
    register_common(&mut engine);
    engine.register("guarded", |ctx: DurareContext, how: String| async move {
        let candidate = execute_candidate(&ctx, "turn", || async {
            Ok(Proposal {
                output: json!("secret"),
                metadata: json!({}),
                writes: vec![insert(Some("a"))],
            })
        })
        .await?;
        let mut acceptance = review_candidate(&candidate);
        match how.as_str() {
            "rejected" => {
                acceptance.accepted = false;
                acceptance.reason = Some("a reviewer found a blocker".into());
            }
            "other-candidate" => acceptance.candidate = "someone-else:1".into(),
            "other-digest" => acceptance.digest = "0".repeat(64),
            _ => {}
        }
        let published =
            publish_candidate(&ctx, "publish", &candidate, &acceptance, Vec::new()).await;
        // A later step: what it can see is only what was published.
        let visible = ctx
            .transaction("later", |mut tx| {
                Box::pin(async move {
                    tx.query_one("SELECT COUNT(*) AS n FROM atomic_publications", &[])
                        .await?
                        .int("n")
                })
            })
            .await?;
        Ok((published.err().map(|e| e.to_string()), visible))
    });
    engine.launch().await?;
    setup(&engine, 1).await?;
    for how in ["rejected", "other-candidate", "other-digest"] {
        let (refused, visible): (Option<String>, i64) =
            run(&engine, "guarded", &unique("guarded"), how).await?;
        let refused = refused.unwrap_or_default();
        assert!(refused.contains("was not accepted"), "{how}: {refused}");
        assert_eq!(visible, 0, "{how}: a later step sees nothing");
    }
    assert!(
        run::<_, (Option<String>, i64)>(&engine, "guarded", &unique("guarded"), "rejected")
            .await?
            .0
            .is_some_and(|e| e.contains("a reviewer found a blocker")),
        "the refusal carries the reviewer's reason"
    );
    assert_eq!(counts(&engine).await?, (0, 0));
    // The control: accepted, it is published and visible.
    let (refused, visible): (Option<String>, i64) =
        run(&engine, "guarded", &unique("guarded"), "accepted").await?;
    assert_eq!((refused, visible), (None, 1));
    engine.shutdown(Duration::from_secs(5)).await
}

/// `run_atom` records an atom as sibling steps, turn then publish, and a
/// replay after a crash finds them in the same order.
async fn run_atom_records_sibling_steps_in_a_stable_order(mut engine: Durare) -> Result<()> {
    let crashed = Arc::new(AtomicUsize::new(0));
    let authors = Arc::new(AtomicUsize::new(0));
    register_common(&mut engine);
    let (c, a) = (crashed.clone(), authors.clone());
    engine.register("two-atoms", move |ctx: DurareContext, (): ()| {
        let (c, a) = (c.clone(), a.clone());
        async move {
            let a1 = a.clone();
            let plan = run_atom(
                &ctx,
                "plan",
                move || async move {
                    a1.fetch_add(1, Ordering::SeqCst);
                    Ok(Proposal {
                        output: json!({"steps": ["one"]}),
                        metadata: json!({"thread_id": "t-plan"}),
                        writes: vec![insert(Some("plan"))],
                    })
                },
                Vec::new(),
            )
            .await?;
            if c.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("crash between the two atoms");
            }
            let steps = plan.output["steps"].clone();
            let a2 = a.clone();
            let review = run_atom(
                &ctx,
                "review",
                move || async move {
                    a2.fetch_add(1, Ordering::SeqCst);
                    Ok(Proposal {
                        output: json!({"reviewed": steps}),
                        metadata: json!({"thread_id": "t-review"}),
                        writes: Vec::new(),
                    })
                },
                Vec::new(),
            )
            .await?;
            Ok((plan, review))
        }
    });
    engine.launch().await?;
    setup(&engine, 1).await?;
    engine.start("two-atoms", "two-atoms-1", ()).await?;
    assert!(
        engine.result::<Value>("two-atoms-1").await.is_err(),
        "crashed"
    );
    let before: Vec<(i32, String)> = engine
        .steps("two-atoms-1")
        .await?
        .into_iter()
        .map(|s| (s.step_id, s.name))
        .collect();
    engine.recover().await?;
    let (plan, review): (Published, Published) = engine.result("two-atoms-1").await?;
    let after: Vec<(i32, String)> = engine
        .steps("two-atoms-1")
        .await?
        .into_iter()
        .map(|s| (s.step_id, s.name))
        .collect();
    let names: Vec<&str> = after.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(
        names,
        ["plan.turn", "plan.publish", "review.turn", "review.publish"]
    );
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "the replay kept the order"
    );
    assert!(after.windows(2).all(|w| w[0].0 < w[1].0), "{after:?}");
    assert_eq!(authors.load(Ordering::SeqCst), 2, "each author ran once");
    assert_eq!(review.output, json!({"reviewed": ["one"]}));
    assert_eq!(plan.metadata["thread_id"], "t-plan");
    // The thread each step ran on, from the run and back.
    let threads = engine.threads("two-atoms-1").await?;
    let pairs: Vec<(&str, &str)> = threads
        .iter()
        .map(|t| (t.step.as_str(), t.thread_id.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [("plan.publish", "t-plan"), ("review.publish", "t-review")]
    );
    let back = engine.thread("t-review").await?.expect("indexed");
    assert_eq!(
        (
            back.run.as_str(),
            back.step.as_str(),
            back.candidate.as_str()
        ),
        ("two-atoms-1", "review.publish", review.candidate.as_str())
    );
    assert_eq!(engine.thread("t-unknown").await?, None);
    assert_eq!(counts(&engine).await?, (1, 2));
    engine.shutdown(Duration::from_secs(5)).await
}

// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn candidate_survives_a_crash_before_publication_on_sqlite() -> Result<()> {
    candidate_survives_a_crash_before_publication(sqlite().await?).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publication_is_all_or_nothing_on_sqlite() -> Result<()> {
    publication_is_all_or_nothing(sqlite().await?).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_precondition_commits_nothing_on_sqlite() -> Result<()> {
    assert!(!a_failed_precondition_commits_nothing(sqlite().await?, 2, FENCE).await?);
    // A precondition that cannot be evaluated refuses too.
    assert!(!a_failed_precondition_commits_nothing(sqlite().await?, 1, BROKEN).await?);
    // A plain read locks nothing, so it never counts as a fence.
    assert!(!a_failed_precondition_commits_nothing(sqlite().await?, 1, READ).await?);
    // The control: the same publication with the precondition holding.
    assert!(a_failed_precondition_commits_nothing(sqlite().await?, 1, FENCE).await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_waits_for_a_fenced_publication_on_sqlite() -> Result<()> {
    a_takeover_waits_for_a_fenced_publication(sqlite().await?, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_publication_meeting_a_takeover_is_refused_on_sqlite() -> Result<()> {
    a_publication_meeting_a_takeover_is_refused(sqlite().await?, false).await
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn candidate_survives_a_crash_before_publication_on_postgres() -> Result<()> {
    candidate_survives_a_crash_before_publication(postgres().await?).await
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publication_is_all_or_nothing_on_postgres() -> Result<()> {
    publication_is_all_or_nothing(postgres().await?).await
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_precondition_commits_nothing_on_postgres() -> Result<()> {
    assert!(!a_failed_precondition_commits_nothing(postgres().await?, 2, FENCE).await?);
    assert!(!a_failed_precondition_commits_nothing(postgres().await?, 1, BROKEN).await?);
    assert!(!a_failed_precondition_commits_nothing(postgres().await?, 1, READ).await?);
    assert!(a_failed_precondition_commits_nothing(postgres().await?, 1, FENCE).await?);
    Ok(())
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_waits_for_a_fenced_publication_on_postgres() -> Result<()> {
    a_takeover_waits_for_a_fenced_publication(postgres().await?, true).await
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_publication_meeting_a_takeover_is_refused_on_postgres() -> Result<()> {
    a_publication_meeting_a_takeover_is_refused(postgres().await?, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unaccepted_candidate_is_never_published_on_sqlite() -> Result<()> {
    an_unaccepted_candidate_is_never_published(sqlite().await?).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_atom_records_sibling_steps_in_a_stable_order_on_sqlite() -> Result<()> {
    run_atom_records_sibling_steps_in_a_stable_order(sqlite().await?).await
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unaccepted_candidate_is_never_published_on_postgres() -> Result<()> {
    an_unaccepted_candidate_is_never_published(postgres().await?).await
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_atom_records_sibling_steps_in_a_stable_order_on_postgres() -> Result<()> {
    run_atom_records_sibling_steps_in_a_stable_order(postgres().await?).await
}
