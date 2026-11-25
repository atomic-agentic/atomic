// SPDX-License-Identifier: Apache-2.0
//! Claim and checkpoint share the durable SQLite database and commit.

use atomic_app::graph_claim::{
    ClaimTurnLimits, bounded_claimed_turn, claim_before_execution, claimed_turn,
    claimed_turn_with_limits, validate_before_turn,
};
use atomic_durable::{DurableError, DurableRuntime, Durare, Param, WorkflowContext};
#[cfg(feature = "postgres-tests")]
use atomic_graph::transactional_claim;
use atomic_graph::{
    sqlite::SqliteStore,
    store::{Actor, Draft, GraphStore},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

struct MarkDropped(Arc<AtomicBool>);

impl Drop for MarkDropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[cfg(feature = "dev-failpoints")]
struct ReapChild(std::process::Child);

#[cfg(feature = "dev-failpoints")]
impl std::ops::Deref for ReapChild {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(feature = "dev-failpoints")]
impl std::ops::DerefMut for ReapChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(feature = "dev-failpoints")]
impl Drop for ReapChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn path(label: &str) -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "atomic-ws3j-{label}-{}-{}.sqlite",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

async fn current_lease(rt: &Durare) -> (String, i64) {
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            let row = tx
                .query_one(
                    "SELECT acquisition,epoch FROM atomic_graph_leases WHERE item_id=?",
                    &[Param::Text("at-work".into())],
                )
                .await?;
            Ok((row.text("acquisition")?, row.int("epoch")?))
        })
    })
    .await
    .expect("current lease")
}

async fn lease_expiry(rt: &Durare) -> i64 {
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            let row = tx
                .query_one(
                    "SELECT expires_at FROM atomic_graph_leases WHERE item_id=?",
                    &[Param::Text("at-work".into())],
                )
                .await?;
            row.int("expires_at")
        })
    })
    .await
    .expect("authoritative lease expiry")
}

async fn wait_for_park(rt: &Durare, expected: usize) -> atomic_durable::Park {
    wait_for_park_in(rt, "run-takeover", expected).await
}

