// SPDX-License-Identifier: Apache-2.0

//! Code-changing steps under Codex's `workspace-write` sandbox, at the
//! pinned revision, with the real Linux sandbox helper:
//!
//! - atomic makes the commit, outside the sandbox, after the turn ends. The
//!   scripted fake model edits a file and then tries to commit from inside
//!   the turn: the edit lands, the commit fails, because the sandbox keeps
//!   the worktree's git directory read-only. After the turn, atomic commits
//!   the edit on the base.
//! - build caches live outside worktrees, shared per project: a second step,
//!   in a fresh worktree, finds what the first step's build left in them.
//! - a read-only step in a worktree inside the state directory sees its
//!   checkout, and not the state directory's private part.
//! - a project and its state under the system temporary directory stay out
//!   of reach: the turn cannot write `/tmp`, the project's git directory or
//!   the state directory, and writes its temporary files to a directory of
//!   its own.
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot start
//! fails the tests.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![recursion_limit = "512"]
#![allow(clippy::unwrap_used)]

mod support;

use atomic_core::Sandbox;
use atomic_core::codex::{CodexPaths, ModelEndpoint, PrivateState, StateGuard, TurnSpec};
use atomic_core::git::{Git, Signature};
use atomic_core::runtime::{Artifacts, CodexRuntime, run_turn};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::usage::Metering;
use atomic_core::worktree::{Caches, Worktrees};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

