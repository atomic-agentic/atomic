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
//! - every wrong or partial fix kept beside a task fails its checker, for the
//!   reason it was written to show;
//! - a workspace holding a link, a Python startup hook, a name the hidden
//!   tests use or a module named like one of the standard library's fails,
//!   solved or not, and nothing in the caller's environment reaches the
//!   attempt's code; a test file the attempt adds, or a virtual environment,
//!   does not fail a solution.
//!
//! The checkers need `python3`, `bash` and GNU coreutils' `timeout`; a missing
//! one fails the test. They run on Linux only, as the development container
//! and CI's Linux runner do.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How many tasks there are.
const TASKS: usize = 14;

/// The longest a checker may take before the test gives up on it.
const LIMIT: Duration = Duration::from_secs(300);

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/coding-tasks/tasks")
}

/// A fresh scratch directory named after `tag`.
fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("coding-tasks-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["tmp", "home", "logs"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
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

/// Run `task`'s checker on `workspace` from `work`, in an environment of
/// its own with `extra` added: its exit code and what it printed. Its output
/// goes to a file, so a process it leaves behind cannot hold the test up,
/// and it is killed past [`LIMIT`].
fn check_with(
    task: &Path,
    workspace: &Path,
    work: &Path,
    extra: &[(&str, &str)],
) -> (Option<i32>, String) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let log = work.join("logs").join(format!("check-{n}.log"));
    let out = std::fs::File::create(&log).expect("log");
    let mut child = Command::new(task.join("check.sh"))
        .arg(workspace)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", work.join("home"))
        .env("TMPDIR", work.join("tmp"))
        .envs(extra.iter().copied())
        .stdin(Stdio::null())
        .stdout(out.try_clone().expect("log"))
        .stderr(out)
        .spawn()
        .unwrap_or_else(|e| panic!("run {}/check.sh: {e}", task.display()));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if started.elapsed() > LIMIT {
            let _ = child.kill();
            panic!("{}/check.sh ran past {LIMIT:?}", task.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (
        status.code(),
        std::fs::read_to_string(&log).unwrap_or_default(),
    )
}

fn check(task: &Path, workspace: &Path, work: &Path) -> (Option<i32>, String) {
    check_with(task, workspace, work, &[])
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
    for tool in ["python3", "bash", "timeout", "tar", "cmp", "mktemp", "env"] {
        let found = Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
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

        let ws = work.join("ws").join(&*name);
        copy_over(&seed, &ws);
        let seeded = files(&ws);
        let (code, log) = check(task, &ws, &work);
        assert_eq!(
            code,
            Some(1),
            "{name}: the seed must fail its checker with 1\n{log}"
        );
        assert!(seeded == files(&ws), "{name}: the checker changed the seed");

        copy_over(&task.join("solution"), &ws);
        let before = files(&ws);
        let (code, log) = check(task, &ws, &work);
        assert_eq!(code, Some(0), "{name}: the solution must pass\n{log}");
        assert!(
            before == files(&ws),
            "{name}: the checker changed the workspace"
        );
    }
    // Every checker cleans up after itself.
    assert_eq!(
        std::fs::read_dir(work.join("tmp")).expect("tmp").count(),
        0,
        "a checker left files in its TMPDIR"
    );
    std::fs::remove_dir_all(&work).expect("clean up");
}

#[test]
fn every_wrong_fix_fails_for_its_reason() {
    require_tools();
    let work = scratch("wrong");
    let mut wrong = 0;
    for task in tasks() {
        let Ok(entries) = std::fs::read_dir(task.join("wrong")) else {
            continue;
        };
        let mut fixes: Vec<PathBuf> = entries
            .map(|e| e.expect("entry").path())
            .filter(|p| p.is_dir())
            .collect();
        fixes.sort();
        for fix in fixes {
            let what = format!(
                "{}/{}",
                task.file_name().expect("task").to_string_lossy(),
                fix.file_name().expect("fix").to_string_lossy()
            );
            let expect = std::fs::read_to_string(fix.with_extension("expect"))
                .unwrap_or_else(|e| panic!("{what}: its .expect file: {e}"));
            let ws = work.join("ws").join(what.replace('/', "-"));
            copy_over(&task.join("seed"), &ws);
            copy_over(&fix, &ws);
            let (code, log) = check(&task, &ws, &work);
            assert_eq!(code, Some(1), "{what}: a wrong fix must fail\n{log}");
            assert!(
                log.contains(expect.trim()),
                "{what}: not for its reason\n{log}"
            );
            wrong += 1;
        }
    }
    assert!(wrong >= 7, "{wrong} wrong fixes");
    std::fs::remove_dir_all(&work).expect("clean up");
}

/// A workspace of `task`, solved if `solved`, under `work`.
fn workspace(task: &Path, work: &Path, tag: &str, solved: bool) -> PathBuf {
    let ws = work.join("ws").join(tag);
    let _ = std::fs::remove_dir_all(&ws);
    copy_over(&task.join("seed"), &ws);
    if solved {
        copy_over(&task.join("solution"), &ws);
    }
    ws
}

#[test]
fn the_grading_copy_holds_nothing_that_could_stand_in_for_the_tests() {
    require_tools();
    let task = root().join("invoice-rounding");
    let work = scratch("boundary");
    // The positive control: solved, it passes.
    let ws = workspace(&task, &work, "solved", true);
    assert_eq!(check(&task, &ws, &work).0, Some(0));

    let refused = |tag: &str, solved: bool, plant: &dyn Fn(&Path), says: &str| {
        let ws = workspace(&task, &work, tag, solved);
        plant(&ws);
        let (code, log) = check(&task, &ws, &work);
        assert_eq!(code, Some(1), "{tag}: {log}");
        assert!(log.contains(says), "{tag}: {log}");
    };
    let target = work.join("elsewhere.py");
    std::fs::write(&target, "").expect("target");
    refused(
        "link",
        true,
        &|ws| std::os::unix::fs::symlink(&target, ws.join("hidden.py")).expect("link"),
        "a link or a special file",
    );
    refused(
        "hook",
        true,
        &|ws| std::fs::write(ws.join("sitecustomize.py"), "import sys\n").expect("hook"),
        "Python startup hook",
    );
    // A package that would be imported in place of the hidden tests, in a
    // workspace that does not solve the task.
    refused(
        "shadow-hidden",
        false,
        &|ws| {
            std::fs::create_dir_all(ws.join("hidden_test")).expect("dir");
            std::fs::write(ws.join("hidden_test/__init__.py"), "").expect("package");
        },
        "a name the hidden tests use",
    );
    // A module that would be imported in place of the test runner.
    refused(
        "shadow-runner",
        false,
        &|ws| std::fs::write(ws.join("unittest.py"), "import os\nos._exit(0)\n").expect("module"),
        "standard library module",
    );

    // Nothing in the caller's environment reaches the attempt: not a
    // `python3` earlier on the PATH that passes everything, nor a module
    // path or startup file.
    let shims = work.join("shims");
    std::fs::create_dir_all(&shims).expect("shims");
    let shim = shims.join("python3");
    std::fs::write(&shim, "#!/bin/sh\nexit 0\n").expect("shim");
    let mut permissions = std::fs::metadata(&shim).expect("shim").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&shim, permissions).expect("mode");
    std::fs::write(shims.join("unittest.py"), "import os\nos._exit(0)\n").expect("module");
    let path = format!("{}:/usr/local/bin:/usr/bin:/bin", shims.display());
    let shims_text = shims.display().to_string();
    let ws = workspace(&task, &work, "environment", false);
    let (code, log) = check_with(
        &task,
        &ws,
        &work,
        &[
            ("PATH", &path),
            ("PYTHONPATH", &shims_text),
            ("PYTHONSTARTUP", "/dev/null"),
        ],
    );
    assert_eq!(
        code,
        Some(1),
        "the caller's environment passed the seed: {log}"
    );

    // What a solution may hold: a test of its own, and a virtual
    // environment with its usual links.
    let task = root().join("config-cache-profile");
    let ws = workspace(&task, &work, "own-test", true);
    std::fs::write(
        ws.join("tests/test_more.py"),
        "import unittest\n\n\nclass More(unittest.TestCase):\n    def test_more(self):\n        self.assertTrue(True)\n",
    )
    .expect("a test of its own");
    std::fs::create_dir_all(ws.join(".venv/lib")).expect("venv");
    std::os::unix::fs::symlink("lib", ws.join(".venv/lib64")).expect("venv link");
    let (code, log) = check(&task, &ws, &work);
    assert_eq!(
        code,
        Some(0),
        "an added test or a .venv failed a solution: {log}"
    );
    std::fs::remove_dir_all(&work).expect("clean up");
}
