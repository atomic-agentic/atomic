// SPDX-License-Identifier: Apache-2.0

//! Beads JSONL import and export through the installed `atomic` executable.

use atomic_app::pipeline::GRAPH_PIPELINE;
use atomic_graph::sqlite::SqliteStore;
use atomic_graph::store::{Actor, Evidence, Finish, GraphStore, Status};
use serde_json::{Value, json};
use sqlx::{Connection, Row};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");
const EXPORT: &str = concat!(
    r#"{"_type":"issue","id":"bd-epic","title":"One epic","description":"Parent","status":"open","priority":1,"issue_type":"epic","created_at":"2026-01-01T00:00:00Z","labels":["v0.2"]}"#,
    "\n",
    r#"{"id":"bd-child","title":"One child","description":"Work","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-02T00:00:00Z","parent_issue":"bd-epic","dependencies":[{"issue_id":"bd-child","depends_on_id":"bd-epic","type":"related"}]}"#,
    "\n"
);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "atomic-graph-cli-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("home")).expect("private home");
        Self { root }
    }

    fn project(&self, label: &str) -> PathBuf {
        let path = self.root.join(label);
        std::fs::create_dir_all(&path).expect("project");
        std::fs::write(
            path.join("atomic.yaml"),
            format!("atomic: 1\nname: {label}\n"),
        )
        .expect("manifest");
        path
    }

    fn command(&self, project: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(ATOMIC);
        command
            .args(args)
            .current_dir(project)
            .env("HOME", self.root.join("home"))
            .env("ATOMIC_HOME", self.root.join("state"))
            .env("ATOMIC_CREDENTIAL_STORE", "file")
            .env("USER", "graph-cli-test")
            .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9");
        for name in [
            "CODEX_HOME",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "ATOMIC_DATABASE_URL",
            "ATOMIC_DATABASE_SCHEMA",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "ATOMIC_MODEL_BASE_URL",
            "ATOMIC_MODEL_API_KEY_ENV",
            "ATOMIC_PROFILE",
        ] {
            command.env_remove(name);
        }
        command
    }

    fn run(&self, project: &Path, args: &[&str]) -> Output {
        self.command(project, args)
            .output()
            .expect("atomic graph command ran")
    }

    fn enroll(&self, project: &Path) {
        success(&self.run(project, &["graph", "export"]));
        let id = self.state_id(project);
        let secret = "graph CLI enrolled operator secret";
        let store = atomic_core::credential::FileSecrets::open(
            &self.root.join("state/credentials"),
            project,
        )
        .expect("file credential store");
        atomic_core::credential::SecretStore::save(
            &store,
            &format!("operator:graph-cli-test@{id}"),
            secret,
        )
        .expect("save operator secret");
        let database = self.database(project);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let durable = atomic_durable::runtime::Durare::sqlite(&database)
                .await
                .expect("durable database");
            atomic_durable::gate::enroll(
                &durable,
                &atomic_durable::gate::HumanCredential::new(
                    atomic_durable::gate::Principal::local("graph-cli-test"),
                    secret,
                ),
            )
            .await
            .expect("enroll CLI operator");
        });
    }

    fn state_id(&self, project: &Path) -> String {
        let label = project
            .file_name()
            .expect("project label")
            .to_string_lossy();
        std::fs::read_dir(self.root.join("state/projects"))
            .expect("project state")
            .map(|entry| {
                entry
                    .expect("state entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .find(|name| name.starts_with(&format!("{label}-")))
            .expect("project state id")
    }

    fn database(&self, project: &Path) -> PathBuf {
        self.root
            .join("state/private")
            .join(self.state_id(project))
            .join("atomic.db")
    }

    fn run_stdin(&self, project: &Path, args: &[&str], input: &str) -> Output {
        use std::io::Write;
        let mut child = self
            .command(project, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start atomic graph command");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write JSONL input");
        child.wait_with_output().expect("atomic graph command ran")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn success(output: &Output) -> String {
    assert!(
        output.status.success(),
        "atomic failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).expect("UTF-8 output")
}

#[test]
fn enrolled_person_creates_links_claims_and_closes_with_json() {
    let fixture = Fixture::new("human-commands");
    let project = fixture.project("project");
    fixture.enroll(&project);
    let first: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "create", "First", "--id", "at-first"],
    )))
    .expect("created JSON");
    assert_eq!(first["id"], "at-first");
    assert_eq!(first["trust"], "human");
    assert_eq!(first["created_by"], "local:graph-cli-test");
    success(&fixture.run(
        &project,
        &["graph", "create", "Second", "--id", "at-second"],
    ));
    let linked: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &[
            "--json",
            "graph",
            "link",
            "at-first",
            "related",
            "at-second",
        ],
    )))
    .expect("linked JSON");
    assert_eq!(linked["kind"], "related");
    let lease: Value = serde_json::from_str(&success(
        &fixture.run(&project, &["--json", "graph", "claim", "at-first"]),
    ))
    .expect("claim JSON");
    assert_eq!(lease["item_id"], "at-first");
    assert_eq!(lease["claimant"], "local:graph-cli-test");
    let closed: Value = serde_json::from_str(&success(
        &fixture.run(&project, &["--json", "graph", "close", "at-first"]),
    ))
    .expect("closed JSON");
    assert_eq!(closed["status"], "closed");
    let exported = success(&fixture.run(&project, &["graph", "export"]));
    assert!(exported.contains(r#""id":"at-first""#));
    assert!(exported.contains(r#""status":"closed""#));
}

#[test]
fn graph_search_cli_resolves_punctuated_ids_filters_status_and_rebuilds() {
    let fixture = Fixture::new("search");
    let project = fixture.project("project");
    fixture.enroll(&project);
    success(&fixture.run(
        &project,
        &[
            "graph",
            "create",
            "Violet sample",
            "--id",
            "at-search.1",
            "--body",
            "Plain excerpt",
        ],
    ));
    let exact: Value = serde_json::from_str(&success(
        &fixture.run(&project, &["--json", "graph", "search", "at-search.1"]),
    ))
    .expect("search JSON");
    assert_eq!(exact["results"][0]["id"], "at-search.1");
    assert_eq!(exact["results"][0]["excerpt"], "Plain excerpt");
    let filtered: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "search", "Violet", "--status", "closed"],
    )))
    .expect("filtered search JSON");
    assert_eq!(filtered["results"].as_array().expect("results").len(), 0);
    success(&fixture.run(&project, &["graph", "search-rebuild"]));
    let rebuilt = success(&fixture.run(&project, &["graph", "search", "Violet"]));
    assert!(rebuilt.contains("at-search.1 [open] Violet sample"));

    let input = fixture.root.join("search-beads.jsonl");
    std::fs::write(
        &input,
        concat!(
            r#"{"id":"bd-search.2-alpha","title":"Closed sample","description":"Unique corundum phrase in a description","status":"closed","priority":2,"issue_type":"task","created_at":"2026-01-02T00:00:00Z"}"#,
            "\n"
        ),
    )
    .expect("import fixture");
    let file = input.to_str().expect("fixture path");
    success(&fixture.run(&project, &["graph", "import", file]));
    success(&fixture.run(&project, &["graph", "import", file]));
    let phrase: Value = serde_json::from_str(&success(
        &fixture.run(&project, &["--json", "graph", "search", "corundum phrase"]),
    ))
    .expect("phrase search JSON");
    assert_eq!(phrase["results"].as_array().expect("hits").len(), 1);
    assert_eq!(phrase["results"][0]["id"], "bd-search.2-alpha");
    assert_eq!(phrase["results"][0]["status"], "closed");
    let excluded: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &[
            "--json",
            "graph",
            "search",
            "corundum phrase",
            "--status",
            "open",
        ],
    )))
    .expect("state-filtered search JSON");
    assert!(excluded["results"].as_array().expect("hits").is_empty());
    let punctuated: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "search", "bd-search.2-alpha"],
    )))
    .expect("punctuated ID search JSON");
    assert_eq!(punctuated["results"][0]["id"], "bd-search.2-alpha");
}

