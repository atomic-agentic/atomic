// SPDX-License-Identifier: Apache-2.0

//! The hardened git command against a hostile repository and a hostile
//! worktree: atomic commits in a worktree where the atom planted a git
//! directory, hooks, config, attributes naming drivers and a nested
//! repository, in a repository whose own config and hooks name programs
//! too, and none of those programs runs. Hostile arguments are refused
//! before git runs, and every command stops at its limits.
//!
//! The control: `the_traps_are_real` shows that plain git, without the
//! hardening, runs the same traps.

#![allow(clippy::unwrap_used)]

mod support;

use atomic_core::git::{Git, GitError, Signature};
use atomic_core::worktree::{Worktree, Worktrees};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use support::{Scratch, executable, git, write};

/// Every program the fixture plants, by the marker it leaves.
const TRAPS: &[&str] = &[
    "pager",
    "fsmonitor",
    "textconv",
    "external",
    "gpg",
    "hook",
    "ssh",
    "askpass",
    "editor",
    "clean",
    "smudge",
    "process",
    "credential",
    "nested-fsmonitor",
    "nested-hook",
    "planted-fsmonitor",
    "planted-hook",
    "planted-clean",
];

/// Hooks git might run while atomic checks out, stages, commits and moves
/// refs.
const HOOKS: &[&str] = &[
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "post-checkout",
    "reference-transaction",
    "post-index-change",
    "pre-auto-gc",
    "post-rewrite",
];

struct Fixture {
    scratch: Scratch,
    project: PathBuf,
    markers: PathBuf,
    worktrees: Worktrees,
    base: String,
}

impl Fixture {
    fn root(&self) -> &Path {
        self.scratch.path()
    }

    fn markers(&self) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(&self.markers)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        found.sort();
        found
    }
}

fn trap(markers: &Path, name: &str) -> String {
    format!(
        "#!/bin/sh\n/usr/bin/touch {}/{name}\ncat\nexit 0\n",
        markers.display()
    )
}

/// A project whose own config, included config and hooks each name a program
/// that leaves a marker, with a base commit whose attributes send every file
/// through a filter and a diff driver.
fn hostile(tag: &str) -> Fixture {
    let scratch = Scratch::new(tag);
    let root = scratch.path().to_owned();
    let (markers, traps, hooks) = (root.join("markers"), root.join("traps"), root.join("hooks"));
    for dir in [&markers, &traps, &hooks] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let (project, _, _) = support::project(&root);
    write(&project.join(".gitattributes"), "* filter=evil diff=evil\n");
    git(&project, &["add", ".gitattributes"]);
    git(&project, &["commit", "-q", "-m", "attributes"]);
    let base = git(&project, &["rev-parse", "HEAD"]).trim().to_owned();
    for name in TRAPS {
        executable(&traps.join(name), &trap(&markers, name));
    }
    for hook in HOOKS {
        let body = format!("#!/bin/sh\n/usr/bin/touch {}/hook\n", markers.display());
        executable(&hooks.join(hook), &body);
        executable(&project.join(".git/hooks").join(hook), &body);
    }
    let t = traps.display();
    let included = root.join("included.config");
    write(
        &included,
        &format!("[core]\n\tpager = {t}/pager\n[filter \"sneaky\"]\n\tclean = {t}/clean\n"),
    );
    let config = project.join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[core]\n\tpager = {t}/pager\n\tfsmonitor = {t}/fsmonitor\n\thooksPath = {hooks}\n\
         \tsshCommand = {t}/ssh\n\taskPass = {t}/askpass\n\teditor = {t}/editor\n\
         [diff]\n\texternal = {t}/external\n[diff \"evil\"]\n\ttextconv = {t}/textconv\n\
         [filter \"evil\"]\n\tclean = {t}/clean\n\tsmudge = {t}/smudge\n\trequired = true\n\
         [filter \"proc\"]\n\tprocess = {t}/process\n\
         [credential]\n\thelper = {t}/credential\n\
         [log]\n\tshowSignature = true\n[gpg]\n\tprogram = {t}/gpg\n\
         [commit]\n\tgpgSign = true\n[include]\n\tpath = {included}\n",
        hooks = hooks.display(),
        included = included.display(),
    ));
    write(&config, &text);
    let git = Git::find().unwrap();
    let repo = git.discover(&project).unwrap();
    let worktrees = Worktrees::new(repo, root.join("state/worktrees"));
    Fixture {
        scratch,
        project,
        markers,
        worktrees,
        base,
    }
}

