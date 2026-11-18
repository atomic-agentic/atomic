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
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot start
//! fails the tests.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![recursion_limit = "512"]
#![allow(clippy::unwrap_used)]

use atomic_core::Sandbox;
use atomic_core::codex::{self, CodexPaths, ModelEndpoint, TurnSpec};
use atomic_core::git::{Git, Signature};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
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

/// A project with one commit, in a scratch directory outside the system
/// temporary directory (which the workspace-write sandbox leaves writable),
/// and its worktrees.
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
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-sandboxed-{tag}-{}", std::process::id()));
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
    let answer = codex::run(
        &paths,
        &f.dir.join("codex-home"),
        cwd,
        &ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
        },
        &TurnSpec {
            prompt: "Do the work.".into(),
            sandbox: Sandbox::WorkspaceWrite,
            ..extra
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
