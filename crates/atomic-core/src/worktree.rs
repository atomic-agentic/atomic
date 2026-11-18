// SPDX-License-Identifier: Apache-2.0

//! A git worktree per code-changing step, kept in the state directory.
//!
//! A crash during an atom re-runs its step, and a re-run on a half-edited
//! tree would build on whatever the crash left. So a step that changes code
//! runs in its own worktree of the project's repository, and every attempt:
//!
//! 1. **resets** the worktree to the step's base commit, by removing it and
//!    checking it out again, so nothing an earlier attempt wrote survives;
//! 2. **runs** the atom with the worktree as its working directory;
//! 3. **commits**, after the turn has ended and outside any sandbox: atomic
//!    stages the whole worktree, writes a commit whose only parent is the
//!    base, and keeps it alive with a ref of its own,
//!    `refs/atomic/<run>/steps/<step>/<attempt>`;
//! 4. **returns** the commit's ID, which later steps build on.
//!
//! Re-running a step is therefore idempotent: it starts from the base again
//! and yields a new commit. An attempt that committed and then crashed before
//! its step's checkpoint leaves an orphan commit and ref; the invariant is
//! one *accepted* commit per completed step, not one commit ever made, and
//! [`Worktrees::cleanup`] removes the rest.
//!
//! Worktrees live under `<state>/worktrees/`, outside the repository, so
//! editors and build tools in the project never see them. A worktree an atom
//! wrote to is untrusted: every git command on it goes through
//! [`crate::git`], with the git directory given explicitly, so nothing the
//! atom planted in the worktree is ever read as git's own.
//!
//! A mutable ref given as a run's base (a branch name) is resolved to a
//! commit once, at admission, and pinned under `refs/atomic/<run>/base`
//! ([`Worktrees::pin_base`]); later movement of the branch changes nothing.
//!
//! Everything here blocks on git; async callers use
//! [`Worktrees::run_step`], which moves the git work off the async workers.

use crate::git::{Git, GitError, Repo, Signature, is_sha};
use crate::state::ProjectState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Why a worktree could not be prepared, committed or cleaned up.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    /// git failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// A file operation failed.
    #[error("{what} {path}: {source}")]
    Io {
        /// What was being done.
        what: &'static str,
        /// The path.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// Something was not as atomic left it.
    #[error("{0}")]
    Invalid(String),
}

type Result<T> = std::result::Result<T, WorktreeError>;

fn io(what: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> WorktreeError {
    let path = path.to_owned();
    move |source| WorktreeError::Io { what, path, source }
}

/// The worktrees of one project's repository.
#[derive(Clone, Debug)]
pub struct Worktrees {
    /// The project's repository, by its common git directory.
    repo: Repo,
    /// `<state>/worktrees`.
    dir: PathBuf,
}

/// One step's worktree, checked out at its base and ready for the atom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    /// The run.
    pub run: String,
    /// The step.
    pub step: String,
    /// The commit it was checked out at.
    pub base: String,
    /// The worktree: the atom's working directory.
    pub path: PathBuf,
    /// Its git directory, inside the repository and outside the worktree.
    git_dir: PathBuf,
}

/// The commit a step's attempt made. Runtime-owned: it is what atomic
/// recorded, never what a model said.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepCommit {
    /// The base commit; the commit's only parent.
    pub base: String,
    /// The commit.
    pub commit: String,
    /// The ref that keeps it alive.
    pub reference: String,
    /// Which attempt at the step made it, from 1.
    pub attempt: u32,
}

impl StepCommit {
    /// Record this commit in a step's run metadata, under `commit` (the
    /// commit's ID), `base` and `commit_ref`, replacing anything already
    /// there: a model's output can never stand in for it.
    pub fn record(&self, metadata: &mut Value) {
        if !metadata.is_object() {
            *metadata = Value::Object(serde_json::Map::new());
        }
        if let Some(m) = metadata.as_object_mut() {
            m.insert("commit".into(), Value::String(self.commit.clone()));
            m.insert("base".into(), Value::String(self.base.clone()));
            m.insert("commit_ref".into(), Value::String(self.reference.clone()));
        }
    }
}

/// What one step asks of its worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepRequest {
    /// The run (workflow id).
    pub run: String,
    /// The step, unique within the run.
    pub step: String,
    /// The commit the step starts from: a whole commit ID.
    pub base: String,
    /// The commit message.
    pub message: String,
    /// Who the commit is by.
    pub signature: Signature,
}

