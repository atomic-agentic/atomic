// SPDX-License-Identifier: Apache-2.0

//! A Codex turn runs in the workspace its backend opened, under Codex's real
//! Linux sandbox:
//!
//! - the turn's directory, sandbox and environment are the ones the backend
//!   gave, not the ones the run asked for;
//! - an exec atom's scratch directory, which its command, unsandboxed, uses
//!   as `HOME` and `TMPDIR`, is outside everything a `workspace-write` turn
//!   may write: a turn that tries to plant a file in a live one fails.
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot start
//! fails the tests.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![recursion_limit = "512"]

mod support;

use atomic_core::codex::{CodexPaths, ModelEndpoint, TurnSpec};
use atomic_core::runtime::{Artifacts, CodexRuntime, run_codex_atom, run_turn};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::usage::Metering;
use atomic_core::{Atom, Sandbox};
use atomic_exec::{Backend, ExecError, ExecutionBackend, LocalBackend, Open, Workspace};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "atomic-turn-workspace-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["project", "elsewhere", "codex-home", "bin"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
    let dir = std::fs::canonicalize(dir).expect("canonical");
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_atomic"),
        dir.join("bin/codex-linux-sandbox"),
    )
    .expect("sandbox helper");
    dir
}

/// One scratch root for every test here, as one project's backend has:
/// tests run at once, and turns that share a writable root (here `/tmp`,
/// which `workspace-write` leaves writable at this Codex revision) must
/// share the TMPDIR where Codex's sandbox keeps its mount-point registry.
fn shared_scratch() -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!(
                "atomic-turn-workspace-state-{}",
                std::process::id()
            ))
            .join("scratch");
        let _ = std::fs::remove_dir_all(&root);
        root
    })
    .clone()
}

fn runtime(dir: &Path, model: &FakeModel, backend: Backend) -> CodexRuntime {
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
        },
        defaults: Default::default(),
        artifacts: Artifacts::none(),
        meter: Metering::none(),
        guard: atomic_core::codex::StateGuard::NoProjectState,
        backend,
    }
}

/// What the turn's command printed, as Codex reported it to the model.
fn command_output(model: &FakeModel) -> String {
    let requests = model.requests();
    let follow_up = requests.get(1).expect("Codex answered the tool call");
    follow_up["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool(command: &str) -> Reply {
    Reply::Tool {
        name: "exec_command".into(),
        arguments: json!({"cmd": command, "yield_time_ms": 10000}),
    }
}

/// A backend that opens on this machine but puts the workspace somewhere
/// else than the run asked: another directory, a writable sandbox and an
/// environment of its own.
struct Relocating {
    local: LocalBackend,
    root: PathBuf,
}

impl ExecutionBackend for Relocating {
    fn connect(&self) -> Result<atomic_exec::Statement, ExecError> {
        self.local.connect()
    }
    fn open(&self, request: &Open) -> Result<Workspace, ExecError> {
        let mut ws = self.local.open(request)?;
        ws.root = Some(self.root.clone());
        ws.sandbox = Sandbox::WorkspaceWrite;
        ws.env = [("PLACED".to_owned(), "by-the-backend".to_owned())].into();
        Ok(ws)
    }
    fn run(&self, command: &atomic_exec::Command) -> Result<atomic_exec::Completed, ExecError> {
        self.local.run(command)
    }
    fn close(&self, workspace: &Workspace) -> Result<(), ExecError> {
        self.local.close(workspace)
    }
}

/// A read-only codex atom that reports whether its work is done.
struct Report;

impl Atom for Report {
    type In = Value;
    type Out = Value;
    const NAME: &'static str = "report";
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn output_schema(&self) -> Value {
        json!({"type": "object", "additionalProperties": false, "required": ["done"],
               "properties": {"done": {"type": "boolean"}}})
    }
    fn instructions(&self) -> Cow<'static, str> {
        "Report.".into()
    }
}

#[test]
fn a_turn_runs_where_its_workspace_says() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("placed");
        let model = FakeModel::start(vec![
            tool("pwd; echo placed=$PLACED; touch made-here; echo touched=$?"),
            Reply::Text(r#"{"done": true}"#.into()),
        ])
        .await
        .expect("fake model");
        let backend = Backend::connect(Relocating {
            local: LocalBackend::in_dir(shared_scratch()),
            root: dir.join("elsewhere"),
        })
        .expect("connects");
        let rt = runtime(&dir, &model, backend);
        let (out, _) = run_codex_atom(&Report, &json!({}), &rt)
            .await
            .expect("runs");
        assert_eq!(out, json!({"done": true}));
        let output = command_output(&model);
        assert!(
            output.contains(&dir.join("elsewhere").display().to_string()),
            "the turn ran outside its workspace: {output}"
        );
        assert!(
            output.contains("placed=by-the-backend"),
            "the workspace's environment: {output}"
        );
        // The atom asked for read-only; the backend's workspace is writable.
        assert!(output.contains("touched=0"), "{output}");
        assert!(dir.join("elsewhere/made-here").is_file());
        assert!(!dir.join("project/made-here").exists());
    });
}

