// SPDX-License-Identifier: Apache-2.0
//! Real goal commands against a private enrolled project store.

use serde_json::{Value, from_str};
use std::path::Path;
use std::process::{Command, Output};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

fn run(root: &Path, project: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(ATOMIC);
    command
        .args(args)
        .current_dir(project)
        .env("HOME", root.join("home"))
        .env("ATOMIC_HOME", root.join("state"))
        .env("ATOMIC_CREDENTIAL_STORE", "file")
        .env("USER", "goals-cli-test")
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
    command.output().expect("atomic command")
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "atomic failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 output")
}

#[test]
fn enrolled_operator_manages_goals_and_reads_persisted_brief() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-goals-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    std::fs::create_dir_all(root.join("home")).expect("home");
    std::fs::create_dir_all(project.join("goals")).expect("goals");
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: project\n").expect("manifest");
    std::fs::write(project.join("goals/review.goal.yaml"), "title: Review regressions\ndescription: Keep new failures visible\nmeasure:\n  kind: cel\n  expr: '1 + 1'\nbudget:\n  period: day\n  tokens: 100\n  runs: 3\n").expect("definition");

    success(run(&root, &project, &["graph", "export"]));
    let refused = run(&root, &project, &["goals", "add", "review"]);
    assert!(
        !refused.status.success(),
        "unenrolled command unexpectedly added a goal"
    );
    let state_id = std::fs::read_dir(root.join("state/projects"))
        .expect("project state")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .find(|name| name.starts_with("project-"))
        .expect("project state ID");
    let secret = "goal CLI operator secret";
    let secrets =
        atomic_core::credential::FileSecrets::open(&root.join("state/credentials"), &project)
            .expect("credential store");
    atomic_core::credential::SecretStore::save(
        &secrets,
        &format!("operator:goals-cli-test@{state_id}"),
        secret,
    )
    .expect("save credential");
    let database = root.join("state/private").join(&state_id).join("atomic.db");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let durable = atomic_durable::runtime::Durare::sqlite(&database)
            .await
            .expect("database");
        atomic_durable::gate::enroll(
            &durable,
            &atomic_durable::gate::HumanCredential::new(
                atomic_durable::gate::Principal::local("goals-cli-test"),
                secret,
            ),
        )
        .await
        .expect("enroll");
    });

    let added: Value = from_str(&success(run(
        &root,
        &project,
        &["--json", "goals", "add", "review"],
    )))
    .expect("add JSON");
    assert_eq!(added["id"], "goal-review");
    let listed: Value =
        from_str(&success(run(&root, &project, &["--json", "goals", "list"]))).expect("list JSON");
    assert_eq!(listed[0]["title"], "Review regressions");
    success(run(
        &root,
        &project,
        &[
            "goals",
            "feedback",
            "review",
            "Include the failing test",
            "--kind",
            "gate-rejection",
            "--request-id",
            "feedback-1",
        ],
    ));
    success(run(&root, &project, &["goals", "pause", "review"]));
    let paused: Value = from_str(&success(run(
        &root,
        &project,
        &["--json", "goals", "show", "review"],
    )))
    .expect("paused JSON");
    assert_eq!(paused["active"], false);
    assert_eq!(paused["feedback"][0]["text"], "Include the failing test");
    assert_eq!(paused["tokens_remaining"], 100);
    assert_eq!(paused["runs_remaining"], 3);
    assert_eq!(paused["definition"]["measure"]["expr"], "1 + 1");
    assert_eq!(paused["measure"], Value::Null);
    let shown = success(run(&root, &project, &["goals", "show", "review"]));
    assert!(
        shown.contains("measure: CEL 1 + 1"),
        "declared measure should be visible before the first wake: {shown}"
    );
    success(run(&root, &project, &["goals", "resume", "review"]));
    let resumed: Value = from_str(&success(run(
        &root,
        &project,
        &["--json", "goals", "show", "review"],
    )))
    .expect("resumed JSON");
    assert_eq!(resumed["active"], true);
    let _ = std::fs::remove_dir_all(root);
}