/// A step's failure: its worktree's, or its own body's.
#[derive(Debug, thiserror::Error)]
pub enum StepError<E> {
    /// Preparing or committing the worktree failed.
    #[error(transparent)]
    Worktree(#[from] WorktreeError),
    /// The step's body failed; nothing was committed.
    #[error("{0}")]
    Body(E),
}

/// What [`Worktrees::cleanup`] removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cleanup {
    /// Refs of attempts no accepted candidate names.
    pub refs: Vec<String>,
    /// Worktree directories.
    pub worktrees: Vec<PathBuf>,
}

impl Worktrees {
    /// The worktrees of the repository the project at `state` is in, kept
    /// under `state`'s directory.
    pub fn open(state: &ProjectState) -> Result<Self> {
        let git = Git::find()?;
        let repo = git.discover(state.root())?;
        Ok(Self::new(repo, state.dir().join("worktrees")))
    }

    /// The worktrees of `repo` (its common git directory, as
    /// [`Git::discover`] finds it), kept under `dir`.
    pub fn new(repo: Repo, dir: PathBuf) -> Self {
        Self { repo, dir }
    }

    /// The project's repository.
    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// Where the worktrees are kept.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Pin run `run`'s base: resolve `rev` (a commit ID or a ref name such
    /// as a branch) to a commit and record it under `refs/atomic/<run>/base`.
    /// Once pinned, the pinned commit is returned whatever `rev` now names,
    /// so admission can be retried and a branch can move without changing
    /// the run.
    pub fn pin_base(&self, run: &str, rev: &str) -> Result<String> {
        let name = format!("refs/atomic/{}/base", component(run));
        if let Some(sha) = self.repo.ref_target(&name)? {
            return Ok(sha);
        }
        let sha = self.repo.resolve_commit(rev)?;
        match self.repo.create_ref(&name, &sha) {
            Ok(()) => Ok(sha),
            // Pinned meanwhile by another admission: that one stands.
            Err(e) => self.repo.ref_target(&name)?.ok_or(WorktreeError::Git(e)),
        }
    }

    /// The commit run `run`'s base is pinned to, if it is.
    pub fn base(&self, run: &str) -> Result<Option<String>> {
        let name = format!("refs/atomic/{}/base", component(run));
        Ok(self.repo.ref_target(&name)?)
    }

    /// The worktree of step `step` in run `run`: its path, whether or not it
    /// exists.
    pub fn path(&self, run: &str, step: &str) -> PathBuf {
        self.dir.join(worktree_name(run, step))
    }

    /// Reset step `step`'s worktree to `base`: remove whatever is there and
    /// check `base` out afresh, detached. Returns the worktree, ready for the
    /// atom.
    pub fn prepare(&self, run: &str, step: &str, base: &str) -> Result<Worktree> {
        if !is_sha(base) {
            return Err(WorktreeError::Invalid(format!(
                "a step's base must be a whole commit ID, not {base:?}"
            )));
        }
        if self.repo.resolve_commit(base)? != base {
            return Err(WorktreeError::Invalid(format!("{base} is not a commit")));
        }
        std::fs::create_dir_all(&self.dir).map_err(io("create", &self.dir))?;
        let dir = std::fs::canonicalize(&self.dir).map_err(io("resolve", &self.dir))?;
        let name = worktree_name(run, step);
        let path = dir.join(&name);
        self.remove(&path, &name)?;
        let target = path
            .to_str()
            .ok_or_else(|| WorktreeError::Invalid(format!("{} is not UTF-8", path.display())))?;
        self.repo.run(
            &["worktree", "add", "--detach", "--force", target, base],
            None,
        )?;
        let git_dir = self.admin_dir(&name);
        self.check_admin(&git_dir, &path)?;
        Ok(Worktree {
            run: run.to_owned(),
            step: step.to_owned(),
            base: base.to_owned(),
            path,
            git_dir,
        })
    }

