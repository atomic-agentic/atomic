// SPDX-License-Identifier: Apache-2.0

//! Synthetic repositories for the git and worktree tests, built here with
//! plain git and none of the person's own config.

#![allow(dead_code, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Plain git for building fixtures: no hardening, none of the person's own
/// config, fixed dates.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(atomic_core::git::Git::find().unwrap().program())
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
        .env("GIT_AUTHOR_DATE", "2026-11-01T10:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-11-01T10:00:00Z")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// A fresh directory for one test, removed when dropped.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("atomic-git-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self(std::fs::canonicalize(&root).unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

pub fn executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    write(path, text);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A project repository at `<root>/project` with two commits on `main`:
/// `a.txt` holds `one\ntwo\n`, then `one\nTWO\n`. Returns the project and
/// the two commits.
pub fn project(root: &Path) -> (PathBuf, String, String) {
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    git(&project, &["init", "-q"]);
    write(&project.join("a.txt"), "one\ntwo\n");
    write(&project.join(".gitignore"), "ignored/\n");
    git(&project, &["add", "a.txt", ".gitignore"]);
    git(&project, &["commit", "-q", "-m", "first"]);
    let first = git(&project, &["rev-parse", "HEAD"]).trim().to_owned();
    write(&project.join("a.txt"), "one\nTWO\n");
    git(&project, &["commit", "-q", "-am", "second"]);
    let second = git(&project, &["rev-parse", "HEAD"]).trim().to_owned();
    (project, first, second)
}

/// The paths in commit `sha`'s tree, sorted.
pub fn tree_paths(project: &Path, sha: &str) -> Vec<String> {
    let mut paths: Vec<String> = git(project, &["ls-tree", "-r", "--name-only", "-z", sha])
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    paths.sort();
    paths
}

/// File `path` as it is in commit `sha`.
pub fn show(project: &Path, sha: &str, path: &str) -> String {
    git(project, &["show", &format!("{sha}:{path}")])
}
