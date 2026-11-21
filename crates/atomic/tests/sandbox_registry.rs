// SPDX-License-Identifier: Apache-2.0

//! A turn's commands and its patches share one registry of the mount points
//! Codex's Linux sandbox makes.
//!
//! Codex's sandbox protects paths such as `.git` in a turn's writable roots
//! by mounting over them, making an empty mount point where there is none
//! and removing it when it is done, unless its registry says another
//! sandbox is still using it. The registry is in the `TMPDIR` of the
//! sandbox helper, outside the sandbox. A turn's commands give their helper
//! the backend's shared directory; Codex's file helper, which applies the
//! turn's patches under the same sandbox, takes the `TMPDIR` of the process
//! that runs the turn, and atomic points its own there before anything
//! starts.
//!
//! Here, turns at once in one directory each run shell commands and patches
//! in turn, in a process whose `TMPDIR` is the backend's shared directory,
//! as atomic's is: none fails, and the one registry is there. The same
//! turns in a process given another `TMPDIR` split the registry: the file
//! helper's appears there.
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot start
//! fails the tests.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![allow(clippy::unwrap_used)]

mod support;

use atomic_core::Sandbox;
use atomic_core::codex::{CodexPaths, ModelEndpoint, TurnSpec};
use atomic_core::runtime::{Artifacts, CodexRuntime, run_turn};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::usage::Metering;
use atomic_exec::{Backend, LocalBackend};
use serde_json::json;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Set in the process this test starts again, to the directory its turns
/// run in: that process runs the turns.
const TURNS_IN: &str = "ATOMIC_TEST_SANDBOX_REGISTRY_TURNS_IN";
const NAME: &str = "commands_and_patches_at_once_in_one_directory_share_one_registry";
const TURNS: usize = 4;
const EDITS: usize = 25;

#[test]
fn commands_and_patches_at_once_in_one_directory_share_one_registry() {
    support::require_sandbox();
    if let Some(dir) = std::env::var_os(TURNS_IN) {
        return turns_at_once(Path::new(&dir));
    }
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-sandbox-registry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    // The process's TMPDIR is the backend's shared directory, as atomic's
    // main makes its own: every command and patch succeeds.
    let shared = root.join("shared");
    let tmp = LocalBackend::in_dir(shared.join("scratch"))
        .shared_tmp()
        .unwrap()
        .to_owned();
    let out = turns_in_a_process(&shared, &tmp);
    assert!(
        out.status.success(),
        "turns with one registry failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        registry_in(&tmp).is_dir(),
        "no registry in {}",
        tmp.display()
    );

    // Given another TMPDIR, the file helper keeps its registry there, apart
    // from the commands': the registry is split. The turns may fail as they
    // race, or not; either way the registry is split.
    let split = root.join("split");
    let elsewhere = split.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let out = turns_in_a_process(&split, &elsewhere);
    assert!(
        registry_in(&elsewhere).is_dir(),
        "the file helper kept no registry in the TMPDIR it was given, {}:\n{}{}",
        elsewhere.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// This test, run again with `TMPDIR` set to `tmpdir`, running the turns in
/// `dir`.
fn turns_in_a_process(dir: &Path, tmpdir: &Path) -> std::process::Output {
    std::fs::create_dir_all(dir).unwrap();
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(TURNS_IN, dir)
        .env("TMPDIR", tmpdir)
        .output()
        .unwrap()
}

/// Where Codex's sandbox keeps its registry when its helper's `TMPDIR` is
/// `tmp`.
fn registry_in(tmp: &Path) -> PathBuf {
    let uid = std::fs::metadata(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
        .uid();
    std::fs::canonicalize(tmp)
        .unwrap()
        .join(format!("codex-bwrap-synthetic-mount-targets-{uid}"))
}

/// [`TURNS`] `workspace-write` turns at once on one backend, in one
/// directory, each running a shell command and then applying a patch,
/// [`EDITS`] times. Every command prints what it should and every patch
/// lands.
fn turns_at_once(dir: &Path) {
    let dir = std::fs::canonicalize(dir).unwrap();
    for sub in ["project", "codex-home", "bin"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let helper = dir.join("bin/codex-linux-sandbox");
    let _ = std::fs::remove_file(&helper);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_atomic"), &helper).unwrap();
    on_codex_runtime(async move {
        let backend = Backend::connect(LocalBackend::in_dir(dir.join("scratch"))).unwrap();
        let turn = |n: usize| {
            let dir = dir.clone();
            let backend = backend.clone();
            async move {
                let mut script: Vec<Reply> = (0..EDITS)
                    .flat_map(|i| {
                        [
                            exec(&format!("echo turn{n}-ok-{i}")),
                            exec(&format!(
                                "apply_patch <<'EOF'\n*** Begin Patch\n\
                                 *** Add File: turn{n}-{i}.txt\n+turn{n}-{i}\n\
                                 *** End Patch\nEOF\n"
                            )),
                        ]
                    })
                    .collect();
                script.push(Reply::Text("Done.".into()));
                let model = FakeModel::start(script).await.unwrap();
                let rt = runtime(&dir, &model);
                let answer = run_turn(
                    &rt,
                    &backend,
                    &TurnSpec {
                        prompt: "Do the work.".into(),
                        sandbox: Sandbox::WorkspaceWrite,
                        ..TurnSpec::default()
                    },
                )
                .await
                .unwrap();
                assert_eq!(answer, "Done.");
                // The last request carries every call's output. What failed
                // is returned, not asserted here, so that every turn ends.
                let last = model.requests().last().cloned().unwrap().to_string();
                (0..EDITS)
                    .filter_map(|i| {
                        let ran = last.contains(&format!("turn{n}-ok-{i}\\n"));
                        let landed = std::fs::read_to_string(
                            dir.join("project").join(format!("turn{n}-{i}.txt")),
                        )
                        .is_ok_and(|text| text == format!("turn{n}-{i}\n"));
                        (!ran || !landed)
                            .then(|| format!("turn{n} edit {i}: ran={ran} landed={landed}"))
                    })
                    .collect::<Vec<_>>()
            }
        };
        let turns: Vec<_> = (0..TURNS).map(|n| tokio::spawn(turn(n))).collect();
        let mut failed = Vec::new();
        for t in turns {
            failed.extend(t.await.unwrap());
        }
        assert!(failed.is_empty(), "failed: {failed:#?}");
    });
}

fn exec(command: &str) -> Reply {
    Reply::Tool {
        name: "exec_command".into(),
        arguments: json!({"cmd": command, "yield_time_ms": 10000}),
    }
}

fn runtime(dir: &Path, model: &FakeModel) -> CodexRuntime {
    CodexRuntime {
        paths: CodexPaths::with_helpers(
            PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
            dir.join("bin/codex-linux-sandbox"),
        ),
        codex_home: dir.join("codex-home"),
        cwd: dir.join("project"),
        endpoint: ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
            sign_in: None,
        },
        defaults: Default::default(),
        artifacts: Artifacts::none(),
        meter: Metering::none(),
        guard: atomic_core::codex::StateGuard::NoProjectState,
        caches: None,
    }
}