    /// Commit everything in `worktree` on top of its base, after the atom's
    /// turn has ended, and keep the commit with a new attempt ref.
    pub fn commit(
        &self,
        worktree: &Worktree,
        message: &str,
        signature: &Signature,
    ) -> Result<StepCommit> {
        // The git directory is the one atomic created, outside the worktree,
        // and still belongs to it; nothing in the worktree is consulted.
        self.check_admin(&worktree.git_dir, &worktree.path)?;
        let tree = self.repo.linked(&worktree.git_dir, &worktree.path);
        let commit = tree.commit_all(&worktree.base, message, signature)?;
        let prefix = format!(
            "refs/atomic/{}/steps/{}/",
            component(&worktree.run),
            component(&worktree.step)
        );
        // The next attempt number; a concurrent writer that takes it makes
        // the create fail, and the next number is tried.
        for _ in 0..8 {
            let attempt = self
                .repo
                .refs(&prefix)?
                .iter()
                .filter_map(|r| r.name.strip_prefix(&prefix)?.parse::<u32>().ok())
                .max()
                .unwrap_or(0)
                + 1;
            let reference = format!("{prefix}{attempt}");
            if self.repo.create_ref(&reference, &commit).is_ok() {
                return Ok(StepCommit {
                    base: worktree.base.clone(),
                    commit,
                    reference,
                    attempt,
                });
            }
        }
        Err(WorktreeError::Invalid(format!(
            "could not record an attempt ref under {prefix}"
        )))
    }

    /// Run one attempt of a code-changing step: reset its worktree to the
    /// base, run `body` in it, and commit what it left. Call it from inside
    /// one durable step; a retry of that step runs it again from the base.
    /// Nothing is committed when `body` fails.
    pub async fn run_step<T, E, F, Fut>(
        self: &Arc<Self>,
        request: StepRequest,
        body: F,
    ) -> std::result::Result<(T, StepCommit), StepError<E>>
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: Future<Output = std::result::Result<T, E>>,
    {
        let this = self.clone();
        let (run, step, base) = (
            request.run.clone(),
            request.step.clone(),
            request.base.clone(),
        );
        let worktree = blocking(move || this.prepare(&run, &step, &base)).await?;
        let out = body(worktree.path.clone()).await.map_err(StepError::Body)?;
        let this = self.clone();
        let commit =
            blocking(move || this.commit(&worktree, &request.message, &request.signature)).await?;
        Ok((out, commit))
    }

    /// Remove run `run`'s worktrees, and every attempt ref of its steps that
    /// names none of `accepted`, the commits of its accepted candidates. The
    /// run's pinned base stays. Commits no ref names are left for git's own
    /// garbage collection.
    pub fn cleanup(&self, run: &str, accepted: &[String]) -> Result<Cleanup> {
        let mut report = Cleanup::default();
        let prefix = format!("refs/atomic/{}/steps/", component(run));
        for r in self.repo.refs(&prefix)? {
            if !accepted.contains(&r.sha) {
                self.repo.delete_ref(&r.name, &r.sha)?;
                report.refs.push(r.name);
            }
        }
        let start = format!("{}.", component(run));
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
            Err(e) => return Err(io("read", &self.dir)(e)),
        };
        let dir = std::fs::canonicalize(&self.dir).map_err(io("resolve", &self.dir))?;
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with(&start))
            .collect();
        names.sort();
        for name in names {
            let path = dir.join(&name);
            self.remove(&path, &name)?;
            report.worktrees.push(path);
        }
        Ok(report)
    }

    /// The repository's record of worktree `name`.
    fn admin_dir(&self, name: &str) -> PathBuf {
        self.repo.git_dir().join("worktrees").join(name)
    }

    /// Check that `git_dir` is the repository's record of a worktree at
    /// `path`. Both files read here are in the repository, not the worktree.
    fn check_admin(&self, git_dir: &Path, path: &Path) -> Result<()> {
        let gitdir = git_dir.join("gitdir");
        let recorded = std::fs::read_to_string(&gitdir).map_err(io("read", &gitdir))?;
        if Path::new(recorded.trim_end_matches('\n')) != path.join(".git") {
            return Err(WorktreeError::Invalid(format!(
                "{} records a worktree at {}, not {}",
                git_dir.display(),
                recorded.trim_end(),
                path.display()
            )));
        }
        Ok(())
    }

    /// Remove the worktree at `path` named `name` and the repository's
    /// record of it, whatever the atom left there.
    fn remove(&self, path: &Path, name: &str) -> Result<()> {
        remove_tree(path)?;
        let admin = self.admin_dir(name);
        if admin.exists() {
            // Only a record of this very worktree is removed.
            self.check_admin(&admin, path)?;
            remove_tree(&admin)?;
        }
        Ok(())
    }
}

