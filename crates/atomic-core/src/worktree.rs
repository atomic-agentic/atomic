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

/// Directories whose next creations fail as on a full disk, each so many
/// times, by path: for this crate's tests only.
#[cfg(test)]
pub(crate) static FULL_FOR_TESTS: std::sync::Mutex<Vec<(PathBuf, usize)>> =
    std::sync::Mutex::new(Vec::new());

/// A full disk at `path`, if a test asked for one there.
#[cfg(test)]
fn full_for_tests(path: &Path) -> Option<WorktreeError> {
    let mut full = FULL_FOR_TESTS.lock().ok()?;
    let (_, left) = full.iter_mut().find(|(p, left)| p == path && *left > 0)?;
    *left -= 1;
    Some(WorktreeError::Io {
        what: "create",
        path: path.to_owned(),
        source: std::io::Error::from(std::io::ErrorKind::StorageFull),
    })
}

impl WorktreeError {
    /// Whether trying again later may succeed, as when the disk or a quota
    /// is full: an I/O error of the kind [`atomic_exec::workspace::transient`]
    /// names, or git saying so (it runs in the C locale). A run waits for
    /// room on such an error rather than failing.
    pub fn may_pass(&self) -> bool {
        match self {
            WorktreeError::Io { source, .. } | WorktreeError::Git(GitError::Spawn(source)) => {
                atomic_exec::workspace::transient(source)
            }
            WorktreeError::Git(GitError::Failed { stderr, .. }) => {
                stderr.contains("No space left on device") || stderr.contains("Disk quota exceeded")
            }
            _ => false,
        }
    }
}

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
    /// Waiting for room to prepare the worktree, on a full disk, ended
    /// before there was any: the process waits no longer, or is stopping
    /// ([`crate::runtime::wait_for_room`]). Nothing ran.
    #[error("{0}")]
    Waited(crate::typed::AtomError),
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

    /// The build caches of run `run`'s worktree steps, beside the worktrees
    /// in the state directory: `cache/<run>-<nonce>`, named as the run's
    /// worktrees are and with the nonce its first step recorded, so a run
    /// id used again, after its store was wiped, say, or names that differ
    /// only in case, never finds another run's caches. Nothing one run
    /// writes there reaches another. `rustup` is the person's toolchains, as
    /// found when the run began.
    pub fn caches(
        &self,
        run: &str,
        nonce: Option<&str>,
        rustup: Option<PathBuf>,
    ) -> Result<Caches> {
        let state = self.dir.parent().ok_or_else(|| {
            WorktreeError::Invalid(format!(
                "the worktrees directory {} has no parent to keep caches in",
                self.dir.display()
            ))
        })?;
        Caches::for_run(&state.join("cache"), run, nonce, rustup)
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
        #[cfg(test)]
        if let Some(full) = full_for_tests(&self.dir) {
            return Err(full);
        }
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
        if crate::failpoint::armed() {
            crate::failpoint::failpoint(&format!("commit:before-ref:{}", worktree.step));
        }
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
        // On a full disk the step waits for room, as for the execution
        // backend's, rather than failing its run.
        let waiting = crate::runtime::Waiting {
            label: &request.step,
            run: &request.run,
            step: &request.step,
            what: "the disk",
            doing: "hold the step's worktree",
        };
        let worktree = crate::runtime::wait_for_room(waiting, move || {
            match this.prepare(&run, &step, &base) {
                Err(e) if e.may_pass() => Err(crate::runtime::NotYet::Room(format!(
                    "the step's worktree: {e}"
                ))),
                prepared => Ok(prepared),
            }
        })
        .await
        .map_err(StepError::Waited)??;
        if crate::failpoint::armed() {
            crate::failpoint::failpoint(&format!("worktree:after-prepare:{}", request.step));
        }
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
        // The unaccepted refs are gone and the worktrees are not.
        if crate::failpoint::armed() {
            crate::failpoint::failpoint(&format!("cleanup:after-refs:{run}"));
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

/// Build and package caches kept outside every worktree: a run's own, in
/// `<state>/cache/<run>-<nonce>` ([`Caches::for_run`]), for its worktree and
/// exec steps, removed when the run ends.
///
/// An atom's commands find them through [`env`](Self::env), and a
/// `workspace-write` thread is given [`dir`](Self::dir) as a writable root
/// (`TurnSpec::env` and `TurnSpec::writable_roots` in [`crate::codex`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caches {
    dir: PathBuf,
    /// The person's rustup toolchains, which commands may only read.
    rustup: Option<PathBuf>,
    /// Cargo's target directory, one worktree's own.
    target: Option<PathBuf>,
}

impl Caches {
    /// The build caches of run `run` under `root`, the state directory's
    /// `cache/`: `<root>/<run>-<nonce>`, named as the run's worktrees are and
    /// with the nonce its first step recorded, so a run id used again, after
    /// its store was wiped, say, or names that differ only in case, never
    /// finds another run's caches. Nothing one run writes there reaches
    /// another. `rustup` is the person's toolchains, as found when the run
    /// began.
    pub fn for_run(
        root: &Path,
        run: &str,
        nonce: Option<&str>,
        rustup: Option<PathBuf>,
    ) -> Result<Self> {
        let name = match nonce {
            Some(nonce) if !nonce.is_empty() && nonce.bytes().all(|b| b.is_ascii_hexdigit()) => {
                format!("{}-{nonce}", component(run))
            }
            Some(nonce) => {
                return Err(WorktreeError::Invalid(format!(
                    "the run's cache nonce {nonce:?} is not hexadecimal"
                )));
            }
            None => component(run),
        };
        Ok(Caches {
            dir: root.join(name),
            rustup,
            target: None,
        })
    }

    /// Caches kept under `dir`.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            rustup: None,
            target: None,
        }
    }

    /// These caches for the worktree at `tree`: the same caches, with a
    /// Cargo target directory of that worktree's own, so no worktree runs
    /// what another built.
    pub fn in_worktree(&self, tree: &Path) -> Self {
        let name = tree
            .file_name()
            .map_or_else(|| "worktree".into(), |n| n.to_string_lossy().into_owned());
        Self {
            target: Some(self.dir.join("cargo-target").join(name)),
            ..self.clone()
        }
    }

    /// These caches for exec step `name`: the same caches, with a Cargo
    /// target directory of that step's own, so no step runs what another
    /// built.
    /// `cargo-target/<name>`, which no worktree's can be: a worktree's is
    /// named `<run>.<step>`, and a name as a file name holds no `.`.
    pub fn for_exec(&self, name: &str) -> Self {
        Self {
            target: Some(self.dir.join("cargo-target").join(component(name))),
            ..self.clone()
        }
    }

    /// The person's rustup toolchains, as found when the caches were made.
    pub fn rustup(&self) -> Option<&Path> {
        self.rustup.as_deref()
    }

    /// Where they are.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Create the cache directory, readable by its owner only. A link in
    /// its place is refused: atomic did not make it, and following it
    /// would put the run's caches wherever it points.
    pub fn create(&self) -> Result<()> {
        #[cfg(test)]
        if let Some(full) = full_for_tests(&self.dir) {
            return Err(full);
        }
        if std::fs::symlink_metadata(&self.dir).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(WorktreeError::Invalid(format!(
                "{} is a link, not the run's cache directory; atomic did not make it",
                self.dir.display()
            )));
        }
        crate::state::private_dir(&self.dir).map_err(|e| match e {
            // As it failed, so a full disk reads as one (see
            // [`WorktreeError::may_pass`]).
            crate::state::StateError::Create { source, .. } => WorktreeError::Io {
                what: "create",
                path: self.dir.clone(),
                source,
            },
            e => WorktreeError::Invalid(e.to_string()),
        })
    }

    /// [`create`](Self::create), as [`crate::runtime::wait_for_room`] reads
    /// it: a full disk or quota is a reason to wait, anything else is for
    /// good.
    pub fn create_for_room(&self) -> std::result::Result<(), crate::runtime::NotYet> {
        let Err(e) = self.create() else {
            return Ok(());
        };
        let why = format!("the run's build caches: {e}");
        if e.may_pass() {
            Err(crate::runtime::NotYet::Room(why))
        } else {
            Err(crate::runtime::NotYet::Fail(crate::AtomError::Runtime(why)))
        }
    }

    /// [`create`](Self::create) for step `step` of run `run`, whose atom is
    /// `label`, waiting for room on a full disk as
    /// [`crate::runtime::wait_for_room`] does.
    pub async fn create_waiting(
        &self,
        label: &str,
        run: &str,
        step: &str,
    ) -> std::result::Result<(), crate::AtomError> {
        let caches = self.clone();
        crate::runtime::wait_for_room(
            crate::runtime::Waiting {
                label,
                run,
                step,
                what: "the disk",
                doing: "hold the run's build caches",
            },
            move || caches.create_for_room(),
        )
        .await
    }

    /// Remove the cache directory and everything in it; a repeat finds
    /// nothing to remove.
    /// Directories a tool made read-only, as Go's module cache does, are
    /// made writable first; a link is removed, never followed.
    pub fn remove(&self) -> Result<()> {
        remove_tree(&self.dir)
    }

    /// The environment that points common build tools at the caches: the
    /// XDG cache home, Cargo's target directory, and npm's, pip's and Go's
    /// caches, plus `ATOMIC_CACHE_DIR` for anything else, and `RUSTUP_HOME`
    /// at the person's toolchains when they were found. Cargo's home is the
    /// command's own `HOME`'s, never shared.
    pub fn env(&self) -> std::collections::BTreeMap<String, String> {
        let at = |sub: &str| self.dir.join(sub).to_string_lossy().into_owned();
        [
            ("ATOMIC_CACHE_DIR", self.dir.to_string_lossy().into_owned()),
            ("XDG_CACHE_HOME", at("xdg")),
            (
                "CARGO_TARGET_DIR",
                self.target
                    .as_ref()
                    .map_or_else(|| at("cargo-target"), |t| t.to_string_lossy().into_owned()),
            ),
            ("npm_config_cache", at("npm")),
            ("PIP_CACHE_DIR", at("pip")),
            ("GOCACHE", at("go-build")),
            ("GOMODCACHE", at("go-mod")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .chain(
            self.rustup
                .iter()
                .map(|r| ("RUSTUP_HOME".to_owned(), r.to_string_lossy().into_owned())),
        )
        .collect()
    }
}

/// Where rustup keeps its toolchains, as `env` reads the variables: its
/// `RUSTUP_HOME`, or `.rustup` in the home directory, when that exists. A
/// command's own `HOME` is not the person's, so without this the rustup
/// proxies on `PATH` would find no toolchain. Toolchains hold no secrets,
/// and commands may only read them.
pub fn rustup_home(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let absolute = |name: &str| env(name).map(PathBuf::from).filter(|p| p.is_absolute());
    absolute("RUSTUP_HOME")
        .or_else(|| absolute("HOME").map(|h| h.join(".rustup")))
        .filter(|p| p.is_dir())
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

    #[test]
    fn rustup_toolchains_are_found_where_the_person_keeps_them() {
        let dir = std::env::temp_dir().join(format!("atomic-rustup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home/.rustup")).expect("rustup home");
        std::fs::create_dir_all(dir.join("elsewhere")).expect("elsewhere");
        let home = dir.join("home").display().to_string();
        let elsewhere = dir.join("elsewhere").display().to_string();
        let env = |vars: Vec<(&'static str, String)>| {
            move |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.clone())
            }
        };
        assert_eq!(
            rustup_home(&env(vec![("HOME", home.clone())])),
            Some(dir.join("home/.rustup"))
        );
        assert_eq!(
            rustup_home(&env(vec![("HOME", home), ("RUSTUP_HOME", elsewhere)])),
            Some(dir.join("elsewhere"))
        );
        assert_eq!(
            rustup_home(&env(vec![("HOME", "/nonexistent".into())])),
            None,
            "nothing that is not there"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_run_has_caches_of_its_own_and_each_worktree_a_target_of_its_own() {
        let git = crate::git::Git::find().expect("git");
        let worktrees = Worktrees::new(git.repo("/state/repo.git"), "/state/worktrees".into());
        let caches = worktrees
            .caches("run-1", Some("ab12"), Some("/toolchains".into()))
            .expect("caches");
        assert_eq!(caches.dir(), Path::new("/state/cache/run-1-ab12"));
        // Another run, the same id started again, and ids that differ in
        // case only: each its own.
        let dir = |run: &str, nonce: &str| {
            worktrees
                .caches(run, Some(nonce), None)
                .expect("caches")
                .dir()
                .to_owned()
        };
        assert_ne!(dir("run-1", "ab12"), dir("run-2", "ab12"));
        assert_ne!(dir("run-1", "ab12"), dir("run-1", "cd34"));
        assert_ne!(
            dir("Run-A", "ab12").to_string_lossy().to_lowercase(),
            dir("run-a", "cd34").to_string_lossy().to_lowercase()
        );
        // Any id a run's worktrees take, named as they are, inside the
        // state's caches.
        for run in ["feature/x", "..", "../x", &"r".repeat(300)] {
            let at = dir(run, "ab12");
            assert_eq!(at.parent(), Some(Path::new("/state/cache")), "{run:?}");
            assert!(at.file_name().is_some_and(|n| n.len() < 100), "{run:?}");
        }
        assert!(worktrees.caches("run-1", Some("../x"), None).is_err());
        let orphan = Worktrees::new(git.repo("/state/repo.git"), "/".into());
        assert!(
            orphan.caches("run-1", None, None).is_err(),
            "never in the worktrees"
        );
        let env = caches.env();
        assert!(
            !env.contains_key("CARGO_HOME"),
            "the attempt's own: {env:?}"
        );
        assert_eq!(env["RUSTUP_HOME"], "/toolchains");
        let (one, two) = (
            caches
                .in_worktree(Path::new("/state/worktrees/run-1-a"))
                .env(),
            caches
                .in_worktree(Path::new("/state/worktrees/run-1-b"))
                .env(),
        );
        assert_ne!(one["CARGO_TARGET_DIR"], two["CARGO_TARGET_DIR"]);
        for (name, value) in &one {
            if name != "RUSTUP_HOME" {
                assert!(Path::new(value).starts_with(caches.dir()), "{name}={value}");
            }
        }
        // Each exec step builds into a target of its own, never a
        // worktree's, and shares the rest of the run's caches.
        let tree = caches
            .in_worktree(&Path::new("/state/worktrees").join(worktree_name("run-1", "a")))
            .env();
        let targets: Vec<String> = ["a", "b", "a.b", "_a"]
            .iter()
            .map(|turn| caches.for_exec(turn).env()["CARGO_TARGET_DIR"].clone())
            .chain([
                tree["CARGO_TARGET_DIR"].clone(),
                env["CARGO_TARGET_DIR"].clone(),
            ])
            .collect();
        let mut distinct = targets.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), targets.len(), "{targets:?}");
        let exec = caches.for_exec("a").env();
        assert!(
            Path::new(&exec["CARGO_TARGET_DIR"]).starts_with(caches.dir().join("cargo-target"))
        );
        for name in [
            "ATOMIC_CACHE_DIR",
            "XDG_CACHE_HOME",
            "GOCACHE",
            "RUSTUP_HOME",
        ] {
            assert_eq!(exec[name], env[name], "{name}");
        }
    }

    /// A full disk or quota may pass, in a file operation or as git says
    /// it; anything else is for good. A cache directory that cannot be made
    /// says why as it failed, so a full disk reads as one.
    #[test]
    fn only_a_full_disk_or_quota_may_pass() {
        use std::io::{Error, ErrorKind};
        let io = |kind| WorktreeError::Io {
            what: "create",
            path: "/state/cache".into(),
            source: Error::from(kind),
        };
        assert!(io(ErrorKind::StorageFull).may_pass());
        assert!(io(ErrorKind::QuotaExceeded).may_pass());
        assert!(!io(ErrorKind::PermissionDenied).may_pass());
        assert!(!io(ErrorKind::NotADirectory).may_pass());
        let git = |stderr: &str| {
            WorktreeError::Git(GitError::Failed {
                what: "worktree".into(),
                code: Some(128),
                stderr: stderr.into(),
            })
        };
        assert!(git("fatal: could not create directory 'x': No space left on device").may_pass());
        assert!(git("error: unable to write file: Disk quota exceeded").may_pass());
        assert!(!git("fatal: invalid reference: abc").may_pass());
        assert!(!WorktreeError::Invalid("No space left on device".into()).may_pass());

        let dir = std::env::temp_dir().join(format!("atomic-caches-kind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("file"), "").expect("file");
        let refused = Caches::new(dir.join("file/run-1"))
            .create()
            .expect_err("not a directory");
        assert!(
            matches!(&refused, WorktreeError::Io { source, .. } if source.kind() == ErrorKind::NotADirectory),
            "{refused:?}"
        );
        assert!(!refused.may_pass());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On a full disk, a step's caches and its worktree wait for room, then
    /// are made: through the calls a run makes, [`Caches::create_waiting`]
    /// and [`Worktrees::run_step`]. Nothing else waits.
    #[test]
    fn a_full_disk_makes_a_steps_caches_and_worktree_wait_for_room() {
        let full = |path: &Path, times: usize| {
            FULL_FOR_TESTS
                .lock()
                .expect("full")
                .push((path.to_owned(), times));
        };
        let left = |path: &Path| {
            FULL_FOR_TESTS
                .lock()
                .expect("full")
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, left)| *left)
        };
        let dir = crate::git::tests::scratch("full-disk");
        let (repo, first, _) = crate::git::tests::history(&dir);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let caches = Caches::new(dir.join("state/cache/run-1-ab"));
            full(caches.dir(), 2);
            caches
                .create_waiting("implement", "run-1", "implement")
                .await
                .expect("made once there is room");
            assert!(caches.dir().is_dir());
            assert_eq!(left(caches.dir()), Some(0), "both full tries were made");

            let worktrees = Arc::new(Worktrees::new(
                crate::git::Git::find().expect("git").discover(&repo).expect("repo"),
                dir.join("state/worktrees"),
            ));
            full(&dir.join("state/worktrees"), 2);
            let request = StepRequest {
                run: "run-1".into(),
                step: "implement".into(),
                base: first.clone(),
                message: "Implement it".into(),
                signature: Signature::default(),
            };
            let ran = std::sync::atomic::AtomicUsize::new(0);
            let (out, _) = worktrees
                .run_step(request, |tree| {
                    ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async move { Ok::<_, String>(tree.is_dir()) }
                })
                .await
                .expect("prepared once there is room");
            assert!(out, "the body ran in the worktree");
            assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(left(&dir.join("state/worktrees")), Some(0));

            // A cache directory that can never be made fails at once.
            std::fs::write(dir.join("file"), "").expect("file");
            let refused = Caches::new(dir.join("file/run-1"))
                .create_waiting("implement", "run-1", "implement")
                .await;
            assert!(
                matches!(&refused, Err(crate::AtomError::Runtime(m)) if m.contains("the run's build caches")),
                "{refused:?}"
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_runs_caches_are_private_and_removed_whole() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("atomic-run-caches-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let caches = Caches::new(dir.join("cache/run-1"));
        caches.create().expect("create");
        let mode = std::fs::metadata(caches.dir())
            .expect("made")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        std::fs::write(caches.dir().join("built"), "x").expect("write");
        // A directory a tool made read-only, as Go's module cache is.
        let locked = caches.dir().join("go-mod/m@v1");
        std::fs::create_dir_all(&locked).expect("locked");
        std::fs::write(locked.join("go.mod"), "module m\n").expect("file");
        for d in [&locked, &caches.dir().join("go-mod")] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o500)).expect("mode");
        }
        caches.remove().expect("remove");
        assert!(!caches.dir().exists());
        caches.remove().expect("a repeat finds nothing");
        // A link in its place, which something else made, is refused.
        std::fs::create_dir_all(dir.join("elsewhere")).expect("elsewhere");
        std::fs::set_permissions(
            dir.join("elsewhere"),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("mode");
        std::os::unix::fs::symlink(dir.join("elsewhere"), caches.dir()).expect("link");
        assert!(caches.create().is_err(), "a link was taken for the caches");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