#[test]
fn graph_create_request_key_replays_without_extra_rows_and_refuses_conflicts() {
    let fixture = Fixture::new("create-request-key");
    let project = fixture.project("project");
    fixture.enroll(&project);
    let command = [
        "--json",
        "graph",
        "create",
        "Stable title",
        "--body",
        "Stable body",
        "--kind",
        "epic",
        "--request-id",
        "create-key-1",
    ];
    let first: Value = serde_json::from_str(&success(&fixture.run(&project, &command)))
        .expect("first create JSON");
    let second: Value = serde_json::from_str(&success(&fixture.run(&project, &command)))
        .expect("replayed create JSON");
    assert_eq!(first, second);
    let id = first["id"].as_str().expect("generated item ID");
    assert!(id.starts_with("at-"));
    success(&fixture.run(&project, &["graph", "pin", id]));
    let after_pin: Value = serde_json::from_str(&success(&fixture.run(&project, &command)))
        .expect("replay after item changed");
    assert_eq!(after_pin["id"], id);
    assert_eq!(after_pin["status"], "pinned");

    let changed = fixture.run(
        &project,
        &[
            "graph",
            "create",
            "Changed title",
            "--body",
            "Stable body",
            "--kind",
            "epic",
            "--request-id",
            "create-key-1",
        ],
    );
    assert!(!changed.status.success(), "changed replay was accepted");
    assert!(String::from_utf8_lossy(&changed.stderr).contains("already used"));
    let changed_status = fixture.run(
        &project,
        &[
            "graph",
            "create",
            "Stable title",
            "--body",
            "Stable body",
            "--kind",
            "epic",
            "--pinned",
            "--request-id",
            "create-key-1",
        ],
    );
    assert!(
        !changed_status.status.success(),
        "changed initial status was accepted"
    );
    assert!(String::from_utf8_lossy(&changed_status.stderr).contains("already used"));
    let changed_id = fixture.run(
        &project,
        &[
            "graph",
            "create",
            "Stable title",
            "--body",
            "Stable body",
            "--kind",
            "epic",
            "--id",
            "at-other",
            "--request-id",
            "create-key-1",
        ],
    );
    assert!(!changed_id.status.success(), "changed target was accepted");
    assert!(String::from_utf8_lossy(&changed_id.stderr).contains("already used"));

    let database = fixture.database(&project);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let mut sql = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
            .await
            .expect("graph database");
        let items: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_items")
            .fetch_one(&mut sql)
            .await
            .expect("item count");
        let requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_requests")
            .fetch_one(&mut sql)
            .await
            .expect("request count");
        assert_eq!((items, requests), (1, 1));
    });

    let fresh_a = success(&fixture.run(&project, &["graph", "create", "Fresh title"]));
    let fresh_b = success(&fixture.run(&project, &["graph", "create", "Fresh title"]));
    assert_ne!(fresh_a, fresh_b, "ordinary create lost its fresh key");
}

#[test]
fn percent_escape_state_home_keeps_create_export_and_dry_run_on_one_database() {
    let fixture = Fixture::new("literal%41");
    let project = fixture.project("project");
    fixture.enroll(&project);
    let command = [
        "--json",
        "graph",
        "create",
        "Literal state item",
        "--id",
        "at-literal-state",
        "--request-id",
        "literal-state-create",
    ];
    let first = success(&fixture.run(&project, &command));
    assert_eq!(first, success(&fixture.run(&project, &command)));
    let exported = success(&fixture.run(&project, &["graph", "export"]));
    assert_eq!(exported.lines().count(), 1);
    assert!(exported.contains("\"id\":\"at-literal-state\""));
    let input = fixture.root.join("literal-state-export.jsonl");
    std::fs::write(&input, &exported).expect("export fixture");
    let dry_run: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &[
            "--json",
            "graph",
            "import",
            input.to_str().expect("fixture path"),
            "--dry-run",
        ],
    )))
    .expect("dry-run JSON");
    assert_eq!(dry_run["items"], 1);
    assert_eq!(dry_run["added"], json!([]));
    assert_eq!(
        success(&fixture.run(&project, &["graph", "export"])),
        exported
    );
    let literal = fixture.database(&project);
    assert!(literal.exists(), "literal state database is missing");
    let decoded = PathBuf::from(literal.to_string_lossy().replace("%41", "A"));
    assert!(!decoded.exists(), "write landed in percent-decoded sibling");

    // Simulate an older writer's decoded database, then perform the documented
    // operator-directed SQLite backup into the literal state location.
    std::fs::create_dir_all(decoded.parent().expect("decoded state parent"))
        .expect("decoded state directory");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("backup runtime");
    runtime.block_on(async {
        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&literal);
        let mut connection = sqlx::SqliteConnection::connect_with(&options)
            .await
            .expect("literal database");
        sqlx::query("VACUUM INTO ?")
            .bind(decoded.to_str().expect("decoded state path"))
            .execute(&mut connection)
            .await
            .expect("consistent legacy snapshot");
        connection.close().await.expect("close database");
    });
    std::fs::remove_file(&literal).expect("remove literal fixture");
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", literal.display()));
        if sidecar.exists() {
            std::fs::remove_file(sidecar).expect("remove stale sidecar");
        }
    }
    assert!(decoded.exists(), "legacy snapshot is missing");
    std::fs::copy(&decoded, &literal).expect("restore verified legacy snapshot");
    assert_eq!(
        success(&fixture.run(&project, &["graph", "export"])),
        exported
    );
    assert_eq!(success(&fixture.run(&project, &command)), first);
}

