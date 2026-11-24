// SPDX-License-Identifier: Apache-2.0
//! Production-path authorization checks against the shared durable SQLite DB.

use atomic_core::git::{Git, Signature};
use atomic_core::workflow::Criterion;
use atomic_core::worktree::Worktrees;
use atomic_durable::gate::{self, HumanCredential, Principal};
use atomic_durable::runtime::Durare;
use atomic_graph::sqlite::SqliteStore;
use atomic_graph::store::{
    Actor, ClosePolicySource, Draft, Evidence, Finish, GraphStore, Status, StoreError, Trust,
};
use sqlx::{Connection, Row, SqliteConnection};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime};

fn path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "atomic-graph-admin-auth-{label}-{}.sqlite",
        std::process::id()
    ))
}

async fn enrolled(label: &str) -> (PathBuf, Durare, HumanCredential) {
    let path = path(label);
    let _ = std::fs::remove_file(&path);
    let runtime = Durare::sqlite(&path).await.expect("durable SQLite");
    let credential = HumanCredential::new(Principal::local("owner"), "first secret");
    gate::enroll(&runtime, &credential)
        .await
        .expect("enroll operator");
    (path, runtime, credential)
}

#[tokio::test]
async fn operator_close_then_reopen_advances_the_release_epoch() {
    let (path, _runtime, credential) = enrolled("status-revokes-review").await;
    let human = SqliteStore::open_authorized(&path, SystemTime::now, credential)
        .expect("authorized graph store");
    let agent = SqliteStore::open(&path, SystemTime::now).expect("agent graph store");
    human
        .create_item(
            Actor::human("spoofed"),
            "create-review-item".into(),
            Draft::new("at-review", "Review candidate"),
        )
        .await
        .expect("create item");
    let lease = agent
        .claim(
            Actor::agent("agent"),
            "run-review".into(),
            "at-review".into(),
            Duration::from_secs(60),
        )
        .await
        .expect("claim")
        .expect("lease");
    agent
        .finish(lease.clone(), Finish::Release)
        .await
        .expect("release for a pending decision");
    let mut conn = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("inspect graph");
    let released: i64 =
        sqlx::query_scalar("SELECT claim_epoch FROM atomic_graph_items WHERE id='at-review'")
            .fetch_one(&mut conn)
            .await
            .expect("release epoch");
    assert_eq!(released, lease.epoch);
    human
        .set_status(Actor::human("spoofed"), "at-review".into(), Status::Open)
        .await
        .expect("repeat open status");
    let replayed: i64 =
        sqlx::query_scalar("SELECT claim_epoch FROM atomic_graph_items WHERE id='at-review'")
            .fetch_one(&mut conn)
            .await
            .expect("epoch after no-op replay");
    assert_eq!(
        replayed, released,
        "no-op status replay revoked an open item"
    );
    human
        .set_status(Actor::human("spoofed"), "at-review".into(), Status::Closed)
        .await
        .expect("operator close");
    human
        .set_status(Actor::human("spoofed"), "at-review".into(), Status::Open)
        .await
        .expect("operator reopen");
    let reopened: i64 =
        sqlx::query_scalar("SELECT claim_epoch FROM atomic_graph_items WHERE id='at-review'")
            .fetch_one(&mut conn)
            .await
            .expect("reopened epoch");
    assert!(
        reopened > released,
        "a reopened item regained the release epoch of an older review gate"
    );
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn only_enrolled_operator_can_pin_an_open_epic() {
    let (path, _runtime, credential) = enrolled("pin-open-epic").await;
    let human = SqliteStore::open_authorized(&path, SystemTime::now, credential)
        .expect("authorized graph store");
    let agent = SqliteStore::open(&path, SystemTime::now).expect("agent graph store");
    let mut epic = Draft::new("at-epic", "Imported epic");
    epic.kind = "epic".into();
    human
        .create_item(Actor::human("owner"), "create-open-epic".into(), epic)
        .await
        .expect("create open epic");
    human
        .create_item(
            Actor::human("owner"),
            "create-task".into(),
            Draft::new("at-task", "Ordinary task"),
        )
        .await
        .expect("create task");
    assert!(matches!(
        agent
            .set_status(Actor::agent("agent"), "at-epic".into(), Status::Pinned)
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    let forged = SqliteStore::open_authorized(
        &path,
        SystemTime::now,
        HumanCredential::new(Principal::local("owner"), "wrong secret"),
    )
    .expect("forged operator store");
    assert!(
        forged
            .set_status(Actor::human("owner"), "at-epic".into(), Status::Pinned)
            .await
            .is_err()
    );
    assert!(matches!(
        human
            .set_status(Actor::human("owner"), "at-task".into(), Status::Pinned)
            .await,
        Err(StoreError::InvalidStatusTransition { .. })
    ));
    let mut conn = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("inspect graph");
    let before: i64 =
        sqlx::query_scalar("SELECT claim_epoch FROM atomic_graph_items WHERE id='at-epic'")
            .fetch_one(&mut conn)
            .await
            .expect("open epoch");
    human
        .set_status(Actor::human("owner"), "at-epic".into(), Status::Pinned)
        .await
        .expect("operator pins open epic");
    human
        .set_status(Actor::human("owner"), "at-epic".into(), Status::Pinned)
        .await
        .expect("repeat pin is idempotent");
    let (status, after): (String, i64) =
        sqlx::query_as("SELECT status,claim_epoch FROM atomic_graph_items WHERE id='at-epic'")
            .fetch_one(&mut conn)
            .await
            .expect("pinned epic");
    assert_eq!(status, "pinned");
    assert_eq!(after, before + 1, "repeat pin changed the epoch");
    human
        .set_status(Actor::human("owner"), "at-epic".into(), Status::Closed)
        .await
        .expect("close epic");
    assert!(matches!(
        human
            .set_status(Actor::human("owner"), "at-epic".into(), Status::Pinned)
            .await,
        Err(StoreError::InvalidStatusTransition { .. })
    ));
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn operator_unpin_then_repin_cannot_revive_an_active_epic_run() {
    let (path, _runtime, credential) = enrolled("epic-binding-revocation").await;
    let human = SqliteStore::open_authorized(&path, SystemTime::now, credential)
        .expect("authorized graph store");
    let mut epic = Draft::new("at-epic", "Epic");
    epic.kind = "epic".into();
    epic.status = Status::Pinned;
    human
        .create_item(Actor::human("owner"), "create-epic".into(), epic)
        .await
        .expect("create pinned epic");
    let mut child = Draft::new("at-child", "Child");
    child.parent = Some("at-epic".into());
    human
        .create_item(Actor::human("owner"), "create-child".into(), child)
        .await
        .expect("create child");
    let mut human_child = Draft::new("at-human-child", "Human child");
    human_child.parent = Some("at-epic".into());
    human
        .create_item(
            Actor::human("owner"),
            "create-human-child".into(),
            human_child,
        )
        .await
        .expect("create human child");
    let mut collision_child = Draft::new("at-collision-child", "Name collision child");
    collision_child.parent = Some("at-epic".into());
    human
        .create_item(
            Actor::human("owner"),
            "create-collision-child".into(),
            collision_child,
        )
        .await
        .expect("create name collision child");
    let agent = SqliteStore::open(&path, SystemTime::now).expect("agent graph store");
    let child_lease = agent
        .claim(
            Actor::agent("agent"),
            "run-epic".into(),
            "at-child".into(),
            Duration::from_secs(60),
        )
        .await
        .expect("claim child")
        .expect("child lease");
    let human_lease = human
        .claim(
            Actor::human("owner"),
            "ignored".into(),
            "at-human-child".into(),
            Duration::from_secs(60),
        )
        .await
        .expect("operator claims sibling")
        .expect("human sibling lease");
    let collision_lease = agent
        .claim(
            Actor::agent("other agent"),
            "run-epic".into(),
            "at-collision-child".into(),
            Duration::from_secs(60),
        )
        .await
        .expect("other agent claims with run name")
        .expect("name collision lease");
    human
        .authorize_graph_run("at-epic", "run-epic")
        .await
        .expect("authorize run");
    let mut conn = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("inspect graph");
    sqlx::query(
        "INSERT INTO atomic_graph_epic_runs (epic_id,run_id,state) VALUES ('at-epic','run-epic','active')",
    )
    .execute(&mut conn)
    .await
    .expect("bind active run");
    sqlx::query("INSERT INTO atomic_graph_acquisitions (acquisition,item_id,claimant,session,epoch) VALUES (?,?,?,?,?)")
        .bind(&child_lease.acquisition)
        .bind(&child_lease.item_id)
        .bind(&child_lease.claimant)
        .bind(i64::try_from(child_lease.session).expect("session fits"))
        .bind(child_lease.epoch)
        .execute(&mut conn)
        .await
        .expect("record graph-run acquisition");
    human
        .set_status(Actor::human("owner"), "at-epic".into(), Status::Open)
        .await
        .expect("unpin epic");
    let child_row =
        sqlx::query("SELECT status,claim_epoch FROM atomic_graph_items WHERE id='at-child'")
            .fetch_one(&mut conn)
            .await
            .expect("child after epic close");
    assert_eq!(child_row.get::<String, _>("status"), "open");
    assert!(child_row.get::<i64, _>("claim_epoch") > child_lease.epoch);
    let child_leases: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_leases WHERE item_id='at-child'")
            .fetch_one(&mut conn)
            .await
            .expect("child lease count");
    assert_eq!(child_leases, 0, "epic close retained a child claim");
    let human_row =
        sqlx::query("SELECT status,claim_epoch FROM atomic_graph_items WHERE id='at-human-child'")
            .fetch_one(&mut conn)
            .await
            .expect("human child after epic close");
    assert_eq!(human_row.get::<String, _>("status"), "in_progress");
    assert_eq!(human_row.get::<i64, _>("claim_epoch"), human_lease.epoch);
    let human_leases: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM atomic_graph_leases WHERE item_id='at-human-child'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("human sibling lease count");
    assert_eq!(
        human_leases, 1,
        "epic close revoked an independent human claim"
    );
    let collision_row = sqlx::query(
        "SELECT status,claim_epoch FROM atomic_graph_items WHERE id='at-collision-child'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("same-name child after epic close");
    assert_eq!(collision_row.get::<String, _>("status"), "in_progress");
    assert_eq!(
        collision_row.get::<i64, _>("claim_epoch"),
        collision_lease.epoch
    );
    let collision_leases: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM atomic_graph_leases WHERE item_id='at-collision-child'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("same-name child lease count");
    assert_eq!(
        collision_leases, 1,
        "claimant text alone revoked an unrelated lease"
    );
    human
        .set_status(Actor::human("owner"), "at-epic".into(), Status::Pinned)
        .await
        .expect("repin epic");
    let state: String =
        sqlx::query_scalar("SELECT state FROM atomic_graph_epic_runs WHERE epic_id='at-epic'")
            .fetch_one(&mut conn)
            .await
            .expect("binding state");
    assert_eq!(state, "invalidated", "repin revived the old run");
    assert!(matches!(
        human.authorize_graph_run("at-epic", "run-epic").await,
        Err(StoreError::EpicAdmission { .. })
    ));
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn item_criteria_require_current_operator_and_refuse_invalid_changes_atomically() {
    let (path, runtime, credential) = enrolled("item-criteria-auth").await;
    let human = SqliteStore::open_authorized(&path, SystemTime::now, credential)
        .expect("authorized graph store");
    human
        .create_item(
            Actor::human("spoofed"),
            "create-criteria-item".into(),
            Draft::new("at-criteria", "Criteria"),
        )
        .await
        .expect("create item");
    let plain = SqliteStore::open(&path, SystemTime::now).expect("plain graph store");
    let criteria = vec![
        Criterion::Test {
            argv: vec!["cargo".into(), "test".into()],
            timeout_ms: 30_000,
        },
        Criterion::Cel {
            expr: "output.ok == true".into(),
        },
    ];
    assert!(matches!(
        plain
            .set_item_criteria(
                Actor::human("owner"),
                "at-criteria".into(),
                criteria.clone(),
            )
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    assert!(matches!(
        human
            .set_item_criteria(
                Actor::agent("agent"),
                "at-criteria".into(),
                criteria.clone(),
            )
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    let fake = SqliteStore::open_authorized(
        &path,
        SystemTime::now,
        HumanCredential::new(Principal::local("owner"), "wrong secret"),
    )
    .expect("fake store opens");
    assert!(matches!(
        fake.set_item_criteria(
            Actor::human("owner"),
            "at-criteria".into(),
            criteria.clone(),
        )
        .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    human
        .set_item_criteria(
            Actor::human("spoofed"),
            "at-criteria".into(),
            criteria.clone(),
        )
        .await
        .expect("set criteria");
    let stored = human
        .item_criteria("at-criteria")
        .await
        .expect("read criteria")
        .expect("stored criteria");
    assert_eq!(stored.criteria, criteria);
    assert_eq!(stored.set_by, "local:owner");
    let invalid = vec![Criterion::Test {
        argv: vec!["sh".into(), "-c".into(), "true".into()],
        timeout_ms: 30_000,
    }];
    assert!(matches!(
        human
            .set_item_criteria(Actor::human("owner"), "at-criteria".into(), invalid)
            .await,
        Err(StoreError::InvalidCriteria { .. })
    ));
    assert_eq!(
        human
            .item_criteria("at-criteria")
            .await
            .expect("criteria after invalid set")
            .expect("original criteria")
            .criteria,
        criteria,
    );
    let rotated = HumanCredential::new(Principal::local("owner"), "second secret");
    gate::replace_enrollment(&runtime, &rotated)
        .await
        .expect("rotate operator credential");
    assert!(matches!(
        human
            .set_item_criteria(Actor::human("owner"), "at-criteria".into(), Vec::new())
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    let new_human = SqliteStore::open_authorized(&path, SystemTime::now, rotated)
        .expect("new authorized store");
    new_human
        .set_item_criteria(Actor::human("owner"), "at-criteria".into(), Vec::new())
        .await
        .expect("clear criteria");
    assert!(
        new_human
            .item_criteria("at-criteria")
            .await
            .expect("read cleared criteria")
            .is_none()
    );
    let _ = std::fs::remove_file(path);
}

fn fixture_git(at: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .arg("-c")
        .arg("user.name=atomic")
        .arg("-c")
        .arg("user.email=atomic@localhost")
        .args(args)
        .current_dir(at)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("fixture git starts");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("fixture git output")
}

#[tokio::test]
async fn only_host_git_proof_meets_verified_policy_and_branch_changes_invalidate_it() {
    let scratch =
        std::env::temp_dir().join(format!("atomic-graph-verified-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let project = scratch.join("project");
    std::fs::create_dir_all(&project).expect("project");
    fixture_git(&project, &["init", "-q", "-b", "main"]);
    std::fs::write(project.join("root.txt"), "root\n").expect("root file");
    fixture_git(&project, &["add", "--", "root.txt"]);
    fixture_git(&project, &["commit", "-q", "-m", "root"]);
    let repo = Git::find().expect("git").discover(&project).expect("repo");
    let worktrees = Worktrees::new(repo, scratch.join("worktrees"));
    let base = worktrees.pin_base("run-proof", "main").expect("pin base");
    let wt = worktrees
        .prepare("run-proof", "write", &base)
        .expect("prepare worktree");
    std::fs::write(wt.path.join("new.txt"), "change\n").expect("candidate file");
    let commit = worktrees
        .commit(&wt, "candidate", &Signature::default())
        .expect("host commit");

    let (path, runtime, credential) = enrolled("verified-host-proof").await;
    let human =
        SqliteStore::open_authorized(&path, SystemTime::now, credential).expect("human store");
    let agent = SqliteStore::open(&path, SystemTime::now).expect("agent store");
    for id in ["verified", "branch-changed"] {
        human
            .create_item(
                Actor::human("owner"),
                format!("create-{id}"),
                Draft::new(id, id),
            )
            .await
            .expect("create item");
        human
            .set_item_close_policy(
                Actor::human("owner"),
                id.into(),
                vec![Evidence::VerifiedCommit],
                Some("refs/heads/main".into()),
            )
            .await
            .expect("verified policy");
    }
    let lease = agent
        .claim(
            Actor::agent("agent"),
            "run-proof".into(),
            "verified".into(),
            Duration::from_secs(60),
        )
        .await
        .expect("claim")
        .expect("lease");
    assert!(
        matches!(
            agent
                .link_verified_commit(lease.clone(), &worktrees, "run-proof", &commit)
                .await,
            Err(StoreError::NeedsHumanCredential { .. })
        ),
        "a model-facing store cannot mint branch proof"
    );
    assert!(
        matches!(
            human
                .link_verified_commit(lease.clone(), &worktrees, "wrong-run", &commit)
                .await,
            Err(StoreError::Fenced { .. })
        ),
        "a different run cannot use the lease"
    );
    let mut forged = commit.clone();
    forged.reference = "refs/heads/main".into();
    assert!(
        human
            .link_verified_commit(lease.clone(), &worktrees, "run-proof", &forged)
            .await
            .is_err(),
        "an arbitrary ref cannot pose as the host attempt ref"
    );
    forged = commit.clone();
    forged.base = "f".repeat(commit.base.len());
    assert!(
        human
            .link_verified_commit(lease.clone(), &worktrees, "run-proof", &forged)
            .await
            .is_err(),
        "a forged base cannot pass the parent check"
    );
    assert!(
        human
            .link_verified_commit(lease.clone(), &worktrees, "run-proof", &commit)
            .await
            .is_err(),
        "the branch does not yet reach the attempt"
    );
    agent
        .link_commit(lease.clone(), commit.commit.clone())
        .await
        .expect("model link");
    let mut conn = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("inspect");
    sqlx::query(
        "UPDATE atomic_graph_commit_links SET indexed=1,on_branch=1 WHERE item_id='verified'",
    )
    .execute(&mut conn)
    .await
    .expect("simulate a legacy forged proof row");
    assert!(
        matches!(agent.finish(lease.clone(), Finish::Close).await,
        Err(StoreError::NeedsEvidence { required, .. }) if required == "verified_commit"),
        "legacy true/true flags with no branch proof must fail closed"
    );
    assert_eq!(
        agent
            .show(Actor::agent("agent"), "verified".into())
            .await
            .unwrap()
            .unwrap()
            .item
            .status,
        Status::InProgress
    );

    fixture_git(
        &project,
        &["update-ref", "refs/heads/main", &commit.commit, &base],
    );
    assert!(
        matches!(
            agent
                .link_verified_commit(lease.clone(), &worktrees, "run-proof", &commit)
                .await,
            Err(StoreError::NeedsHumanCredential { .. })
        ),
        "a model-facing store cannot mint proof even with reachable Git objects"
    );
    human
        .link_verified_commit(lease.clone(), &worktrees, "run-proof", &commit)
        .await
        .expect("host Git proof");
    agent
        .link_commit(lease.clone(), commit.commit.clone())
        .await
        .expect("model retry of an already linked SHA");
    let row = sqlx::query("SELECT sha,verified_branch,verified_head,pinned_base FROM atomic_graph_commit_links WHERE item_id='verified'")
        .fetch_one(&mut conn).await.expect("stored proof");
    assert_eq!(row.get::<String, _>("sha"), commit.commit);
    assert_eq!(row.get::<String, _>("verified_branch"), "refs/heads/main");
    assert_eq!(row.get::<String, _>("verified_head"), commit.commit);
    assert_eq!(row.get::<String, _>("pinned_base"), base);
    agent
        .finish(lease, Finish::Close)
        .await
        .expect("verified close");

    let changed = agent
        .claim(
            Actor::agent("agent"),
            "run-proof".into(),
            "branch-changed".into(),
            Duration::from_secs(60),
        )
        .await
        .expect("claim second")
        .expect("second lease");
    assert!(
        worktrees
            .recorded_step_commit("run-proof", "write", 2)
            .unwrap()
            .is_none(),
        "a nonexistent attempt is not evidence"
    );
    let recorded = worktrees
        .recorded_step_commit("run-proof", "write", 1)
        .expect("read Git attempt")
        .expect("recorded attempt");
    assert_eq!(recorded, commit);
    let replacement = HumanCredential::new(Principal::local("owner"), "rotated secret");
    gate::replace_enrollment(&runtime, &replacement)
        .await
        .expect("rotate operator credential");
    assert!(
        matches!(
            human
                .link_verified_commit(changed.clone(), &worktrees, "run-proof", &recorded)
                .await,
            Err(StoreError::NeedsHumanCredential { .. })
        ),
        "the old credential is checked inside the proof write transaction"
    );
    let rotated = SqliteStore::open_authorized(&path, SystemTime::now, replacement)
        .expect("rotated operator store");
    rotated
        .link_verified_commit(changed.clone(), &worktrees, "run-proof", &recorded)
        .await
        .expect("host-derived proof for second item");
    rotated
        .set_item_close_policy(
            Actor::human("owner"),
            "branch-changed".into(),
            vec![Evidence::VerifiedCommit],
            Some("refs/heads/other".into()),
        )
        .await
        .expect("change authorized branch");
    assert!(
        matches!(agent.finish(changed.clone(), Finish::Close).await,
        Err(StoreError::NeedsEvidence { required, .. }) if required == "verified_commit"),
        "the old branch attestation cannot satisfy a new branch policy"
    );
    assert_eq!(
        agent
            .show(Actor::agent("agent"), "branch-changed".into())
            .await
            .unwrap()
            .unwrap()
            .item
            .status,
        Status::InProgress
    );
    drop(conn);
    drop(rotated);
    drop(agent);
    drop(human);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(scratch);
}

#[tokio::test]
async fn item_policy_isolated_between_items_for_admin_and_fenced_close() {
    let (path, _runtime, credential) = enrolled("item-policy-isolation").await;
    let human =
        SqliteStore::open_authorized(&path, SystemTime::now, credential).expect("authorized store");
    let agent = SqliteStore::open(&path, SystemTime::now).expect("agent store");
    for id in ["at-policy-a", "at-policy-b"] {
        human
            .create_item(
                Actor::human("spoofed"),
                format!("create-{id}"),
                Draft::new(id, id),
            )
            .await
            .expect("create item");
    }
    human
        .set_item_close_policy(
            Actor::human("spoofed"),
            "at-policy-a".into(),
            vec![Evidence::Commit],
            None,
        )
        .await
        .expect("set item A policy");
    let a_policy = human
        .item_close_policy("at-policy-a")
        .await
        .expect("read item A policy")
        .expect("item A policy exists");
    assert_eq!(a_policy.source, ClosePolicySource::Item);
    assert_eq!(a_policy.requires, vec![Evidence::Commit]);
    assert_eq!(a_policy.set_by.as_deref(), Some("local:owner"));
    assert_eq!(human.item_close_policy("at-policy-b").await.unwrap(), None);
    assert!(matches!(
        human.item_close_policy("at-missing").await,
        Err(StoreError::MissingItem { .. })
    ));
    assert!(matches!(
        human
            .set_status(Actor::human("owner"), "at-policy-a".into(), Status::Closed)
            .await,
        Err(StoreError::NeedsEvidence { item_id, required })
            if item_id == "at-policy-a" && required == "commit"
    ));
    human
        .set_status(Actor::human("owner"), "at-policy-b".into(), Status::Closed)
        .await
        .expect("unrestricted B admin close");
    let lease = agent
        .claim(
            Actor::agent("agent"),
            "agent".into(),
            "at-policy-a".into(),
            Duration::from_secs(60),
        )
        .await
        .expect("claim A")
        .expect("available A");
    assert!(matches!(
        agent.finish(lease.clone(), Finish::Close).await,
        Err(StoreError::NeedsEvidence { item_id, required })
            if item_id == "at-policy-a" && required == "commit"
    ));
    agent
        .link_commit(lease.clone(), "a".repeat(40))
        .await
        .expect("link unverified commit");
    agent
        .finish(lease, Finish::Close)
        .await
        .expect("commit meets A policy");
    let mut conn = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("inspect SQLite");
    let policies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_item_policy")
        .fetch_one(&mut conn)
        .await
        .expect("count item policies");
    assert_eq!(policies, 1, "only item A received a policy");
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn item_policy_requires_current_human_credential_and_valid_branch() {
    let (path, runtime, first) = enrolled("item-policy-auth").await;
    let owner = SqliteStore::open_authorized(&path, SystemTime::now, first)
        .expect("first authorized store");
    owner
        .create_item(
            Actor::human("spoofed"),
            "create-policy-item".into(),
            Draft::new("at-policy", "Policy"),
        )
        .await
        .expect("create item");
    let plain = SqliteStore::open(&path, SystemTime::now).expect("plain store");
    assert!(matches!(
        plain
            .set_item_close_policy(
                Actor::human("owner"),
                "at-policy".into(),
                vec![Evidence::Commit],
                None,
            )
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    assert!(matches!(
        plain
            .set_item_close_policy(
                Actor::agent("agent"),
                "at-policy".into(),
                vec![Evidence::Commit],
                None,
            )
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    let fake = SqliteStore::open_authorized(
        &path,
        SystemTime::now,
        HumanCredential::new(Principal::local("owner"), "fake secret"),
    )
    .expect("fake store opens without authority");
    assert!(matches!(
        fake.set_item_close_policy(
            Actor::human("owner"),
            "at-policy".into(),
            vec![Evidence::Commit],
            None,
        )
        .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    for branch in [
        None,
        Some("refs/tags/main"),
        Some("refs/heads/a..b"),
        Some("refs/heads/a.lock"),
    ] {
        assert!(matches!(
            owner
                .set_item_close_policy(
                    Actor::human("spoofed"),
                    "at-policy".into(),
                    vec![Evidence::VerifiedCommit],
                    branch.map(str::to_owned),
                )
                .await,
            Err(StoreError::InvalidClosePolicy { .. })
        ));
    }
    let second = HumanCredential::new(Principal::local("owner"), "second secret");
    gate::replace_enrollment(&runtime, &second)
        .await
        .expect("rotate credential");
    assert!(matches!(
        owner
            .set_item_close_policy(
                Actor::human("owner"),
                "at-policy".into(),
                vec![Evidence::VerifiedCommit],
                Some("refs/heads/main".into()),
            )
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    let mut conn = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("inspect SQLite");
    let policies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_item_policy")
        .fetch_one(&mut conn)
        .await
        .expect("count refused policies");
    assert_eq!(policies, 0, "refusals wrote no item policy");
    let rotated =
        SqliteStore::open_authorized(&path, SystemTime::now, second).expect("new authorized store");
    rotated
        .set_item_close_policy(
            Actor::human("spoofed"),
            "at-policy".into(),
            vec![Evidence::VerifiedCommit],
            Some("refs/heads/release/v0.2".into()),
        )
        .await
        .expect("new credential sets branch policy");
    let row = sqlx::query(
        "SELECT authorized_branch,set_by FROM atomic_graph_item_policy WHERE item_id='at-policy'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("read policy row");
    assert_eq!(
        row.get::<String, _>("authorized_branch"),
        "refs/heads/release/v0.2"
    );
    assert_eq!(row.get::<String, _>("set_by"), "local:owner");
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn per_item_override_and_clear_preserve_legacy_global_policy() {
    let (path, _runtime, credential) = enrolled("item-policy-legacy").await;
    let owner =
        SqliteStore::open_authorized(&path, SystemTime::now, credential).expect("authorized store");
    owner
        .set_close_policy(Actor::human("owner"), vec![Evidence::Commit])
        .await
        .expect("legacy default policy");
    for id in ["at-legacy-a", "at-legacy-b"] {
        owner
            .create_item(
                Actor::human("owner"),
                format!("create-{id}"),
                Draft::new(id, id),
            )
            .await
            .expect("create item");
    }
    owner
        .set_item_close_policy(
            Actor::human("owner"),
            "at-legacy-a".into(),
            vec![Evidence::VerifiedCommit],
            Some("refs/heads/main".into()),
        )
        .await
        .expect("explicit item override");
    let override_policy = owner
        .item_close_policy("at-legacy-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(override_policy.source, ClosePolicySource::Item);
    assert_eq!(override_policy.requires, vec![Evidence::VerifiedCommit]);
    assert_eq!(
        override_policy.authorized_branch.as_deref(),
        Some("refs/heads/main")
    );
    let fallback = owner
        .item_close_policy("at-legacy-b")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fallback.source, ClosePolicySource::LegacyGlobal);
    assert_eq!(fallback.requires, vec![Evidence::Commit]);
    assert!(matches!(
        owner
            .set_status(Actor::human("owner"), "at-legacy-a".into(), Status::Closed)
            .await,
        Err(StoreError::NeedsEvidence { item_id, required })
            if item_id == "at-legacy-a" && required == "verified_commit"
    ));
    assert!(matches!(
        owner
            .set_status(Actor::human("owner"), "at-legacy-b".into(), Status::Closed)
            .await,
        Err(StoreError::NeedsEvidence { item_id, required })
            if item_id == "at-legacy-b" && required == "commit"
    ));
    owner
        .set_item_close_policy(Actor::human("owner"), "at-legacy-a".into(), vec![], None)
        .await
        .expect("clear override");
    let restored = owner
        .item_close_policy("at-legacy-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored.source, ClosePolicySource::LegacyGlobal);
    assert_eq!(restored.requires, vec![Evidence::Commit]);
    assert!(matches!(
        owner
            .set_status(Actor::human("owner"), "at-legacy-a".into(), Status::Closed)
            .await,
        Err(StoreError::NeedsEvidence { item_id, required })
            if item_id == "at-legacy-a" && required == "commit"
    ));
    let mut conn = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("inspect SQLite");
    let item_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_item_policy")
        .fetch_one(&mut conn)
        .await
        .expect("count item rows");
    assert_eq!(item_rows, 0, "clearing item row restores global fallback");
    let global_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_policy WHERE id=1")
            .fetch_one(&mut conn)
            .await
            .expect("count global policy");
    assert_eq!(global_rows, 1, "legacy policy was retained");
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn branchless_verified_legacy_write_refuses_and_migrated_row_fails_closed() {
    let (path, _runtime, credential) = enrolled("legacy-verified-branch").await;
    let owner =
        SqliteStore::open_authorized(&path, SystemTime::now, credential).expect("authorized store");
    owner
        .create_item(
            Actor::human("owner"),
            "legacy-verified-create".into(),
            Draft::new("at-legacy-verified", "Legacy verified"),
        )
        .await
        .expect("create item");
    owner
        .set_close_policy(Actor::human("owner"), vec![Evidence::Commit])
        .await
        .expect("set compatible tenant default");
    assert!(matches!(
        owner
            .set_close_policy(Actor::human("owner"), vec![Evidence::VerifiedCommit])
            .await,
        Err(StoreError::InvalidClosePolicy { .. })
    ));
    let mut conn = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("inspect SQLite");
    let row = sqlx::query(
        "SELECT requires_commit,requires_verified_commit FROM atomic_graph_policy WHERE id=1",
    )
    .fetch_one(&mut conn)
    .await
    .expect("tenant default unchanged");
    assert_eq!(row.get::<i64, _>("requires_commit"), 1);
    assert_eq!(row.get::<i64, _>("requires_verified_commit"), 0);
    sqlx::query("UPDATE atomic_graph_policy SET requires_verified_commit=1 WHERE id=1")
        .execute(&mut conn)
        .await
        .expect("simulate a preserved pre-v14 legacy row");
    let preserved = owner
        .item_close_policy("at-legacy-verified")
        .await
        .expect("read preserved legacy policy")
        .expect("legacy policy remains present");
    assert_eq!(preserved.source, ClosePolicySource::LegacyGlobal);
    assert_eq!(
        preserved.requires,
        vec![Evidence::Commit, Evidence::VerifiedCommit]
    );
    assert!(matches!(
        owner
            .set_status(
                Actor::human("owner"),
                "at-legacy-verified".into(),
                Status::Closed,
            )
            .await,
        Err(StoreError::InvalidClosePolicy { .. })
    ));
    let status = owner
        .show(Actor::agent("reader"), "at-legacy-verified".into())
        .await
        .expect("show")
        .expect("item")
        .item
        .status;
    assert_eq!(status, Status::Open);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn imported_policy_text_cannot_create_a_human_item_policy() {
    let (path, _runtime, credential) = enrolled("item-policy-import").await;
    let owner = SqliteStore::open_authorized(&path, SystemTime::now, credential)
        .expect("authorized importer");
    let jsonl = r#"{"id":"bd-external","title":"External","status":"open","issue_type":"task","close_policy":{"requires":["verified_commit"],"authorized_branch":"refs/heads/main"}}"#;
    owner
        .import(Actor::human("owner"), jsonl.into(), false)
        .await
        .expect("import external record");
    let imported = owner
        .show(Actor::agent("reader"), "bd-external".into())
        .await
        .expect("show imported")
        .expect("imported item");
    assert_eq!(imported.item.trust, Trust::External);
    let mut conn = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("inspect SQLite");
    let policies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_item_policy")
        .fetch_one(&mut conn)
        .await
        .expect("count policies");
    assert_eq!(policies, 0, "import did not promote policy text");
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn forged_human_actor_and_fake_credential_write_nothing() {
    let (path, _runtime, credential) = enrolled("forged").await;
    let plain = SqliteStore::open(&path, SystemTime::now).expect("plain store");
    assert!(matches!(
        plain
            .create_item(
                Actor::human("owner"),
                "same-request".into(),
                Draft::new("at-x", "X")
            )
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    assert!(matches!(
        plain.set_close_policy(Actor::human("owner"), vec![]).await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    let fake = SqliteStore::open_authorized(
        &path,
        SystemTime::now,
        HumanCredential::new(Principal::local("owner"), "fake secret"),
    )
    .expect("fake handle opens without gaining authority");
    assert!(matches!(
        fake.create_item(
            Actor::human("owner"),
            "same-request".into(),
            Draft::new("at-x", "X")
        )
        .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    let authorized =
        SqliteStore::open_authorized(&path, SystemTime::now, credential).expect("authorized store");
    let item = authorized
        .create_item(
            Actor::human("spoofed"),
            "same-request".into(),
            Draft::new("at-x", "X"),
        )
        .await
        .expect("real credential");
    assert_eq!(item.created_by, "local:owner");
    assert_eq!(item.trust, Trust::Human);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn rotation_fences_old_human_lease_and_plain_token_use() {
    let (path, runtime, first) = enrolled("rotation").await;
    let old = SqliteStore::open_authorized(&path, SystemTime::now, first).expect("old handle");
    old.create_item(
        Actor::human("ignored"),
        "create".into(),
        Draft::new("at-work", "Work"),
    )
    .await
    .expect("create before rotation");
    let lease = old
        .claim(
            Actor::human("ignored"),
            "spoofed claimant".into(),
            "at-work".into(),
            Duration::from_secs(90),
        )
        .await
        .expect("claim")
        .expect("available");
    assert_eq!(lease.claimant, "local:owner");
    let plain = SqliteStore::open(&path, SystemTime::now).expect("plain store");
    assert!(matches!(
        plain
            .claim(
                Actor::agent("owner"),
                "local:owner".into(),
                "at-work".into(),
                Duration::from_secs(90)
            )
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    assert!(matches!(
        plain
            .heartbeat(lease.clone(), Duration::from_secs(90))
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    assert!(matches!(
        plain
            .note(
                Actor::agent("owner"),
                lease.clone(),
                "note".into(),
                "x".into()
            )
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    assert!(matches!(
        plain.link_commit(lease.clone(), "a".repeat(40)).await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    assert!(matches!(
        plain.finish(lease.clone(), Finish::Close).await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    let replacement = HumanCredential::new(Principal::local("owner"), "second secret");
    gate::replace_enrollment(&runtime, &replacement)
        .await
        .expect("rotate");
    assert!(matches!(
        old.finish(lease.clone(), Finish::Close).await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    assert!(
        matches!(
            old.create_item(
                Actor::human("owner"),
                "create".into(),
                Draft::new("at-work", "Work")
            )
            .await,
            Err(StoreError::NeedsHumanCredential { .. })
        ),
        "rotation must be checked before a replay key"
    );
    assert!(matches!(
        old.create_item(
            Actor::human("owner"),
            "after".into(),
            Draft::new("at-after", "After")
        )
        .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    let new =
        SqliteStore::open_authorized(&path, SystemTime::now, replacement).expect("new handle");
    new.finish(lease, Finish::Close)
        .await
        .expect("new secret may recover same principal's lease");
    assert_eq!(
        new.show(Actor::agent("reader"), "at-work".into())
            .await
            .expect("show")
            .expect("item")
            .item
            .status,
        Status::Closed
    );
    new.create_item(
        Actor::human("spoofed"),
        "after".into(),
        Draft::new("at-after", "After"),
    )
    .await
    .expect("failed old write left no request id");
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn simultaneous_rotation_and_write_serialize_at_the_write_transaction() {
    let (path, runtime, first) = enrolled("concurrent-rotation").await;
    let old = SqliteStore::open_authorized(&path, SystemTime::now, first).expect("old handle");
    let replacement = HumanCredential::new(Principal::local("owner"), "second secret");
    let (write, rotation) = tokio::join!(
        old.create_item(
            Actor::human("spoof"),
            "racing".into(),
            Draft::new("at-race", "Race")
        ),
        gate::replace_enrollment(&runtime, &replacement),
    );
    rotation.expect("rotation committed");
    assert!(
        write.is_ok() || matches!(&write, Err(StoreError::NeedsHumanCredential { .. })),
        "write must either commit before rotation or refuse after it: {write:?}"
    );
    assert!(matches!(
        old.create_item(
            Actor::human("owner"),
            "after-race".into(),
            Draft::new("at-after", "After")
        )
        .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    let current =
        SqliteStore::open_authorized(&path, SystemTime::now, replacement).expect("current handle");
    current
        .create_item(
            Actor::human("wrong"),
            "after-race".into(),
            Draft::new("at-after", "After"),
        )
        .await
        .expect("post-rotation request was not consumed");
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn rotation_commits_first_and_old_write_refuses_without_rows() {
    let (path, runtime, first) = enrolled("rotation-first").await;
    let replacement = HumanCredential::new(Principal::local("owner"), "second secret");
    // Get the verifier through the production enrollment path, then commit it
    // under a held SQLite writer before the old credential may write.
    let verifier_path = self::path("rotation-verifier");
    let _ = std::fs::remove_file(&verifier_path);
    let verifier_runtime = Durare::sqlite(&verifier_path)
        .await
        .expect("verifier store");
    gate::enroll(&verifier_runtime, &replacement)
        .await
        .expect("enroll replacement");
    let mut verifier_conn =
        SqliteConnection::connect(&format!("sqlite://{}", verifier_path.display()))
            .await
            .expect("verifier connection");
    let verifier: String =
        sqlx::query("SELECT verifier FROM atomic_operators WHERE principal='local:owner'")
            .fetch_one(&mut verifier_conn)
            .await
            .expect("replacement verifier")
            .try_get("verifier")
            .expect("verifier text");
    let old = SqliteStore::open_authorized(&path, SystemTime::now, first).expect("old handle");
    let mut held = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("held connection");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut held)
        .await
        .expect("take write lock");
    sqlx::query("UPDATE atomic_operators SET verifier=? WHERE principal='local:owner'")
        .bind(verifier)
        .execute(&mut held)
        .await
        .expect("stage rotation");
    let (started, receiving) = tokio::sync::oneshot::channel();
    let waiting = tokio::spawn(async move {
        let _ = started.send(());
        old.create_item(
            Actor::human("spoofed"),
            "ordered-race".into(),
            Draft::new("at-race", "Race"),
        )
        .await
    });
    receiving
        .await
        .expect("old writer task launched while rotation holds the lock");
    tokio::time::sleep(Duration::from_millis(20)).await;
    sqlx::query("COMMIT")
        .execute(&mut held)
        .await
        .expect("rotation commits first");
    assert!(matches!(
        waiting.await.expect("writer task"),
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    let counts: (i64, i64) = (
        sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_items WHERE id='at-race'")
            .fetch_one(&mut held)
            .await
            .expect("item count"),
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM atomic_graph_requests WHERE request_id='ordered-race'",
        )
        .fetch_one(&mut held)
        .await
        .expect("request count"),
    );
    assert_eq!(
        counts,
        (0, 0),
        "old secret left graph or replay rows after rotation"
    );
    assert_eq!(
        gate::replace_enrollment(&runtime, &replacement)
            .await
            .expect("production verifier agrees"),
        gate::Enrollment::AlreadyEnrolled
    );
    let current =
        SqliteStore::open_authorized(&path, SystemTime::now, replacement).expect("new handle");
    current
        .create_item(
            Actor::human("spoofed"),
            "ordered-race".into(),
            Draft::new("at-race", "Race"),
        )
        .await
        .expect("new secret may use refused request");
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(verifier_path);
}

#[tokio::test]
async fn authenticated_write_commits_before_rotation_and_old_secret_then_refuses() {
    let (path, runtime, first) = enrolled("write-first").await;
    let old = SqliteStore::open_authorized(&path, SystemTime::now, first).expect("old handle");
    old.create_item(
        Actor::human("spoofed"),
        "before".into(),
        Draft::new("at-before", "Before"),
    )
    .await
    .expect("old credential commits before rotation");
    let replacement = HumanCredential::new(Principal::local("owner"), "second secret");
    gate::replace_enrollment(&runtime, &replacement)
        .await
        .expect("rotate after write");
    let current =
        SqliteStore::open_authorized(&path, SystemTime::now, replacement).expect("new handle");
    let before = current
        .show(Actor::agent("reader"), "at-before".into())
        .await
        .expect("show")
        .expect("committed item remains")
        .item;
    assert_eq!(
        (before.created_by.as_str(), before.trust),
        ("local:owner", Trust::Human)
    );
    assert!(matches!(
        old.create_item(
            Actor::human("owner"),
            "after".into(),
            Draft::new("at-after", "After")
        )
        .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    current
        .create_item(
            Actor::human("wrong"),
            "after".into(),
            Draft::new("at-after", "After"),
        )
        .await
        .expect("rotated secret uses refused request");
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn committed_import_requires_current_credential_but_dry_run_is_read_only() {
    let (path, runtime, first) = enrolled("import").await;
    let jsonl = r#"{"id":"bd-one","title":"One","status":"open","issue_type":"task"}
"#;
    let plain = SqliteStore::open(&path, SystemTime::now).expect("plain store");
    assert!(matches!(
        plain
            .import(Actor::agent("importer"), jsonl.into(), false)
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    assert!(matches!(
        plain
            .import(Actor::agent("importer"), jsonl.into(), true)
            .await,
        Err(StoreError::ImportDryRun(_))
    ));
    let fake = SqliteStore::open_authorized(
        &path,
        SystemTime::now,
        HumanCredential::new(Principal::local("owner"), "fake secret"),
    )
    .expect("fake handle opens without authority");
    assert!(matches!(
        fake.import(Actor::human("owner"), jsonl.into(), false)
            .await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    let old = SqliteStore::open_authorized(&path, SystemTime::now, first).expect("old handle");
    let replacement = HumanCredential::new(Principal::local("owner"), "second secret");
    gate::replace_enrollment(&runtime, &replacement)
        .await
        .expect("rotate");
    assert!(matches!(
        old.import(Actor::human("owner"), jsonl.into(), false).await,
        Err(StoreError::NeedsHumanCredential { .. })
    ));
    assert!(
        plain
            .all_items(Actor::agent("reader"))
            .await
            .expect("items")
            .is_empty()
    );
    let mut sql = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("inspect requests");
    let requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_requests")
        .fetch_one(&mut sql)
        .await
        .expect("request count");
    assert_eq!(requests, 0, "refused imports left no request row");
    let new =
        SqliteStore::open_authorized(&path, SystemTime::now, replacement).expect("new handle");
    let report = new
        .import(Actor::human("owner"), jsonl.into(), false)
        .await
        .expect("current credential imports");
    assert_eq!(report.added, ["bd-one"]);
    assert_eq!(
        new.show(Actor::agent("reader"), "bd-one".into())
            .await
            .expect("show")
            .expect("item")
            .item
            .trust,
        Trust::External
    );
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn opening_and_exporting_do_not_rebuild_the_graph_projection() {
    let path = path("read-only-export");
    let _ = std::fs::remove_file(&path);
    let store = SqliteStore::open(&path, SystemTime::now).expect("graph store");
    store
        .create_item(Actor::agent("author"), "a".into(), Draft::new("at-a", "A"))
        .await
        .expect("item a");
    store
        .create_item(Actor::agent("author"), "b".into(), Draft::new("at-b", "B"))
        .await
        .expect("item b");
    store
        .add_edge(
            Actor::agent("author"),
            "at-a".into(),
            atomic_graph::store::EdgeKind::Blocks,
            "at-b".into(),
        )
        .await
        .expect("blocked projection");
    let mut sql = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("second connection");
    sqlx::query("CREATE TRIGGER refuse_projection_delete BEFORE DELETE ON atomic_graph_blocked BEGIN SELECT RAISE(ABORT, 'read path wrote projection'); END")
        .execute(&mut sql).await.expect("install write detector");
    let reopened =
        SqliteStore::open(&path, SystemTime::now).expect("open does not write projection");
    let exported = reopened
        .export(Actor::agent("reader"))
        .await
        .expect("export is read-only");
    assert!(exported.contains("at-a"));
    assert_eq!(
        reopened
            .ready(Actor::agent("reader"), 10)
            .await
            .expect("ready is read-only")
            .len(),
        1
    );
    let _ = std::fs::remove_file(path);
}
