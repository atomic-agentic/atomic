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
use durare::{DurableEngine, SqliteProvider, StateProvider, WorkflowOptions};
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

async fn sqlite() -> Result<Arc<dyn StateProvider>> {
    let db = std::env::temp_dir().join(format!("{}.db", unique("atomic_publication")));
    let url = format!("sqlite://{}?mode=rwc", db.display());
    Ok(Arc::new(SqliteProvider::connect(&url).await?))
}

#[cfg(feature = "postgres-tests")]
async fn postgres() -> Result<Arc<dyn StateProvider>> {
    // A suite that cannot reach its backend fails; it never skips.
    let url = std::env::var("ATOMIC_TEST_DATABASE_URL")
        .expect("the postgres-tests feature needs ATOMIC_TEST_DATABASE_URL");
    Ok(Arc::new(
        durare::PostgresProvider::connect_with_schema(&url, &unique("atomic_pub")).await?,
    ))
}

/// Rows in the tables these tests write to, read through a transaction.
async fn counts(engine: &DurableEngine) -> Result<(i64, i64)> {
    let id = unique("audit");
    engine
        .start::<_, (i64, i64)>("audit", (), WorkflowOptions::with_id(&id))
        .await?
        .result()
        .await
}

fn register_common(engine: &mut DurableEngine) {
    engine.register("setup", |ctx: DurableContext, epoch: i64| async move {
        ctx.transaction("tables", move |tx| {
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
    engine.register("audit", |ctx: DurableContext, (): ()| async move {
        ctx.transaction("count", |tx| {
            Box::pin(async move {
                let items = tx.query_one("SELECT COUNT(*) AS n FROM items", &[]).await?;
                let pubs = tx
                    .query_one("SELECT COUNT(*) AS n FROM atomic_publications", &[])
                    .await?;
                Ok((items.try_get::<i64>("n")?, pubs.try_get::<i64>("n")?))
            })
        })
        .await
    });
}

/// Wait until execution `id` has checkpointed its candidate and had its
/// publication refused: interrupted, not finished, with no publish checkpoint.
async fn assert_refused(engine: &DurableEngine, id: &str) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let steps = engine.get_workflow_steps(id).await?;
        if steps.iter().any(|s| s.name == "turn") || Instant::now() > until {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let infos = engine.get_workflow_steps(id).await?;
    let steps: Vec<String> = infos.into_iter().map(|s| s.name).collect();
    assert_eq!(steps, ["turn"], "no checkpoint for the refused publication");
    let status = engine
        .list_workflows(&durare::ListFilter {
            workflow_ids: vec![id.to_owned()],
            ..durare::ListFilter::default()
        })
        .await?;
    assert_eq!(
        status[0].status, "PENDING",
        "interrupted, not finished: {status:?}"
    );
    Ok(())
}

async fn setup(engine: &DurableEngine, epoch: i64) -> Result<()> {
    engine
        .start::<_, ()>("setup", epoch, WorkflowOptions::with_id(unique("setup")))
        .await?
        .result()
        .await
}

fn insert(name: Option<&str>) -> Write {
    Write {
        sql: "INSERT INTO items (name) VALUES (?)".into(),
        params: vec![name.map_or(Value::Null, |n| json!(n))],
    }
}

// ---------------------------------------------------------------------------

async fn candidate_survives_a_crash_before_publication(
    provider: Arc<dyn StateProvider>,
) -> Result<()> {
    let author_calls = Arc::new(AtomicUsize::new(0));
    let body_runs = Arc::new(AtomicUsize::new(0));

    let mut engine = DurableEngine::new(provider).await?;
    register_common(&mut engine);
    let (calls, runs) = (author_calls.clone(), body_runs.clone());
    engine.register("atom", move |ctx: DurableContext, (): ()| {
        let (calls, runs) = (calls.clone(), runs.clone());
        async move {
            let candidate = execute_candidate(&ctx, "turn", move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Proposal {
                    output: json!({"answer": 42}),
                    writes: vec![insert(Some("a")), insert(Some("b"))],
                })
            })
            .await?;
            if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                // The engine treats a panic in a workflow body as a crash: the run
                // stays PENDING with its checkpoints, and recovery re-dispatches it.
                panic!("crash after the candidate checkpoint, before publication");
            }
            publish_candidate(&ctx, "publish", &candidate, Vec::new()).await
        }
    });
    engine.launch().await?;
    setup(&engine, 1).await?;

    let first = engine
        .start::<_, Value>("atom", (), WorkflowOptions::with_id("atom-1"))
        .await?;
    assert!(first.result().await.is_err(), "the first execution crashed");
    // Nothing reached the tables before publication.
    assert_eq!(counts(&engine).await?, (0, 0));

    engine.recover().await?;
    let output: Value = engine.retrieve_workflow("atom-1").await?.result().await?;
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

async fn publication_is_all_or_nothing(provider: Arc<dyn StateProvider>) -> Result<()> {
    let mut engine = DurableEngine::new(provider).await?;
    register_common(&mut engine);
    engine.register(
        "atom",
        |ctx: DurableContext, writes: Vec<Write>| async move {
            let candidate = execute_candidate(&ctx, "turn", move || async move {
                Ok(Proposal {
                    output: json!("done"),
                    writes,
                })
            })
            .await?;
            let once = publish_candidate(&ctx, "publish", &candidate, Vec::new()).await?;
            // The same candidate again: the publication key returns what was
            // published, with no second set of writes.
            let twice = publish_candidate(&ctx, "publish-again", &candidate, Vec::new()).await?;
            assert_eq!(once, twice);
            Ok(once)
        },
    );
    engine.launch().await?;
    setup(&engine, 1).await?;

    // The second write violates NOT NULL, so neither write, nor the
    // publication, commits.
    let failed = engine
        .start::<_, Value>(
            "atom",
            vec![insert(Some("a")), insert(None)],
            WorkflowOptions::with_id("atom-bad"),
        )
        .await?
        .result()
        .await;
    assert!(failed.is_err(), "a failing write fails the publication");
    assert_eq!(counts(&engine).await?, (0, 0));

    let ok: Value = engine
        .start::<_, Value>(
            "atom",
            vec![insert(Some("a")), insert(Some("b"))],
            WorkflowOptions::with_id("atom-good"),
        )
        .await?
        .result()
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
    provider: Arc<dyn StateProvider>,
    epoch_now: i64,
    fence: &'static str,
) -> Result<bool> {
    let mut engine = DurableEngine::new(provider).await?;
    register_common(&mut engine);
    engine.register("atom", move |ctx: DurableContext, (): ()| async move {
        let candidate = execute_candidate(&ctx, "turn", || async {
            Ok(Proposal {
                output: json!("published"),
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
        publish_candidate(&ctx, "publish", &candidate, vec![fence]).await
    });
    engine.launch().await?;
    // Another owner may have taken the run over and moved it to a new epoch.
    setup(&engine, epoch_now).await?;

    let id = unique("fenced");
    let handle = engine
        .start::<_, Value>("atom", (), WorkflowOptions::with_id(&id))
        .await?;
    let published = if epoch_now == 1 && fence == FENCE {
        assert_eq!(handle.result().await?, json!("published"));
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
fn register_race(engine: &mut DurableEngine, taken: Arc<Notify>, release: Arc<Notify>) {
    engine.register("racer", |ctx: DurableContext, (): ()| async move {
        let candidate = execute_candidate(&ctx, "turn", || async {
            Ok(Proposal {
                output: json!("published"),
                writes: vec![insert(Some("a"))],
            })
        })
        .await?;
        let fence = Precondition {
            what: "run-1 is still at epoch 1".into(),
            sql: FENCE.into(),
            params: vec![json!(1), json!("run-1")],
        };
        publish_candidate(&ctx, "publish", &candidate, vec![fence]).await
    });
    engine.register("takeover", move |ctx: DurableContext, hold: bool| {
        let (taken, release) = (taken.clone(), release.clone());
        async move {
            ctx.transaction("take", move |tx| {
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
                    n.try_get::<i64>("n")
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
    provider: Arc<dyn StateProvider>,
    interleave: bool,
) -> Result<()> {
    let mut engine = DurableEngine::new(provider).await?;
    register_common(&mut engine);
    register_race(&mut engine, Arc::default(), Arc::default());
    engine.launch().await?;
    setup(&engine, 1).await?;

    let id = unique("race_publish_first");
    let pause = interleave.then(|| pause_after_fences(&id));
    let publication = engine
        .start::<_, Value>("racer", (), WorkflowOptions::with_id(&id))
        .await?;
    let publication = match &pause {
        Some(pause) => {
            within("the publication to pass its fence", pause.0.notified()).await;
            Some(publication)
        }
        None => {
            let out = within("the publication", publication.result()).await?;
            assert_eq!(out, json!("published"));
            None
        }
    };

    let takeover = engine
        .start::<_, i64>(
            "takeover",
            false,
            WorkflowOptions::with_id(unique("takeover")),
        )
        .await?;
    let takeover = tokio::spawn(async move { takeover.result().await });
    if let (Some(pause), Some(publication)) = (pause, publication) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !takeover.is_finished(),
            "the takeover must wait while a fenced publication is open"
        );
        pause.1.notify_one();
        let out = within("the publication", publication.result()).await?;
        assert_eq!(out, json!("published"));
    }
    let seen = within("the takeover", takeover)
        .await
        .map_err(|e| Error::app(e.to_string()))??;
    assert_eq!(seen, 1, "the takeover is ordered after the publication");
    assert_eq!(counts(&engine).await?, (1, 1));
    engine.shutdown(Duration::from_secs(5)).await
}

/// A takeover that has moved the epoch but not yet committed holds the row: a
/// publication waits at its fence, finds the new epoch once the takeover
/// commits, and is refused with nothing written. Without `interleave`, the
/// publication simply runs after the takeover.
async fn a_publication_meeting_a_takeover_is_refused(
    provider: Arc<dyn StateProvider>,
    interleave: bool,
) -> Result<()> {
    let (taken, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let mut engine = DurableEngine::new(provider).await?;
    register_common(&mut engine);
    register_race(&mut engine, taken.clone(), release.clone());
    engine.launch().await?;
    setup(&engine, 1).await?;

    let takeover = engine
        .start::<_, i64>(
            "takeover",
            interleave,
            WorkflowOptions::with_id(unique("takeover")),
        )
        .await?;
    let id = unique("race_takeover_first");
    if interleave {
        within("the takeover to update the epoch", taken.notified()).await;
        let publication = engine
            .start::<_, Value>("racer", (), WorkflowOptions::with_id(&id))
            .await?;
        // Let the publication reach its fence and wait there.
        tokio::time::sleep(Duration::from_millis(500)).await;
        release.notify_one();
        assert_eq!(within("the takeover", takeover.result()).await?, 0);
        drop(publication);
    } else {
        assert_eq!(within("the takeover", takeover.result()).await?, 0);
        let publication = engine
            .start::<_, Value>("racer", (), WorkflowOptions::with_id(&id))
            .await?;
        drop(publication);
    }
    assert_refused(&engine, &id).await?;
    assert_eq!(
        counts(&engine).await?,
        (0, 0),
        "the stale owner wrote nothing"
    );
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