#[test]
fn distinct_create_keys_with_the_same_short_hash_keep_separate_items_and_replay() {
    let fixture = Fixture::new("create-request-collision");
    let project = fixture.project("project");
    fixture.enroll(&project);
    let keys = ["batch-36219", "batch-59213"];
    let mut ids = Vec::new();
    for key in keys {
        let args = [
            "--json",
            "graph",
            "create",
            "Independent item",
            "--request-id",
            key,
        ];
        let first: Value = serde_json::from_str(&success(&fixture.run(&project, &args)))
            .expect("created item JSON");
        let replay: Value = serde_json::from_str(&success(&fixture.run(&project, &args)))
            .expect("replayed item JSON");
        assert_eq!(first, replay, "key {key} did not replay its own item");
        ids.push(first["id"].as_str().expect("generated ID").to_owned());
    }
    assert_eq!(
        &ids[0][..11],
        &ids[1][..11],
        "regression keys no longer collide under the old eight-hex-digit ID"
    );
    assert_ne!(ids[0], ids[1], "independent requests collided");
    assert_eq!(ids[0].len(), 27, "generated ID lost its 96-bit hash");
    let database = fixture.database(&project);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let mut sql = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
            .await
            .expect("graph database");
        let items: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_items")
            .fetch_one(&mut sql)
            .await
            .expect("item count");
        let requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_requests")
            .fetch_one(&mut sql)
            .await
            .expect("request count");
        assert_eq!((items, requests), (2, 2));
    });
}

#[test]
fn keyed_create_replays_its_preexisting_short_id() {
    let fixture = Fixture::new("create-legacy-short-id");
    let project = fixture.project("project");
    fixture.enroll(&project);
    let key = "legacy-generated-key";
    let generated = atomic_graph::sqlite::hash_id("graph-cli", key.as_bytes());
    let old_id = &generated[..11];
    let original: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &[
            "--json",
            "graph",
            "create",
            "Prior item",
            "--id",
            old_id,
            "--request-id",
            key,
        ],
    )))
    .expect("original item JSON");
    let replay: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &[
            "--json",
            "graph",
            "create",
            "Prior item",
            "--request-id",
            key,
        ],
    )))
    .expect("replayed item JSON");
    assert_eq!(original, replay, "preexisting short ID changed on replay");
    assert_eq!(replay["id"], old_id);
}

#[test]
fn operator_cli_reconciles_only_the_named_failed_epic_binding() {
    let fixture = Fixture::new("reconcile-failed");
    let project = fixture.project("project");
    fixture.enroll(&project);
    success(&fixture.run(
        &project,
        &[
            "graph",
            "create",
            "Recoverable epic",
            "--id",
            "at-failed-epic",
            "--kind",
            "epic",
            "--pinned",
        ],
    ));
    let database = fixture.database(&project);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let store = SqliteStore::open_authorized(
            &database,
            SystemTime::now,
            atomic_durable::gate::HumanCredential::new(
                atomic_durable::gate::Principal::local("graph-cli-test"),
                "graph CLI enrolled operator secret",
            ),
        )
        .expect("authorized graph store");
        store
            .authorize_graph_run("at-failed-epic", "failed-run")
            .await
            .expect("authorize failed run");
        let mut sql = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
            .await
            .expect("graph database");
        sqlx::query("INSERT INTO workflow_status (workflow_uuid,status,name) VALUES (?,'ERROR',?)")
            .bind("failed-run")
            .bind(GRAPH_PIPELINE)
            .execute(&mut sql)
            .await
            .expect("failed run fixture");
        sqlx::query(
            "INSERT INTO atomic_graph_epic_runs (epic_id,run_id,state) VALUES (?,?,'active')",
        )
        .bind("at-failed-epic")
        .bind("failed-run")
        .execute(&mut sql)
        .await
        .expect("active binding fixture");
    });
    let reconciled: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &[
            "--json",
            "graph",
            "reconcile-failed",
            "at-failed-epic",
            "--run",
            "failed-run",
        ],
    )))
    .expect("reconciled JSON");
    assert_eq!(reconciled["binding"], "released");
    assert_eq!(reconciled["failed_run"], "failed-run");
    success(&fixture.run(
        &project,
        &[
            "graph",
            "reconcile-failed",
            "at-failed-epic",
            "--run",
            "failed-run",
        ],
    ));
    runtime.block_on(async {
        let mut sql = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
            .await
            .expect("graph database");
        let binding: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM atomic_graph_epic_runs WHERE epic_id='at-failed-epic'",
        )
        .fetch_one(&mut sql)
        .await
        .expect("binding count");
        let authorization: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM atomic_graph_run_authorizations WHERE run_id='failed-run'",
        )
        .fetch_one(&mut sql)
        .await
        .expect("authorization count");
        assert_eq!((binding, authorization), (0, 0));
    });
}

