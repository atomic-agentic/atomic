// SPDX-License-Identifier: Apache-2.0
//! Black-box JSONL roundtrip through the separately pinned Dolt-backed bd.
#![cfg(target_os = "linux")]

use atomic_durable::gate::{self, HumanCredential, Principal};
use atomic_durable::runtime::Durare;
use atomic_graph::sqlite::SqliteStore;
use atomic_graph::store::{Actor, Draft, GraphStore, Status, Trust};
use serde_json::Value;
use sqlx::{Connection, Row, SqliteConnection};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: &str = "bd version 1.3.2 (9b65b9e19: HEAD@9b65b9e198ac)";

fn scratch(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "atomic-bd-memory-{label}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&path).expect("fresh test directory");
    path
}

fn pinned_bd() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("ATOMIC_TEST_BD_MEMORY_BIN")
            .expect("ATOMIC_TEST_BD_MEMORY_BIN must name the pinned v1.3.2 bd"),
    );
    assert!(path.is_absolute(), "pinned bd path must be absolute");
    let output = Command::new(&path)
        .arg("version")
        .output()
        .expect("bd version");
    assert!(output.status.success(), "bd version failed: {output:?}");
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("version UTF-8")
            .trim(),
        VERSION
    );
    path
}

fn run_bd(bd: &Path, project: &Path, args: &[&str]) -> String {
    let output = Command::new(bd)
        .args(args)
        .current_dir(project)
        .env("BD_ACTOR", "memory-interoperability-test")
        .env("BD_NO_DAEMON", "1")
        .output()
        .expect("run pinned bd");
    assert!(
        output.status.success(),
        "bd {args:?} failed: status={}; stderr={}; stdout={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stdout).expect("bd output UTF-8")
}

async fn graph(path: &Path) -> SqliteStore {
    let durable = Durare::sqlite(path).await.expect("durable graph store");
    let credential = HumanCredential::new(Principal::local("owner"), "memory interop secret");
    gate::enroll(&durable, &credential)
        .await
        .expect("enroll graph operator");
    SqliteStore::open_authorized(path, SystemTime::now, credential).expect("authorized graph")
}

#[test]
fn modern_bd_roundtrips_native_pinned_item_and_memories_without_promoting_trust() {
    let bd = pinned_bd();
    let root = scratch("roundtrip");
    let first_path = root.join("first.sqlite");
    let second_path = root.join("second.sqlite");
    let bd_project = root.join("beads-project");
    std::fs::create_dir(&bd_project).expect("beads project");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let first_export = runtime.block_on(async {
        let first = graph(&first_path).await;
        let human = Actor::human("owner");
        let mut pinned = Draft::new("at-pinned", "Pinned native thread");
        pinned.status = Status::Pinned;
        first
            .create_item(human.clone(), "create-pinned".into(), pinned)
            .await
            .expect("native pinned item");
        first
            .remember(
                human.clone(),
                "remember-human".into(),
                "human-key".into(),
                "human value".into(),
            )
            .await
            .expect("human memory");
        first
            .import(
                human.clone(),
                "{\"_type\":\"memory\",\"key\":\"external-key\",\"value\":\"external value\"}\n"
                    .into(),
                false,
            )
            .await
            .expect("external memory");
        first.export(human).await.expect("Atomic export")
    });
    let input = root.join("atomic.jsonl");
    std::fs::write(&input, first_export).expect("write disposable JSONL");

    run_bd(
        &bd,
        &bd_project,
        &[
            "init",
            "--non-interactive",
            "--prefix",
            "at",
            "--skip-hooks",
            "--skip-agents",
        ],
    );
    run_bd(
        &bd,
        &bd_project,
        &["import", "--input", input.to_str().expect("UTF-8 path")],
    );
    let reexport = run_bd(&bd, &bd_project, &["export", "--include-memories"]);
    let rows: Vec<Value> = reexport
        .lines()
        .map(|line| serde_json::from_str(line).expect("bd JSONL"))
        .collect();
    let pinned = rows
        .iter()
        .find(|row| row["id"] == "at-pinned")
        .expect("bd retained native pinned item");
    assert_eq!(pinned["pinned"], true);
    assert_eq!(pinned["metadata"]["atomic_graph"]["schema_version"], 1);
    assert_eq!(
        pinned["metadata"]["atomic_graph"]["native_status"],
        "pinned"
    );
    let memories: BTreeMap<String, String> = rows
        .iter()
        .filter(|row| row["_type"] == "memory")
        .map(|row| {
            (
                row["key"].as_str().expect("memory key").to_owned(),
                row["value"].as_str().expect("memory value").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        memories,
        BTreeMap::from([
            ("external-key".into(), "external value".into()),
            ("human-key".into(), "human value".into()),
        ])
    );

    runtime.block_on(async {
        let second = graph(&second_path).await;
        let human = Actor::human("owner");
        second
            .import(human.clone(), reexport, false)
            .await
            .expect("Atomic reimport of bd export");
        let shown = second
            .show(human.clone(), "at-pinned".into())
            .await
            .expect("show reimported pinned")
            .expect("pinned item");
        assert_eq!(shown.item.status, Status::Pinned);
        assert_eq!(shown.item.trust, Trust::External);
        assert!(
            second
                .prompt_memories(human, 10)
                .await
                .expect("trusted prompt memories")
                .is_empty(),
            "imported memories must not be promoted into trusted prompt context"
        );
        let mut conn = SqliteConnection::connect(&format!("sqlite://{}", second_path.display()))
            .await
            .expect("inspect imported memory trust");
        let actual: BTreeMap<String, (String, String)> =
            sqlx::query("SELECT key,value,trust FROM atomic_graph_memories ORDER BY key")
                .fetch_all(&mut conn)
                .await
                .expect("read imported memories")
                .iter()
                .map(|row| {
                    (
                        row.get::<String, _>("key"),
                        (row.get::<String, _>("value"), row.get::<String, _>("trust")),
                    )
                })
                .collect();
        assert_eq!(
            actual,
            BTreeMap::from([
                (
                    "external-key".into(),
                    ("external value".into(), "external".into())
                ),
                (
                    "human-key".into(),
                    ("human value".into(), "external".into())
                ),
            ])
        );
    });
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn installer_refuses_wrong_archive_digest_before_unpacking() {
    let root = scratch("wrong-digest");
    let fake_archive = root.join("wrong.tar.gz");
    std::fs::write(&fake_archive, b"not the pinned release archive").expect("fake archive");
    let destination = root.join("install");
    let installer = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("scripts/bd-memory-pin.sh");
    let output = Command::new("bash")
        .arg(installer)
        .arg(&destination)
        .env("ATOMIC_TEST_BD_MEMORY_ARCHIVE", &fake_archive)
        .output()
        .expect("run installer with wrong archive");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("checksum mismatch"),
        "wrong digest did not fail at the checksum: {output:?}"
    );
    assert!(
        !destination.join("bd").exists(),
        "unverified bd was unpacked"
    );
    let _ = std::fs::remove_dir_all(root);
}
