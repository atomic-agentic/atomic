// SPDX-License-Identifier: Apache-2.0

//! The coding tasks under `tests/coding-tasks/`, eval fixtures for atoms that
//! change code, keep their contract (see its README):
//!
//! - each task has a prompt, a seed project, a checker, a reference solution
//!   and, but for one that checks in its checker, hidden tests that share no
//!   path with the seed;
//! - the checker fails the seed with exit 1, not some other failure, so a
//!   checker that crashes cannot pass for one that grades;
//! - the checker passes the seed with the solution laid over it, and leaves
//!   the workspace it grades as it found it;
//! - a workspace holding a link or a Python startup hook fails, solved or
//!   not.
//!
//! The checkers need `python3`, `bash` and the POSIX tools; a missing one
//! fails the test.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// How many tasks there are.
const TASKS: usize = 14;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/coding-tasks/tasks")
}

/// A fresh scratch directory named after `tag`.
fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("coding-tasks-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Copy the tree at `from` over `to`, keeping each file's mode.
fn copy_over(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("dir");
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("entry");
        let (path, target) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().expect("type").is_dir() {
            copy_over(&path, &target);
        } else {
            std::fs::copy(&path, &target).expect("copy");
        }
    }
}

/// Every file under `dir`, by relative path, with its contents.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let relative = path.strip_prefix(base).expect("under base").to_owned();
                out.insert(relative, std::fs::read(&path).expect("read"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// Run `task`'s checker on `workspace`: its exit code and what it printed.
fn check(task: &Path, workspace: &Path, tmp: &Path) -> (Option<i32>, String) {
    let out = Command::new(task.join("check.sh"))
        .arg(workspace)
        .env("TMPDIR", tmp)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("run {}/check.sh: {e}", task.display()));
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code(), log)
}

fn tasks() -> Vec<PathBuf> {
    let mut tasks: Vec<PathBuf> = std::fs::read_dir(root())
        .expect("the coding tasks")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.is_dir())
        .collect();
    tasks.sort();
    tasks
}

fn require_tools() {
    for tool in ["python3", "bash", "timeout", "tar", "diff", "cmp", "mktemp"] {
        let found = Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        assert!(
            found,
            "the coding tasks' checkers need {tool}, which is not installed"
        );
    }
}

#[test]
fn every_seed_fails_its_checker_and_every_solution_passes() {
    require_tools();
    let tasks = tasks();
    assert_eq!(tasks.len(), TASKS, "{tasks:?}");
    let work = scratch("contract");
    let tmp = work.join("tmp");
    std::fs::create_dir_all(&tmp).expect("tmp");
    for task in &tasks {
        let name = task.file_name().expect("name").to_string_lossy();
        let prompt = std::fs::read_to_string(task.join("prompt.txt")).unwrap_or_default();
        assert!(!prompt.trim().is_empty(), "{name}: no prompt");
        let seed = task.join("seed");
        // One task checks everything in its checker; the rest hide tests.
        assert!(
            task.join("hidden").is_dir() || name == "backup-exclude-sh",
            "{name}: no hidden tests"
        );
        if task.join("hidden").is_dir() {
            let shared: Vec<_> = files(&task.join("hidden"))
                .into_keys()
                .filter(|f| seed.join(f).exists())
                .collect();
            assert!(
                shared.is_empty(),
                "{name}: hidden files in the seed: {shared:?}"
            );
        }

        let ws = work.join(&*name);
        copy_over(&seed, &ws);
        let seeded = files(&ws);
        let (code, log) = check(task, &ws, &tmp);
        assert_eq!(
            code,
            Some(1),
            "{name}: the seed must fail its checker with 1\n{log}"
        );
        assert!(seeded == files(&ws), "{name}: the checker changed the seed");

        copy_over(&task.join("solution"), &ws);
        let before = files(&ws);
        let (code, log) = check(task, &ws, &tmp);
        assert_eq!(code, Some(0), "{name}: the solution must pass\n{log}");
        assert!(
            before == files(&ws),
            "{name}: the checker changed the workspace"
        );
    }
    // Every checker cleans up after itself.
    assert_eq!(
        std::fs::read_dir(&tmp).expect("tmp").count(),
        0,
        "a checker left files in its TMPDIR"
    );
    std::fs::remove_dir_all(&work).expect("clean up");
}

#[test]
fn a_workspace_holding_a_link_or_a_startup_hook_fails() {
    require_tools();
    let task = root().join("invoice-rounding");
    let work = scratch("hooks");
    let (ws, tmp) = (work.join("ws"), work.join("tmp"));
    std::fs::create_dir_all(&tmp).expect("tmp");
    copy_over(&task.join("seed"), &ws);
    copy_over(&task.join("solution"), &ws);
    // The positive control: solved, it passes.
    assert_eq!(check(&task, &ws, &tmp).0, Some(0));

    std::os::unix::fs::symlink("/etc/hostname", ws.join("hidden_test.py")).expect("link");
    let (code, log) = check(&task, &ws, &tmp);
    assert_eq!(code, Some(1), "a link: {log}");
    assert!(log.contains("a link or a special file"), "{log}");
    std::fs::remove_file(ws.join("hidden_test.py")).expect("unlink");

    std::fs::write(ws.join("sitecustomize.py"), "import sys\n").expect("hook");
    let (code, log) = check(&task, &ws, &tmp);
    assert_eq!(code, Some(1), "a startup hook: {log}");
    assert!(log.contains("Python startup hook"), "{log}");
    std::fs::remove_dir_all(&work).expect("clean up");
}