#[test]
fn refused_human_close_leaves_no_claim_and_evidenced_close_succeeds() {
    let fixture = Fixture::new("close-policy");
    let project = fixture.project("project");
    fixture.enroll(&project);
    success(&fixture.run(
        &project,
        &["graph", "create", "Policy work", "--id", "at-policy"],
    ));
    let database = fixture.database(&project);
    let store = SqliteStore::open_authorized(
        &database,
        SystemTime::now,
        atomic_durable::gate::HumanCredential::new(
            atomic_durable::gate::Principal::local("graph-cli-test"),
            "graph CLI enrolled operator secret",
        ),
    )
    .expect("authorized graph store");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime
        .block_on(store.set_close_policy(Actor::human("ignored"), vec![Evidence::Commit]))
        .expect("required commit policy");
    let before = runtime.block_on(async {
        let mut sql = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
            .await
            .expect("inspect graph");
        let row =
            sqlx::query("SELECT status,claim_epoch FROM atomic_graph_items WHERE id='at-policy'")
                .fetch_one(&mut sql)
                .await
                .expect("item row");
        let leases: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM atomic_graph_leases WHERE item_id='at-policy'",
        )
        .fetch_one(&mut sql)
        .await
        .expect("lease count");
        (
            row.get::<String, _>("status"),
            row.get::<i64, _>("claim_epoch"),
            leases,
        )
    });
    assert_eq!(before, ("open".to_owned(), 0, 0));
    let refused = fixture.run(&project, &["graph", "close", "at-policy"]);
    assert!(
        !refused.status.success(),
        "missing evidence closed the item"
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("commit"));
    let after = runtime.block_on(async {
        let mut sql = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
            .await
            .expect("inspect graph");
        let row =
            sqlx::query("SELECT status,claim_epoch FROM atomic_graph_items WHERE id='at-policy'")
                .fetch_one(&mut sql)
                .await
                .expect("item row");
        let leases: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM atomic_graph_leases WHERE item_id='at-policy'",
        )
        .fetch_one(&mut sql)
        .await
        .expect("lease count");
        (
            row.get::<String, _>("status"),
            row.get::<i64, _>("claim_epoch"),
            leases,
        )
    });
    assert_eq!(
        after, before,
        "rejected close changed status, epoch or lease"
    );
    let lease = runtime
        .block_on(store.claim(
            Actor::human("ignored"),
            String::new(),
            "at-policy".into(),
            Duration::from_secs(90),
        ))
        .expect("claim")
        .expect("available");
    let sha = "a".repeat(40);
    runtime
        .block_on(store.link_commit(lease.clone(), sha.clone()))
        .expect("commit link");
    runtime
        .block_on(store.finish(lease, Finish::Release))
        .expect("release");
    let closed: Value = serde_json::from_str(&success(
        &fixture.run(&project, &["--json", "graph", "close", "at-policy"]),
    ))
    .expect("closed JSON");
    assert_eq!(closed["status"], "closed");
    let evidence = runtime.block_on(async {
        let mut sql = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
            .await
            .expect("inspect evidence");
        let linked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM atomic_graph_commit_links WHERE item_id='at-policy' AND sha=?",
        )
        .bind(&sha)
        .fetch_one(&mut sql)
        .await
        .expect("commit link count");
        linked
    });
    assert_eq!(
        evidence, 1,
        "administrative close removed the commit evidence"
    );
    assert_eq!(
        runtime
            .block_on(store.show(Actor::agent("reader"), "at-policy".into()))
            .expect("show")
            .expect("item")
            .item
            .status,
        Status::Closed
    );
}

#[test]
fn repeated_human_close_preserves_terminal_outcome_and_finish_identity() {
    let fixture = Fixture::new("repeat-close");
    let project = fixture.project("project");
    fixture.enroll(&project);
    for id in ["at-success", "at-failure", "at-admin"] {
        success(&fixture.run(&project, &["graph", "create", id, "--id", id]));
    }
    let database = fixture.database(&project);
    let store = SqliteStore::open_authorized(
        &database,
        SystemTime::now,
        atomic_durable::gate::HumanCredential::new(
            atomic_durable::gate::Principal::local("graph-cli-test"),
            "graph CLI enrolled operator secret",
        ),
    )
    .expect("authorized graph store");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    for (id, outcome) in [("at-success", Finish::Close), ("at-failure", Finish::Fail)] {
        let lease = runtime
            .block_on(store.claim(
                Actor::human("ignored"),
                String::new(),
                id.to_owned(),
                Duration::from_secs(90),
            ))
            .expect("claim")
            .expect("available");
        runtime
            .block_on(store.link_commit(lease.clone(), format!("sha-{id}")))
            .expect("linked evidence");
        runtime
            .block_on(store.finish(lease, outcome))
            .expect("terminal finish");
    }
    success(&fixture.run(&project, &["graph", "close", "at-admin"]));
    let snapshot = |id: &str| {
        runtime.block_on(async {
        let mut sql = sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
            .await.expect("inspect close");
        let row = sqlx::query("SELECT status,claim_epoch,closed_at,terminal_outcome,last_finish_session,last_finish_epoch,last_finish_claimant,last_finish_acquisition FROM atomic_graph_items WHERE id=?")
            .bind(id).fetch_one(&mut sql).await.expect("closed item");
        let links = sqlx::query("SELECT sha FROM atomic_graph_commit_links WHERE item_id=? ORDER BY sha")
            .bind(id).fetch_all(&mut sql).await.expect("commit links")
            .iter().map(|row| row.get::<String, _>("sha")).collect::<Vec<_>>();
        (
            row.get::<String, _>("status"),
            row.get::<i64, _>("claim_epoch"),
            row.get::<Option<i64>, _>("closed_at"),
            row.get::<Option<String>, _>("terminal_outcome"),
            row.get::<Option<i64>, _>("last_finish_session"),
            row.get::<Option<i64>, _>("last_finish_epoch"),
            row.get::<Option<String>, _>("last_finish_claimant"),
            row.get::<Option<String>, _>("last_finish_acquisition"),
            links,
        )
    })
    };
    for id in ["at-success", "at-failure", "at-admin"] {
        let before = snapshot(id);
        success(&fixture.run(&project, &["graph", "close", id]));
        assert_eq!(snapshot(id), before, "repeat close rewrote {id}");
    }
}

#[test]
fn operator_sets_shows_and_clears_one_item_policy_with_json() {
    let fixture = Fixture::new("item-policy");
    let project = fixture.project("project");
    fixture.enroll(&project);
    for id in ["at-policy-a", "at-policy-b"] {
        success(&fixture.run(&project, &["graph", "create", id, "--id", id]));
    }
    let database = fixture.database(&project);
    let count_rows = || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let mut sql =
                sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
                    .await
                    .expect("inspect graph policy");
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM atomic_graph_item_policy")
                .fetch_one(&mut sql)
                .await
                .expect("item policy count")
        })
    };
    let empty: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "policy", "show", "at-policy-a"],
    )))
    .expect("policy JSON");
    assert_eq!(empty, json!({"item_id":"at-policy-a","policy":null}));
    assert_eq!(count_rows(), 0, "read-only show wrote a policy");

    let set: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &[
            "--json",
            "graph",
            "policy",
            "set",
            "at-policy-a",
            "--require",
            "commit",
        ],
    )))
    .expect("set policy JSON");
    assert_eq!(set["policy"]["requires"], json!(["commit"]));
    assert_eq!(set["policy"]["source"], "item");
    assert_eq!(set["policy"]["set_by"], "local:graph-cli-test");
    assert_eq!(count_rows(), 1);
    let other: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "policy", "show", "at-policy-b"],
    )))
    .expect("other policy JSON");
    assert_eq!(other["policy"], Value::Null, "A's policy changed B");
    let refused = fixture.run(&project, &["graph", "close", "at-policy-a"]);
    assert!(!refused.status.success(), "A closed without its commit");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("commit"));
    success(&fixture.run(&project, &["graph", "close", "at-policy-b"]));

    for args in [
        vec![
            "graph",
            "policy",
            "set",
            "at-policy-a",
            "--require",
            "verified-commit",
        ],
        vec![
            "graph",
            "policy",
            "set",
            "at-policy-a",
            "--require",
            "verified-commit",
            "--branch",
            "main",
        ],
    ] {
        let invalid = fixture.run(&project, &args);
        assert!(
            !invalid.status.success(),
            "invalid branch policy was stored"
        );
        assert!(
            String::from_utf8_lossy(&invalid.stderr).contains("refs/heads"),
            "wrong refusal: {}",
            String::from_utf8_lossy(&invalid.stderr)
        );
        assert_eq!(count_rows(), 1, "invalid policy added a row");
    }
    let verified: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &[
            "--json",
            "graph",
            "policy",
            "set",
            "at-policy-a",
            "--require",
            "verified-commit",
            "--branch",
            "refs/heads/main",
        ],
    )))
    .expect("verified policy JSON");
    assert_eq!(verified["policy"]["requires"], json!(["verified_commit"]));
    assert_eq!(verified["policy"]["authorized_branch"], "refs/heads/main");
    let cleared: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "policy", "clear", "at-policy-a"],
    )))
    .expect("cleared policy JSON");
    assert_eq!(cleared["policy"], Value::Null);
    assert_eq!(count_rows(), 0);
    success(&fixture.run(&project, &["graph", "close", "at-policy-a"]));
}

