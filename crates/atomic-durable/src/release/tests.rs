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
    assert!(matches!(
        release_park(&rt, &mine, command(&park.id, "c2", None)).await,
        Err(ReleaseError::AlreadyReleased { by, .. }) if by == operator()
    ));
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
