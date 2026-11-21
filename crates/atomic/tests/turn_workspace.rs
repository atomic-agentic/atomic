// SPDX-License-Identifier: Apache-2.0

//! A Codex turn runs in the workspace its backend opened, under Codex's real
//! Linux sandbox:
//!
//! - the turn's directory, sandbox and environment are the ones the backend
//!   gave, not the ones the run asked for;
//! - an exec atom's scratch directory, which its command, unsandboxed, uses
//!   as `HOME` and `TMPDIR`, is outside everything a `workspace-write` turn
//!   may write: a turn that tries to plant a file in a live one fails;
//! - turns at once on one backend share Codex's registry of mount points and
//!   nothing else: each command's `TMPDIR` is its own turn's.
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

/// One scratch root, and so one backend directory for Codex's registry of
/// the mount points its sandbox makes, for every test here, as one
/// project's turns share theirs: the tests' turns run at once.
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
        caches: None,
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
/// directory, each running many commands. Codex's sandbox creates and
/// removes mount points for the paths it protects in their shared writable
/// root, and keeps a registry of them in its helper's TMPDIR, the backend's
/// shared directory, so the turns never remove a mount point another is
/// using. Each command's own TMPDIR is its turn's alone: no command finds
/// another turn's file there, and no turn can write the shared directory.
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
                // Each command leaves a file in its TMPDIR and counts what it
                // finds there that is not its own turn's.
                let mut script: Vec<Reply> = (0..COMMANDS)
                    .map(|i| {
                        tool(&format!(
                            "touch \"$TMPDIR/turn{n}-{i}\" && echo turn{n}-ok-{i} \
                             others=$(ls -A \"$TMPDIR\" | grep -vc '^turn{n}-')"
                        ))
                    })
                    .collect();
                // And last, it tries to write the backend's shared directory.
                let shared = shared_scratch().join(".tmp").join(format!("turn{n}"));
                script.push(tool(&format!(
                    "touch '{}' 2>/dev/null; echo shared-write-{n}=$?",
                    shared.display()
                )));
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
                // so far; the last one carries them all. What failed is
                // returned, not asserted here, so that every turn ends first.
                let last = model.requests().last().cloned().expect("requests");
                let failed = (0..COMMANDS)
                    .filter(|i| {
                        !last
                            .to_string()
                            .contains(&format!("turn{n}-ok-{i} others=0\\n"))
                    })
                    .count();
                let wrote_shared =
                    !last.to_string().contains(&format!("shared-write-{n}=1")) || shared.exists();
                (failed, wrote_shared)
            }
        };
        let turns: Vec<_> = (0..TURNS).map(|n| tokio::spawn(turn(n))).collect();
        let (mut failed, mut wrote_shared) = (Vec::new(), Vec::new());
        for t in turns {
            let (f, w) = t.await.expect("the turn");
            failed.push(f);
            wrote_shared.push(w);
        }
        assert_eq!(
            wrote_shared,
            vec![false; TURNS],
            "turns that could write the backend's shared directory"
        );
        assert_eq!(
            failed,
            vec![0; TURNS],
            "commands in each turn that failed, or found another turn's files in their TMPDIR"
        );
        // Nor was anything made there to protect, as it would be in a
        // writable root.
        let shared = shared_scratch().join(".tmp");
        for name in [".git", ".agents", ".codex", ".aws"] {
            assert!(
                !shared.join(name).exists(),
                "{name} appeared in the backend's shared {}",
                shared.display()
            );
        }
    });
}

/// A command that is exactly the flag naming the sandbox helper's inner
/// stage leaves the helper's outer stage outer: Codex's registry of mount
/// points stays in the backend's shared directory, out of the turn's reach,
/// and does not move into the turn's own TMPDIR, which the turn may write.
#[test]
fn a_command_naming_the_inner_stage_leaves_the_registry_shared() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("inner-flag");
        let backend = Backend::connect(LocalBackend::in_dir(shared_scratch())).expect("connects");
        let model = FakeModel::start(vec![
            tool("--apply-seccomp-then-exec"),
            tool("ls -A \"$TMPDIR\"; echo listed"),
            Reply::Text("Done.".into()),
        ])
        .await
        .expect("fake model");
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
        let last = model
            .requests()
            .last()
            .cloned()
            .expect("requests")
            .to_string();
        assert!(last.contains("listed"), "{last}");
        assert!(
            !last.contains("codex-bwrap-synthetic-mount-targets"),
            "Codex's registry moved into the turn's own TMPDIR: {last}"
        );
    });
}

/// Admits every call with `limit` and records each settlement.
struct Recorder {
    limit: atomic_core::usage::Limit,
    settled: std::sync::Mutex<Vec<atomic_core::usage::Outcome>>,
}

