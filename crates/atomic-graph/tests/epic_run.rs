// SPDX-License-Identifier: Apache-2.0
//! The graph work loop's short checkpoint transactions, with real projection.

use atomic_core::workflow::Criterion;
use atomic_durable::gate::{self, HumanCredential, Principal};
use atomic_durable::publication::{Acceptance, Candidate, Proposal, Write, publish_candidate_in};
use atomic_durable::{DurableError, DurableRuntime, Durare, Param};
use atomic_graph::sqlite::SqliteStore;
use atomic_graph::store::{
    Actor, Draft, EdgeKind, Evidence, Finish, GraphStore, Status, StoreError,
};
use atomic_graph::transactional_claim::{self, ClaimNext, CommitProof, ReviewFollowup};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

fn path(label: &str) -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "atomic-epic-run-{label}-{}-{}.sqlite",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

async fn setup(label: &str) -> (Durare, SqliteStore, std::path::PathBuf) {
    let path = path(label);
    let rt = Durare::sqlite(&path)
        .await
        .expect("runtime with v15 schema");
    let credential = HumanCredential::new(Principal::local("operator"), "epic-run-secret");
    gate::enroll(&rt, &credential)
        .await
        .expect("operator enrollment");
    let store =
        SqliteStore::open_authorized(&path, SystemTime::now, credential).expect("graph store");
    (rt, store, path)
}

async fn create(store: &SqliteStore, id: &str, parent: Option<&str>, pinned: bool) {
    let mut draft = Draft::new(id, id);
    draft.kind = if parent.is_none() {
        "epic".into()
    } else {
        "task".into()
    };
    draft.parent = parent.map(str::to_owned);
    if pinned {
        draft.status = Status::Pinned;
    }
    store
        .create_item(Actor::human("spoofed"), format!("create-{id}"), draft)
        .await
        .expect("create graph item");
}

async fn select(rt: &Durare, step: &str) -> ClaimNext {
    let step = step.to_owned();
    rt.control_transaction(move |mut tx| {
        let step = step.clone();
        Box::pin(async move {
            transactional_claim::claim_next_for_epic(
                &mut tx,
                "run-one",
                &step,
                "epic",
                "run-one",
                Duration::from_secs(900),
                |_| Ok(()),
            )
            .await
        })
    })
    .await
    .expect("select and claim")
}

