// SPDX-License-Identifier: Apache-2.0

//! Releasing a park, on SQLite and (with `postgres-tests`) PostgreSQL:
//!
//! - with no credential enrolled, or the wrong one, nothing is released,
//!   raised or recorded, and the run stays parked;
//! - with the enrolled credential, the cap is raised, the park is marked
//!   released and journaled with who and when, in one transaction, and the
//!   run runs the parked turn again;
//! - a replay returns the first result and runs nothing more; another
//!   command on the released park is refused, and so is a reused id;
//! - a raise of an allowance that does not exist releases nothing.

use super::*;
use crate::publication::{Proposal, Published, run_atom_at};
use crate::runtime::{DurableError, Durare, RunStatus};
use crate::usage::Caps;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn unique(tag: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "{tag}_{}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        now_ms()
    )
}

async fn sqlite() -> Durare {
    let path = std::env::temp_dir().join(format!("{}.db", unique("atomic_release")));
    Durare::sqlite(&path).await.expect("sqlite engine")
}

/// An engine on a schema of its own, which the returned guard drops.
#[cfg(feature = "postgres-tests")]
async fn postgres() -> (Durare, crate::testing::TestSchema) {
    let schema = crate::testing::schema("release").await;
    let rt = Durare::postgres(schema.url(), schema.name())
        .await
        .expect("postgres engine");
    (rt, schema)
}

fn operator() -> Principal {
    Principal::local("dev")
}

/// An allowance to raise, as the ledger keeps one.
async fn allowance<R: DurableRuntime>(rt: &R, scope: &str, tokens: i64) {
    let scope = scope.to_owned();
    rt.control_transaction(move |mut tx| {
        let scope = scope.clone();
        Box::pin(async move {
            tx.execute(
                "INSERT INTO atomic_usage_allowances (scope, parent, tokens_cap, millis_cap, \
                 micro_usd_cap, tokens_charged, millis_charged, micro_usd_charged) \
                 VALUES (?, CAST(? AS TEXT), ?, ?, CAST(? AS BIGINT), 0, 0, 0)",
                &[
                    Param::Text(scope),
                    Param::Null,
                    Param::Int(tokens),
                    Param::Int(60_000),
                    Param::Null,
                ],
            )
            .await
            .map(|_| ())
        })
    })
    .await
    .expect("allowance");
}

async fn caps_of<R: DurableRuntime>(rt: &R, scope: &str) -> i64 {
    let scope = scope.to_owned();
    rt.control_transaction(move |mut tx| {
        let scope = scope.clone();
        Box::pin(async move {
            tx.query_one(
                "SELECT tokens_cap FROM atomic_usage_allowances WHERE scope = ?",
                &[Param::Text(scope)],
            )
            .await?
            .int("tokens_cap")
        })
    })
    .await
    .expect("caps")
}

fn command(park: &str, id: &str, raise: Option<Raise>) -> ReleaseCommand {
    ReleaseCommand {
        command_id: id.to_owned(),
        park: park.to_owned(),
        raise,
        comment: Some("raised for the review".into()),
    }
}