#[test]
fn operator_sets_shows_and_clears_structured_item_criteria() {
    let fixture = Fixture::new("item-criteria");
    let project = fixture.project("project");
    fixture.enroll(&project);
    success(&fixture.run(
        &project,
        &["graph", "create", "Criteria work", "--id", "at-criteria"],
    ));
    let criteria =
        r#"[{"kind":"text","text":"Record the tested behavior"},{"kind":"cel","expr":"true"}]"#;
    let set: Value = serde_json::from_str(&success(&fixture.run_stdin(
        &project,
        &[
            "--json",
            "graph",
            "criteria",
            "set",
            "at-criteria",
            "--file",
            "-",
        ],
        criteria,
    )))
    .expect("criteria set JSON");
    let expected: Value = serde_json::from_str(criteria).expect("fixture criteria JSON");
    assert_eq!(set["criteria"], expected);
    assert_eq!(set["set_by"], "local:graph-cli-test");
    let shown: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "criteria", "show", "at-criteria"],
    )))
    .expect("criteria show JSON");
    assert_eq!(shown, set);

    let invalid = fixture.run_stdin(
        &project,
        &["graph", "criteria", "set", "at-criteria", "--file", "-"],
        r#"[{"kind":"test","argv":["sh","-c","true"],"timeout_ms":1000}]"#,
    );
    assert!(
        !invalid.status.success(),
        "direct shell criterion was stored"
    );
    let after_invalid: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "criteria", "show", "at-criteria"],
    )))
    .expect("criteria after invalid set");
    assert_eq!(after_invalid, set, "invalid replacement changed criteria");

    let account = format!("operator:graph-cli-test@{}", fixture.state_id(&project));
    let secrets = atomic_core::credential::FileSecrets::open(
        &fixture.root.join("state/credentials"),
        &project,
    )
    .expect("file credential store");
    atomic_core::credential::SecretStore::save(&secrets, &account, "forged secret")
        .expect("forge local file secret");
    let forged = fixture.run(&project, &["graph", "criteria", "clear", "at-criteria"]);
    assert!(
        !forged.status.success(),
        "forged credential cleared criteria"
    );
    let still_shown: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "criteria", "show", "at-criteria"],
    )))
    .expect("criteria after forged clear");
    assert_eq!(still_shown, set, "forged clear changed criteria");
    atomic_core::credential::SecretStore::save(
        &secrets,
        &account,
        "graph CLI enrolled operator secret",
    )
    .expect("restore operator secret");
    let cleared: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "criteria", "clear", "at-criteria"],
    )))
    .expect("criteria clear JSON");
    assert_eq!(cleared["criteria"], json!([]));
    assert_eq!(cleared["set_by"], Value::Null);
}

#[test]
fn fake_and_rotated_cli_credentials_cannot_write_an_item_policy() {
    let fixture = Fixture::new("policy-credential");
    let project = fixture.project("project");
    fixture.enroll(&project);
    success(&fixture.run(
        &project,
        &[
            "graph",
            "create",
            "Credential work",
            "--id",
            "at-credential",
        ],
    ));
    let database = fixture.database(&project);
    let account = format!("operator:graph-cli-test@{}", fixture.state_id(&project));
    let secrets = atomic_core::credential::FileSecrets::open(
        &fixture.root.join("state/credentials"),
        &project,
    )
    .expect("file credential store");
    atomic_core::credential::SecretStore::save(&secrets, &account, "wrong secret")
        .expect("replace file secret with fake");
    let set = [
        "graph",
        "policy",
        "set",
        "at-credential",
        "--require",
        "commit",
    ];
    let fake = fixture.run(&project, &set);
    assert!(!fake.status.success(), "fake credential wrote a policy");
    assert!(
        String::from_utf8_lossy(&fake.stderr).contains("credential"),
        "wrong refusal: {}",
        String::from_utf8_lossy(&fake.stderr)
    );
    let inspect = |reason: &str| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let mut sql =
                sqlx::SqliteConnection::connect(&format!("sqlite://{}", database.display()))
                    .await
                    .expect("inspect graph policy");
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM atomic_graph_item_policy WHERE item_id='at-credential'",
            )
            .fetch_one(&mut sql)
            .await
            .expect("item policy count");
            assert_eq!(count, 0, "{reason} wrote a policy row");
        });
    };
    inspect("fake credential");
    atomic_core::credential::SecretStore::save(
        &secrets,
        &account,
        "graph CLI enrolled operator secret",
    )
    .expect("restore original file secret");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let durable = atomic_durable::runtime::Durare::sqlite(&database)
            .await
            .expect("durable database");
        atomic_durable::gate::replace_enrollment(
            &durable,
            &atomic_durable::gate::HumanCredential::new(
                atomic_durable::gate::Principal::local("graph-cli-test"),
                "replacement secret",
            ),
        )
        .await
        .expect("rotate enrollment");
    });
    let rotated = fixture.run(&project, &set);
    assert!(
        !rotated.status.success(),
        "rotated credential wrote a policy"
    );
    assert!(
        String::from_utf8_lossy(&rotated.stderr).contains("credential"),
        "wrong refusal: {}",
        String::from_utf8_lossy(&rotated.stderr)
    );
    inspect("rotated credential");
    let shown: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "policy", "show", "at-credential"],
    )))
    .expect("read-only policy JSON");
    assert_eq!(shown["policy"], Value::Null);
}