/// Build and package caches shared by every worktree of a project, kept
/// outside all of them under `<state>/cache`, so a fresh worktree does not
/// build cold and resetting a worktree never discards them.
///
/// An atom's commands find them through [`env`](Self::env), and a
/// `workspace-write` thread is given [`dir`](Self::dir) as a writable root
/// (`TurnSpec::env` and `TurnSpec::writable_roots` in [`crate::codex`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caches {
    dir: PathBuf,
}

impl Caches {
    /// The caches of the project at `state`.
    pub fn for_state(state: &ProjectState) -> Self {
        Self::new(state.dir().join("cache"))
    }

    /// Caches kept under `dir`.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Where they are.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Create the cache directory.
    pub fn create(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir).map_err(io("create", &self.dir))
    }

    /// The environment that points common build tools at the caches: the
    /// XDG cache home, Cargo's target directory, and npm's, pip's and Go's
    /// caches, plus `ATOMIC_CACHE_DIR` for anything else.
    pub fn env(&self) -> std::collections::BTreeMap<String, String> {
        let at = |sub: &str| self.dir.join(sub).to_string_lossy().into_owned();
        [
            ("ATOMIC_CACHE_DIR", self.dir.to_string_lossy().into_owned()),
            ("XDG_CACHE_HOME", at("xdg")),
            ("CARGO_TARGET_DIR", at("cargo-target")),
            ("npm_config_cache", at("npm")),
            ("PIP_CACHE_DIR", at("pip")),
            ("GOCACHE", at("go-build")),
            ("GOMODCACHE", at("go-mod")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
    }
}

/// Run blocking git work off the async workers.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| WorktreeError::Invalid(format!("the git task failed: {e}")))?
}

/// Remove `path`, a directory or anything else, without following links.
/// Directories an atom made read-only are made writable first.
fn remove_tree(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io("inspect", path)(e)),
    };
    if !meta.is_dir() {
        return std::fs::remove_file(path).map_err(io("remove", path));
    }
    if std::fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    make_writable(path);
    std::fs::remove_dir_all(path).map_err(io("remove", path))
}

/// Make every directory under `path` writable by its owner, never following
/// a link.
fn make_writable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        if !meta.is_dir() {
            return;
        }
        let mode = meta.permissions().mode() | 0o700;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                make_writable(&entry.path());
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// A run or step name as one part of a ref and a file name: kept as it is
/// when it is plain (an ASCII letter or digit, then letters, digits, `_` and
/// `-`, up to 64 in all), otherwise `_`, a readable prefix and a hash of the
/// whole name, which no plain name can equal.
fn component(name: &str) -> String {
    let plain = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if plain {
        return name.to_owned();
    }
    let readable: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    let hash = Sha256::digest(name.as_bytes());
    format!("_{readable}-{}", &format!("{hash:x}")[..12])
}

/// A step's worktree's directory name, and git's name for it: the run and
/// the step, joined by a `.` neither can hold.
fn worktree_name(run: &str, step: &str) -> String {
    format!("{}.{}", component(run), component(step))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_are_kept_and_others_are_hashed() {
        assert_eq!(component("cr-1"), "cr-1");
        assert_eq!(component("implement_2"), "implement_2");
        for odd in [
            "",
            "a.b",
            "a/b",
            "..",
            "-x",
            "_x",
            "implement[3]",
            "with space",
            "é",
            &"a".repeat(65),
        ] {
            let c = component(odd);
            assert!(c.starts_with('_'), "{odd:?} -> {c}");
            assert!(crate::git::is_ref_name(&c), "{odd:?} -> {c}");
            assert!(!c.contains('.'), "{odd:?} -> {c}");
        }
        assert_ne!(component("a/b"), component("a_b"));
        assert_ne!(component("a.b"), component("a/b"));
        assert_eq!(worktree_name("r", "s"), "r.s");
    }

    #[test]
    fn a_recorded_commit_replaces_what_the_model_said() {
        let commit = StepCommit {
            base: "1".repeat(40),
            commit: "2".repeat(40),
            reference: "refs/atomic/r/steps/s/1".into(),
            attempt: 1,
        };
        let mut metadata = serde_json::json!({"commit": "a model's claim", "thread_id": "t"});
        commit.record(&mut metadata);
        assert_eq!(metadata["commit"], "2".repeat(40));
        assert_eq!(metadata["base"], "1".repeat(40));
        assert_eq!(metadata["thread_id"], "t");
        let mut empty = Value::Null;
        commit.record(&mut empty);
        assert_eq!(empty["commit_ref"], "refs/atomic/r/steps/s/1");
    }
}