impl atomic_core::usage::Meter for Recorder {
    fn admit(
        &self,
        call: atomic_core::usage::Call,
    ) -> atomic_core::usage::MeterFuture<'_, atomic_core::usage::Ticket> {
        Box::pin(async move {
            Ok(atomic_core::usage::Ticket {
                request: format!("r:{}", call.index),
                attempt: 1,
                model: call.model,
                admitted: std::time::Instant::now(),
                limit: self.limit,
                bounded_by: None,
            })
        })
    }

    fn settle(
        &self,
        _: atomic_core::usage::Ticket,
        outcome: atomic_core::usage::Outcome,
    ) -> atomic_core::usage::MeterFuture<'_, ()> {
        Box::pin(async move {
            self.settled.lock().expect("settled").push(outcome);
            Ok(())
        })
    }
}

#[test]
fn a_turn_cut_off_while_a_command_keeps_running_is_charged_as_one_in_flight() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("yielding");
        // The model starts a command that keeps running and yields back at
        // once, so Codex reports its start and not its end; the turn's next
        // request is then in flight when its time runs out.
        let model = FakeModel::start(vec![
            Reply::Tool {
                name: "exec_command".into(),
                arguments: json!({"cmd": "sleep 60", "yield_time_ms": 300}),
            },
            Reply::Delayed {
                after: std::time::Duration::from_secs(120),
                reply: Box::new(Reply::Text(r#"{"done": true}"#.into())),
            },
        ])
        .await
        .expect("fake model");
        let recorder = std::sync::Arc::new(Recorder {
            limit: atomic_core::usage::Limit {
                output_tokens: None,
                time: Some(std::time::Duration::from_secs(8)),
            },
            settled: std::sync::Mutex::new(Vec::new()),
        });
        let mut rt = runtime(
            &dir,
            &model,
            Backend::connect(LocalBackend::in_dir(shared_scratch())).expect("connects"),
        );
        rt.meter = Metering::new(recorder.clone());
        let spec = TurnSpec {
            prompt: "build it".into(),
            ..TurnSpec::default()
        };
        let ended = run_turn(&rt, &spec).await;
        let e = ended.expect_err("cut off");
        assert!(format!("{e}").contains("cut off"), "{e}");
        assert_eq!(model.calls(), 2, "the request after the command was made");
        let one = atomic_core::usage::Usage {
            input_tokens: 1,
            output_tokens: 1,
        };
        assert_eq!(
            *recorder.settled.lock().expect("settled"),
            [atomic_core::usage::Outcome::Partial {
                reported: one,
                last: Some(one),
            }],
            "the request in flight while the command ran is charged"
        );
    });
}

/// The variable that says `/tmp` belongs to this test run alone, as in
/// `scripts/dev.sh`'s container or a CI job's machine: only then does a test
/// create and remove `/tmp/.codex`, which any Codex session sharing that
/// `/tmp` would see.
const PRIVATE_TMP: &str = "ATOMIC_TEST_PRIVATE_TMP";

/// Creates and removes `/tmp/.codex` as fast as it can until dropped, then
/// stops, and leaves no `/tmp/.codex` of its own behind, whatever the test
/// did; one it did not create it never touches.
struct TmpCodexToggler {
    stop: Arc<std::sync::atomic::AtomicBool>,
    toggled: Arc<std::sync::atomic::AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TmpCodexToggler {
    const TARGET: &str = "/tmp/.codex";

    fn start() -> Self {
        assert!(
            !Path::new(Self::TARGET).exists(),
            "/tmp/.codex exists already, and this test touches only one it makes"
        );
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let toggled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let thread = {
            let (stop, toggled) = (stop.clone(), toggled.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    if std::fs::create_dir(Self::TARGET).is_ok() {
                        // Ours until removed: removed at once.
                        let _ = std::fs::remove_dir(Self::TARGET);
                        toggled.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            })
        };
        Self {
            stop,
            toggled,
            thread: Some(thread),
        }
    }

    fn toggled(&self) -> usize {
        self.toggled.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for TmpCodexToggler {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A `/tmp/.codex` that something else creates and removes while a turn runs
/// does not break the turn's sandbox. Codex keeps a `.codex` read-only in
/// every writable root, so one that came and went under a writable `/tmp`
/// failed bubblewrap's remount; `/tmp` is no writable root of a turn's.
///
/// It runs only where `/tmp` is this test run's own ([`PRIVATE_TMP`]), and
/// fails rather than skips elsewhere under `sandbox-tests`.
#[test]
fn a_tmp_codex_that_comes_and_goes_does_not_break_a_turn() {
    support::require_sandbox();
    assert!(
        std::env::var_os(PRIVATE_TMP).is_some_and(|v| v == "1"),
        "this test creates and removes /tmp/.codex, which a Codex session sharing /tmp would \
         see: run it where /tmp is this run's own, as scripts/dev.sh and CI do, with \
         {PRIVATE_TMP}=1"
    );
    const COMMANDS: usize = 60;
    // As fast as it can: with `/tmp` a writable root, this breaks about half
    // of the turn's commands.
    let toggler = TmpCodexToggler::start();
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
        let _ = std::fs::remove_dir_all(&dir);
    });
    let toggled = toggler.toggled();
    drop(toggler);
    assert!(
        toggled > COMMANDS,
        "/tmp/.codex came and went during the turn"
    );
}