fn plain_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(Git::find().unwrap().program())
        .args([
            "-c",
            "user.name=synthetic-author",
            "-c",
            "user.email=synthetic@example.invalid",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// A project with one commit, in a scratch directory, and its worktrees.
struct Fixture {
    dir: PathBuf,
    project: PathBuf,
    worktrees: Worktrees,
    base: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(tag: &str) -> Fixture {
    fixture_in(Path::new(env!("CARGO_TARGET_TMPDIR")), tag)
}

fn fixture_in(parent: &Path, tag: &str) -> Fixture {
    support::require_sandbox();
    let dir = parent.join(format!("atomic-sandboxed-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["project", "state", "codex-home", "bin"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let dir = std::fs::canonicalize(dir).unwrap();
    let project = dir.join("project");
    plain_git(&project, &["init", "-q"]);
    std::fs::write(project.join("a.txt"), "base\n").unwrap();
    plain_git(&project, &["add", "a.txt"]);
    plain_git(&project, &["commit", "-q", "-m", "base"]);
    let repo = Git::find().unwrap().discover(&project).unwrap();
    let worktrees = Worktrees::new(repo, dir.join("state/worktrees"));
    let base = worktrees.pin_base("sandboxed", "main").unwrap();
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_atomic"),
        dir.join("bin/codex-linux-sandbox"),
    )
    .unwrap();
    Fixture {
        dir,
        project,
        worktrees,
        base,
    }
}

/// Run one workspace-write turn in `cwd` whose model runs `command` once,
/// and return what the command printed, as Codex reported it to the model.
async fn sandboxed(f: &Fixture, cwd: &Path, command: &str, extra: TurnSpec) -> String {
    sandboxed_with(
        f,
        cwd,
        command,
        &StateGuard::NoProjectState,
        TurnSpec {
            sandbox: Sandbox::WorkspaceWrite,
            ..extra
        },
    )
    .await
}

/// Where the turns' backend keeps its scratch directories. This can itself
/// be under /tmp when the checkout and Cargo target directory are there.
fn scratch_root() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-sandboxed-scratch-{}", std::process::id()))
}

/// Run one turn of `spec` under `guard` in `cwd` whose model runs `command`
/// once, and return what the command printed.
async fn sandboxed_with(
    f: &Fixture,
    cwd: &Path,
    command: &str,
    guard: &StateGuard,
    spec: TurnSpec,
) -> String {
    let model = FakeModel::start(vec![
        Reply::Tool {
            name: "exec_command".into(),
            arguments: json!({"cmd": command, "workdir": cwd, "yield_time_ms": 10000}),
        },
        Reply::Text("Done.".into()),
    ])
    .await
    .unwrap();
    let paths = CodexPaths::with_helpers(
        PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
        f.dir.join("bin/codex-linux-sandbox"),
    );
    let runtime = CodexRuntime {
        paths,
        codex_home: f.dir.join("codex-home"),
        cwd: cwd.to_owned(),
        endpoint: ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
            sign_in: None,
        },
        defaults: Default::default(),
        artifacts: Artifacts::none(),
        meter: Metering::none(),
        guard: guard.clone(),
        caches: None,
    };
    let backend =
        atomic_exec::Backend::connect(atomic_exec::LocalBackend::in_dir(scratch_root())).unwrap();
    let answer = run_turn(
        &runtime,
        &backend,
        &TurnSpec {
            prompt: "Do the work.".into(),
            ..spec
        },
    )
    .await
    .unwrap();
    assert_eq!(answer, "Done.");
    let requests = model.requests();
    let follow_up = requests.get(1).expect("Codex answered the tool call");
    let output = follow_up["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].to_string())
        .collect::<Vec<_>>()
        .join("\n");
    eprintln!("the command in the sandbox printed: {output}");
    output
}

#[test]
fn the_turn_cannot_commit_and_atomic_commits_after_it() {
    on_codex_runtime(async {
        let f = fixture("commit");
        let wt = f
            .worktrees
            .prepare("sandboxed", "implement", &f.base)
            .unwrap();
        let command = "printf 'changed\\n' > a.txt; echo wrote-exit=$?; \
                       git add -A && git -c user.name=atom -c user.email=atom@example.invalid \
                       commit -qm inside; echo commit-exit=$?";
        let output = sandboxed(&f, &wt.path, command, TurnSpec::default()).await;

        // The sandbox ran the command and let it write in the worktree...
        assert!(output.contains("wrote-exit=0"), "{output}");
        assert_eq!(
            std::fs::read_to_string(wt.path.join("a.txt")).unwrap(),
            "changed\n"
        );
        // ...but not commit: the worktree's git directory is read-only.
        assert!(output.contains("Read-only file system"), "{output}");
        assert!(output.contains("commit-exit="), "{output}");
        assert!(
            !output.contains("commit-exit=0"),
            "the turn committed: {output}"
        );
        assert_eq!(
            f.worktrees.repo().refs("refs/heads/").unwrap().len(),
            1,
            "no branch moved"
        );

        // After the turn, atomic commits, outside the sandbox.
        let commit = f
            .worktrees
            .commit(&wt, "The atom's change", &Signature::default())
            .unwrap();
        let repo = f.worktrees.repo();
        assert_eq!(repo.parents(&commit.commit).unwrap(), vec![f.base.clone()]);
        assert_eq!(
            repo.changed_paths(&f.base, &commit.commit).unwrap(),
            ["a.txt"]
        );
        assert_eq!(
            plain_git(&f.project, &["show", &format!("{}:a.txt", commit.commit)]),
            "changed\n"
        );
    });
}

#[test]
fn a_fresh_worktree_reuses_the_shared_cache() {
    on_codex_runtime(async {
        let f = fixture("cache");
        let caches = Caches::new(f.dir.join("state/cache"));
        caches.create().unwrap();
        let spec = TurnSpec {
            env: caches.env(),
            writable_roots: vec![caches.dir().to_owned()],
            ..TurnSpec::default()
        };
        // A build that is cold without its output in the cache and warm with
        // it, keyed on the sources.
        let build = "key=$(sha256sum a.txt | cut -c1-16); out=\"$CARGO_TARGET_DIR/$key\"; \
                     if [ -f \"$out\" ]; then echo build=warm; \
                     else mkdir -p \"$CARGO_TARGET_DIR\" && echo built > \"$out\" && echo build=cold; fi; \
                     echo target=$CARGO_TARGET_DIR";

        let first = f.worktrees.prepare("sandboxed", "first", &f.base).unwrap();
        let output = sandboxed(&f, &first.path, build, spec.clone()).await;
        assert!(output.contains("build=cold"), "{output}");
        let target = caches.dir().join("cargo-target");
        assert!(
            output.contains(&format!("target={}", target.display())),
            "{output}"
        );
        f.worktrees
            .commit(&first, "first", &Signature::default())
            .unwrap();

        // Another step, in a fresh worktree, and the first step's worktree
        // reset: the cache is outside both, and the build is warm.
        let second = f.worktrees.prepare("sandboxed", "second", &f.base).unwrap();
        f.worktrees.prepare("sandboxed", "first", &f.base).unwrap();
        assert_ne!(first.path, second.path);
        assert!(!target.starts_with(&first.path) && !target.starts_with(&second.path));
        let output = sandboxed(&f, &second.path, build, spec).await;
        assert!(output.contains("build=warm"), "{output}");
        assert!(!second.path.join("target").exists());
    });
}

#[test]
fn a_project_under_the_system_temporary_directory_stays_out_of_reach() {
    on_codex_runtime(async {
        let f = fixture_in(&std::env::temp_dir(), "tmp");
        assert!(
            f.dir
                .starts_with(std::env::temp_dir().canonicalize().unwrap())
        );
        let state = f.dir.join("state");
        let wt = f
            .worktrees
            .prepare("sandboxed", "implement", &f.base)
            .unwrap();
        assert!(wt.path.starts_with(&state));
        let common = f.project.join(".git");
        let command = format!(
            "printf 'changed\\n' > a.txt; echo wrote-exit=$?; \
             echo x > /tmp/atomic-sandbox-probe-$$; echo tmp-exit=$?; \
             echo x > {common}/forged; echo common-exit=$?; \
             git update-ref refs/heads/forged HEAD; echo ref-exit=$?; \
             echo x > {state}/private/forged; echo state-exit=$?; \
             t=$(mktemp) && echo temp=$t; echo mktemp-exit=$?",
            common = common.display(),
            state = state.display(),
        );
        let guard = StateGuard::Deny(PrivateState {
            state_dir: state.clone(),
            private: state.join("private"),
            also: Vec::new(),
        });
        let spec = TurnSpec {
            sandbox: Sandbox::WorkspaceWrite,
            ..TurnSpec::default()
        };
        let output = sandboxed_with(&f, &wt.path, &command, &guard, spec).await;

        assert!(output.contains("wrote-exit=0"), "{output}");
        for refused in ["tmp", "common", "ref", "state"] {
            assert!(output.contains(&format!("{refused}-exit=")), "{output}");
            assert!(
                !output.contains(&format!("{refused}-exit=0")),
                "{refused} was writable: {output}"
            );
        }
        assert!(!common.join("forged").exists());
        assert!(!state.join("private/forged").exists());
        assert_eq!(f.worktrees.repo().refs("refs/heads/").unwrap().len(), 1);
        // Its temporary files go to the turn's private directory.
        assert!(output.contains("mktemp-exit=0"), "{output}");
        // The output is the JSON text of what the command printed.
        let temp = output
            .split("temp=")
            .nth(1)
            .and_then(|rest| rest.split("\\n").next())
            .expect("mktemp printed its file")
            .to_owned();
        // The turn's own temporary directory is inside its scratch directory
        // under the backend's root, not in the shared registry directory.
        assert!(
            Path::new(&temp).starts_with(scratch_root())
                && !Path::new(&temp).starts_with(scratch_root().join(".tmp"))
                && temp.contains("/tmp/tmp."),
            "{temp} is not in the turn's own temporary directory"
        );
    });
}

#[test]
fn a_read_only_turn_in_a_worktree_sees_its_checkout() {
    on_codex_runtime(async {
        let f = fixture("read-only");
        let state = f.dir.join("state");
        std::fs::create_dir_all(state.join("private")).unwrap();
        std::fs::write(state.join("private/atomic.db"), "store").unwrap();
        let wt = f.worktrees.prepare("sandboxed", "review", &f.base).unwrap();
        assert!(wt.path.starts_with(&state));
        let guard = StateGuard::Deny(PrivateState {
            state_dir: state.clone(),
            private: state.join("private"),
            also: Vec::new(),
        });
        // One stream, so what the commands print arrives in order: Codex
        // reports stdout and stderr separately, in no fixed order.
        let command = format!(
            "exec 2>&1; echo cwd=$(pwd); cat a.txt; echo read-exit=$?; \
             cat {}/private/atomic.db; echo private-exit=$?; echo x > a.txt; echo write-exit=$?",
            state.display()
        );
        let output = sandboxed_with(
            &f,
            &wt.path,
            &command,
            &guard,
            TurnSpec {
                sandbox: Sandbox::ReadOnly,
                ..TurnSpec::default()
            },
        )
        .await;
        assert!(
            output.contains(&format!("cwd={}", wt.path.display())),
            "the turn did not run in its worktree: {output}"
        );
        assert!(output.contains("base\\nread-exit=0"), "{output}");
        assert!(
            !output.contains("store"),
            "the private state leaked: {output}"
        );
        assert!(!output.contains("private-exit=0"), "{output}");
        assert!(!output.contains("write-exit=0"), "{output}");
        assert_eq!(
            std::fs::read_to_string(wt.path.join("a.txt")).unwrap(),
            "base\n"
        );
    });
}

/// Whether `dir` holds a file ending in `.sqlite`.
fn holds_databases(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(".sqlite"))
}

/// What Codex records of a project's threads, its state databases and its
/// logs, goes with the project's private state, which every project's
/// commands are denied by one entry: after a turn the project's records are
/// there and not in its Codex home, and a turn cannot read another project's.
#[test]
fn codex_records_go_with_the_projects_private_state() {
    on_codex_runtime(async {
        let f = fixture("codex-records");
        let state = f.dir.join("state");
        let private = state.join("private");
        // Another project's Codex records, where they now go.
        let other = private.join("another-project/codex/state_5.sqlite");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, "another project's records").unwrap();
        let guard = StateGuard::Deny(PrivateState {
            state_dir: state.clone(),
            private: private.clone(),
            also: Vec::new(),
        });
        let command = format!("exec 2>&1; cat {}; echo records-exit=$?", other.display());
        let output = sandboxed_with(
            &f,
            &f.project,
            &command,
            &guard,
            TurnSpec {
                sandbox: Sandbox::ReadOnly,
                ..TurnSpec::default()
            },
        )
        .await;
        assert!(
            !output.contains("another project's records") && output.contains("records-exit=1"),
            "the turn read another project's Codex records: {output}"
        );
        // This project's, by its state directory's name.
        let records = private.join("state/codex");
        assert!(
            holds_databases(&records),
            "no Codex records in {}",
            records.display()
        );
        assert!(
            !holds_databases(&f.dir.join("codex-home")),
            "Codex records are still kept in the Codex home"
        );
    });
}
