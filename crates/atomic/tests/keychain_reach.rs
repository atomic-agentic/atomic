// SPDX-License-Identifier: Apache-2.0

//! On macOS the operator's credential is kept in the login keychain, and
//! neither an exec atom's command nor a Codex turn's command can reach it:
//! a keychain item this test stores, which `security` reads back outside
//! any sandbox, cannot be read through the keychain's services from inside
//! one, and the keychain files under `~/Library/Keychains` cannot be read
//! or listed.
//!
//! Needs Seatbelt and an unlocked default keychain, so it runs with the
//! `sandbox-tests` feature; with the feature on, a keychain that cannot be
//! written fails the test.

#![cfg(all(target_os = "macos", feature = "sandbox-tests"))]
#![recursion_limit = "512"]
#![allow(clippy::unwrap_used)]

mod support;

use atomic_core::Sandbox;
use atomic_core::codex::{CodexPaths, ExecSandbox, ModelEndpoint, StateGuard, TurnSpec};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_exec::{Confine, ExecutionBackend, LocalBackend};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A keychain item that is deleted however the test ends.
struct Item(String);

impl Item {
    fn store(service: &str, secret: &str) -> Self {
        let added = Command::new("/usr/bin/security")
            .args(["add-generic-password", "-U", "-a", "atomic-test", "-s"])
            .arg(service)
            .args(["-w", secret])
            .output()
            .unwrap();
        assert!(
            added.status.success(),
            "the test needs an unlocked default keychain to store an item in: {}",
            String::from_utf8_lossy(&added.stderr)
        );
        Self(service.to_owned())
    }
}

impl Drop for Item {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/security")
            .args(["delete-generic-password", "-a", "atomic-test", "-s"])
            .arg(&self.0)
            .output();
    }
}

/// The person's real home, as the system records it.
fn real_home() -> PathBuf {
    let out = Command::new("/usr/bin/id").arg("-un").output().unwrap();
    let user = String::from_utf8(out.stdout).unwrap();
    let out = Command::new("/usr/bin/dscl")
        .args([
            ".",
            "-read",
            &format!("/Users/{}", user.trim()),
            "NFSHomeDirectory",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    PathBuf::from(
        text.split_once(':')
            .map(|(_, home)| home.trim())
            .expect("dscl names the home directory"),
    )
}

/// What a command under Seatbelt prints when it tries every way to the
/// item and the keychain files.
fn probe(service: &str, home: &Path) -> String {
    format!(
        "/usr/bin/security find-generic-password -a atomic-test -s {service} -w; \
         echo \"find=$?\"; /bin/ls {keychains}; echo \"list=$?\"; \
         /bin/cat {keychains}/login.keychain-db >/dev/null; echo \"read=$?\"",
        keychains = home.join("Library/Keychains").display()
    )
}

fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-keychain-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["project", "state/codex", "scratch"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    std::fs::canonicalize(dir).unwrap()
}

/// Every attempt failed and the secret never showed.
fn assert_refused(what: &str, output: &str, secret: &str) {
    assert!(
        !output.contains(secret),
        "{what} read the keychain item: {output}"
    );
    for attempt in ["find", "list", "read"] {
        assert!(
            output.contains(&format!("{attempt}=")) && !output.contains(&format!("{attempt}=0")),
            "{what}: the {attempt} attempt was not refused: {output}"
        );
    }
}

#[test]
fn an_exec_command_cannot_reach_the_keychain() {
    support::require_sandbox();
    let dir = scratch("exec");
    let service = format!("atomic-keychain-exec-{}", std::process::id());
    let secret = format!("secret-{}", std::process::id());
    let _item = Item::store(&service, &secret);
    // The positive control: outside any sandbox the item is there to read.
    let read = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-a",
            "atomic-test",
            "-s",
            &service,
            "-w",
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&read.stdout).trim(), secret);

    let state = dir.join("state");
    let sandbox = ExecSandbox::new(
        &CodexPaths::with_helpers(
            PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
            dir.join("no-linux-helper"),
        ),
        StateGuard::deny_state_dir(&state).unwrap(),
        state.join("codex"),
    )
    .unwrap();
    let command = atomic_exec::Command {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), probe(&service, &real_home())],
        cwd: dir.join("project"),
        writable: vec![dir.join("scratch")],
        time_limit: std::time::Duration::from_secs(60),
        output_limit: 64 * 1024,
        ..Default::default()
    };
    let confined = sandbox.confine(&command, &dir.join("scratch")).unwrap();
    let done = LocalBackend::default().run(&confined).unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&done.stdout),
        String::from_utf8_lossy(&done.stderr)
    );
    assert_refused("an exec command", &output, &secret);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_turns_command_cannot_reach_the_keychain() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("turn");
        let service = format!("atomic-keychain-turn-{}", std::process::id());
        let secret = format!("secret-{}", std::process::id());
        let _item = Item::store(&service, &secret);
        let project = dir.join("project");
        let state = dir.join("state");
        let model = FakeModel::start(vec![
            Reply::Tool {
                name: "exec_command".into(),
                arguments: json!({
                    "cmd": probe(&service, &real_home()),
                    "workdir": project,
                    "yield_time_ms": 20000
                }),
            },
            Reply::Text("Done.".into()),
        ])
        .await
        .unwrap();
        let runtime = atomic_core::runtime::CodexRuntime {
            paths: CodexPaths::with_helpers(
                PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
                dir.join("no-linux-helper"),
            ),
            codex_home: state.join("codex"),
            cwd: project.clone(),
            endpoint: ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: None,
            },
            defaults: Default::default(),
            artifacts: atomic_core::runtime::Artifacts::none(),
            meter: atomic_core::usage::Metering::none(),
            guard: StateGuard::deny_state_dir(&state).unwrap(),
            caches: None,
        };
        let backend =
            atomic_exec::Backend::connect(LocalBackend::in_dir(dir.join("scratch"))).unwrap();
        let answer = atomic_core::runtime::run_turn(
            &runtime,
            &backend,
            &TurnSpec {
                prompt: "Do the work.".into(),
                sandbox: Sandbox::WorkspaceWrite,
                writable_roots: vec![project.clone()],
                ..TurnSpec::default()
            },
        )
        .await;
        assert_eq!(answer.unwrap(), "Done.");
        let requests = model.requests();
        let output = requests.get(1).expect("Codex answered the tool call")["input"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "function_call_output")
            .map(|item| item["output"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("the command in the sandbox printed: {output}");
        assert_refused("a turn's command", &output, &secret);
        let _ = std::fs::remove_dir_all(&dir);
    });
}
