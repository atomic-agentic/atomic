// SPDX-License-Identifier: Apache-2.0

//! A claim's first start time is durable graph state, not lease expiry or import age.

use atomic_durable::{DurableRuntime, Durare};
use atomic_graph::sqlite::SqliteStore;
use atomic_graph::store::{Actor, Draft, GraphStore, StoreError};
use atomic_graph::transactional_claim;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn path(label: &str) -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "atomic-graph-claim-time-{label}-{}-{}.sqlite",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn store(path: &std::path::Path, clock: &Arc<AtomicI64>) -> SqliteStore {
    let clock = Arc::clone(clock);
    SqliteStore::open(path, move || {
        UNIX_EPOCH + Duration::from_millis(clock.load(Ordering::SeqCst) as u64)
    })
    .expect("graph store")
}

#[tokio::test]
async fn first_claim_time_survives_replay_renewal_and_takeover() {
    let path = path("direct");
    let clock = Arc::new(AtomicI64::new(1_000));
    let graph = store(&path, &clock);
    graph
        .create_item(
            Actor::agent("author"),
            "create-one".into(),
            Draft::new("one", "One"),
        )
        .await
        .expect("create item");
    assert_eq!(
        graph.first_claimed_at_ms("one".into()).await.expect("read"),
        None
    );
    assert!(matches!(
        graph.first_claimed_at_ms("absent".into()).await,
        Err(StoreError::MissingItem { item_id }) if item_id == "absent"
    ));
    assert!(
        graph
            .claim(
                Actor::agent("author"),
                "run-a".into(),
                "absent".into(),
                Duration::from_secs(2)
            )
            .await
            .expect("missing claim")
            .is_none()
    );

    let first = graph
        .claim(
            Actor::agent("author"),
            "run-a".into(),
            "one".into(),
            Duration::from_secs(2),
        )
        .await
        .expect("first claim")
        .expect("first lease");
    assert_eq!(
        graph.first_claimed_at_ms("one".into()).await.expect("read"),
        Some(1_000)
    );

    clock.store(2_000, Ordering::SeqCst);
    let replay = graph
        .claim(
            Actor::agent("author"),
            "run-a".into(),
            "one".into(),
            Duration::from_secs(2),
        )
        .await
        .expect("same acquisition")
        .expect("replayed lease");
    assert_eq!(replay, first);
    graph
        .heartbeat(replay, Duration::from_secs(2))
        .await
        .expect("renew lease");
    assert!(
        graph
            .claim(
                Actor::agent("other"),
                "run-b".into(),
                "one".into(),
                Duration::from_secs(2)
            )
            .await
            .expect("held item")
            .is_none()
    );
    assert_eq!(
        graph.first_claimed_at_ms("one".into()).await.expect("read"),
        Some(1_000)
    );

    clock.store(10_000, Ordering::SeqCst);
    let takeover = graph
        .claim(
            Actor::agent("other"),
            "run-b".into(),
            "one".into(),
            Duration::from_secs(2),
        )
        .await
        .expect("takeover")
        .expect("new lease");
    assert!(takeover.epoch > first.epoch);
    assert_eq!(
        graph.first_claimed_at_ms("one".into()).await.expect("read"),
        Some(1_000)
    );
    drop(graph);
    std::fs::remove_file(path).expect("remove fixture");
}

#[tokio::test]
async fn workflow_claim_records_only_a_successful_first_claim() {
    let path = path("workflow");
    let runtime = Durare::sqlite(&path).await.expect("durable runtime");
    let clock = Arc::new(AtomicI64::new(20_000));
    let graph = store(&path, &clock);
    graph
        .create_item(
            Actor::agent("author"),
            "create-work".into(),
            Draft::new("work", "Work"),
        )
        .await
        .expect("create work");
    let mut blocked = Draft::new("blocked", "Blocked");
    blocked.status = atomic_graph::store::Status::Blocked;
    graph
        .create_item(Actor::agent("author"), "create-blocked".into(), blocked)
        .await
        .expect("create blocked");

    let refused = runtime
        .control_transaction(|mut tx| {
            Box::pin(async move {
                transactional_claim::claim(
                    &mut tx,
                    "blocked",
                    "run",
                    "blocked-key",
                    Duration::from_secs(60),
                )
                .await
            })
        })
        .await
        .expect("refused claim transaction");
    assert!(refused.is_none());
    assert_eq!(
        graph
            .first_claimed_at_ms("blocked".into())
            .await
            .expect("read"),
        None
    );

    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64;
    let first = runtime
        .control_transaction(|mut tx| {
            Box::pin(async move {
                transactional_claim::claim(
                    &mut tx,
                    "work",
                    "run",
                    "work-key",
                    Duration::from_secs(60),
                )
                .await
            })
        })
        .await
        .expect("claim transaction")
        .expect("lease");
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64;
    let started = graph
        .first_claimed_at_ms("work".into())
        .await
        .expect("read")
        .expect("recorded time");
    assert!((before..=after).contains(&started));

    let replay = runtime
        .control_transaction(|mut tx| {
            Box::pin(async move {
                transactional_claim::claim(
                    &mut tx,
                    "work",
                    "run",
                    "work-key",
                    Duration::from_secs(60),
                )
                .await
            })
        })
        .await
        .expect("replay transaction")
        .expect("replayed lease");
    assert_eq!(replay, first);
    assert_eq!(
        graph
            .first_claimed_at_ms("work".into())
            .await
            .expect("read"),
        Some(started)
    );
    drop(graph);
    drop(runtime);
    std::fs::remove_file(path).expect("remove fixture");
}