#[test]
fn policy_show_and_clear_name_the_legacy_fallback() {
    let fixture = Fixture::new("policy-legacy");
    let project = fixture.project("project");
    fixture.enroll(&project);
    success(&fixture.run(
        &project,
        &["graph", "create", "Legacy work", "--id", "at-legacy"],
    ));
    let store = SqliteStore::open_authorized(
        &fixture.database(&project),
        SystemTime::now,
        atomic_durable::gate::HumanCredential::new(
            atomic_durable::gate::Principal::local("graph-cli-test"),
            "graph CLI enrolled operator secret",
        ),
    )
    .expect("authorized graph store");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime
        .block_on(store.set_close_policy(Actor::human("ignored"), vec![Evidence::Commit]))
        .expect("legacy global policy");
    let legacy: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "policy", "show", "at-legacy"],
    )))
    .expect("legacy policy JSON");
    assert_eq!(legacy["policy"]["source"], "legacy_global");
    assert_eq!(legacy["policy"]["requires"], json!(["commit"]));
    assert_eq!(legacy["policy"]["set_by"], Value::Null);

    let overridden: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &[
            "--json",
            "graph",
            "policy",
            "set",
            "at-legacy",
            "--require",
            "verified-commit",
            "--branch",
            "refs/heads/release",
        ],
    )))
    .expect("item override JSON");
    assert_eq!(overridden["policy"]["source"], "item");
    assert_eq!(
        overridden["policy"]["authorized_branch"],
        "refs/heads/release"
    );
    let cleared: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "policy", "clear", "at-legacy"],
    )))
    .expect("restored fallback JSON");
    assert_eq!(
        cleared, legacy,
        "clear silently discarded legacy enforcement"
    );
    let refused = fixture.run(&project, &["graph", "close", "at-legacy"]);
    assert!(!refused.status.success(), "legacy fallback was bypassed");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("commit"));
}

#[test]
fn import_twice_and_export_reimport_are_stable() {
    let fixture = Fixture::new("roundtrip");
    let first = fixture.project("first");
    let second = fixture.project("second");
    fixture.enroll(&first);
    fixture.enroll(&second);
    let input = fixture.root.join("beads.jsonl");
    std::fs::write(&input, EXPORT).expect("export fixture");
    let file = input.to_str().expect("fixture path");

    let first_report: Value = serde_json::from_str(&success(
        &fixture.run(&first, &["--json", "graph", "import", file]),
    ))
    .expect("JSON report");
    assert_eq!(first_report["items"], 2);
    assert_eq!(first_report["added"], json!(["bd-epic", "bd-child"]));
    assert_eq!(first_report["external"], 2);
    let exported = success(&fixture.run(&first, &["graph", "export"]));
    assert_eq!(exported.lines().count(), 2);

    let second_report: Value = serde_json::from_str(&success(
        &fixture.run(&first, &["--json", "graph", "import", file]),
    ))
    .expect("JSON report");
    assert_eq!(second_report["added"], json!([]));
    assert_eq!(second_report["unchanged"], json!(["bd-epic", "bd-child"]));
    assert_eq!(
        success(&fixture.run(&first, &["graph", "export"])),
        exported
    );

    success(&fixture.run_stdin(&second, &["graph", "import", "-"], &exported));
    assert_eq!(
        success(&fixture.run(&second, &["graph", "export"])),
        exported
    );
    let json_export: Value = serde_json::from_str(&success(
        &fixture.run(&first, &["--json", "graph", "export"]),
    ))
    .expect("JSON export");
    assert_eq!(json_export["jsonl"], exported);
}

#[test]
fn operator_pins_an_imported_open_epic_only() {
    let fixture = Fixture::new("import-pin");
    let project = fixture.project("project");
    fixture.enroll(&project);
    success(&fixture.run_stdin(&project, &["graph", "import", "-"], EXPORT));

    let child = fixture.run(&project, &["graph", "pin", "bd-child"]);
    assert!(
        !child.status.success(),
        "ordinary imported child was pinned"
    );

    let account = format!("operator:graph-cli-test@{}", fixture.state_id(&project));
    let secrets = atomic_core::credential::FileSecrets::open(
        &fixture.root.join("state/credentials"),
        &project,
    )
    .expect("file credential store");
    atomic_core::credential::SecretStore::save(&secrets, &account, "forged secret")
        .expect("forge local file secret");
    let forged = fixture.run(&project, &["graph", "pin", "bd-epic"]);
    assert!(
        !forged.status.success(),
        "forged credential pinned imported epic"
    );
    atomic_core::credential::SecretStore::save(
        &secrets,
        &account,
        "graph CLI enrolled operator secret",
    )
    .expect("restore operator secret");

    let pinned: Value = serde_json::from_str(&success(
        &fixture.run(&project, &["--json", "graph", "pin", "bd-epic"]),
    ))
    .expect("pin JSON");
    assert_eq!(pinned, json!({"id":"bd-epic","status":"pinned"}));
    assert_eq!(
        serde_json::from_str::<Value>(&success(
            &fixture.run(&project, &["--json", "graph", "pin", "bd-epic"],)
        ))
        .expect("repeat pin JSON"),
        pinned,
        "repeat pin was not idempotent"
    );
    let exported = success(&fixture.run(&project, &["graph", "export"]));
    assert!(exported.contains("\"id\":\"bd-epic\""));
    assert!(exported.contains("\"pinned\":true"));
}

#[test]
fn malformed_file_refuses_every_line_and_dry_run_writes_nothing() {
    let fixture = Fixture::new("refusal");
    let project = fixture.project("project");
    fixture.enroll(&project);
    let valid = fixture.root.join("valid.jsonl");
    std::fs::write(&valid, EXPORT).expect("valid fixture");
    let valid_file = valid.to_str().expect("fixture path");
    let dry: Value = serde_json::from_str(&success(&fixture.run(
        &project,
        &["--json", "graph", "import", valid_file, "--dry-run"],
    )))
    .expect("dry-run report");
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["added"], json!(["bd-epic", "bd-child"]));
    assert_eq!(success(&fixture.run(&project, &["graph", "export"])), "");

    let bad = fixture.root.join("bad.jsonl");
    std::fs::write(
        &bad,
        format!(
            "{}\n{}\n",
            EXPORT.lines().next().expect("first line"),
            r#"{"id":"bd-bad","title":"Bad","status":"unknown"}"#
        ),
    )
    .expect("bad fixture");
    let output = fixture.run(
        &project,
        &["graph", "import", bad.to_str().expect("fixture path")],
    );
    assert!(!output.status.success(), "unknown status was accepted");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unsupported status unknown"),
        "wrong refusal: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(success(&fixture.run(&project, &["graph", "export"])), "");
}