#[tokio::test]
async fn accepted_low_review_finding_is_deduped_outside_the_active_epic() {
    let (rt, store, path) = setup("review-followup").await;
    create(&store, "epic", None, true).await;
    create(&store, "one", Some("epic"), false).await;
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorize");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("bind");
    let ClaimNext::Claimed { lease, .. } = select(&rt, "claim-followup").await else {
        panic!("expected claimed item");
    };
    let finding = ReviewFollowup {
        reviewer: "independent-reviewer".into(),
        file: "docs/guide.md".into(),
        line: Some(12),
        severity: "low".into(),
        message: "Clarify the explanation".into(),
    };
    let invalid = ReviewFollowup {
        severity: "blocker".into(),
        ..finding.clone()
    };
    let failed_lease = lease.clone();
    let failed_finding = finding.clone();
    rt.control_transaction(move |mut tx| {
        let failed_lease = failed_lease.clone();
        let failed_finding = failed_finding.clone();
        let invalid = invalid.clone();
        Box::pin(async move {
            transactional_claim::file_review_followups(
                &mut tx,
                &failed_lease,
                "epic",
                "run-one",
                "one.review",
                &[failed_finding, invalid],
            )
            .await
        })
    })
    .await
    .expect_err("one invalid finding rolls back the entire batch");
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            let row = tx
                .query_one(
                    "SELECT COUNT(*) AS n FROM atomic_graph_items WHERE id LIKE 'atomic-review-%'",
                    &[],
                )
                .await?;
            assert_eq!(row.int("n")?, 0, "no partial follow-up or goal");
            Ok(())
        })
    })
    .await
    .expect("inspect rolled-back batch");
    for _ in 0..2 {
        let lease = lease.clone();
        let finding = finding.clone();
        rt.control_transaction(move |mut tx| {
            let lease = lease.clone();
            let finding = finding.clone();
            Box::pin(async move {
                transactional_claim::file_review_followups(
                    &mut tx,
                    &lease,
                    "epic",
                    "run-one",
                    "one.review",
                    &[finding],
                )
                .await
            })
        })
        .await
        .expect("file or replay finding");
    }
    rt.control_transaction(|mut tx| Box::pin(async move {
        let rows = tx.query_one(
            "SELECT COUNT(*) AS n FROM atomic_graph_items WHERE kind='task' AND id LIKE 'atomic-review-%'",
            &[],
        ).await?;
        assert_eq!(rows.int("n")?, 1, "replay cannot duplicate follow-up");
        let row = tx.query_one(
            "SELECT f.parent AS parent,g.kind AS parent_kind,g.status AS parent_status, \
             g.parent AS parent_parent, \
             e.target AS discovered FROM atomic_graph_items f \
             JOIN atomic_graph_items g ON g.id=f.parent \
             JOIN atomic_graph_edges e ON e.source=f.id AND e.kind='discovered-from' \
             WHERE f.id LIKE 'atomic-review-%' AND f.kind='task'",
            &[],
        ).await?;
        assert_eq!(row.text("parent_kind")?, "goal");
        assert_eq!(row.text("parent_status")?, "pinned");
        assert_eq!(row.text_opt("parent_parent")?, None);
        assert!(row.text("parent")?.starts_with("atomic-review-goal-"));
        assert_eq!(row.text("discovered")?, "one");
        Ok(())
    })).await.expect("inspect follow-up graph");
    let lease_for_close = lease.clone();
    rt.control_transaction(move |mut tx| {
        let lease_for_close = lease_for_close.clone();
        Box::pin(async move {
            transactional_claim::close_with_commit(&mut tx, &lease_for_close, &"a".repeat(40)).await
        })
    })
    .await
    .expect("close original item");
    assert!(
        matches!(
            select(&rt, "after-review-followup").await,
            ClaimNext::Complete
        ),
        "follow-up goal cannot keep the active epic open"
    );
    drop(rt);
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn claim_freezes_operator_criteria_separately_from_imported_prose() {
    let (rt, store, path) = setup("item-criteria-snapshot").await;
    create(&store, "epic", None, true).await;
    let mut child = Draft::new("one", "One");
    child.parent = Some("epic".into());
    child.body = "imported acceptance prose: never execute me".into();
    store
        .create_item(Actor::human("operator"), "create-one".into(), child)
        .await
        .expect("create child");
    let first = vec![Criterion::Test {
        argv: vec!["cargo".into(), "test".into()],
        timeout_ms: 30_000,
    }];
    store
        .set_item_criteria(Actor::human("operator"), "one".into(), first.clone())
        .await
        .expect("set structured criteria");
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorize run");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("bind epic");
    let selected = select(&rt, "claim-criteria").await;
    let ClaimNext::Claimed { item, .. } = &selected else {
        panic!("expected child claim");
    };
    assert_eq!(item.criteria, first);
    assert_eq!(
        item.acceptance_criteria, None,
        "body prose was executed as criteria"
    );
    let checkpoint = serde_json::to_value(&selected).expect("encode claim checkpoint");
    let changed = vec![Criterion::Cel {
        expr: "output.ok == true".into(),
    }];
    store
        .set_item_criteria(Actor::human("operator"), "one".into(), changed.clone())
        .await
        .expect("change operator criteria");
    assert_eq!(
        store
            .item_criteria("one")
            .await
            .expect("read criteria")
            .expect("set criteria")
            .criteria,
        changed,
    );
    let frozen: ClaimNext = serde_json::from_value(checkpoint).expect("decode claim checkpoint");
    let ClaimNext::Claimed { item: frozen, .. } = frozen else {
        panic!("checkpoint lost item");
    };
    assert_eq!(
        frozen.criteria, first,
        "later operator edit changed a claim checkpoint"
    );
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn review_reclaim_requires_the_original_epic_still_pinned() {
    let (rt, store, path) = setup("review-parent-fence").await;
    create(&store, "epic", None, true).await;
    create(&store, "one", Some("epic"), false).await;
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorize run");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("bind epic");
    let ClaimNext::Claimed { lease, .. } = select(&rt, "claim-review-parent").await else {
        panic!("expected claimed child");
    };
    let old = lease.clone();
    let released_epoch = rt
        .control_transaction(move |mut tx| {
            let old = old.clone();
            Box::pin(async move {
                transactional_claim::release_for_review_gate(&mut tx, &old, "review-gate").await
            })
        })
        .await
        .expect("release review claim");
    // Simulate a persisted parent change independently of the admin-path
    // binding invalidation. The re-claim guard itself must see the parent.
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute(
                "UPDATE atomic_graph_items SET status='closed' WHERE id='epic'",
                &[],
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("change parent status");
    let error = rt
        .control_transaction(move |mut tx| {
            let lease = lease.clone();
            Box::pin(async move {
                transactional_claim::reclaim_after_review_gate(
                    &mut tx,
                    &lease,
                    released_epoch,
                    "review-gate",
                    Duration::from_secs(900),
                )
                .await
            })
        })
        .await
        .expect_err("stale review reclaimed a child of a closed epic");
    assert!(matches!(error, DurableError::Parked { .. }), "{error}");
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn invalidated_epic_binding_stays_fenced_after_operator_repins() {
    let (rt, store, path) = setup("repin-fence").await;
    create(&store, "epic", None, true).await;
    create(&store, "one", Some("epic"), false).await;
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorize run");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("bind epic");
    store
        .set_status(Actor::human("operator"), "epic".into(), Status::Open)
        .await
        .expect("unpin epic");
    store
        .set_status(Actor::human("operator"), "epic".into(), Status::Pinned)
        .await
        .expect("repin epic");
    let rebind = rt
        .control_transaction(|mut tx| {
            Box::pin(
                async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
            )
        })
        .await;
    assert!(matches!(rebind, Err(DurableError::Parked { .. })));
    let select = rt
        .control_transaction(|mut tx| {
            Box::pin(async move {
                transactional_claim::claim_next_for_epic(
                    &mut tx,
                    "run-one",
                    "after-repin",
                    "epic",
                    "run-one",
                    Duration::from_secs(900),
                    |_| Ok(()),
                )
                .await
            })
        })
        .await;
    assert!(matches!(select, Err(DurableError::Parked { .. })));
    let finish = rt
        .control_transaction(|mut tx| {
            Box::pin(async move {
                transactional_claim::finish_epic_run(&mut tx, "epic", "run-one").await
            })
        })
        .await;
    assert!(matches!(finish, Err(DurableError::Parked { .. })));
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn three_dependent_children_close_with_commits_and_refresh_ready_projection() {
    let (rt, store, path) = setup("three").await;
    create(&store, "epic", None, true).await;
    for id in ["one", "two", "three"] {
        create(&store, id, Some("epic"), false).await;
    }
    store
        .set_item_close_policy(
            Actor::human("operator"),
            "one".into(),
            vec![Evidence::Commit],
            None,
        )
        .await
        .expect("first child requires an actual commit link");
    store
        .add_edge(
            Actor::human("operator"),
            "two".into(),
            EdgeKind::Blocks,
            "one".into(),
        )
        .await
        .expect("one blocks two");
    store
        .add_edge(
            Actor::human("operator"),
            "three".into(),
            EdgeKind::Blocks,
            "two".into(),
        )
        .await
        .expect("two blocks three");
    // An excluded child never causes an otherwise finished epic to pend.
    let mut note = Draft::new("pinned-note", "Pinned note");
    note.parent = Some("epic".into());
    note.status = Status::Pinned;
    store
        .create_item(Actor::human("operator"), "create-pinned-note".into(), note)
        .await
        .expect("pinned child");
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorized admission");
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("repeat authorization");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("bind epic");
    assert!(
        matches!(
            store.authorize_graph_run("epic", "run-two").await,
            Err(StoreError::EpicAdmission { .. })
        ),
        "different run cannot take binding after bind"
    );
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("same run bind repeats");
    for (ordinal, expected) in ["one", "two", "three"].iter().enumerate() {
        let selected = select(&rt, &format!("claim-{ordinal}")).await;
        let ClaimNext::Claimed { item, lease } = selected else {
            panic!("expected {expected}");
        };
        assert_eq!(
            item.id, *expected,
            "dependency projection selects the next child"
        );
        let sha = format!("{:040x}", ordinal + 1);
        rt.control_transaction(move |mut tx| {
            let (lease, sha) = (lease.clone(), sha.clone());
            Box::pin(
                async move { transactional_claim::close_with_commit(&mut tx, &lease, &sha).await },
            )
        })
        .await
        .expect("close and refresh projection in one transaction");
    }
    assert_eq!(select(&rt, "claim-done").await, ClaimNext::Complete);
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::finish_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("mark done");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::finish_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("same run done repeats");
    assert!(
        matches!(
            store.authorize_graph_run("epic", "run-three").await,
            Err(StoreError::EpicAdmission { .. })
        ),
        "a completed binding cannot silently be reworked by another run"
    );
    let rows = rt.control_transaction(|mut tx| Box::pin(async move {
        let closed = tx.query_one("SELECT COUNT(*) AS n FROM atomic_graph_items WHERE parent=? AND status='closed'", &[Param::Text("epic".into())]).await?.int("n")?;
        let links = tx.query_one("SELECT COUNT(*) AS n FROM atomic_graph_commit_links WHERE item_id IN ('one','two','three')", &[]).await?.int("n")?;
        let bound = tx.query_one("SELECT state FROM atomic_graph_epic_runs WHERE epic_id='epic'", &[]).await?.text("state")?;
        Ok((closed, links, bound))
    })).await.expect("inspect result");
    assert_eq!(rows, (3, 3, "done".into()));
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn invalid_item_mapping_refuses_before_any_claim_or_epoch_change() {
    let (rt, store, path) = setup("invalid-mapping").await;
    create(&store, "epic", None, true).await;
    create(&store, "one", Some("epic"), false).await;
    let plain = SqliteStore::open(&path, SystemTime::now).expect("plain graph store");
    assert!(
        matches!(
            plain.authorize_graph_run("epic", "run-one").await,
            Err(StoreError::NeedsHumanCredential { .. })
        ),
        "an agent cannot authorize a run"
    );
    let wrong = SqliteStore::open_authorized(
        &path,
        SystemTime::now,
        HumanCredential::new(Principal::local("operator"), "wrong secret"),
    )
    .expect("store with wrong credential");
    assert!(
        matches!(
            wrong.authorize_graph_run("epic", "run-one").await,
            Err(StoreError::NeedsHumanCredential { .. })
        ),
        "the current enrolled credential is checked in the write transaction"
    );
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorize");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("bind");
    let error = rt
        .control_transaction(|mut tx| {
            Box::pin(async move {
                transactional_claim::claim_next_for_epic(
                    &mut tx,
                    "run-one",
                    "bad-mapping",
                    "epic",
                    "run-one",
                    Duration::from_secs(900),
                    |_| {
                        Err(DurableError::Failed(
                            "item schema rejects mapped atom input".into(),
                        ))
                    },
                )
                .await
            })
        })
        .await
        .expect_err("mapping must refuse");
    assert!(error.to_string().contains("item schema rejects"));
    let state = rt
        .control_transaction(|mut tx| {
            Box::pin(async move {
                let epoch = tx
                    .query_one(
                        "SELECT claim_epoch FROM atomic_graph_items WHERE id='one'",
                        &[],
                    )
                    .await?
                    .int("claim_epoch")?;
                let leases = tx
                    .query_one("SELECT COUNT(*) AS n FROM atomic_graph_leases", &[])
                    .await?
                    .int("n")?;
                Ok((epoch, leases))
            })
        })
        .await
        .expect("inspect refusal");
    assert_eq!(state, (0, 0));
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn operator_reconciles_terminal_failed_mapping_run_without_displacing_a_lease() {
    let (rt, store, path) = setup("failed-run-reconcile").await;
    create(&store, "epic", None, true).await;
    create(&store, "one", Some("epic"), false).await;
    create(&store, "closed-child", Some("epic"), false).await;
    store
        .set_status(
            Actor::human("operator"),
            "closed-child".into(),
            Status::Closed,
        )
        .await
        .expect("previous child close");
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorize old run");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("bind old run");
    let mapping_error = rt
        .control_transaction(|mut tx| {
            Box::pin(async move {
                transactional_claim::claim_next_for_epic(
                    &mut tx,
                    "run-one",
                    "bad-mapping",
                    "epic",
                    "run-one",
                    Duration::from_secs(900),
                    |_| Err(DurableError::Failed("invalid item mapping".into())),
                )
                .await
            })
        })
        .await
        .expect_err("bad mapping must fail after bind");
    assert!(mapping_error.to_string().contains("invalid item mapping"));
    assert!(
        matches!(
            store.authorize_graph_run("epic", "run-two").await,
            Err(StoreError::EpicAdmission { .. })
        ),
        "binding strands corrected run"
    );

    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute(
                "INSERT INTO workflow_status(workflow_uuid,status,name) VALUES ('run-one','PENDING','atomic.graph.pipeline.v1')",
                &[],
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("record unfinished durable run");
    assert!(
        matches!(
            store.reconcile_failed_epic_run("epic", "run-one").await,
            Err(StoreError::EpicAdmission { .. })
        ),
        "unfinished run must retain binding"
    );
    let plain = SqliteStore::open(&path, SystemTime::now).expect("plain store");
    assert!(
        matches!(
            plain.reconcile_failed_epic_run("epic", "run-one").await,
            Err(StoreError::NeedsHumanCredential { .. })
        ),
        "agent cannot reconcile"
    );
    let wrong = SqliteStore::open_authorized(
        &path,
        SystemTime::now,
        HumanCredential::new(Principal::local("operator"), "wrong secret"),
    )
    .expect("forged store");
    assert!(
        matches!(
            wrong.reconcile_failed_epic_run("epic", "run-one").await,
            Err(StoreError::NeedsHumanCredential { .. })
        ),
        "forged credential cannot reconcile"
    );

    let ClaimNext::Claimed { lease, .. } = select(&rt, "claim-before-failure").await else {
        panic!("ready child");
    };
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute(
                "UPDATE workflow_status SET status='ERROR' WHERE workflow_uuid='run-one'",
                &[],
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("record terminal failure");
    assert!(
        matches!(
            store.reconcile_failed_epic_run("epic", "run-one").await,
            Err(StoreError::EpicAdmission { .. })
        ),
        "failed run still holding a lease cannot be displaced"
    );
    store
        .finish(lease, Finish::Release)
        .await
        .expect("release failed run lease");
    rt.control_transaction(|mut tx| Box::pin(async move {
        tx.execute("UPDATE workflow_status SET name='unrelated.workflow' WHERE workflow_uuid='run-one'", &[]).await?;
        Ok(())
    })).await.expect("make failed run unrelated");
    assert!(
        matches!(
            store.reconcile_failed_epic_run("epic", "run-one").await,
            Err(StoreError::EpicAdmission { .. })
        ),
        "unrelated failed workflow cannot release graph binding"
    );
    rt.control_transaction(|mut tx| Box::pin(async move {
        tx.execute("UPDATE workflow_status SET name='atomic.graph.pipeline.v1' WHERE workflow_uuid='run-one'", &[]).await?;
        tx.execute("DELETE FROM atomic_graph_run_authorizations WHERE run_id='run-one'", &[]).await?;
        Ok(())
    })).await.expect("remove old authorization in test fixture");
    assert!(
        matches!(
            store.reconcile_failed_epic_run("epic", "run-one").await,
            Err(StoreError::EpicAdmission { .. })
        ),
        "binding without operator authorization cannot be released"
    );
    rt.control_transaction(|mut tx| Box::pin(async move {
        tx.execute("INSERT INTO atomic_graph_run_authorizations (run_id,epic_id,principal) VALUES ('run-one','epic','operator')", &[]).await?;
        Ok(())
    })).await.expect("restore old authorization");
    store
        .reconcile_failed_epic_run("epic", "run-one")
        .await
        .expect("reconcile failed run");
    store
        .reconcile_failed_epic_run("epic", "run-one")
        .await
        .expect("retry reconciliation");
    store
        .authorize_graph_run("epic", "run-two")
        .await
        .expect("admit corrected run");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-two").await },
        )
    })
    .await
    .expect("bind corrected run");
    store
        .reconcile_failed_epic_run("epic", "run-one")
        .await
        .expect("retry after new owner");
    let state = rt.control_transaction(|mut tx| Box::pin(async move {
        let bound = tx.query_one("SELECT run_id FROM atomic_graph_epic_runs WHERE epic_id='epic'", &[]).await?.text("run_id")?;
        let old_auth = tx.query_one("SELECT COUNT(*) AS n FROM atomic_graph_run_authorizations WHERE run_id='run-one'", &[]).await?.int("n")?;
        let closed = tx.query_one("SELECT status FROM atomic_graph_items WHERE id='closed-child'", &[]).await?.text("status")?;
        Ok((bound, old_auth, closed))
    })).await.expect("inspect recovered graph");
    assert_eq!(state, ("run-two".into(), 0, "closed".into()));
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn terminal_failure_release_fences_exact_claim_and_leaves_handoff() {
    let (rt, store, path) = setup("terminal-failure-release").await;
    create(&store, "epic", None, true).await;
    create(&store, "one", Some("epic"), false).await;
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorize");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("bind");
    let ClaimNext::Claimed { lease, .. } = select(&rt, "claim-for-failure").await else {
        panic!("ready child");
    };
    let released = rt
        .control_transaction({
            let lease = lease.clone();
            move |mut tx| {
                let lease = lease.clone();
                Box::pin(async move {
                    transactional_claim::release_after_failure(
                        &mut tx,
                        &lease,
                        "criteria failed after repair",
                    )
                    .await
                })
            }
        })
        .await
        .expect("release on terminal failure");
    assert_eq!(released, lease.epoch + 1);
    let state = rt.control_transaction(|mut tx| Box::pin(async move {
        let item = tx.query_one("SELECT status,claim_epoch FROM atomic_graph_items WHERE id='one'", &[]).await?;
        let leases = tx.query_one("SELECT COUNT(*) AS n FROM atomic_graph_leases WHERE item_id='one'", &[]).await?.int("n")?;
        let note = tx.query_one("SELECT text,trust FROM atomic_graph_notes WHERE item_id='one' ORDER BY ordinal DESC LIMIT 1", &[]).await?;
        Ok((item.text("status")?,item.int("claim_epoch")?,leases,note.text("text")?,note.text("trust")?))
    })).await.expect("inspect release");
    assert_eq!(state.0, "open");
    assert_eq!(state.1, released);
    assert_eq!(state.2, 0);
    assert!(state.3.contains("criteria failed after repair"));
    assert_eq!(state.4, "agent");
    let stale = rt
        .control_transaction(move |mut tx| {
            let lease = lease.clone();
            Box::pin(async move {
                transactional_claim::release_after_failure(&mut tx, &lease, "retry").await
            })
        })
        .await
        .expect_err("stale token must park");
    assert!(matches!(stale, DurableError::Parked { .. }));
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn admission_refuses_missing_and_unpinned_epics_with_guidance() {
    let (_rt, store, path) = setup("admission-refusal").await;
    let missing = store
        .authorize_graph_run("absent", "missing-run")
        .await
        .expect_err("missing epic refused");
    assert!(matches!(&missing, StoreError::EpicAdmission { .. }));
    assert!(missing.to_string().contains("--kind epic --pinned"));
    create(&store, "epic", None, false).await;
    let open = store
        .authorize_graph_run("epic", "open-run")
        .await
        .expect_err("open epic refused");
    assert!(matches!(&open, StoreError::EpicAdmission { .. }));
    assert!(open.to_string().contains("pinned"));
    store
        .set_status(Actor::human("operator"), "epic".into(), Status::Closed)
        .await
        .expect("human closes empty epic");
    let closed = store
        .authorize_graph_run("epic", "closed-run")
        .await
        .expect_err("closed epic refused");
    assert!(matches!(&closed, StoreError::EpicAdmission { .. }));
    assert!(closed.to_string().contains("pinned"));
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn verified_commit_policy_cannot_be_satisfied_by_an_unverified_worktree_sha() {
    let (rt, store, path) = setup("verified-refusal").await;
    create(&store, "epic", None, true).await;
    create(&store, "one", Some("epic"), false).await;
    store
        .set_item_close_policy(
            Actor::human("operator"),
            "one".into(),
            vec![Evidence::VerifiedCommit],
            Some("refs/heads/main".into()),
        )
        .await
        .expect("operator sets verified policy");
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorize");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("bind");
    let ClaimNext::Claimed { lease, .. } = select(&rt, "claim-verified").await else {
        panic!("one ready work child");
    };
    let lease_for_retry = lease.clone();
    let lease_for_proof = lease.clone();
    let error = rt
        .control_transaction(move |mut tx| {
            let lease = lease.clone();
            Box::pin(async move {
                transactional_claim::close_with_commit(&mut tx, &lease, &"a".repeat(40)).await
            })
        })
        .await
        .expect_err("bare SHA must not claim branch verification");
    assert!(error.to_string().contains("host-verified branch evidence"));
    let state = rt
        .control_transaction(|mut tx| {
            Box::pin(async move {
                let status = tx
                    .query_one("SELECT status FROM atomic_graph_items WHERE id='one'", &[])
                    .await?
                    .text("status")?;
                let links = tx
                    .query_one(
                        "SELECT COUNT(*) AS n FROM atomic_graph_commit_links WHERE item_id='one'",
                        &[],
                    )
                    .await?
                    .int("n")?;
                Ok((status, links))
            })
        })
        .await
        .expect("no partial close");
    assert_eq!(state, ("in_progress".into(), 0));
    // Legacy flags and a missing branch-bound proof must not satisfy this
    // rule, even if an untrusted caller writes the link row directly.
    rt.control_transaction(|mut tx| Box::pin(async move {
        tx.execute(
            "INSERT INTO atomic_graph_commit_links (item_id,sha,indexed,on_branch) VALUES ('one',?,1,1)",
            &[Param::Text("a".repeat(40))],
        ).await?;
        Ok(())
    })).await.expect("legacy flags fixture");
    let refused = rt
        .control_transaction(move |mut tx| {
            let lease = lease_for_retry.clone();
            Box::pin(async move {
                transactional_claim::close_with_commit(&mut tx, &lease, &"a".repeat(40)).await
            })
        })
        .await
        .expect_err("legacy flags cannot attest a branch");
    assert!(
        refused
            .to_string()
            .contains("host-verified branch evidence")
    );
    // The host attestation path stores branch, head, and pinned base. The
    // checkpoint close may consume that exact existing proof, preserving it.
    rt.control_transaction(|mut tx| Box::pin(async move {
        tx.execute(
            "UPDATE atomic_graph_commit_links SET verified_branch='refs/heads/main',verified_head=?,pinned_base=? WHERE item_id='one' AND sha=?",
            &[Param::Text("b".repeat(40)), Param::Text("c".repeat(40)), Param::Text("a".repeat(40))],
        ).await?;
        Ok(())
    })).await.expect("host attestation fixture");
    rt.control_transaction(move |mut tx| {
        let lease = lease_for_proof.clone();
        Box::pin(async move {
            transactional_claim::close_with_commit(&mut tx, &lease, &"a".repeat(40)).await
        })
    })
    .await
    .expect("host-attested same-SHA close");
    let item = store
        .show(Actor::agent("inspect"), "one".into())
        .await
        .expect("show")
        .expect("item");
    assert_eq!(item.item.status, Status::Closed);
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn staged_graph_commit_needs_exact_host_branch_proof_and_never_downgrades_it() {
    let (rt, store, path) = setup("staged-verified-proof").await;
    create(&store, "epic", None, true).await;
    create(&store, "one", Some("epic"), false).await;
    store
        .set_item_close_policy(
            Actor::human("operator"),
            "one".into(),
            vec![Evidence::VerifiedCommit],
            Some("refs/heads/main".into()),
        )
        .await
        .expect("set branch policy");
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorize run");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("bind epic");
    let ClaimNext::Claimed { lease, .. } = select(&rt, "claim-staged-proof").await else {
        panic!("one ready item");
    };
    let sha = "a".repeat(40);
    let stage_lease = lease.clone();
    let stage_sha = sha.clone();
    let proof = rt
        .control_transaction(move |mut tx| {
            let lease = stage_lease.clone();
            let sha = stage_sha.clone();
            Box::pin(async move {
                transactional_claim::stage_commit_for_close(&mut tx, &lease, &sha).await
            })
        })
        .await
        .expect("stage host SHA");
    assert_eq!(
        proof,
        CommitProof::Missing {
            branch: "refs/heads/main".into()
        }
    );
    let staged = rt
        .control_transaction(|mut tx| {
            Box::pin(async move {
                let row = tx.query_one(
                    "SELECT indexed,on_branch FROM atomic_graph_commit_links WHERE item_id='one'",
                    &[],
                )
                .await?;
                Ok((row.int("indexed")?, row.int("on_branch")?))
            })
        })
        .await
        .expect("staged link");
    assert_eq!(staged, (0, 0));
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute(
                "UPDATE atomic_graph_commit_links SET indexed=1,on_branch=1,verified_branch='refs/heads/other',verified_head=?,pinned_base=? WHERE item_id='one'",
                &[Param::Text("b".repeat(40)), Param::Text("c".repeat(40))],
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("wrong branch proof fixture");
    let wrong_lease = lease.clone();
    let wrong_sha = sha.clone();
    let wrong = rt
        .control_transaction(move |mut tx| {
            let lease = wrong_lease.clone();
            let sha = wrong_sha.clone();
            Box::pin(async move {
                transactional_claim::stage_commit_for_close(&mut tx, &lease, &sha).await
            })
        })
        .await
        .expect("recheck wrong branch");
    assert_eq!(wrong, proof);
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute(
                "UPDATE atomic_graph_commit_links SET verified_branch='refs/heads/main' WHERE item_id='one'",
                &[],
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("exact branch proof fixture");
    let ready_lease = lease.clone();
    let ready_sha = sha.clone();
    let ready = rt
        .control_transaction(move |mut tx| {
            let lease = ready_lease.clone();
            let sha = ready_sha.clone();
            Box::pin(async move {
                transactional_claim::stage_commit_for_close(&mut tx, &lease, &sha).await
            })
        })
        .await
        .expect("recheck correct branch");
    assert_eq!(ready, CommitProof::Ready);
    let retained = rt
        .control_transaction(|mut tx| {
            Box::pin(async move {
                let row = tx.query_one(
                    "SELECT indexed,on_branch,verified_branch,verified_head,pinned_base FROM atomic_graph_commit_links WHERE item_id='one'",
                    &[],
                )
                .await?;
                Ok((
                    row.int("indexed")?,
                    row.int("on_branch")?,
                    row.text("verified_branch")?,
                    row.text("verified_head")?,
                    row.text("pinned_base")?,
                ))
            })
        })
        .await
        .expect("retained proof");
    assert_eq!(retained.0, 1);
    assert_eq!(retained.1, 1);
    assert_eq!(retained.2, "refs/heads/main");
    assert_eq!(retained.3, "b".repeat(40));
    assert_eq!(retained.4, "c".repeat(40));
    store
        .set_status(Actor::human("operator"), "one".into(), Status::Open)
        .await
        .expect("operator revokes claim");
    let fenced_lease = lease.clone();
    let fenced_sha = sha.clone();
    let fenced = rt
        .control_transaction(move |mut tx| {
            let lease = fenced_lease.clone();
            let sha = fenced_sha.clone();
            Box::pin(async move {
                transactional_claim::stage_commit_for_close(&mut tx, &lease, &sha).await
            })
        })
        .await
        .expect("stale claim is semantic result");
    assert_eq!(fenced, CommitProof::Fenced);
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn graph_publication_and_fenced_close_are_one_rollback_safe_transaction() {
    let (rt, store, path) = setup("atomic-publish-close").await;
    create(&store, "epic", None, true).await;
    create(&store, "one", Some("epic"), false).await;
    store
        .set_item_close_policy(
            Actor::human("operator"),
            "one".into(),
            vec![Evidence::VerifiedCommit],
            Some("refs/heads/main".into()),
        )
        .await
        .expect("set verified policy");
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorize");
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await?;
            tx.execute("CREATE TABLE graph_atomic_write (value TEXT NOT NULL)", &[])
                .await?;
            Ok(())
        })
    })
    .await
    .expect("fixture");
    let ClaimNext::Claimed { lease, .. } = select(&rt, "claim-atomic-publish").await else {
        panic!("one ready item");
    };
    let sha = "a".repeat(40);
    let proposal = Proposal {
        output: json!({"done":true}),
        metadata: json!({}),
        writes: vec![Write {
            sql: "INSERT INTO graph_atomic_write (value) VALUES (?)".into(),
            params: vec![json!("published")],
        }],
    };
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&proposal).expect("serialize graph proposal"))
    );
    let candidate = Candidate {
        id: "run-one:accepted".into(),
        proposal,
        digest: digest.clone(),
    };
    let acceptance = Acceptance {
        candidate: candidate.id.clone(),
        digest,
        accepted: true,
        reason: None,
    };
    let missing_lease = lease.clone();
    let missing_sha = sha.clone();
    let missing_candidate = candidate.clone();
    let missing_acceptance = acceptance.clone();
    let missing = rt
        .control_transaction(move |mut tx| {
            let lease = missing_lease.clone();
            let sha = missing_sha.clone();
            let candidate = missing_candidate.clone();
            let acceptance = missing_acceptance.clone();
            Box::pin(async move {
                let proof =
                    transactional_claim::stage_commit_for_close(&mut tx, &lease, &sha).await?;
                if proof != CommitProof::Ready {
                    return Ok(proof);
                }
                publish_candidate_in(
                    &mut tx,
                    "graph.00000.fix.publish",
                    &candidate,
                    &acceptance,
                    &[],
                )
                .await?;
                transactional_claim::close_with_commit(&mut tx, &lease, &sha).await?;
                Ok(CommitProof::Ready)
            })
        })
        .await
        .expect("proof probe");
    assert_eq!(
        missing,
        CommitProof::Missing {
            branch: "refs/heads/main".into()
        }
    );
    let before = rt.control_transaction(|mut tx| Box::pin(async move {
        let writes = tx.query_one("SELECT COUNT(*) AS n FROM graph_atomic_write", &[]).await?.int("n")?;
        let publications = tx.query_one("SELECT COUNT(*) AS n FROM atomic_publications WHERE publication_key='run-one:accepted'", &[]).await?.int("n")?;
        let status = tx.query_one("SELECT status FROM atomic_graph_items WHERE id='one'", &[]).await?.text("status")?;
        Ok((writes, publications, status))
    })).await.expect("inspect missing proof");
    assert_eq!(before, (0, 0, "in_progress".into()));
    rt.control_transaction(|mut tx| Box::pin(async move {
        tx.execute(
            "UPDATE atomic_graph_commit_links SET indexed=1,on_branch=1,verified_branch='refs/heads/main',verified_head=?,pinned_base=? WHERE item_id='one' AND sha=?",
            &[Param::Text("b".repeat(40)), Param::Text("c".repeat(40)), Param::Text("a".repeat(40))],
        ).await?;
        Ok(())
    })).await.expect("record host proof fixture");
    let unaccepted_candidate = candidate.clone();
    let unaccepted_acceptance = acceptance.clone();
    let unaccepted = rt
        .control_transaction(move |mut tx| {
            let candidate = unaccepted_candidate.clone();
            let mut acceptance = unaccepted_acceptance.clone();
            acceptance.accepted = false;
            Box::pin(async move {
                publish_candidate_in(
                    &mut tx,
                    "graph.00000.fix.publish",
                    &candidate,
                    &acceptance,
                    &[],
                )
                .await
            })
        })
        .await;
    assert!(
        unaccepted.is_err(),
        "the in-transaction API accepted an unreviewed candidate"
    );
    let rollback_lease = lease.clone();
    let rollback_sha = sha.clone();
    let rollback_candidate = candidate.clone();
    let rollback_acceptance = acceptance.clone();
    let rolled_back = rt
        .control_transaction(move |mut tx| {
            let lease = rollback_lease.clone();
            let sha = rollback_sha.clone();
            let candidate = rollback_candidate.clone();
            let acceptance = rollback_acceptance.clone();
            Box::pin(async move {
                assert_eq!(
                    transactional_claim::stage_commit_for_close(&mut tx, &lease, &sha).await?,
                    CommitProof::Ready
                );
                publish_candidate_in(
                    &mut tx,
                    "graph.00000.fix.publish",
                    &candidate,
                    &acceptance,
                    &[],
                )
                .await?;
                Err::<(), _>(DurableError::Failed(
                    "forced post-publication close failure".into(),
                ))
            })
        })
        .await;
    assert!(rolled_back.is_err());
    let after_rollback = rt.control_transaction(|mut tx| Box::pin(async move {
        let writes = tx.query_one("SELECT COUNT(*) AS n FROM graph_atomic_write", &[]).await?.int("n")?;
        let publications = tx.query_one("SELECT COUNT(*) AS n FROM atomic_publications WHERE publication_key='run-one:accepted'", &[]).await?.int("n")?;
        Ok((writes, publications))
    })).await.expect("inspect rollback");
    assert_eq!(after_rollback, (0, 0));
    let final_lease = lease.clone();
    let final_sha = sha.clone();
    let final_candidate = candidate.clone();
    let final_acceptance = acceptance.clone();
    let output = rt
        .control_transaction(move |mut tx| {
            let lease = final_lease.clone();
            let sha = final_sha.clone();
            let candidate = final_candidate.clone();
            let acceptance = final_acceptance.clone();
            Box::pin(async move {
                assert_eq!(
                    transactional_claim::stage_commit_for_close(&mut tx, &lease, &sha).await?,
                    CommitProof::Ready
                );
                let output = publish_candidate_in(
                    &mut tx,
                    "graph.00000.fix.publish",
                    &candidate,
                    &acceptance,
                    &[],
                )
                .await?;
                transactional_claim::close_with_commit(&mut tx, &lease, &sha).await?;
                Ok(output)
            })
        })
        .await
        .expect("atomic publish and close");
    assert_eq!(output, json!({"done":true}));
    let final_state = rt.control_transaction(|mut tx| Box::pin(async move {
        let writes = tx.query_one("SELECT COUNT(*) AS n FROM graph_atomic_write", &[]).await?.int("n")?;
        let publications = tx.query_one("SELECT COUNT(*) AS n FROM atomic_publications WHERE publication_key='run-one:accepted'", &[]).await?.int("n")?;
        let status = tx.query_one("SELECT status FROM atomic_graph_items WHERE id='one'", &[]).await?.text("status")?;
        Ok((writes, publications, status))
    })).await.expect("inspect committed result");
    assert_eq!(final_state, (1, 1, "closed".into()));
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn released_human_claim_does_not_poison_later_graph_verified_close() {
    let (rt, store, path) = setup("released-human-then-graph").await;
    create(&store, "epic", None, true).await;
    create(&store, "one", Some("epic"), false).await;
    store
        .set_item_close_policy(
            Actor::human("operator"),
            "one".into(),
            vec![Evidence::VerifiedCommit],
            Some("refs/heads/main".into()),
        )
        .await
        .expect("set verified policy");
    let human = store
        .claim(
            Actor::human("operator"),
            "ignored claimant".into(),
            "one".into(),
            Duration::from_secs(90),
        )
        .await
        .expect("human claim")
        .expect("human lease");
    let human_refused = rt
        .control_transaction({
            let human = human.clone();
            move |mut tx| {
                let human = human.clone();
                Box::pin(async move {
                    transactional_claim::stage_commit_for_close(&mut tx, &human, &"a".repeat(40))
                        .await
                })
            }
        })
        .await
        .expect_err("current human claim cannot stage a graph close");
    assert!(human_refused.to_string().contains("human claim"));
    let human_close_refused = rt
        .control_transaction({
            let human = human.clone();
            move |mut tx| {
                let human = human.clone();
                Box::pin(async move {
                    transactional_claim::close_with_commit(&mut tx, &human, &"a".repeat(40)).await
                })
            }
        })
        .await
        .expect_err("current human claim cannot use graph close");
    assert!(human_close_refused.to_string().contains("human claim"));
    store
        .finish(human.clone(), Finish::Release)
        .await
        .expect("release human claim");
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorize graph run");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("bind graph run");
    let ClaimNext::Claimed { lease, .. } = select(&rt, "claim-after-human-release").await else {
        panic!("released child is ready for graph run");
    };
    let sha = "a".repeat(40);
    let missing = rt
        .control_transaction({
            let lease = lease.clone();
            let sha = sha.clone();
            move |mut tx| {
                let lease = lease.clone();
                let sha = sha.clone();
                Box::pin(async move {
                    transactional_claim::stage_commit_for_close(&mut tx, &lease, &sha).await
                })
            }
        })
        .await
        .expect("stage later graph token");
    assert_eq!(
        missing,
        CommitProof::Missing {
            branch: "refs/heads/main".into()
        }
    );
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute(
                "UPDATE atomic_graph_commit_links SET indexed=1,on_branch=1,verified_branch='refs/heads/main',verified_head=?,pinned_base=? WHERE item_id='one' AND sha=?",
                &[Param::Text("b".repeat(40)), Param::Text("c".repeat(40)), Param::Text("a".repeat(40))],
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("host proof fixture");
    rt.control_transaction({
        let lease = lease.clone();
        let sha = sha.clone();
        move |mut tx| {
            let lease = lease.clone();
            let sha = sha.clone();
            Box::pin(async move {
                assert_eq!(
                    transactional_claim::stage_commit_for_close(&mut tx, &lease, &sha).await?,
                    CommitProof::Ready
                );
                transactional_claim::close_with_commit(&mut tx, &lease, &sha).await
            })
        }
    })
    .await
    .expect("graph token closes after exact branch proof");
    let item = store
        .show(Actor::agent("inspect"), "one".into())
        .await
        .expect("show")
        .expect("closed item");
    assert_eq!(item.item.status, Status::Closed);
    drop(store);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn imported_beads_metadata_is_frozen_in_claimed_work_item() {
    let (rt, store, path) = setup("imported-criteria").await;
    create(&store, "epic", None, true).await;
    let exported = serde_json::json!({
        "id": "imported-task", "title": "Imported task", "description": "Implement the change",
        "acceptance_criteria": "A three-item epic is worked to completion",
        "labels": ["security", "durability"],
        "status": "open", "priority": 2, "issue_type": "task",
        "created_at": "2026-01-02T00:00:00Z", "parent_issue": "epic"
    })
    .to_string();
    store
        .import(Actor::human("operator"), exported, false)
        .await
        .expect("import beads item");
    store
        .authorize_graph_run("epic", "run-one")
        .await
        .expect("authorize");
    rt.control_transaction(|mut tx| {
        Box::pin(
            async move { transactional_claim::bind_epic_run(&mut tx, "epic", "run-one").await },
        )
    })
    .await
    .expect("bind");
    let ClaimNext::Claimed { item, .. } = select(&rt, "claim-imported").await else {
        panic!("imported task is ready");
    };
    assert_eq!(item.id, "imported-task");
    assert_eq!(item.body, "Implement the change");
    assert_eq!(item.labels, ["security", "durability"]);
    assert_eq!(
        item.acceptance_criteria.as_deref(),
        Some("A three-item epic is worked to completion")
    );
    let input = serde_json::to_value(&item).expect("typed host item");
    assert_eq!(
        input["acceptance_criteria"],
        "A three-item epic is worked to completion"
    );
    drop(store);
    let _ = std::fs::remove_file(path);
}
