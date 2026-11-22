// SPDX-License-Identifier: Apache-2.0

//! What git records about a repository, for a test that says a run left it
//! alone. The suites that run a coding step share this one reading of a
//! repository, so the decoys planted in `run_cli`'s fixture test, which show
//! that each part of the reading notices a change, speak for every comparison
//! of a repository made anywhere.

use std::path::Path;
use std::process::Command;

/// git in `dir`, as a person runs it: with an identity of its own, reading
/// nothing from their configuration or from the system's.
pub fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=dev",
            "-c",
            "user.email=dev@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// What git records about the repository at `dir` that a run must not change:
/// its refs, its worktrees, its working tree with every untracked file, its own
/// configuration (`.git/config`, where a remote, a hook path or a filter driver
/// would appear) and the counts and sizes of its objects. A test compares two
/// of these, before a run and after it, to say the repository was left alone.
/// Leaving any one of the five readings out is a control of its own under
/// `scripts/controls/`; each is shown to fail a test by a decoy planted in
/// `run_cli`'s fixture test, which compares this same reading.
pub fn git_state(dir: &Path) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}",
        git_in(dir, &["for-each-ref"]),
        git_in(dir, &["worktree", "list", "--porcelain"]),
        git_in(dir, &["status", "--porcelain", "--untracked-files=all"]),
        git_in(dir, &["config", "--list", "--local"]),
        git_in(dir, &["count-objects", "-v"]),
    )
}