async fn wait_for_park_in(rt: &Durare, run_id: &str, expected: usize) -> atomic_durable::Park {
    for _ in 0..100 {
        let waiting = atomic_durable::release::waiting(rt)
            .await
            .expect("read unreleased parks");
        if waiting.len() == 1 {
            let parks = atomic_durable::publication::parks(rt, run_id)
                .await
                .expect("read claim parks");
            if parks.len() == expected {
                return waiting.into_iter().next().expect("one waiting park");
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("claim did not record releasable park occurrence {expected}");
}

async fn release_claim_park(rt: &Durare, park: &atomic_durable::Park, command_id: &str) {
    let credential = atomic_durable::gate::HumanCredential::new(
        atomic_durable::gate::Principal::local("owner"),
        "owner's enrolled credential",
    );
    let _ = atomic_durable::gate::enroll(rt, &credential).await;
    atomic_durable::release::release_park(
        rt,
        &credential,
        atomic_durable::release::ReleaseCommand {
            command_id: command_id.to_owned(),
            park: park.id.clone(),
            raise: None,
            comment: None,
        },
    )
    .await
    .expect("authorized claim park release");
}

#[tokio::test]
async fn heartbeat_advances_the_lease_during_a_delayed_bounded_turn() {
    let path = path("heartbeat");
    let store = SqliteStore::open(&path, std::time::SystemTime::now).expect("graph store");
    store
        .create_item(
            Actor::agent("owner"),
            "create-heartbeat-work".into(),
            Draft::new("at-work", "Work"),
        )
        .await
        .expect("seed item");
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut rt = Durare::sqlite(&path).await.expect("durable runtime");
    rt.register("bounded-heartbeat", {
        let entered = entered.clone();
        let release = release.clone();
        let calls = calls.clone();
        move |ctx, (): ()| {
            let entered = entered.clone();
            let release = release.clone();
            let calls = calls.clone();
            async move {
                let lease = claim_before_execution(
                    &ctx,
                    "claim",
                    "at-work".into(),
                    "first".into(),
                    Duration::from_millis(500),
                )
                .await?
                .expect("ready item");
                // The turn starts near the original expiry. Its own bounded
                // deadline remains shorter than a fresh lease horizon.
                tokio::time::sleep(Duration::from_millis(400)).await;
                let control = ctx.clone();
                ctx.step("turn", move |_| async move {
                    bounded_claimed_turn(
                        &control,
                        lease,
                        ClaimTurnLimits {
                            lease_for: Duration::from_secs(2),
                            turn_timeout: Duration::from_secs(1),
                            heartbeat_every: Duration::from_millis(30),
                        },
                        move |lease| async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            entered.notify_one();
                            release.notified().await;
                            Ok(lease.acquisition)
                        },
                    )
                    .await
                })
                .await
            }
        }
    });
    rt.launch().await.expect("launch");
    rt.start("bounded-heartbeat", "heartbeat-run", ())
        .await
        .expect("start");
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("bounded turn started");
    let before = lease_expiry(&rt).await;
    tokio::time::sleep(Duration::from_millis(160)).await;
    let after = lease_expiry(&rt).await;
    assert!(
        after > before + 50,
        "heartbeat must advance lease expiry while a turn is active: {before} -> {after}"
    );
    assert!(
        store
            .claim(
                Actor::agent("other"),
                "other".into(),
                "at-work".into(),
                Duration::from_secs(1),
            )
            .await
            .expect("contending claim")
            .is_none(),
        "control-plane heartbeat kept the original holder after its first expiry"
    );
    release.notify_one();
    assert!(
        rt.result::<String>("heartbeat-run")
            .await
            .expect("bounded turn result")
            .starts_with("atomic-claim:")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn claimed_turn_refuses_to_outlive_its_shorter_deadline() {
    let path = path("timeout");
    let store = SqliteStore::open(&path, std::time::SystemTime::now).expect("graph store");
    store
        .create_item(
            Actor::agent("owner"),
            "create-timeout-work".into(),
            Draft::new("at-work", "Work"),
        )
        .await
        .expect("seed item");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut rt = Durare::sqlite(&path).await.expect("durable runtime");
    rt.register("bounded-timeout", {
        let calls = calls.clone();
        move |ctx, (): ()| {
            let calls = calls.clone();
            async move {
                claimed_turn_with_limits(
                    &ctx,
                    "claim",
                    "turn",
                    "at-work".into(),
                    "first".into(),
                    ClaimTurnLimits {
                        lease_for: Duration::from_millis(250),
                        turn_timeout: Duration::from_millis(50),
                        heartbeat_every: Duration::from_millis(10),
                    },
                    move |_| async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(120)).await;
                        Ok("late".to_owned())
                    },
                )
                .await
            }
        }
    });
    rt.launch().await.expect("launch");
    rt.start("bounded-timeout", "timeout-run", ())
        .await
        .expect("start");
    let failure = rt
        .result::<Option<String>>("timeout-run")
        .await
        .expect_err("turn exceeds its deadline");
    assert!(
        failure
            .to_string()
            .contains("claimed turn exceeded its timeout"),
        "{failure}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let stopped_at = lease_expiry(&rt).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        lease_expiry(&rt).await,
        stopped_at,
        "timed-out turn cannot leave a renewing background task"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn claimed_turn_rejects_a_deadline_at_or_beyond_its_lease_before_claiming() {
    let path = path("invalid-limits");
    let store = SqliteStore::open(&path, std::time::SystemTime::now).expect("graph store");
    store
        .create_item(
            Actor::agent("owner"),
            "create-invalid-limits-work".into(),
            Draft::new("at-work", "Work"),
        )
        .await
        .expect("seed item");
    let mut rt = Durare::sqlite(&path).await.expect("durable runtime");
    rt.register("invalid-turn-limits", move |ctx, (): ()| async move {
        claimed_turn_with_limits(
            &ctx,
            "claim",
            "turn",
            "at-work".into(),
            "first".into(),
            ClaimTurnLimits {
                lease_for: Duration::from_millis(100),
                turn_timeout: Duration::from_millis(100),
                heartbeat_every: Duration::from_millis(10),
            },
            move |_| async move { Ok("must not run".to_owned()) },
        )
        .await
    });
    rt.launch().await.expect("launch");
    rt.start("invalid-turn-limits", "invalid-limits-run", ())
        .await
        .expect("start");
    let failure = rt
        .result::<Option<String>>("invalid-limits-run")
        .await
        .expect_err("deadline cannot equal the lease horizon");
    assert!(
        failure
            .to_string()
            .contains("turn timeout, which must be shorter than the lease"),
        "{failure}"
    );
    assert!(
        rt.control_transaction(|mut tx| Box::pin(async move {
            Ok(tx
                .query_opt(
                    "SELECT acquisition FROM atomic_graph_leases WHERE item_id=?",
                    &[Param::Text("at-work".into())],
                )
                .await?
                .is_none())
        }))
        .await
        .expect("inspect lease"),
        "invalid limits must not claim an item"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn canceling_a_bounded_turn_stops_its_heartbeat_in_the_same_process() {
    let path = path("cancel-heartbeat");
    let store = SqliteStore::open(&path, std::time::SystemTime::now).expect("graph store");
    store
        .create_item(
            Actor::agent("owner"),
            "create-cancel-work".into(),
            Draft::new("at-work", "Work"),
        )
        .await
        .expect("seed item");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut rt = Durare::sqlite(&path).await.expect("durable runtime");
    rt.register("cancel-heartbeat", {
        let calls = calls.clone();
        move |ctx, (): ()| {
            let calls = calls.clone();
            async move {
                let lease = claim_before_execution(
                    &ctx,
                    "claim",
                    "at-work".into(),
                    "first".into(),
                    Duration::from_millis(500),
                )
                .await?
                .expect("ready item");
                let control = ctx.clone();
                ctx.step("turn", move |_| async move {
                    let canceled = tokio::time::timeout(
                        Duration::from_millis(150),
                        bounded_claimed_turn(
                            &control,
                            lease,
                            ClaimTurnLimits {
                                lease_for: Duration::from_millis(500),
                                turn_timeout: Duration::from_millis(350),
                                heartbeat_every: Duration::from_millis(25),
                            },
                            move |_| async move {
                                calls.fetch_add(1, Ordering::SeqCst);
                                tokio::time::sleep(Duration::from_millis(300)).await;
                                Ok("late".to_owned())
                            },
                        ),
                    )
                    .await;
                    if canceled.is_ok() {
                        return Err(DurableError::Failed(
                            "outer cancellation failed to stop the turn".into(),
                        ));
                    }
                    Ok("canceled".to_owned())
                })
                .await
            }
        }
    });
    rt.launch().await.expect("launch");
    rt.start("cancel-heartbeat", "cancel-heartbeat-run", ())
        .await
        .expect("start");
    assert_eq!(
        rt.result::<String>("cancel-heartbeat-run")
            .await
            .expect("cancellation recorded"),
        "canceled"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let stopped_at = lease_expiry(&rt).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        lease_expiry(&rt).await,
        stopped_at,
        "no detached renewal may survive the canceled turn"
    );
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute(
                "UPDATE atomic_graph_leases SET expires_at=0 WHERE item_id=?",
                &[Param::Text("at-work".into())],
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("expire test lease");
    assert!(
        store
            .claim(
                Actor::agent("other"),
                "other".into(),
                "at-work".into(),
                Duration::from_secs(1),
            )
            .await
            .expect("take over expired lease")
            .is_some(),
        "canceled heartbeat cannot hold the item"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn takeover_during_a_turn_cancels_it_before_publication() {
    let path = path("heartbeat-takeover");
    let store = SqliteStore::open(&path, std::time::SystemTime::now).expect("graph store");
    store
        .create_item(
            Actor::agent("owner"),
            "create-takeover-work".into(),
            Draft::new("at-work", "Work"),
        )
        .await
        .expect("seed item");
    let entered = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let published = Arc::new(AtomicUsize::new(0));
    let mut rt = Durare::sqlite(&path).await.expect("durable runtime");
    rt.register("heartbeat-takeover", {
        let entered = entered.clone();
        let dropped = dropped.clone();
        let published = published.clone();
        move |ctx, (): ()| {
            let entered = entered.clone();
            let dropped = dropped.clone();
            let published = published.clone();
            async move {
                let result = claimed_turn_with_limits(
                    &ctx,
                    "claim",
                    "turn",
                    "at-work".into(),
                    "first".into(),
                    ClaimTurnLimits {
                        lease_for: Duration::from_secs(2),
                        turn_timeout: Duration::from_secs(1),
                        heartbeat_every: Duration::from_millis(25),
                    },
                    move |_| async move {
                        let _dropped = MarkDropped(dropped);
                        entered.notify_one();
                        std::future::pending::<()>().await;
                        Ok("never".to_owned())
                    },
                )
                .await?;
                published.fetch_add(1, Ordering::SeqCst);
                Ok(result)
            }
        }
    });
    rt.launch().await.expect("launch");
    rt.start("heartbeat-takeover", "heartbeat-takeover-run", ())
        .await
        .expect("start");
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("turn started");
    let old_epoch = current_lease(&rt).await.1;
    rt.control_transaction(move |mut tx| {
        Box::pin(async move {
            let new_epoch = old_epoch + 1;
            tx.execute(
                "UPDATE atomic_graph_leases SET claimant='other',session=?,epoch=?,acquisition='takeover' WHERE item_id=?",
                &[
                    Param::Int(new_epoch),
                    Param::Int(new_epoch),
                    Param::Text("at-work".into()),
                ],
            )
            .await?;
            tx.execute(
                "UPDATE atomic_graph_items SET claim_epoch=? WHERE id=?",
                &[Param::Int(new_epoch), Param::Text("at-work".into())],
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("simulate an authoritative takeover");
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        rt.result::<Option<String>>("heartbeat-takeover-run"),
    )
    .await
    .expect("heartbeat must notice takeover")
    .expect_err("stale turn must not publish");
    assert!(error.to_string().contains("taken over"), "{error}");
    assert!(dropped.load(Ordering::SeqCst), "turn future was canceled");
    assert_eq!(published.load(Ordering::SeqCst), 0);
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[cfg(feature = "dev-failpoints")]
fn register_heartbeat_process(
    rt: &mut Durare,
    calls: std::path::PathBuf,
    started: std::path::PathBuf,
) {
    rt.register("heartbeat-process", move |ctx, (): ()| {
        let calls = calls.clone();
        let started = started.clone();
        async move {
            claimed_turn_with_limits(
                &ctx,
                "claim",
                "turn",
                "at-work".into(),
                "first".into(),
                ClaimTurnLimits {
                    lease_for: Duration::from_secs(5),
                    turn_timeout: Duration::from_secs(4),
                    heartbeat_every: Duration::from_millis(50),
                },
                move |lease| async move {
                    use std::io::Write;
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&calls)
                        .map_err(|error| {
                            DurableError::Engine(format!("open model-call ledger: {error}"))
                        })?;
                    writeln!(file, "called").map_err(|error| {
                        DurableError::Engine(format!("write model-call ledger: {error}"))
                    })?;
                    std::fs::write(&started, "started").map_err(|error| {
                        DurableError::Engine(format!("record started model turn: {error}"))
                    })?;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(lease.acquisition)
                },
            )
            .await
        }
    });
}

#[cfg(feature = "dev-failpoints")]
#[test]
#[ignore = "only run as the child of killing_a_turn_stops_heartbeat_and_recovery_parks_after_takeover"]
fn heartbeat_process_child() {
    let db = std::path::PathBuf::from(std::env::var("HEARTBEAT_DB").expect("child DB"));
    let calls = std::path::PathBuf::from(std::env::var("HEARTBEAT_CALLS").expect("call ledger"));
    let started =
        std::path::PathBuf::from(std::env::var("HEARTBEAT_STARTED").expect("start marker"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let mut rt = Durare::sqlite(&db).await.expect("durable runtime");
        register_heartbeat_process(&mut rt, calls, started);
        rt.launch().await.expect("launch");
        rt.start("heartbeat-process", "heartbeat-process-run", ())
            .await
            .expect("start");
        let _: Option<String> = rt.result("heartbeat-process-run").await.expect("result");
    });
}

#[cfg(feature = "dev-failpoints")]
#[tokio::test]
async fn killing_a_turn_stops_heartbeat_and_recovery_parks_after_takeover() {
    use std::process::{Command, Stdio};
    let path = path("heartbeat-kill");
    let calls = path.with_extension("calls");
    let started = path.with_extension("started");
    let stderr = path.with_extension("stderr");
    let store = SqliteStore::open(&path, std::time::SystemTime::now).expect("graph store");
    store
        .create_item(
            Actor::agent("owner"),
            "create-heartbeat-process-work".into(),
            Draft::new("at-work", "Work"),
        )
        .await
        .expect("seed item");
    let binary = std::env::current_exe().expect("test binary");
    let mut child = ReapChild(
        Command::new(binary)
            .args([
                "--exact",
                "heartbeat_process_child",
                "--ignored",
                "--nocapture",
            ])
            .env("HEARTBEAT_DB", &path)
            .env("HEARTBEAT_CALLS", &calls)
            .env("HEARTBEAT_STARTED", &started)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).expect("child stderr"))
            .spawn()
            .expect("spawn process to kill"),
    );
    let mut rt = Durare::sqlite(&path).await.expect("reader runtime");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !started.is_file() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("child turn started");
    let first_expiry = lease_expiry(&rt).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while lease_expiry(&rt).await <= first_expiry {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("child renewed lease before kill");
    child.kill().expect("SIGKILL running turn");
    let status = child.wait().expect("reap killed child");
    assert!(!status.success(), "child was killed, not completed");
    assert_eq!(
        std::fs::read_to_string(&calls)
            .expect("model-call ledger")
            .lines()
            .count(),
        1
    );
    let stopped_at = lease_expiry(&rt).await;
    tokio::time::sleep(Duration::from_millis(180)).await;
    assert_eq!(
        lease_expiry(&rt).await,
        stopped_at,
        "killed process no longer renews"
    );
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute(
                "UPDATE atomic_graph_leases SET expires_at=0 WHERE item_id=?",
                &[Param::Text("at-work".into())],
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("expire killed holder's lease");
    let replacement = store
        .claim(
            Actor::agent("other"),
            "other".into(),
            "at-work".into(),
            Duration::from_secs(5),
        )
        .await
        .expect("claim after killed holder")
        .expect("takeover");
    register_heartbeat_process(&mut rt, calls.clone(), started.clone());
    rt.launch().await.expect("launch recovered runtime");
    assert_eq!(rt.recover().await.expect("recover old run"), 1);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !atomic_durable::publication::parks(&rt, "heartbeat-process-run")
                .await
                .expect("read recovery parks")
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stale run parked before another model call");
    assert_eq!(
        std::fs::read_to_string(&calls)
            .expect("model-call ledger")
            .lines()
            .count(),
        1,
        "recovery reused the claim checkpoint and did not repeat a model turn"
    );
    let takeover_expiry = lease_expiry(&rt).await;
    tokio::time::sleep(Duration::from_millis(180)).await;
    assert_eq!(
        lease_expiry(&rt).await,
        takeover_expiry,
        "old heartbeat did not restart"
    );
    assert_eq!(current_lease(&rt).await.0, replacement.acquisition);
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    drop(store);
    for file in [path, calls, started, stderr] {
        let _ = std::fs::remove_file(file);
    }
}

#[cfg(feature = "dev-failpoints")]
fn register_repark_process(rt: &mut Durare, calls: std::path::PathBuf) {
    rt.register("repark-process", move |ctx, (): ()| {
        let calls = calls.clone();
        async move {
            let _lease = claim_before_execution(
                &ctx,
                "claim",
                "at-work".into(),
                "original".into(),
                Duration::from_millis(10),
            )
            .await?
            .expect("seed ready claim or its checkpoint");
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&calls)
                .map_err(|error| DurableError::Engine(format!("open turn ledger: {error}")))?;
            writeln!(file, "dispatched")
                .map_err(|error| DurableError::Engine(format!("write turn ledger: {error}")))?;
            Err::<String, _>(DurableError::Interrupted(
                "seed after claim checkpoint".into(),
            ))
        }
    });
}

#[cfg(feature = "dev-failpoints")]
#[test]
#[ignore = "only run as the child of killing_second_repark_resumes_with_a_third_releasable_id"]
fn ws3j_repark_child() {
    let db = std::path::PathBuf::from(std::env::var("REPARK_DB").expect("child DB"));
    let calls = std::path::PathBuf::from(std::env::var("REPARK_CALLS").expect("turn ledger"));
    let second_ready =
        std::path::PathBuf::from(std::env::var("REPARK_SECOND").expect("second park marker"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let mut rt = Durare::sqlite(&db).await.expect("durable runtime");
        register_repark_process(&mut rt, calls);
        rt.launch().await.expect("launch");
        assert_eq!(rt.recover().await.expect("recover claimed run"), 1);
        let first = wait_for_park_in(&rt, "repark-process-run", 1).await;
        release_claim_park(&rt, &first, "release-first-process").await;
        let second = wait_for_park_in(&rt, "repark-process-run", 2).await;
        assert_ne!(first.id, second.id);
        std::fs::write(second_ready, &second.id).expect("record second park");
        let _: String = rt
            .result("repark-process-run")
            .await
            .expect("parked result");
    });
}

#[cfg(feature = "dev-failpoints")]
#[tokio::test]
async fn killing_second_repark_resumes_with_a_third_releasable_id() {
    use std::process::{Command, Stdio};
    let path = path("repark-process");
    let calls = path.with_extension("calls");
    let second_ready = path.with_extension("second");
    let stderr = path.with_extension("stderr");
    let store = SqliteStore::open(&path, std::time::SystemTime::now).expect("graph store");
    store
        .create_item(
            Actor::agent("owner"),
            "create-repark-process-work".into(),
            Draft::new("at-work", "Work"),
        )
        .await
        .expect("seed item");
    let mut seed = Durare::sqlite(&path).await.expect("seed runtime");
    register_repark_process(&mut seed, calls.clone());
    seed.launch().await.expect("launch seed");
    seed.start("repark-process", "repark-process-run", ())
        .await
        .expect("start seed run");
    assert!(matches!(
        seed.result::<String>("repark-process-run").await,
        Err(DurableError::Interrupted(_))
    ));
    seed.shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown seed");
    tokio::time::sleep(Duration::from_millis(30)).await;
    let replacement = store
        .claim(
            Actor::agent("other"),
            "other".into(),
            "at-work".into(),
            Duration::from_secs(5),
        )
        .await
        .expect("take over")
        .expect("replacement claim");
    let binary = std::env::current_exe().expect("test binary");
    let mut child = ReapChild(
        Command::new(binary)
            .args(["--exact", "ws3j_repark_child", "--ignored", "--nocapture"])
            .env("REPARK_DB", &path)
            .env("REPARK_CALLS", &calls)
            .env("REPARK_SECOND", &second_ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).expect("child stderr"))
            .spawn()
            .expect("spawn repark process"),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match std::fs::read_to_string(&second_ready) {
                Ok(second) if !second.is_empty() => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("read second park marker: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("child reached second park");
    let second_id = std::fs::read_to_string(&second_ready).expect("second park id");
    child.kill().expect("SIGKILL second-park process");
    assert!(!child.wait().expect("reap killed process").success());
    let mut resumed = Durare::sqlite(&path).await.expect("resumed runtime");
    register_repark_process(&mut resumed, calls.clone());
    resumed.launch().await.expect("launch resumed");
    assert_eq!(resumed.recover().await.expect("recover after SIGKILL"), 1);
    let second = wait_for_park_in(&resumed, "repark-process-run", 2).await;
    assert_eq!(second.id, second_id);
    release_claim_park(&resumed, &second, "release-second-process").await;
    let third = wait_for_park_in(&resumed, "repark-process-run", 3).await;
    assert_ne!(third.id, second.id);
    assert_eq!(
        std::fs::read_to_string(&calls)
            .expect("turn ledger")
            .lines()
            .count(),
        1,
        "no model turn may run after takeover"
    );
    assert_eq!(current_lease(&resumed).await.0, replacement.acquisition);
    resumed
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown resumed");
    drop(store);
    for file in [path, calls, second_ready, stderr] {
        let _ = std::fs::remove_file(file);
    }
}

#[tokio::test]
async fn contending_workflows_start_exactly_one_turn_dispatch() {
    let path = path("race");
    let store = SqliteStore::open(&path, std::time::SystemTime::now).expect("graph store");
    store
        .create_item(
            Actor::agent("owner"),
            "create-work".into(),
            Draft::new("at-work", "Work"),
        )
        .await
        .expect("seed work item");
    let mut rt = Durare::sqlite(&path).await.expect("durable runtime");
    let calls = Arc::new(AtomicUsize::new(0));
    rt.register("graph-claim-race", {
        let calls = calls.clone();
        move |ctx, (): ()| {
            let calls = calls.clone();
            async move {
                claimed_turn(
                    &ctx,
                    "claim",
                    "turn-dispatch",
                    "at-work".into(),
                    ctx.workflow_id().to_owned(),
                    Duration::from_secs(60),
                    move |lease| async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(lease.acquisition)
                    },
                )
                .await
            }
        }
    });
    rt.launch().await.expect("launch");
    let (first, second) = tokio::join!(
        rt.start("graph-claim-race", "run-one", ()),
        rt.start("graph-claim-race", "run-two", ())
    );
    first.expect("start first");
    second.expect("start second");
    let (first, second) = tokio::join!(
        rt.result::<Option<String>>("run-one"),
        rt.result::<Option<String>>("run-two")
    );
    let outputs = [
        first.expect("first outcome"),
        second.expect("second outcome"),
    ];
    assert_eq!(outputs.iter().filter(|value| value.is_some()).count(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one turn dispatch");
    let winner = outputs
        .into_iter()
        .flatten()
        .next()
        .expect("winner acquisition");
    let holder = store
        .show(Actor::agent("owner"), "at-work".into())
        .await
        .expect("show item")
        .expect("item")
        .holder
        .expect("holder");
    let (actual, epoch) = current_lease(&rt).await;
    assert_eq!(actual, winner);
    assert_eq!(holder.epoch, epoch);
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn replay_after_claim_checkpoint_reuses_acquisition_without_second_claim() {
    let path = path("replay");
    let store = SqliteStore::open(&path, std::time::SystemTime::now).expect("graph store");
    store
        .create_item(
            Actor::agent("owner"),
            "create-work".into(),
            Draft::new("at-work", "Work"),
        )
        .await
        .expect("seed work item");
    let mut rt = Durare::sqlite(&path).await.expect("durable runtime");
    let stop_once = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    rt.register("graph-claim-replay", {
        let stop_once = stop_once.clone();
        let calls = calls.clone();
        move |ctx, (): ()| {
            let stop_once = stop_once.clone();
            let calls = calls.clone();
            async move {
                let lease = claim_before_execution(
                    &ctx,
                    "claim",
                    "at-work".into(),
                    "worker".into(),
                    Duration::from_secs(60),
                )
                .await?
                .expect("ready claim");
                if stop_once.swap(false, Ordering::SeqCst) {
                    return Err(DurableError::Interrupted(
                        "crash after claim checkpoint".into(),
                    ));
                }
                validate_before_turn(&ctx, &lease).await?;
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(lease.acquisition)
            }
        }
    });
    rt.launch().await.expect("launch");
    rt.start("graph-claim-replay", "run-replay", ())
        .await
        .expect("start");
    assert!(matches!(
        rt.result::<String>("run-replay").await,
        Err(DurableError::Interrupted(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let original = current_lease(&rt).await;
    assert_eq!(
        rt.steps("run-replay")
            .await
            .expect("steps")
            .iter()
            .map(|step| step.name.as_str())
            .collect::<Vec<_>>(),
        ["claim"]
    );
    rt.recover().await.expect("recover");
    let acquisition = rt
        .result::<String>("run-replay")
        .await
        .expect("replayed turn");
    assert_eq!(acquisition, original.0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let current = current_lease(&rt).await;
    assert_eq!(current, original);
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn replay_after_takeover_parks_without_a_new_claim_or_model_turn() {
    let path = path("takeover");
    let store = SqliteStore::open(&path, std::time::SystemTime::now).expect("graph store");
    store
        .create_item(
            Actor::agent("owner"),
            "create-work".into(),
            Draft::new("at-work", "Work"),
        )
        .await
        .expect("seed work item");
    let mut rt = Durare::sqlite(&path).await.expect("durable runtime");
    let stop_once = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    rt.register("graph-claim-takeover", {
        let stop_once = stop_once.clone();
        let calls = calls.clone();
        move |ctx, (): ()| {
            let stop_once = stop_once.clone();
            let calls = calls.clone();
            async move {
                let lease = claim_before_execution(
                    &ctx,
                    "claim",
                    "at-work".into(),
                    "original".into(),
                    Duration::from_millis(10),
                )
                .await?
                .expect("initial ready claim");
                if stop_once.swap(false, Ordering::SeqCst) {
                    return Err(DurableError::Interrupted("crash after claim commit".into()));
                }
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(lease.acquisition)
            }
        }
    });
    rt.launch().await.expect("launch");
    rt.start("graph-claim-takeover", "run-takeover", ())
        .await
        .expect("start");
    assert!(matches!(
        rt.result::<String>("run-takeover").await,
        Err(DurableError::Interrupted(_))
    ));
    let original = current_lease(&rt).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let replacement = store
        .claim(
            Actor::agent("owner"),
            "replacement".into(),
            "at-work".into(),
            Duration::from_secs(60),
        )
        .await
        .expect("takeover claim")
        .expect("replacement lease");
    assert_ne!(replacement.acquisition, original.0);
    rt.recover().await.expect("recover");
    let first_park = wait_for_park(&rt, 1).await;
    release_claim_park(&rt, &first_park, "release-one").await;
    let second_park = wait_for_park(&rt, 2).await;
    assert_ne!(
        first_park.id, second_park.id,
        "each wait needs a releasable identity"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");

    // Restart while the second occurrence waits. The old release must not
    // authorize a model turn or make a second release impossible.
    let mut resumed = Durare::sqlite(&path).await.expect("reopen durable runtime");
    resumed.register("graph-claim-takeover", {
        let calls = calls.clone();
        move |ctx, (): ()| {
            let calls = calls.clone();
            async move {
                let lease = claim_before_execution(
                    &ctx,
                    "claim",
                    "at-work".into(),
                    "original".into(),
                    Duration::from_millis(10),
                )
                .await?
                .expect("replayed claim checkpoint");
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(lease.acquisition)
            }
        }
    });
    resumed.launch().await.expect("launch resumed runtime");
    resumed.recover().await.expect("recover parked workflow");
    let still_waiting = wait_for_park(&resumed, 2).await;
    assert_eq!(still_waiting.id, second_park.id);
    release_claim_park(&resumed, &second_park, "release-two").await;
    let third_park = wait_for_park(&resumed, 3).await;
    assert_ne!(third_park.id, second_park.id);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            resumed.result::<String>("run-takeover")
        )
        .await
        .is_err(),
        "parked workflow must not finish"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "stale claim cannot start a turn"
    );
    assert_eq!(
        current_lease(&resumed).await.0,
        replacement.acquisition,
        "replay did not claim again"
    );
    resumed
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown resumed runtime");
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[cfg(feature = "dev-failpoints")]
#[test]
fn killing_process_after_claim_commit_reuses_acquisition_on_restart() {
    use std::process::{Command, Stdio};
    let path = path("crash");
    let calls = path.with_extension("calls");
    let result = path.with_extension("result");
    let log = path.with_extension("start.log");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let store = SqliteStore::open(&path, std::time::SystemTime::now).expect("graph store");
        store
            .create_item(
                Actor::agent("owner"),
                "create-work".into(),
                Draft::new("at-work", "Work"),
            )
            .await
            .expect("seed item");
    });
    drop(runtime);
    let binary = std::env::current_exe().expect("test binary");
    let start = Command::new(&binary)
        .args(["--exact", "ws3j_claim_child", "--ignored", "--nocapture"])
        .env("WS3J_ROLE", "start")
        .env("WS3J_DB", &path)
        .env("WS3J_CALLS", &calls)
        .env("WS3J_RESULT", &result)
        .env("ATOMIC_FAILPOINT", "checkpoint:after:claim")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).expect("child log"))
        .status()
        .expect("start child");
    assert!(
        !start.success(),
        "claim child must die at the named failpoint"
    );
    let trace = std::fs::read_to_string(&log).expect("read child log");
    assert!(
        trace.contains("checkpoint:after:claim"),
        "wrong child failure: {trace}"
    );
    assert!(
        !calls.exists(),
        "turn must not dispatch before claim checkpoint crash"
    );
    assert!(!result.exists(), "workflow result must remain pending");
    let recover = Command::new(&binary)
        .args(["--exact", "ws3j_claim_child", "--ignored", "--nocapture"])
        .env("WS3J_ROLE", "recover")
        .env("WS3J_DB", &path)
        .env("WS3J_CALLS", &calls)
        .env("WS3J_RESULT", &result)
        .env_remove("ATOMIC_FAILPOINT")
        .stdin(Stdio::null())
        .output()
        .expect("recover child");
    assert!(
        recover.status.success(),
        "recovery failed: {}",
        String::from_utf8_lossy(&recover.stderr)
    );
    let calls_text = std::fs::read_to_string(&calls).expect("turn-dispatch calls");
    assert_eq!(
        calls_text.lines().count(),
        1,
        "recovery dispatched one turn"
    );
    let acquisition = std::fs::read_to_string(&result).expect("recovered result");
    assert!(
        acquisition.starts_with("atomic-claim:"),
        "stable acquisition returned"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        use sqlx::{Connection, Row};
        let url = format!("sqlite://{}?mode=rw", path.display());
        let mut conn = sqlx::SqliteConnection::connect(&url)
            .await
            .expect("read graph DB");
        let row = sqlx::query(
            "SELECT acquisition,epoch FROM atomic_graph_leases WHERE item_id='at-work'",
        )
        .fetch_one(&mut conn)
        .await
        .expect("persisted lease");
        assert_eq!(
            row.try_get::<String, _>("acquisition")
                .expect("acquisition"),
            acquisition
        );
        assert_eq!(
            row.try_get::<i64, _>("epoch").expect("epoch"),
            1,
            "replay did not make a new claim"
        );
    });
    for file in [path, calls, result, log] {
        let _ = std::fs::remove_file(file);
    }
}

#[cfg(feature = "dev-failpoints")]
#[test]
#[ignore = "only run as the child of killing_process_after_claim_commit_reuses_acquisition_on_restart"]
fn ws3j_claim_child() {
    use std::io::Write;
    let role = std::env::var("WS3J_ROLE").expect("child role");
    let path = std::path::PathBuf::from(std::env::var("WS3J_DB").expect("child DB"));
    let calls = std::path::PathBuf::from(std::env::var("WS3J_CALLS").expect("calls path"));
    let result = std::path::PathBuf::from(std::env::var("WS3J_RESULT").expect("result path"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let mut rt = Durare::sqlite(&path).await.expect("durable runtime");
        rt.register("ws3j-crash", move |ctx, (): ()| {
            let calls = calls.clone();
            async move {
                claimed_turn(
                    &ctx,
                    "claim",
                    "turn-dispatch",
                    "at-work".into(),
                    "worker".into(),
                    Duration::from_secs(60),
                    move |lease| async move {
                        let mut file = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&calls)
                            .map_err(|error| {
                                DurableError::Engine(format!("open turn-dispatch ledger: {error}"))
                            })?;
                        writeln!(file, "called").map_err(|error| {
                            DurableError::Engine(format!("write turn-dispatch ledger: {error}"))
                        })?;
                        Ok(lease.acquisition)
                    },
                )
                .await
            }
        });
        rt.launch().await.expect("launch");
        if role == "start" {
            rt.start("ws3j-crash", "ws3j-run", ()).await.expect("start");
        } else {
            assert_eq!(role, "recover");
            assert_eq!(rt.recover().await.expect("recover"), 1);
        }
        let output = rt
            .result::<Option<String>>("ws3j-run")
            .await
            .expect("result")
            .expect("claim won");
        std::fs::write(result, output).expect("write result");
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

#[cfg(feature = "postgres-tests")]
#[tokio::test]
async fn postgres_contenders_cannot_overwrite_a_live_claim_after_both_read_no_lease() {
    let schema = atomic_durable::testing::schema("ws3j_claim_race").await;
    let mut rt = Durare::postgres(schema.url(), schema.name())
        .await
        .expect("postgres runtime");
    rt.control_transaction(|mut tx| Box::pin(async move {
        tx.execute("INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,trust,created_by,created_at) VALUES (?,?,?,?,?,?,?,?,?)",
            &[Param::Text("at-work".into()), Param::Text("Work".into()), Param::Text(String::new()), Param::Text("task".into()), Param::Int(2), Param::Text("open".into()), Param::Text("human".into()), Param::Text("owner".into()), Param::Int(0)]).await?;
        tx.execute("CREATE FUNCTION ws3j_delay_lease() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(0.2); RETURN NEW; END $$", &[]).await?;
        tx.execute("CREATE TRIGGER ws3j_delay_lease BEFORE INSERT ON atomic_graph_leases FOR EACH ROW EXECUTE FUNCTION ws3j_delay_lease()", &[]).await?;
        Ok(())
    })).await.expect("seed and delay concurrent lease inserts");
    let calls = Arc::new(AtomicUsize::new(0));
    rt.register("ws3j-pg-race", {
        let calls = calls.clone();
        move |ctx, (): ()| {
            let calls = calls.clone();
            async move {
                claimed_turn(
                    &ctx,
                    "claim",
                    "turn-dispatch",
                    "at-work".into(),
                    ctx.workflow_id().to_owned(),
                    Duration::from_secs(60),
                    move |lease| async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(lease.acquisition)
                    },
                )
                .await
            }
        }
    });
    rt.launch().await.expect("launch");
    let (one, two) = tokio::join!(
        rt.start("ws3j-pg-race", "pg-one", ()),
        rt.start("ws3j-pg-race", "pg-two", ())
    );
    one.expect("start one");
    two.expect("start two");
    let (one, two) = tokio::join!(
        rt.result::<Option<String>>("pg-one"),
        rt.result::<Option<String>>("pg-two")
    );
    let outputs = [one.expect("one outcome"), two.expect("two outcome")];
    assert_eq!(
        outputs.iter().filter(|value| value.is_some()).count(),
        1,
        "one live lease wins after the insert race"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only the winner dispatches a turn"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

#[cfg(feature = "postgres-tests")]
#[tokio::test]
async fn postgres_contender_cannot_steal_a_lease_renewed_after_its_stale_read() {
    use sqlx::Connection as _;

    let schema = atomic_durable::testing::schema("ws3j_renew_race").await;
    let rt = Arc::new(
        Durare::postgres(schema.url(), schema.name())
            .await
            .expect("postgres runtime"),
    );
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute(
                "INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,trust,created_by,created_at) VALUES (?,?,?,?,?,?,?,?,?)",
                &[Param::Text("at-work".into()), Param::Text("Work".into()), Param::Text(String::new()), Param::Text("task".into()), Param::Int(2), Param::Text("open".into()), Param::Text("human".into()), Param::Text("owner".into()), Param::Int(0)],
            ).await?;
            Ok(())
        })
    })
    .await
    .expect("seed claimable item");
    let original = rt
        .control_transaction(|mut tx| {
            Box::pin(async move {
                Ok(transactional_claim::claim(
                    &mut tx,
                    "at-work",
                    "holder",
                    "holder-acquisition",
                    Duration::from_secs(60),
                )
                .await?
                .expect("first claim"))
            })
        })
        .await
        .expect("claim original lease");
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute(
                "UPDATE atomic_graph_leases SET expires_at=0 WHERE item_id=?",
                &[Param::Text("at-work".into())],
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("expire original lease before the race");

    let (renewed_tx, mut renewed_rx) = tokio::sync::watch::channel(None::<i64>);
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let holder_rt = rt.clone();
    let holder = tokio::spawn(async move {
        holder_rt
            .control_transaction(move |mut tx| {
                let original = original.clone();
                let renewed_tx = renewed_tx.clone();
                let mut release_rx = release_rx.clone();
                Box::pin(async move {
                    transactional_claim::renew(&mut tx, &original, Duration::from_secs(60)).await?;
                    let pid = tx
                        .query_one("SELECT pg_backend_pid() AS pid", &[])
                        .await?
                        .int("pid")?;
                    renewed_tx.send(Some(pid)).expect("watch renewal backend");
                    while !*release_rx.borrow_and_update() {
                        release_rx
                            .changed()
                            .await
                            .expect("release renewal transaction");
                    }
                    Ok(())
                })
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while renewed_rx.borrow().is_none() {
            renewed_rx
                .changed()
                .await
                .expect("renewal backend became ready");
        }
    })
    .await
    .expect("renewal reached its open transaction");
    let holder_pid = renewed_rx.borrow().expect("renewal backend pid");

    let contender_rt = rt.clone();
    let contender = tokio::spawn(async move {
        contender_rt
            .control_transaction(|mut tx| {
                Box::pin(async move {
                    transactional_claim::claim(
                        &mut tx,
                        "at-work",
                        "contender",
                        "contender-acquisition",
                        Duration::from_secs(60),
                    )
                    .await
                })
            })
            .await
    });
    let mut monitor = sqlx::PgConnection::connect(schema.url())
        .await
        .expect("monitor Postgres locks");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1::int = ANY(pg_blocking_pids(pid)) AND query LIKE '%INSERT INTO atomic_graph_leases%')",
            )
            .bind(i32::try_from(holder_pid).expect("backend pid fits i32"))
            .fetch_one(&mut monitor)
            .await
            .expect("inspect blocked contender");
            if blocked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("contender waited on the renewing holder's row lock");

    release_tx.send(true).expect("commit the renewal");
    holder.await.expect("renewal task").expect("commit renewal");
    let attempted = contender
        .await
        .expect("contender task")
        .expect("claim attempt");
    assert!(
        attempted.is_none(),
        "a stale contender must not replace a lease renewed before its UPSERT"
    );
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            let row = tx
                .query_one(
                    "SELECT claimant,epoch,expires_at FROM atomic_graph_leases WHERE item_id=?",
                    &[Param::Text("at-work".into())],
                )
                .await?;
            assert_eq!(row.text("claimant")?, "holder");
            assert_eq!(row.int("epoch")?, 1);
            assert!(row.int("expires_at")? > 0);
            Ok(())
        })
    })
    .await
    .expect("renewed lease remains intact");
}