#[cfg(unix)]
#[test]
fn unreadable_link_loop_is_refused_without_importing() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new("loop");
    let project = fixture.project("project");
    fixture.enroll(&project);
    let loop_file = fixture.root.join("loop.jsonl");
    symlink(&loop_file, &loop_file).expect("link to itself");
    let output = fixture.run(
        &project,
        &["graph", "import", loop_file.to_str().expect("fixture path")],
    );
    assert!(!output.status.success(), "link loop was accepted");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("read beads JSONL"),
        "wrong refusal: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(success(&fixture.run(&project, &["graph", "export"])), "");
}

#[test]
fn configured_remote_graph_refuses_instead_of_using_local_sqlite() {
    let fixture = Fixture::new("postgres");
    let project = fixture.project("project");
    let input = fixture.root.join("beads.jsonl");
    std::fs::write(&input, EXPORT).expect("export fixture");
    let file = input.to_str().expect("fixture path");
    for (args, as_json) in [
        (vec!["graph", "export"], false),
        (vec!["graph", "import", file], false),
        (vec!["--json", "graph", "export"], true),
        (vec!["--json", "graph", "import", file], true),
    ] {
        let output = fixture
            .command(&project, &args)
            .env("ATOMIC_DATABASE_URL", "postgres://127.0.0.1:9/graph_test")
            .output()
            .expect("atomic graph command ran");
        assert!(
            !output.status.success(),
            "PostgreSQL graph command succeeded"
        );
        if as_json {
            let report: Value = serde_json::from_slice(&output.stdout).expect("JSON refusal");
            assert!(
                report["error"]["description"]
                    .as_str()
                    .expect("error description")
                    .contains("require a SQLite project store"),
                "wrong JSON refusal: {report}"
            );
        } else {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("require a SQLite project store"),
                "wrong refusal: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(
            !has_atomic_database(&fixture.root),
            "failed graph command created a local atomic.db for {args:?}"
        );
    }
}

fn has_atomic_database(dir: &Path) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(error) => panic!("cannot inspect {}: {error}", dir.display()),
    };
    for entry in entries {
        let entry = entry.expect("inspect graph test state entry");
        let path = entry.path();
        if path.file_name() == Some(std::ffi::OsStr::new("atomic.db")) {
            return true;
        }
        if entry
            .file_type()
            .expect("inspect graph test state type")
            .is_dir()
            && has_atomic_database(&path)
        {
            return true;
        }
    }
    false
}

#[test]
fn import_redacts_provider_credentials_in_stored_rows_and_exports() {
    let fixture = Fixture::new("import-redaction");
    let project = fixture.project("project");
    fixture.enroll(&project);
    let pat = format!("github_pat_{}", "a".repeat(40));
    let api_key = format!("sk-proj-{}", "b".repeat(40));
    let item = json!({"id":"review","title":"Example","status":"open","description": format!("credential {pat}"),"comments":[{"author":"reviewer","text":format!("credential {api_key}")}]});
    let memory = json!({"_type":"memory","key":"review-note","value":format!("credential {pat}")});
    success(&fixture.run_stdin(
        &project,
        &["graph", "import", "-"],
        &format!("{item}\n{memory}\n"),
    ));
    let output = success(&fixture.run(&project, &["graph", "export"]));
    let db = fixture.database(&project);
    let stored = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let mut conn = sqlx::SqliteConnection::connect_with(&sqlx::sqlite::SqliteConnectOptions::new().filename(db).read_only(true)).await.expect("read database");
        sqlx::query_scalar::<_,String>("SELECT body FROM atomic_graph_items UNION ALL SELECT imported_json FROM atomic_graph_items UNION ALL SELECT text FROM atomic_graph_notes UNION ALL SELECT value FROM atomic_graph_memories")
            .fetch_all(&mut conn).await.expect("query stored text")
    });
    assert_eq!(stored.len(), 4);
    for text in stored.iter().chain([&output]) {
        assert!(
            text.contains("credential "),
            "surrounding prose lost: {text}"
        );
        assert!(
            !text.contains(&pat) && !text.contains(&api_key),
            "stored a credential"
        );
    }
}

#[test]
fn inspection_from_the_cli_leaves_a_missing_database_missing() {
    let fixture = Fixture::new("inspection");
    let project = fixture.project("project");
    assert_eq!(success(&fixture.run(&project, &["graph", "export"])), "");
    let report: Value = serde_json::from_str(&success(&fixture.run_stdin(
        &project,
        &["--json", "graph", "import", "--dry-run", "-"],
        EXPORT,
    )))
    .expect("dry run report");
    assert_eq!(report["added"], json!(["bd-epic", "bd-child"]));
    assert!(
        !has_atomic_database(&fixture.root),
        "inspection created a database"
    );
}

#[test]
fn ready_and_show_inspect_imported_items_without_opening_an_absent_store() {
    let fixture = Fixture::new("ready-show");
    let project = fixture.project("project");
    let ready: Value = serde_json::from_str(&success(
        &fixture.run(&project, &["--json", "graph", "ready"]),
    ))
    .expect("empty ready JSON");
    assert_eq!(ready, json!([]));
    assert!(!has_atomic_database(&fixture.root));
    let missing = fixture.run(&project, &["graph", "show", "bd-absent"]);
    assert!(!missing.status.success(), "missing item must be reported");
    assert!(!has_atomic_database(&fixture.root));

    fixture.enroll(&project);
    let items = concat!(
        r#"{"id":"bd-open","title":"Clear blocker","description":"First body","status":"open","priority":1,"issue_type":"task","created_at":"2026-01-01T00:00:00Z"}"#,
        "\n",
        r#"{"id":"bd-blocked","title":"Held item","description":"Implementation body","acceptance_criteria":"Acceptance text","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-01T00:00:00Z","dependencies":[{"issue_id":"bd-blocked","depends_on_id":"bd-open","type":"blocks"}]}"#,
        "\n"
    );
    success(&fixture.run_stdin(&project, &["graph", "import", "-"], items));
    let ready: Value = serde_json::from_str(&success(
        &fixture.run(&project, &["--json", "graph", "ready"]),
    ))
    .expect("ready JSON");
    assert_eq!(ready.as_array().expect("ready array").len(), 1);
    assert_eq!(ready[0]["id"], "bd-open");
    let shown: Value = serde_json::from_str(&success(
        &fixture.run(&project, &["--json", "graph", "show", "bd-blocked"]),
    ))
    .expect("show JSON");
    assert_eq!(shown["id"], "bd-blocked");
    assert_eq!(shown["body"], "Implementation body");
    assert_eq!(shown["acceptance_criteria"], "Acceptance text");
    assert_eq!(shown["status"], "open");
    assert_eq!(shown["blocked_by"], json!(["bd-open"]));
    assert_eq!(shown["edges"], json!([{"kind":"blocks","to":"bd-open"}]));
}