/// Plant a file in a live exec scratch directory from a `workspace-write`
/// turn, and say whether it landed.
async fn plant_in_live_exec_scratch(dir: &Path, scratch_root: PathBuf) -> (bool, String) {
    // An exec atom's workspace, open while the turn runs.
    let exec = LocalBackend::in_dir(scratch_root.clone());
    let live = exec
        .open(&Open {
            label: "total".into(),
            run: "run-1".into(),
            step: "total".into(),
            attempt: "a1".into(),
            source: Some(dir.join("project")),
            ..Open::default()
        })
        .expect("opens");
    let target = live.scratch.clone().expect("scratch");
    let model = FakeModel::start(vec![
        tool(&format!(
            "mkdir -p '{0}/lib' && echo 'import os' > '{0}/usercustomize.py'; echo planted=$?",
            target.display()
        )),
        Reply::Text("Done.".into()),
    ])
    .await
    .expect("fake model");
    let rt = runtime(
        dir,
        &model,
        Backend::connect(LocalBackend::in_dir(scratch_root)).expect("connects"),
    );
    let answer = run_turn(
        &rt,
        &TurnSpec {
            prompt: "Do the work.".into(),
            sandbox: Sandbox::WorkspaceWrite,
            ..TurnSpec::default()
        },
    )
    .await
    .expect("the turn runs");
    assert_eq!(answer, "Done.");
    let landed = target.join("usercustomize.py").exists();
    exec.close(&live).expect("closes");
    (landed, command_output(&model))
}

#[test]
fn a_turn_cannot_plant_files_in_a_live_exec_scratch() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("plant");
        // Where atomic keeps scratch directories: the project's state
        // directory, outside every root a sandboxed turn may write.
        let (landed, output) = plant_in_live_exec_scratch(&dir, shared_scratch()).await;
        assert!(
            !landed,
            "the turn planted a file in the exec scratch: {output}"
        );
        assert!(
            output.contains("Read-only file system") || output.contains("Permission denied"),
            "{output}"
        );
        assert!(!output.contains("planted=0"), "{output}");
    });
}

/// Several `workspace-write` turns at once on one backend, in the same
/// directory, each running many commands: Codex's sandbox creates and
/// removes mount points for the paths it protects in their shared writable
/// root, and keeps a registry of them in TMPDIR, so the turns share one
/// TMPDIR and never remove a mount point the other is using.
#[test]
fn concurrent_turns_in_one_directory_never_break_each_others_sandbox() {
    support::require_sandbox();
    const TURNS: usize = 4;
    const COMMANDS: usize = 80;
    on_codex_runtime(async {
        let dir = scratch("concurrent");
        let backend = Backend::connect(LocalBackend::in_dir(shared_scratch())).expect("connects");
        let turn = |n: usize| {
            let dir = dir.clone();
            let backend = backend.clone();
            async move {
                let mut script: Vec<Reply> = (0..COMMANDS)
                    .map(|i| tool(&format!("echo turn{n}-ok-{i}")))
                    .collect();
                script.push(Reply::Text("Done.".into()));
                let model = FakeModel::start(script).await.expect("fake model");
                let rt = runtime(&dir, &model, backend);
                let answer = run_turn(
                    &rt,
                    &TurnSpec {
                        prompt: "Do the work.".into(),
                        sandbox: Sandbox::WorkspaceWrite,
                        ..TurnSpec::default()
                    },
                )
                .await
                .expect("the turn runs");
                assert_eq!(answer, "Done.");
                // Each request after the first carries every command's output
                // so far; the last one carries them all.
                let last = model.requests().last().cloned().expect("requests");
                (0..COMMANDS)
                    .filter(|i| !last.to_string().contains(&format!("turn{n}-ok-{i}\\n")))
                    .count()
            }
        };
        let turns: Vec<_> = (0..TURNS).map(|n| tokio::spawn(turn(n))).collect();
        let mut failed = Vec::new();
        for t in turns {
            failed.push(t.await.expect("the turn"));
        }
        assert_eq!(failed, vec![0; TURNS], "commands that failed in each turn");
    });
}

/// A `/tmp/.codex` that something else creates and removes while a turn runs
/// does not break the turn's sandbox. Codex keeps a `.codex` read-only in
/// every writable root, so one that came and went under a writable `/tmp`
/// failed bubblewrap's remount; `/tmp` is no writable root of a turn's.
#[test]
fn a_tmp_codex_that_comes_and_goes_does_not_break_a_turn() {
    support::require_sandbox();
    const COMMANDS: usize = 60;
    let target = Path::new("/tmp/.codex");
    // Only one this test makes and removes: someone else's stays as it is.
    assert!(
        !target.exists(),
        "/tmp/.codex exists already; remove it to run this test"
    );
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let toggled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // As fast as it can: with `/tmp` a writable root, this breaks about half
    // of the turn's commands.
    let toggler = {
        let (stop, toggled) = (stop.clone(), toggled.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                if std::fs::create_dir(target).is_ok() {
                    let _ = std::fs::remove_dir(target);
                    toggled.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _ = std::fs::remove_dir(target);
        })
    };
    on_codex_runtime(async {
        let dir = scratch("tmp-codex");
        let backend = Backend::connect(LocalBackend::in_dir(shared_scratch())).expect("connects");
        let mut script: Vec<Reply> = (0..COMMANDS)
            .map(|i| tool(&format!("echo still-ok-{i}")))
            .collect();
        script.push(Reply::Text("Done.".into()));
        let model = FakeModel::start(script).await.expect("fake model");
        let rt = runtime(&dir, &model, backend);
        let answer = run_turn(
            &rt,
            &TurnSpec {
                prompt: "Do the work.".into(),
                sandbox: Sandbox::WorkspaceWrite,
                ..TurnSpec::default()
            },
        )
        .await
        .expect("the turn runs");
        assert_eq!(answer, "Done.");
        let last = model.requests().last().cloned().expect("requests");
        let failed: Vec<usize> = (0..COMMANDS)
            .filter(|i| !last.to_string().contains(&format!("still-ok-{i}\\n")))
            .collect();
        assert!(
            failed.is_empty(),
            "commands that failed: {failed:?}: {last}"
        );
    });
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    toggler.join().expect("toggler");
    assert!(
        toggled.load(std::sync::atomic::Ordering::SeqCst) > COMMANDS,
        "/tmp/.codex came and went during the turn"
    );
}