/// What an atom with write access to its worktree could leave there: a git
/// directory of its own in place of the worktree's `.git` file, attributes
/// naming drivers, a nested repository with its own config and hooks, files
/// named like options and a link out of the tree.
fn plant(f: &Fixture, worktree: &Worktree) {
    let wt = &worktree.path;
    let t = f.root().join("traps");
    let t = t.display();
    let m = f.markers.display();
    std::fs::remove_file(wt.join(".git")).unwrap();
    let planted = wt.join(".git");
    for dir in ["hooks", "objects", "refs/heads"] {
        std::fs::create_dir_all(planted.join(dir)).unwrap();
    }
    write(&planted.join("HEAD"), "ref: refs/heads/main\n");
    write(
        &planted.join("config"),
        &format!(
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\
             \tfsmonitor = {t}/planted-fsmonitor\n\thooksPath = {wt}/.git/hooks\n\
             [filter \"planted\"]\n\tclean = {t}/planted-clean\n",
            wt = wt.display()
        ),
    );
    for hook in HOOKS {
        executable(
            &planted.join("hooks").join(hook),
            &format!("#!/bin/sh\n/usr/bin/touch {m}/planted-hook\n"),
        );
    }
    write(
        &wt.join(".gitattributes"),
        "* filter=planted diff=evil\n*.txt filter=sneaky\nb.txt filter=proc\n",
    );
    let nested = wt.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-q"]);
    write(&nested.join("inner.txt"), "inner\n");
    git(&nested, &["add", "inner.txt"]);
    git(&nested, &["commit", "-q", "-m", "inner"]);
    let nested_config = nested.join(".git/config");
    let mut text = std::fs::read_to_string(&nested_config).unwrap();
    text.push_str(&format!("[core]\n\tfsmonitor = {t}/nested-fsmonitor\n"));
    write(&nested_config, &text);
    for hook in HOOKS {
        executable(
            &nested.join(".git/hooks").join(hook),
            &format!("#!/bin/sh\n/usr/bin/touch {m}/nested-hook\n"),
        );
    }
    write(&wt.join("a.txt"), "one\nTWO\nthree\n");
    write(&wt.join("b.txt"), "new\n");
    write(&wt.join("-rf"), "not an option\n");
    write(&wt.join("--output=x"), "not an option either\n");
    std::os::unix::fs::symlink("/etc/passwd", wt.join("link")).unwrap();
}

#[test]
fn the_traps_are_real() {
    // Without the hardening, plain git runs what the repository, the base
    // commit and the planted worktree name.
    let f = hostile("control");
    let wt = f.worktrees.prepare("run", "step", &f.base).unwrap();
    assert_eq!(f.markers(), Vec::<String>::new(), "prepare ran nothing");
    plant(&f, &wt);
    let plain = |dir: &Path, args: &[&str]| {
        Command::new(Git::find().unwrap().program())
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .output()
            .unwrap()
    };
    // In the worktree, git finds the planted git directory and its programs.
    plain(&wt.path, &["status"]);
    // In the project, the repository's own config and hooks.
    write(&f.project.join("a.txt"), "changed\n");
    plain(&f.project, &["add", "a.txt"]);
    plain(&f.project, &["log", "-p", "-1", "HEAD~1"]);
    plain(&f.project, &["update-ref", "refs/heads/trap", &f.base]);
    let ran = f.markers();
    for marker in [
        "planted-fsmonitor",
        "fsmonitor",
        "clean",
        "textconv",
        "hook",
    ] {
        assert!(
            ran.contains(&marker.to_string()),
            "{marker} did not run: {ran:?}"
        );
    }
}

#[test]
fn nothing_in_the_worktree_or_the_repository_runs() {
    let f = hostile("safe");
    let wt = f.worktrees.prepare("run", "step", &f.base).unwrap();
    assert!(wt.path.join("a.txt").is_file());
    assert_eq!(f.markers(), Vec::<String>::new(), "checkout ran nothing");
    plant(&f, &wt);
    let commit = f
        .worktrees
        .commit(&wt, "the atom's change", &Signature::default())
        .unwrap();
    assert_eq!(
        f.markers(),
        Vec::<String>::new(),
        "nothing the repository or the worktree names ran"
    );

    let repo = f.worktrees.repo();
    assert_eq!(repo.parents(&commit.commit).unwrap(), vec![f.base.clone()]);
    let changed = repo.changed_paths(&f.base, &commit.commit).unwrap();
    assert_eq!(
        changed,
        [
            "--output=x",
            "-rf",
            ".gitattributes",
            "a.txt",
            "b.txt",
            "link",
            "nested"
        ]
    );
    // Files went in as the atom left them: no filter rewrote them.
    assert_eq!(
        support::show(&f.project, &commit.commit, "a.txt"),
        "one\nTWO\nthree\n"
    );
    assert_eq!(
        support::show(&f.project, &commit.commit, "link"),
        "/etc/passwd"
    );
    // The planted git directory is never part of the commit.
    assert!(
        !support::tree_paths(&f.project, &commit.commit)
            .iter()
            .any(|p| p.starts_with(".git/"))
    );
}

#[test]
fn hostile_arguments_never_reach_git() {
    let f = hostile("arguments");
    let repo = f.worktrees.repo();
    for bad in [
        "--output=/tmp/synthetic",
        "HEAD~1",
        "@{-1}",
        ":/second",
        "main^",
        "-p",
        "a b",
    ] {
        assert!(
            matches!(repo.resolve_commit(bad), Err(GitError::Invalid(_))),
            "{bad}"
        );
        assert!(
            f.worktrees.pin_base("run", bad).is_err(),
            "{bad} was pinned"
        );
        assert!(
            f.worktrees.prepare("run", "step", bad).is_err(),
            "{bad} was checked out"
        );
        assert!(
            matches!(repo.parents(bad), Err(GitError::Invalid(_))),
            "{bad}"
        );
    }
    for bad in ["refs/../x", "refs/a b", "-d", "refs/x.lock", "HEAD"] {
        assert!(
            matches!(repo.create_ref(bad, &f.base), Err(GitError::Invalid(_))),
            "{bad}"
        );
    }
    // A run or step name never leaves the state directory.
    let wt = f
        .worktrees
        .prepare("../../escape", "../../../etc", &f.base)
        .unwrap();
    assert!(
        wt.path
            .starts_with(f.worktrees.dir().canonicalize().unwrap())
    );
    assert_eq!(
        wt.path.parent(),
        Some(f.worktrees.dir().canonicalize().unwrap().as_path())
    );
    // A commit that is not there is only not found.
    assert!(matches!(
        repo.resolve_commit(&"0".repeat(40)),
        Err(GitError::Failed { .. })
    ));
    assert!(
        std::fs::read_dir("/tmp")
            .unwrap()
            .all(|entry| entry.unwrap().file_name() != "synthetic")
    );
    assert_eq!(f.markers(), Vec::<String>::new());
}

#[test]
fn a_command_stops_at_its_limits() {
    let f = hostile("limits");
    let slow = Git::find()
        .unwrap()
        .with_timeout(Duration::from_nanos(1))
        .repo(f.worktrees.repo().git_dir());
    assert!(matches!(
        slow.refs("refs/heads/"),
        Err(GitError::Timeout(_))
    ));
    assert_eq!(f.markers(), Vec::<String>::new());
}