#[test]
fn inspection_works_while_another_connection_holds_the_write_lock() {
    use sqlx::{Connection, SqliteConnection};
    let fixture = Fixture::new("inspection-lock");
    let project = fixture.project("project");
    fixture.enroll(&project);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let options =
            sqlx::sqlite::SqliteConnectOptions::new().filename(fixture.database(&project));
        let mut writer = SqliteConnection::connect_with(&options)
            .await
            .expect("open writer");
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut writer)
            .await
            .expect("hold write lock");
        assert_eq!(success(&fixture.run(&project, &["graph", "export"])), "");
        let report: Value = serde_json::from_str(&success(&fixture.run_stdin(
            &project,
            &["--json", "graph", "import", "--dry-run", "-"],
            EXPORT,
        )))
        .expect("preview");
        assert_eq!(report["added"], json!(["bd-epic", "bd-child"]));
        sqlx::query("ROLLBACK")
            .execute(&mut writer)
            .await
            .expect("release write lock");
        assert_eq!(success(&fixture.run(&project, &["graph", "export"])), "");
    });
}

#[cfg(unix)]
#[test]
fn inspection_refuses_a_database_symlink_loop() {
    let fixture = Fixture::new("inspection-loop");
    let project = fixture.project("project");
    success(&fixture.run(&project, &["graph", "export"]));
    let db = fixture.database(&project);
    std::os::unix::fs::symlink(&db, &db).expect("loop at database path");
    let output = fixture.run(&project, &["graph", "export"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostic");
    assert!(
        stderr.contains("unable to open database file")
            || stderr.contains("Too many levels of symbolic links"),
        "{stderr}"
    );
}

#[test]
fn goals_inspection_leaves_missing_and_older_databases_untouched() {
    use sqlx::SqliteConnection;
    let fixture = Fixture::new("goal-inspection");
    let project = fixture.project("project");
    let inspect = || {
        assert_eq!(
            success(&fixture.run(&project, &["--json", "goals", "list"])).trim(),
            "[]"
        );
        let show = fixture.run(&project, &["goals", "show", "unknown"]);
        assert!(!show.status.success());
        assert!(
            String::from_utf8(show.stderr)
                .expect("diagnostic")
                .contains("is not a registered goal")
        );
    };
    inspect();
    let path = fixture.database(&project);
    assert!(!path.try_exists().expect("check missing database"));
    // Version zero, with existing application data and no atomic tables.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let mut conn = SqliteConnection::connect_with(&options)
            .await
            .expect("create fixture");
        sqlx::query("CREATE TABLE legacy_data (value TEXT)")
            .execute(&mut conn)
            .await
            .expect("legacy table");
        conn.close().await.expect("close fixture");
    });
    let before = std::fs::read(&path).expect("legacy bytes");
    let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions).expect("read-only store");
    inspect();
    assert_eq!(std::fs::read(path).expect("inspected bytes"), before);
}

#[test]
fn operator_restores_an_imported_goal_from_yaml_with_its_children() {
    let fixture = Fixture::new("goal-restore");
    let source = fixture.project("source");
    let destination = fixture.project("destination");
    let yaml = "title: Registered goal\nbudget: { period: day, tokens: 80, runs: 3 }\n";
    for project in [&source, &destination] {
        fixture.enroll(project);
        std::fs::create_dir(project.join("goals")).expect("goal directory");
        std::fs::write(project.join("goals/review.goal.yaml"), yaml).expect("operator definition");
    }
    success(&fixture.run(&source, &["goals", "add", "review"]));
    success(&fixture.run(&source, &["graph", "create", "Check", "--id", "child"]));
    success(&fixture.run(
        &source,
        &["graph", "link", "child", "parent-child", "goal-review"],
    ));
    let exported = success(&fixture.run(&source, &["graph", "export"]));
    success(&fixture.run_stdin(
        &destination,
        &["graph", "import", "-"],
        &exported.replace("Registered goal", "Imported title"),
    ));
    assert_eq!(
        success(&fixture.run(&destination, &["--json", "goals", "list"])).trim(),
        "[]"
    );
    let imported_search: Value = serde_json::from_str(&success(
        &fixture.run(&destination, &["--json", "graph", "search", "Imported"]),
    ))
    .expect("imported title search");
    assert_eq!(imported_search["results"][0]["id"], "goal-review");
    success(&fixture.run(&destination, &["goals", "add", "review"]));
    let stale_search: Value = serde_json::from_str(&success(
        &fixture.run(&destination, &["--json", "graph", "search", "Imported"]),
    ))
    .expect("stale title search");
    assert_eq!(stale_search["results"], json!([]));
    let restored_search: Value = serde_json::from_str(&success(
        &fixture.run(&destination, &["--json", "graph", "search", "Registered"]),
    ))
    .expect("restored title search");
    assert_eq!(restored_search["results"][0]["id"], "goal-review");
    let brief: Value = serde_json::from_str(&success(
        &fixture.run(&destination, &["--json", "goals", "show", "review"]),
    ))
    .expect("goal brief");
    assert_eq!(brief["open_children"][0]["id"], "child");
    assert_eq!(brief["definition"]["title"], "Registered goal");
    let listed: Value = serde_json::from_str(&success(
        &fixture.run(&destination, &["--json", "goals", "list"]),
    ))
    .expect("list");
    assert_eq!(listed[0]["title"], "Registered goal");
    assert_eq!(listed[0]["active"], true);
    // An imported ordinary task that occupies another goal's name is never adopted.
    let task =
        json!({"id":"goal-conflict","title":"Other task","status":"open","issue_type":"task"});
    success(&fixture.run_stdin(&destination, &["graph", "import", "-"], &task.to_string()));
    std::fs::write(destination.join("goals/conflict.goal.yaml"), yaml)
        .expect("conflicting definition");
    let collision = fixture.run(&destination, &["goals", "add", "conflict"]);
    assert!(!collision.status.success());
    assert!(
        String::from_utf8(collision.stderr)
            .expect("diagnostic")
            .contains("already names another graph item")
    );
}
