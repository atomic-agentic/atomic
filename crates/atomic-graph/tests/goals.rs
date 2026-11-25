// SPDX-License-Identifier: Apache-2.0
//! Goal persistence through the same credentialed graph store as normal work.

use atomic_durable::gate::{self, HumanCredential, Principal};
use atomic_durable::runtime::Durare;
use atomic_graph::goal::{
    GoalBudget, GoalDefinition, GoalFeedbackKind, GoalIgnoreKind, GoalOutcome, GoalPeriod,
};
use atomic_graph::sqlite::SqliteStore;
use atomic_graph::store::{Actor, Draft, GraphStore, StoreError};
use serde_json::json;
use sqlx::{Connection, SqliteConnection};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

fn definition() -> GoalDefinition {
    GoalDefinition {
        title: "Triage regressions".into(),
        description: "Keep failures visible".into(),
        measure: None,
        budget: GoalBudget {
            period: GoalPeriod::Day,
            tokens: 100,
            runs: 3,
        },
        atoms: vec![],
        toolsets: vec![],
        approvers: vec![],
        wake: vec![],
    }
}

#[tokio::test]
async fn goal_is_pinned_children_are_ready_and_next_brief_sees_feedback() {
    let path = std::env::temp_dir().join(format!("atomic-goals-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let runtime = Durare::sqlite(&path).await.expect("database");
    let credential = HumanCredential::new(Principal::local("owner"), "secret");
    gate::enroll(&runtime, &credential).await.expect("enroll");
    let millis = Arc::new(AtomicU64::new(1_700_000_000_000));
    let owner_clock = {
        let millis = millis.clone();
        move || SystemTime::UNIX_EPOCH + Duration::from_millis(millis.load(Ordering::SeqCst))
    };
    let agent_clock = {
        let millis = millis.clone();
        move || SystemTime::UNIX_EPOCH + Duration::from_millis(millis.load(Ordering::SeqCst))
    };
    let owner = SqliteStore::open_authorized(&path, owner_clock, credential).expect("owner");
    let agent = SqliteStore::open(&path, agent_clock).expect("agent");
    assert!(matches!(
        agent.add_goal("goal-regression", definition()).await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    owner
        .add_goal("goal-regression", definition())
        .await
        .expect("add");
    owner
        .add_goal("goal-regression", definition())
        .await
        .expect("idempotent add");
    let mut changed = definition();
    changed.title = "Changed".into();
    assert!(owner.add_goal("goal-regression", changed).await.is_err());
    let mut child = Draft::new("task-regression", "Investigate");
    child.parent = Some("goal-regression".into());
    owner
        .create_item(Actor::human("owner"), "create-child".into(), child)
        .await
        .expect("child");
    let ready = agent
        .ready(Actor::agent("worker"), 100)
        .await
        .expect("ready");
    assert!(ready.iter().any(|item| item.id == "task-regression"));
    assert!(!ready.iter().any(|item| item.id == "goal-regression"));

    agent
        .record_goal_wake(
            "goal-regression",
            "wake-1",
            json!({"failures":2}),
            GoalOutcome::Act,
            "two failures",
            30,
        )
        .await
        .expect("first wake");
    agent
        .record_goal_wake(
            "goal-regression",
            "wake-1",
            json!({"failures":2}),
            GoalOutcome::Act,
            "two failures",
            30,
        )
        .await
        .expect("same wake replay");
    assert!(
        agent
            .record_goal_wake(
                "goal-regression",
                "wake-1",
                json!({"failures":3}),
                GoalOutcome::Act,
                "two failures",
                30
            )
            .await
            .is_err()
    );
    owner
        .add_goal_feedback(
            "goal-regression",
            "feedback-1",
            GoalFeedbackKind::GateRejection,
            "Include the failing case",
        )
        .await
        .expect("feedback");
    owner
        .add_goal_feedback(
            "goal-regression",
            "feedback-1",
            GoalFeedbackKind::GateRejection,
            "Include the failing case",
        )
        .await
        .expect("feedback replay");
    assert!(
        owner
            .add_goal_feedback(
                "goal-regression",
                "feedback-1",
                GoalFeedbackKind::Note,
                "Changed"
            )
            .await
            .is_err()
    );
    agent
        .ignore_goal_value(
            "goal-regression",
            GoalIgnoreKind::Fingerprint,
            "candidate-a",
            SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_001_000),
            "already tried",
        )
        .await
        .expect("ignore");
    let next = agent
        .goal_brief("goal-regression")
        .await
        .expect("next brief");
    assert_eq!(
        next.measure.as_ref().expect("sample").value,
        json!({"failures":2})
    );
    assert_eq!(next.feedback[0].text, "Include the failing case");
    assert_eq!(next.feedback[0].principal, "local:owner");
    assert_eq!(next.wakes.len(), 1);
    assert_eq!(next.tokens_remaining, 70);
    assert_eq!(next.runs_remaining, 2);
    assert_eq!(next.open_children[0].id, "task-regression");
    assert_eq!(next.ignores.len(), 1);
    millis.store(1_700_000_001_001, Ordering::SeqCst);
    assert!(
        agent
            .goal_brief("goal-regression")
            .await
            .expect("later brief")
            .ignores
            .is_empty()
    );
    owner
        .set_goal_active("goal-regression", false)
        .await
        .expect("pause");
    assert!(
        agent
            .record_goal_wake(
                "goal-regression",
                "wake-2",
                json!(0),
                GoalOutcome::NoAction,
                "paused",
                0
            )
            .await
            .is_err()
    );
    assert!(
        agent
            .ready(Actor::agent("worker"), 100)
            .await
            .expect("ready after pause")
            .iter()
            .any(|item| item.id == "task-regression")
    );
    owner
        .set_goal_active("goal-regression", true)
        .await
        .expect("resume");
    agent
        .record_goal_wake(
            "goal-regression",
            "wake-2",
            json!(0),
            GoalOutcome::NoAction,
            "no new failures",
            0,
        )
        .await
        .expect("resumed wake");
    let after_no_action = agent
        .goal_brief("goal-regression")
        .await
        .expect("next run budget");
    assert_eq!(
        after_no_action.tokens_remaining, 70,
        "zero-token no-action did not spend tokens"
    );
    assert_eq!(
        after_no_action.runs_remaining, 1,
        "no-action still consumes a run unit"
    );
    let replacement = HumanCredential::new(Principal::local("owner"), "new secret");
    gate::replace_enrollment(&runtime, &replacement)
        .await
        .expect("rotate credential");
    assert!(matches!(
        owner
            .add_goal_feedback(
                "goal-regression",
                "feedback-2",
                GoalFeedbackKind::Note,
                "should refuse"
            )
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    assert!(matches!(
        owner.set_goal_active("goal-regression", false).await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    assert!(
        agent
            .goal_brief("goal-regression")
            .await
            .expect("still active")
            .active
    );
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn goal_brief_bounds_children_and_reports_the_omitted_count() {
    let path = std::env::temp_dir().join(format!(
        "atomic-goal-children-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let runtime = Durare::sqlite(&path).await.expect("database");
    let credential = HumanCredential::new(Principal::local("owner"), "secret");
    gate::enroll(&runtime, &credential).await.expect("enroll");
    let owner = SqliteStore::open_authorized(&path, SystemTime::now, credential).expect("owner");
    owner
        .add_goal("goal-many", definition())
        .await
        .expect("goal");
    let mut conn = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("connect");
    for index in 0..201 {
        sqlx::query("INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at,claim_epoch) VALUES (?,?,'','task',2,'open','goal-many','human','owner',1,0)")
            .bind(format!("task-{index:03}"))
            .bind(format!("Child {index}"))
            .execute(&mut conn).await.expect("child");
    }
    sqlx::query("INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at,claim_epoch) VALUES ('task-closed','Closed','','task',2,'closed','goal-many','human','owner',1,0)")
        .execute(&mut conn).await.expect("closed child");
    conn.close().await.expect("close seed connection");
    let brief = owner.goal_brief("goal-many").await.expect("bounded brief");
    assert_eq!(brief.total_children, 202);
    assert!(brief.children_truncated);
    assert_eq!(brief.open_children.len(), 200);
    assert!(
        brief.closed_children.is_empty(),
        "open children have priority in a truncated brief"
    );
    assert_eq!(
        brief.open_children.first().expect("first child").id,
        "task-000"
    );
    assert_eq!(
        brief.open_children.last().expect("last child").id,
        "task-199"
    );
    let _ = std::fs::remove_file(path);
}
