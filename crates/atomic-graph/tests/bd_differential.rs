// SPDX-License-Identifier: Apache-2.0
//! M5's external oracle: the official SQLite-capable Beads v0.50.3 release.
//! Every bd command names a fresh temporary SQLite file, never this project's
//! Beads database. A missing or wrong oracle fails the suite; it never skips.

use atomic_durable::Durare;
use atomic_durable::gate::{self, HumanCredential, Principal};
use atomic_graph::{
    sqlite::SqliteStore,
    store::{Actor, GraphStore, Item},
};
use chrono::DateTime;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

const VERSION: &str = "bd version 0.50.3 (bd25acbc: HEAD@bd25acbcb1e7)";
const CREATED: &str = "2026-01-01T00:00:00Z";

fn validate_bd_version(bd: &Path) {
    assert!(bd.is_absolute(), "the pinned bd path must be absolute");
    let output = Command::new(bd)
        .arg("version")
        .output()
        .expect("run pinned bd");
    assert!(output.status.success(), "bd version failed: {output:?}");
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("bd version UTF-8")
            .trim(),
        VERSION,
        "the selected bd is not the pinned SQLite release"
    );
}

struct Fixture {
    root: PathBuf,
    bd: PathBuf,
    db: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let bd = PathBuf::from(
            std::env::var_os("ATOMIC_TEST_BD_SQLITE_BIN")
                .expect("ATOMIC_TEST_BD_SQLITE_BIN must name the verified v0.50.3 SQLite bd"),
        );
        validate_bd_version(&bd);
        let root = std::env::temp_dir().join(format!(
            "atomic-bd-sqlite-differential-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).expect("new isolated oracle directory");
        let db = root.join(".beads/beads.db");
        let this = Self { root, bd, db };
        this.run(&[
            "init",
            "--backend",
            "sqlite",
            "--prefix",
            "at",
            "--skip-hooks",
            "--skip-merge-driver",
            "--quiet",
        ]);
        let bytes = std::fs::read(&this.db).expect("bd created the SQLite file");
        assert!(
            bytes.starts_with(b"SQLite format 3\0"),
            "bd did not select its SQLite backend"
        );
        this
    }

    fn run(&self, args: &[&str]) -> String {
        let output = Command::new(&self.bd)
            .arg("--db")
            .arg(&self.db)
            .args(args)
            .current_dir(&self.root)
            .env("BD_ACTOR", "fixture")
            .output()
            .expect("run SQLite bd");
        assert!(
            output.status.success(),
            "bd {args:?} failed: status={}, stderr={}, stdout={}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8(output.stdout).expect("bd output UTF-8")
    }

    fn import(&self, rows: &[Value]) {
        let jsonl = rows
            .iter()
            .map(|row| serde_json::to_string(row).expect("fixture JSON"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let path = self.root.join("items.jsonl");
        std::fs::write(&path, jsonl).expect("write isolated fixture");
        self.run(&[
            "import",
            "--input",
            path.to_str().expect("fixture path UTF-8"),
        ]);
    }

    fn ready(&self) -> Vec<Value> {
        serde_json::from_str(&self.run(&["ready", "--json", "--limit", "100"]))
            .expect("bd ready JSON")
    }

    fn export(&self) -> String {
        self.run(&["export"])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn issue(id: &str, title: &str, status: &str, priority: i32, kind: &str) -> Value {
    let mut value = json!({
        "id": id,
        "title": title,
        "description": format!("{title} body"),
        "status": status,
        "priority": priority,
        "issue_type": kind,
        "owner": "fixture",
        "created_by": "fixture",
        "created_at": CREATED,
        "updated_at": CREATED,
        "labels": ["differential"]
    });
    if status == "closed" {
        value["closed_at"] = json!("2026-01-02T00:00:00Z");
    }
    value
}

fn dependency(row: &mut Value, blocker: &str, kind: &str) {
    let id = row["id"].as_str().expect("fixture id");
    row["dependencies"] = json!([{
        "issue_id": id,
        "depends_on_id": blocker,
        "type": kind
    }]);
}

async fn atomic_from_bd(
    fixture: &Fixture,
    now: impl Fn() -> SystemTime + Send + Sync + 'static,
) -> SqliteStore {
    let path = fixture.root.join("atomic.sqlite");
    let rt = Durare::sqlite(&path)
        .await
        .expect("open independent Atomic SQLite store");
    let credential = HumanCredential::new(
        Principal::local("bd differential operator"),
        "fixture secret",
    );
    gate::enroll(&rt, &credential)
        .await
        .expect("enroll differential operator");
    let store = SqliteStore::open_authorized(&path, now, credential)
        .expect("open authorized independent Atomic SQLite store");
    store
        .import(
            Actor::human("bd differential operator"),
            fixture.export(),
            false,
        )
        .await
        .expect("Atomic accepts pinned bd export");
    store
}

fn ready_ids(rows: &[Value]) -> BTreeSet<String> {
    rows.iter()
        .map(|row| row["id"].as_str().expect("ready id").to_owned())
        .collect()
}

fn atomic_ids(rows: &[Item]) -> BTreeSet<String> {
    rows.iter().map(|item| item.id.clone()).collect()
}

fn exported_by_id(jsonl: &str) -> BTreeMap<String, Value> {
    jsonl
        .lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line).expect("Atomic export JSONL");
            (value["id"].as_str().expect("export id").to_owned(), value)
        })
        .collect()
}

fn compare_ready_fields(bd: &[Value], atomic: &[Item], exported: &BTreeMap<String, Value>) {
    let item_by_id: BTreeMap<_, _> = atomic.iter().map(|item| (item.id.as_str(), item)).collect();
    // These are the Beads JSON fields Atomic promises to carry through import
    // and export. bd's dependency/dependent/comment counts are computed ready
    // projections, not fields in its portable issue JSONL.
    const FIELDS: &[&str] = &[
        "id",
        "title",
        "description",
        "status",
        "priority",
        "issue_type",
        "owner",
        "created_by",
        "created_at",
        "updated_at",
        "labels",
    ];
    for row in bd {
        let id = row["id"].as_str().expect("bd ready id");
        let graph = exported.get(id).expect("Atomic exports each ready item");
        for key in FIELDS {
            assert_eq!(
                graph.get(key),
                row.get(key),
                "{id}: portable JSON field {key}"
            );
        }
        let item = item_by_id.get(id).expect("Atomic ready item");
        assert_eq!(item.id, id);
        assert_eq!(item.title, row["title"].as_str().expect("bd title"));
        assert_eq!(item.body, row["description"].as_str().expect("bd body"));
        assert_eq!(
            item.status.as_str(),
            row["status"].as_str().expect("bd status")
        );
        assert_eq!(item.kind, row["issue_type"].as_str().expect("bd kind"));
        assert_eq!(
            i64::from(item.priority),
            row["priority"].as_i64().expect("bd priority")
        );
        if let Some(due_at) = row.get("due_at").and_then(Value::as_str) {
            let due_at = DateTime::parse_from_rfc3339(due_at)
                .expect("bd due date")
                .to_utc();
            assert_eq!(
                item.due_at,
                Some(SystemTime::from(due_at)),
                "{id}: typed due date"
            );
        }
    }
}

#[tokio::test]
async fn ready_sets_and_portable_json_fields_match_pinned_bd_sqlite() {
    let mut dependent = issue("at-dependent", "Dependent", "open", 2, "task");
    dependency(&mut dependent, "at-blocker", "blocks");
    let mut related = issue("at-related", "Related", "open", 2, "bug");
    dependency(&mut related, "at-blocker", "related");
    let mut after_closed = issue("at-after-closed", "After closed", "open", 3, "task");
    dependency(&mut after_closed, "at-closed-prereq", "blocks");
    let mut past_due = issue("at-past-due", "Past due", "open", 2, "task");
    past_due["due_at"] = json!("2025-01-01T00:00:00Z");

    let scenarios = [
        (
            "status-filter",
            vec![
                issue("at-open", "Open", "open", 1, "task"),
                issue("at-bug", "Bug", "open", 2, "bug"),
                past_due,
                issue("at-closed", "Closed", "closed", 2, "task"),
                issue("at-progress", "Progress", "in_progress", 2, "task"),
                issue("at-blocked", "Blocked", "blocked", 2, "task"),
                issue("at-deferred", "Deferred", "deferred", 2, "task"),
            ],
            vec!["at-open", "at-bug", "at-past-due"],
        ),
        (
            "dependency-filter",
            vec![
                issue("at-blocker", "Blocker", "open", 1, "task"),
                dependent,
                related,
                issue(
                    "at-closed-prereq",
                    "Closed prerequisite",
                    "closed",
                    1,
                    "task",
                ),
                after_closed,
            ],
            vec!["at-blocker", "at-related", "at-after-closed"],
        ),
    ];

    for (label, rows, expected) in scenarios {
        let fixture = Fixture::new(label);
        fixture.import(&rows);
        let bd = fixture.ready();
        let expected: BTreeSet<_> = expected.iter().map(|id| (*id).to_owned()).collect();
        assert_eq!(ready_ids(&bd), expected, "pinned bd baseline: {label}");
        let graph = atomic_from_bd(&fixture, SystemTime::now).await;
        let atomic = graph
            .ready(Actor::agent("comparison"), 100)
            .await
            .expect("Atomic ready");
        assert_eq!(atomic_ids(&atomic), ready_ids(&bd), "ready set: {label}");
        let exported = graph
            .export(Actor::agent("comparison"))
            .await
            .expect("Atomic export");
        compare_ready_fields(&bd, &atomic, &exported_by_id(&exported));
    }
}

#[tokio::test]
async fn future_due_is_a_documented_nonparity_with_pinned_bd_sqlite() {
    let fixture = Fixture::new("future-due");
    let mut future = issue("at-future", "Future due", "open", 1, "task");
    future["due_at"] = json!("2099-01-01T00:00:00Z");
    fixture.import(&[future]);
    assert!(
        ready_ids(&fixture.ready()).contains("at-future"),
        "v0.50.3 bd ready includes an open item despite future due_at"
    );
    let due = SystemTime::from(
        DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
            .expect("future due date")
            .to_utc(),
    );
    let clock = Arc::new(Mutex::new(due - Duration::from_secs(1)));
    let store_clock = Arc::clone(&clock);
    let graph = atomic_from_bd(&fixture, move || {
        *store_clock.lock().expect("test clock lock")
    })
    .await;
    assert!(
        !atomic_ids(
            &graph
                .ready(Actor::agent("comparison"), 100)
                .await
                .expect("Atomic ready")
        )
        .contains("at-future"),
        "Atomic follows its M5 contract: future due_at is not ready"
    );
    *clock.lock().expect("test clock lock") = due + Duration::from_secs(1);
    assert!(
        atomic_ids(
            &graph
                .ready(Actor::agent("comparison"), 100)
                .await
                .expect("Atomic ready after due_at")
        )
        .contains("at-future"),
        "Atomic item becomes ready when its injected clock passes due_at"
    );
}

#[cfg(unix)]
#[test]
fn wrong_version_and_missing_oracle_are_rejected() {
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join(format!("atomic-bd-wrong-version-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("test directory");
    let fake = root.join("bd");
    std::fs::write(
        &fake,
        b"#!/bin/sh\necho 'bd version 1.3.2 (wrong backend)'\n",
    )
    .expect("fake executable");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
        .expect("fake executable permission");
    assert!(
        std::panic::catch_unwind(|| validate_bd_version(&fake)).is_err(),
        "a wrong-version bd must be rejected"
    );
    assert!(
        std::panic::catch_unwind(|| validate_bd_version(&root.join("absent-bd"))).is_err(),
        "a missing bd must fail closed"
    );
    std::fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn oracle_path_is_never_the_project_beads_database() {
    let fixture = Fixture::new("separate-database");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(fixture.db.starts_with(&fixture.root));
    assert!(!fixture.db.starts_with(root));
}