async fn only_the_enrolled_credential_releases_and_raises<R: DurableRuntime>(mut rt: R) {
    let turns = Arc::new(Mutex::new(0usize));
    let t = turns.clone();
    rt.register("parks", move |ctx: R::Context, (): ()| {
        let t = t.clone();
        async move {
            run_atom_at(
                &ctx,
                "plan",
                move |info| {
                    let t = t.clone();
                    async move {
                        let n = {
                            let mut n = t.lock().expect("turns");
                            *n += 1;
                            *n
                        };
                        if n < 2 {
                            return Err(DurableError::Parked {
                                reason: format!("no room, turn {n}"),
                                scope: Some(format!("run:{}", info.workflow_id)),
                            });
                        }
                        Ok(Proposal {
                            output: json!("planned"),
                            metadata: json!({}),
                            writes: Vec::new(),
                        })
                    }
                },
                Vec::new(),
            )
            .await
        }
    });
    rt.launch().await.expect("launch");
    let run = unique("parks");
    let scope = format!("run:{run}");
    allowance(&rt, &scope, 100).await;
    rt.start("parks", &run, ()).await.expect("start");
    let until = Instant::now() + Duration::from_secs(30);
    let park = loop {
        if let Some(p) = crate::parks(&rt, &run).await.expect("parks").first() {
            break p.clone();
        }
        assert!(Instant::now() < until, "the run parked");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(park.scope(), Some(scope.as_str()));
    let raise = || {
        Some(Raise {
            scope: scope.clone(),
            caps: Caps {
                tokens: 1_000,
                millis: 0,
                micro_usd: None,
            },
        })
    };

    // No credential enrolled, then the wrong one: nothing happens.
    let claimed = HumanCredential::new(operator(), "a guess");
    assert!(matches!(
        release_park(&rt, &claimed, command(&park.id, "c1", raise())).await,
        Err(ReleaseError::NotEnrolled(p)) if p == operator()
    ));
    let mine = HumanCredential::new(operator(), "the operator's secret");
    crate::gate::enroll(&rt, &mine).await.expect("enroll");
    assert!(matches!(
        release_park(&rt, &claimed, command(&park.id, "c1", raise())).await,
        Err(ReleaseError::BadCredential(p)) if p == operator()
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*turns.lock().expect("turns"), 1, "still parked");
    assert_eq!(
        rt.status(&run).await.expect("status"),
        Some(RunStatus::Pending)
    );
    assert!(!is_released(&rt, &park.id).await.expect("released"));
    assert_eq!(caps_of(&rt, &scope).await, 100, "not raised");
    assert!(releases(&rt).await.expect("releases").is_empty());

    // Another run's allowance, or one that does not exist, is not this
    // run's to raise: nothing is released or raised.
    let other = format!("run:{}", unique("other"));
    allowance(&rt, &other, 100).await;
    for scope in [other.clone(), "run:nowhere".to_owned()] {
        let elsewhere = Some(Raise {
            scope: scope.clone(),
            caps: Caps {
                tokens: 1_000_000,
                millis: 0,
                micro_usd: None,
            },
        });
        assert!(
            matches!(
                release_park(&rt, &mine, command(&park.id, "c0", elsewhere)).await,
                Err(ReleaseError::NotTheRunsAllowance { .. })
            ),
            "{scope}"
        );
    }
    assert_eq!(
        caps_of(&rt, &other).await,
        100,
        "another run's cap was raised"
    );
    assert!(!is_released(&rt, &park.id).await.expect("released"));

    // The enrolled credential: raised, released, recorded, and run again.
    let release = release_park(&rt, &mine, command(&park.id, "c1", raise()))
        .await
        .expect("released");
    assert_eq!(release.wake_failed, None);
    assert!(!release.raised_nothing);
    let released = release.released;
    assert_eq!(released.by, operator());
    assert_eq!(released.park, park);
    assert_eq!(released.raised.as_ref().map(|a| a.caps.tokens), Some(1_000));
    assert_eq!(caps_of(&rt, &scope).await, 1_000);
    assert!(is_released(&rt, &park.id).await.expect("released"));
    let published: Published = tokio::time::timeout(Duration::from_secs(30), rt.result(&run))
        .await
        .expect("finishes in time")
        .expect("finishes");
    assert_eq!(published.output, json!("planned"));
    assert_eq!(*turns.lock().expect("turns"), 2);
    let journal = releases(&rt).await.expect("releases");
    assert_eq!(journal, std::slice::from_ref(&released));
    assert_eq!(journal[0].comment.as_deref(), Some("raised for the review"));

    // A replay is the same release; another command, or a reused id, is not.
    let again = release_park(&rt, &mine, command(&park.id, "c1", raise()))
        .await
        .expect("replay");
    assert_eq!(again.released, released);
    let again = release_park(&rt, &mine, command(&park.id, "c2", None))
        .await
        .expect_err("already released");
    assert!(
        matches!(&again, ReleaseError::AlreadyReleased { by, wake_failed: None, .. } if *by == operator()),
        "{again}"
    );
    assert!(
        again.to_string().contains("its run was woken again"),
        "{again}"
    );
    assert!(matches!(
        release_park(&rt, &mine, command(&park.id, "c1", None)).await,
        Err(ReleaseError::CommandReused(_))
    ));
    assert_eq!(releases(&rt).await.expect("releases").len(), 1);

    // A park whose run has finished has nothing to release.
    let stale = format!("{run}:99");
    let (stale_id, stale_run) = (stale.clone(), run.clone());
    rt.control_transaction(move |mut tx| {
        let (stale, run) = (stale_id.clone(), stale_run.clone());
        Box::pin(async move {
            tx.execute(
                "INSERT INTO atomic_parks (park_id, run, step, reason, parked_at) \
                 VALUES (?, ?, 'late.park', 'no room', 1)",
                &[Param::Text(stale), Param::Text(run)],
            )
            .await
            .map(|_| ())
        })
    })
    .await
    .expect("a stale park");
    assert!(matches!(
        release_park(&rt, &mine, command(&stale, "c3", None)).await,
        Err(ReleaseError::RunFinished { .. })
    ));

    // Two releases at once: one wins, the other says it was released, and
    // the same command sent twice at once is one release.
    for (park, ids) in [("racer:1", ["r1", "r2"]), ("racer:2", ["r3", "r3"])] {
        rt.control_transaction(move |mut tx| {
            Box::pin(async move {
                tx.execute(
                    "INSERT INTO atomic_parks (park_id, run, step, reason, parked_at) \
                     VALUES (?, 'racer', 'race.park', 'no room', 1)",
                    &[Param::Text(park.to_owned())],
                )
                .await
                .map(|_| ())
            })
        })
        .await
        .expect("a park to race for");
        let (a, b) = tokio::join!(
            release_park(&rt, &mine, command(park, ids[0], None)),
            release_park(&rt, &mine, command(park, ids[1], None)),
        );
        if ids[0] == ids[1] {
            let (a, b) = (a.expect("one release"), b.expect("the same release"));
            assert_eq!(a.released, b.released);
        } else {
            let (won, lost) = match (a, b) {
                (Ok(w), Err(l)) | (Err(l), Ok(w)) => (w, l),
                (a, b) => panic!("one release wins: {a:?}, {b:?}"),
            };
            assert_eq!(won.released.park.id, park);
            assert!(
                matches!(lost, ReleaseError::AlreadyReleased { .. }),
                "{lost}"
            );
        }
    }
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_enrolled_credential_releases_and_raises_on_sqlite() {
    only_the_enrolled_credential_releases_and_raises(sqlite().await).await
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_enrolled_credential_releases_and_raises_on_postgres() {
    // The schema outlives the engine.
    let (rt, _schema) = postgres().await;
    only_the_enrolled_credential_releases_and_raises(rt).await
}

// ---------------------------------------------------------------------------
// Hooks the release transaction calls in tests.

/// Pauses at `before_marking`, by park: the release tells the first notify
/// it has reached the point, then waits on the second.
type Pause = Arc<(tokio::sync::Notify, tokio::sync::Notify)>;
static PAUSES: std::sync::LazyLock<Mutex<std::collections::HashMap<String, Pause>>> =
    std::sync::LazyLock::new(Mutex::default);
/// How many wake-ups each park's run was sent.
static WAKES: std::sync::LazyLock<Mutex<std::collections::HashMap<String, usize>>> =
    std::sync::LazyLock::new(Mutex::default);

/// Hold the next release of `park` after it raised its cap, before it marks
/// the park released, until the returned pause's second notify fires.
fn pause_before_marking(park: &str) -> Pause {
    let pause: Pause = Arc::default();
    PAUSES
        .lock()
        .expect("pauses")
        .insert(park.to_owned(), pause.clone());
    pause
}

/// Called by the release transaction after its savepoint and raise.
pub(super) async fn before_marking(park: &str) {
    let pause = PAUSES.lock().ok().and_then(|mut p| p.remove(park));
    if let Some(pause) = pause {
        pause.0.notify_one();
        pause.1.notified().await;
    }
}

/// Called for every wake-up sent to a released park's run.
pub(super) fn woken(park: &str) {
    *WAKES
        .lock()
        .expect("wakes")
        .entry(park.to_owned())
        .or_default() += 1;
}

fn wakes(park: &str) -> usize {
    WAKES.lock().expect("wakes").get(park).copied().unwrap_or(0)
}

/// A park of run `run`, with nothing waiting on it, and its run's allowance.
async fn bare_park<R: DurableRuntime>(rt: &R, run: &str, tokens: i64) -> String {
    let park = format!("{run}:1");
    let (id, r) = (park.clone(), run.to_owned());
    rt.control_transaction(move |mut tx| {
        let (id, r) = (id.clone(), r.clone());
        Box::pin(async move {
            tx.execute(
                "INSERT INTO atomic_parks (park_id, run, step, reason, parked_at) \
                 VALUES (?, ?, 'plan.park', 'no room', 1)",
                &[Param::Text(id), Param::Text(r)],
            )
            .await
            .map(|_| ())
        })
    })
    .await
    .expect("a park");
    allowance(rt, &format!("run:{run}"), tokens).await;
    park
}

fn raising(run: &str, tokens: i64) -> Option<Raise> {
    Some(Raise {
        scope: format!("run:{run}"),
        caps: Caps {
            tokens,
            millis: 0,
            micro_usd: None,
        },
    })
}

/// A release already made sends its run's wake-up again, and says whether
/// that worked: here the park's run is not one the engine has, so it says
/// waking failed and who will deliver it. (Where it works, the run's own
/// test sees it said so.)
async fn a_release_already_made_wakes_its_run_again<R: DurableRuntime>(rt: R) {
    let mine = HumanCredential::new(operator(), "the operator's secret");
    crate::gate::enroll(&rt, &mine).await.expect("enroll");
    let run = unique("again");
    let park = bare_park(&rt, &run, 100).await;
    let first = release_park(&rt, &mine, command(&park, "first", None))
        .await
        .expect("released");
    assert!(first.wake_failed.is_some(), "no run to wake");
    assert_eq!(wakes(&park), 1);
    let again = release_park(&rt, &mine, command(&park, "second", None))
        .await
        .expect_err("already released");
    assert_eq!(wakes(&park), 2, "the wake-up was not sent again");
    let said = again.to_string();
    assert!(said.contains("was already released by local:dev"), "{said}");
    assert!(said.contains("waking its run again failed"), "{said}");
    assert!(said.contains("atomic runs resume"), "{said}");
    assert!(!said.contains("woken again"), "{said}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_release_already_made_wakes_its_run_again_on_sqlite() {
    a_release_already_made_wakes_its_run_again(sqlite().await).await
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_release_already_made_wakes_its_run_again_on_postgres() {
    let (rt, _schema) = postgres().await;
    a_release_already_made_wakes_its_run_again(rt).await
}

/// Two releases interleaved as only PostgreSQL lets them (SQLite runs one
/// write at a time): the first has raised its cap when the second marks the
/// park released and commits. The first then finds the park taken, undoes
/// its raise, and says the park was already released; the cap is raised by
/// the winner alone.
#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_release_that_loses_the_park_undoes_its_raise_on_postgres() {
    let (rt, _schema) = postgres().await;
    let rt = Arc::new(rt);
    let mine = HumanCredential::new(operator(), "the operator's secret");
    crate::gate::enroll(&*rt, &mine).await.expect("enroll");
    let run = unique("lost");
    let park = bare_park(&*rt, &run, 100).await;
    // The run's atom allowance, under its own, for the winner to raise.
    let atom = format!("atom:{run}:1");
    let (scope, parent) = (atom.clone(), format!("run:{run}"));
    rt.control_transaction(move |mut tx| {
        let (scope, parent) = (scope.clone(), parent.clone());
        Box::pin(async move {
            tx.execute(
                "INSERT INTO atomic_usage_allowances (scope, parent, tokens_cap, millis_cap, \
                 micro_usd_cap, tokens_charged, millis_charged, micro_usd_charged) \
                 VALUES (?, ?, 50, 60000, CAST(? AS BIGINT), 0, 0, 0)",
                &[Param::Text(scope), Param::Text(parent), Param::Null],
            )
            .await
            .map(|_| ())
        })
    })
    .await
    .expect("an atom allowance");
    let pause = pause_before_marking(&park);
    let (first_rt, first_cred, first_park) = (rt.clone(), mine.clone(), park.clone());
    let first = tokio::spawn(async move {
        release_park(
            &*first_rt,
            &first_cred,
            command(&first_park, "first", raising(&run_of(&first_park), 1_000)),
        )
        .await
    });
    // The first has raised the run's cap and holds before marking the park.
    pause.0.notified().await;
    let won = release_park(
        &*rt,
        &mine,
        command(
            &park,
            "second",
            Some(Raise {
                scope: atom.clone(),
                caps: Caps {
                    tokens: 500,
                    millis: 0,
                    micro_usd: None,
                },
            }),
        ),
    )
    .await
    .expect("the second release wins");
    assert_eq!(won.released.command, "second");
    pause.1.notify_one();
    let lost = first
        .await
        .expect("the first release")
        .expect_err("lost the park");
    assert!(
        matches!(lost, ReleaseError::AlreadyReleased { .. }),
        "{lost}"
    );
    assert_eq!(
        caps_of(&*rt, &format!("run:{}", run_of(&park))).await,
        100,
        "the loser's raise stayed"
    );
    assert_eq!(caps_of(&*rt, &atom).await, 500);
}

/// Two releases interleaved on one command id, for different parks: the
/// first has marked its park when the second records the command. The first
/// then finds the id taken, undoes its release and raise, and says the id
/// was used for another command.
#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_release_that_loses_its_command_id_undoes_itself_on_postgres() {
    let (rt, _schema) = postgres().await;
    let rt = Arc::new(rt);
    let mine = HumanCredential::new(operator(), "the operator's secret");
    crate::gate::enroll(&*rt, &mine).await.expect("enroll");
    let (one, two) = (unique("one"), unique("two"));
    let (first_park, second_park) = (
        bare_park(&*rt, &one, 100).await,
        bare_park(&*rt, &two, 100).await,
    );
    let pause = pause_before_marking(&first_park);
    let (first_rt, first_cred, park) = (rt.clone(), mine.clone(), first_park.clone());
    let one_run = one.clone();
    let first = tokio::spawn(async move {
        release_park(
            &*first_rt,
            &first_cred,
            command(&park, "shared", raising(&one_run, 1_000)),
        )
        .await
    });
    pause.0.notified().await;
    release_park(&*rt, &mine, command(&second_park, "shared", None))
        .await
        .expect("the second records the command");
    pause.1.notify_one();
    let lost = first
        .await
        .expect("the first release")
        .expect_err("lost the id");
    assert!(matches!(lost, ReleaseError::CommandReused(_)), "{lost}");
    assert!(
        !is_released(&*rt, &first_park).await.expect("released?"),
        "the loser's park stayed released"
    );
    assert_eq!(
        caps_of(&*rt, &format!("run:{one}")).await,
        100,
        "the loser's raise stayed"
    );
}

/// The run of park `park` (`<run>:<n>`).
fn run_of(park: &str) -> String {
    park.rsplit_once(':')
        .map(|(r, _)| r.to_owned())
        .unwrap_or_default()
}
