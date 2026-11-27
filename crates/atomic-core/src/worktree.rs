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

use crate::git::{Git, GitError, Repo, Signature, TrialMerge, is_sha};
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

impl WorktreeError {
    /// Whether this is one of atomic's own file operations that found the
    /// disk or a quota full, by the kind of its error. Nothing git reports
    /// counts: its failures carry no kind, and what it prints is not read,
    /// since a commit's file names could say anything.
    pub fn disk_full(&self) -> bool {
        let WorktreeError::Io { source, .. } = self else {
            return false;
        };
        [
            std::io::ErrorKind::StorageFull,
            std::io::ErrorKind::QuotaExceeded,
        ]
        .contains(&source.kind())
    }

    /// What a step that failed for this says: for a failure of git's, which
    /// a disk filling during a checkout also is, what to do if that is why.
    pub fn described(&self) -> String {
        match self {
            WorktreeError::Git(_) => {
                format!("{self}; if the disk is full, free space and start the run again")
            }
            _ => self.to_string(),
        }
    }
}

/// Directories whose creation by atomic itself fails with an error of a
/// given kind, so many times, by path: for this crate's tests only, which
/// cannot fill a disk.
#[cfg(test)]
pub(crate) static FAIL_FOR_TESTS: std::sync::Mutex<Vec<FailForTests>> =
    std::sync::Mutex::new(Vec::new());

/// One entry of [`FAIL_FOR_TESTS`]: the directory, the error's kind, and
/// how many more creations of it fail.
#[cfg(test)]
pub(crate) type FailForTests = (PathBuf, std::io::ErrorKind, usize);

/// The error a test asked atomic's own creation of `dir` to fail with.
#[cfg(test)]
fn fail_for_tests(dir: &Path) -> Option<WorktreeError> {
    let mut fail = FAIL_FOR_TESTS.lock().ok()?;
    let (_, kind, left) = fail.iter_mut().find(|(d, _, left)| d == dir && *left > 0)?;
    *left -= 1;
    Some(WorktreeError::Io {
        what: "create",
        path: dir.to_owned(),
        source: std::io::Error::from(*kind),
    })
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

/// Git facts atomic checked before it may use a worktree commit as graph
/// close evidence. The branch head and pinned base are retained so a stored
/// attestation says exactly what was checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchCommitProof {
    /// The verified worktree commit.
    pub commit: String,
    /// The authorized full branch ref.
    pub branch: String,
    /// The branch head when atomic checked reachability.
    pub head: String,
    /// The base recorded for this run, independent of later branch movement.
    pub pinned_base: String,
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
    /// The atom it runs, by name: what a wait for room names.
    pub atom: String,
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
    /// Whether an attempt ref belongs to this run, using the same encoded
    /// component that commit and recovery use for non-plain run IDs.
    pub fn step_ref_belongs_to_run(run: &str, reference: &str) -> bool {
        let prefix = format!("refs/atomic/{}/steps/", component(run));
        reference
            .strip_prefix(&prefix)
            .and_then(|rest| rest.rsplit_once('/'))
            .is_some_and(|(step, attempt)| {
                !step.is_empty() && attempt.parse::<u32>().is_ok_and(|n| n > 0)
            })
    }

    /// The worktrees of the repository the project at `state` is in, kept
    /// under `state`'s directory.
    pub fn open(state: &ProjectState) -> Result<Self> {
        Self::open_at(state.root(), state.dir().join("worktrees"))
    }

    /// The worktrees of the repository `root` is in, kept under `dir`: what
    /// [`Git::discover`] finds from `root`, for a caller that names the
    /// directory and where its worktrees go itself.
    pub fn open_at(root: &Path, dir: PathBuf) -> Result<Self> {
        let git = Git::find()?;
        let repo = git.discover(root)?;
        Ok(Self::new(repo, dir))
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
    /// in the state directory: `cache/<run>-<nonce>.v2`, named as the run's
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

    /// Check a runtime-recorded attempt against this run's pinned base and
    /// the current authorized branch. A caller's `StepCommit` fields are
    /// treated as claims until Git independently confirms every one of them.
    /// A missing ref or a mismatch returns `None`; Git failures propagate.
    pub fn verify_step_commit_on_branch(
        &self,
        run: &str,
        step_commit: &StepCommit,
        branch: &str,
    ) -> Result<Option<BranchCommitProof>> {
        let prefix = format!("refs/atomic/{}/steps/", component(run));
        let Some(rest) = step_commit.reference.strip_prefix(&prefix) else {
            return Ok(None);
        };
        let Some((step, attempt)) = rest.rsplit_once('/') else {
            return Ok(None);
        };
        if step.is_empty()
            || step.contains('/')
            || attempt.parse::<u32>().ok() != Some(step_commit.attempt)
            || step_commit.attempt == 0
        {
            return Ok(None);
        }
        let Some(pinned_base) = self.base(run)? else {
            return Ok(None);
        };
        if self.repo.resolve_commit(&step_commit.commit)? != step_commit.commit
            || self.repo.resolve_commit(&step_commit.base)? != step_commit.base
            || self.repo.ref_target(&step_commit.reference)?.as_deref()
                != Some(step_commit.commit.as_str())
            || self.repo.parents(&step_commit.commit)? != [step_commit.base.clone()]
            || !self.repo.is_ancestor(&pinned_base, &step_commit.commit)?
            || !self.repo.has_everything(&step_commit.commit)?
        {
            return Ok(None);
        }
        let Some(head) = self.repo.ref_target(branch)? else {
            return Ok(None);
        };
        if !self.repo.is_ancestor(&step_commit.commit, &head)? {
            return Ok(None);
        }
        Ok(Some(BranchCommitProof {
            commit: step_commit.commit.clone(),
            branch: branch.to_owned(),
            head,
            pinned_base,
        }))
    }

    /// Reconstruct an attempt recorded by atomic from its Git ref. This is
    /// for a host admin action after the turn: the command names the run,
    /// step and attempt, while Git supplies the SHA and sole parent.
    pub fn recorded_step_commit(
        &self,
        run: &str,
        step: &str,
        attempt: u32,
    ) -> Result<Option<StepCommit>> {
        if attempt == 0 {
            return Ok(None);
        }
        let reference = format!(
            "refs/atomic/{}/steps/{}/{attempt}",
            component(run),
            component(step)
        );
        let Some(commit) = self.repo.ref_target(&reference)? else {
            return Ok(None);
        };
        let parents = self.repo.parents(&commit)?;
        let [base] = parents.as_slice() else {
            return Ok(None);
        };
        Ok(Some(StepCommit {
            base: base.clone(),
            commit,
            reference,
            attempt,
        }))
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
        #[cfg(test)]
        if let Some(e) = fail_for_tests(&self.dir) {
            return Err(e);
        }
        // Reject local attributes before removing a previous attempt: they
        // can select a filter during checkout even with an empty attr source.
        let safe = self.repo.clone().with_empty_attributes()?;
        std::fs::create_dir_all(&self.dir).map_err(io("create", &self.dir))?;
        let dir = std::fs::canonicalize(&self.dir).map_err(io("resolve", &self.dir))?;
        #[cfg(windows)]
        let dir = dunce::simplified(&dir);
        let name = worktree_name(run, step);
        let path = dir.join(&name);
        self.remove(&path, &name)?;
        let target = path
            .to_str()
            .ok_or_else(|| WorktreeError::Invalid(format!("{} is not UTF-8", path.display())))?;
        safe.run(
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

    /// Prepare a disposable checkout of a host-recorded candidate, then
    /// merge the branch's current head into it. This runs in the control
    /// plane, outside an atom sandbox. The source checkout and its refs are
    /// read only. A retry resets the scratch checkout before merging again.
    pub fn trial_merge(
        &self,
        run: &str,
        step: &str,
        candidate: &StepCommit,
        main: &str,
        fixture_source: Option<&Path>,
    ) -> Result<(PathBuf, String, TrialMerge, Option<String>)> {
        if self.repo.has_info_attributes()? {
            return Err(WorktreeError::Invalid(
                "trial merge refuses repository info/attributes; it could select a merge or filter driver".into(),
            ));
        }
        if self.repo.ref_target(&candidate.reference)?.as_deref() != Some(&candidate.commit)
            || self.repo.parents(&candidate.commit)? != [candidate.base.as_str()]
        {
            return Err(WorktreeError::Invalid(
                "the candidate no longer matches its host commit ref and base".into(),
            ));
        }
        let head = match fixture_source {
            Some(source) => {
                // A fixture snapshot was cut at admission and has no live
                // branch refs. Resolve main in the original trusted source
                // now, then bring its objects across without a transport.
                let source = Git::find()?.discover(source)?;
                if source.object_format()? != self.repo.object_format()? {
                    return Err(WorktreeError::Invalid(
                        "the fixture source and snapshot use different object formats".into(),
                    ));
                }
                if source.is_shallow()? {
                    return Err(WorktreeError::Invalid(
                        "the current fixture source has shallow history".into(),
                    ));
                }
                let grafts = source.git_dir().join("info/grafts");
                match std::fs::symlink_metadata(&grafts) {
                    Ok(_) => {
                        return Err(WorktreeError::Invalid(
                            "the current fixture source has grafted history".into(),
                        ));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io("inspect", &grafts)(e)),
                }
                let head = source.resolve_commit(main)?;
                if !self.repo.has_everything(&head)? {
                    source.copy_reachable(&head, &self.repo)?;
                }
                if !self.repo.has_everything(&head)? {
                    return Err(WorktreeError::Invalid(format!(
                        "the fixture snapshot is missing objects for current main {head}"
                    )));
                }
                let pin = format!(
                    "refs/atomic/{}/steps/{}/trial-main-{head}",
                    component(run),
                    component(step)
                );
                if let Err(e) = self.repo.create_ref(&pin, &head)
                    && self.repo.ref_target(&pin)?.as_deref() != Some(head.as_str())
                {
                    return Err(WorktreeError::Git(e));
                }
                head
            }
            None => self.repo.resolve_commit(main)?,
        };
        let worktree = self.prepare(run, step, &candidate.commit)?;
        self.check_admin(&worktree.git_dir, &worktree.path)?;
        let scratch = self
            .repo
            .clone()
            .with_empty_attributes()?
            .linked(&worktree.git_dir, &worktree.path);
        let result = scratch.trial_merge(&head)?;
        let tree = if result == TrialMerge::Clean {
            Some(scratch.index_tree()?)
        } else {
            None
        };
        Ok((worktree.path, head, result, tree))
    }

    /// Check that a trial check did not rewrite tracked merged content.
    pub fn trial_changed_tracked(&self, run: &str, step: &str, tree: &str) -> Result<Vec<String>> {
        let name = worktree_name(run, step);
        let path = self.dir.join(&name);
        let git_dir = self.admin_dir(&name);
        self.check_admin(&git_dir, &path)?;
        let repo = self
            .repo
            .clone()
            .with_empty_attributes()?
            .linked(git_dir, path);
        Ok(repo.changed_tracked(tree)?)
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
        let tree = self
            .repo
            .clone()
            .with_empty_attributes()?
            .linked(&worktree.git_dir, &worktree.path);
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
            label: &request.atom,
            run: &request.run,
            step: &request.step,
            what: "the disk",
            doing: "hold the step's worktree",
        };
        let worktree = crate::runtime::wait_for_room(waiting, move || {
            // Only atomic's own writes that find the disk full wait: a
            // checkout git cannot finish fails the step.
            match this.prepare(&run, &step, &base) {
                Err(e) if e.disk_full() => Err(crate::runtime::NotYet::Room(format!(
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

    /// Remove run `run`'s worktrees, and every attempt ref of its steps except
    /// the exact refs of accepted candidates. Two attempts can create the same
    /// commit object, so a commit ID alone cannot identify the accepted one.
    /// The run's pinned base stays. Commits no ref names are left for git's
    /// own garbage collection.
    pub fn cleanup(&self, run: &str, accepted_refs: &[String]) -> Result<Cleanup> {
        let mut report = Cleanup::default();
        let prefix = format!("refs/atomic/{}/steps/", component(run));
        for r in self.repo.refs(&prefix)? {
            if !accepted_refs.contains(&r.name) {
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
        #[cfg(windows)]
        let dir = dunce::simplified(&dir);
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
/// `<state>/cache/<run>-<nonce>.v2` ([`Caches::for_run`]), for its worktree and
/// exec steps, removed when the run ends.
///
/// An atom's commands find them through [`env`](Self::env), and a
/// `workspace-write` thread is given only [`writable_roots`](Self::writable_roots)
/// (`TurnSpec::env` and `TurnSpec::writable_roots` in [`crate::codex`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caches {
    dir: PathBuf,
    /// A pre-isolation cache for the same run, used only for cleanup.
    legacy: Option<PathBuf>,
    /// The person's rustup toolchains, which commands may only read.
    rustup: Option<PathBuf>,
    /// Name of the directory for this step's build tools.
    scope: String,
}

impl Caches {
    /// The build caches of run `run` under `root`, the state directory's
    /// `cache/`: `<root>/<run>-<nonce>.v2`, named as the run's worktrees are and
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
            // Old versions granted writes to the entire run directory. Resume
            // cold in a sibling whose contents those versions could not plant.
            dir: root.join(format!("{name}.v2")),
            legacy: Some(root.join(name)),
            rustup,
            scope: "unscoped".into(),
        })
    }

    /// Caches kept under `dir`.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            legacy: None,
            rustup: None,
            scope: "unscoped".into(),
        }
    }

    /// The shared run directory and the build caches of this worktree.
    pub fn in_worktree(&self, tree: &Path) -> Self {
        self.for_scope(
            "worktree",
            tree.file_name().unwrap_or_default().as_encoded_bytes(),
        )
    }

    /// The shared run directory and the build caches of exec step `name`.
    pub fn for_exec(&self, name: &str) -> Self {
        self.for_scope("exec", name.as_bytes())
    }

    fn for_scope(&self, kind: &str, name: &[u8]) -> Self {
        // Keep names distinct even on a case-insensitive filesystem.
        Self {
            scope: format!("{kind}-{:x}", Sha256::digest(name)),
            ..self.clone()
        }
    }

    /// A trial check's writable caches live inside its disposable scratch
    /// worktree. Resetting that worktree on a retried durable step clears
    /// every write the earlier check could have made. A candidate that
    /// already contains this path is refused instead of trusted as a cache.
    pub fn for_trial(&self, tree: &Path) -> Result<Self> {
        let dir = tree.join(".atomic-trial-cache");
        match std::fs::symlink_metadata(&dir) {
            Ok(_) => {
                return Err(WorktreeError::Invalid(format!(
                    "the candidate already contains {}; trial checks need a fresh cache",
                    dir.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io("inspect", &dir)(e)),
        }
        Ok(Self {
            dir,
            legacy: None,
            rustup: self.rustup.clone(),
            scope: "trial".into(),
        })
    }

    /// Give an offline trial check the operator's registry, read-only under
    /// the sandbox. Cargo's home, locks and target stay in this disposable
    /// attempt cache. Registry sources stay read-only.
    pub fn prepare_trial_cargo(&self, registry: &Path) -> Result<PathBuf> {
        self.prepare_offline_cargo(self.writable_roots()[1].join("cargo-home"), registry)
    }

    /// A production exec proof gets a new Cargo home for each attempt. An
    /// interrupted attempt may have left a cache view behind; it must never
    /// become the trusted view of a later attempt.
    pub fn prepare_exec_cargo(&self, registry: &Path) -> Result<PathBuf> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|e| {
            WorktreeError::Invalid(format!("cannot name a fresh exec Cargo home: {e}"))
        })?;
        let suffix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        self.prepare_offline_cargo(
            self.writable_roots()[1].join(format!("cargo-home-{suffix}")),
            registry,
        )
    }

    fn prepare_offline_cargo(&self, home: PathBuf, registry: &Path) -> Result<PathBuf> {
        let source = std::fs::canonicalize(registry).map_err(io("inspect", registry))?;
        let scratch = self
            .dir
            .parent()
            .ok_or_else(|| WorktreeError::Invalid("trial cache has no scratch worktree".into()))?;
        if source != registry
            || source.starts_with(scratch)
            || scratch.starts_with(&source)
            || !["index", "cache", "src"]
                .iter()
                .all(|name| source.join(name).is_dir())
        {
            return Err(WorktreeError::Invalid(format!(
                "trial merge offline Cargo cache unavailable at {}",
                registry.display()
            )));
        }
        std::fs::create_dir(&home).map_err(io("create", &home))?;
        // Cargo reads both spellings. Mount empty files read-only for every
        // check so an earlier check cannot install forced env overrides for a
        // later Cargo invocation.
        for name in ["config", "config.toml"] {
            let path = home.join(name);
            std::fs::write(&path, "").map_err(io("create", &path))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let registry_view = home.join("registry");
            std::fs::create_dir(&registry_view).map_err(io("create", &registry_view))?;
            for name in ["index", "cache", "src"] {
                symlink(source.join(name), registry_view.join(name))
                    .map_err(io("link", &registry_view))?;
            }
            if let Some(home_source) = source.parent() {
                let git_cache = home_source.join("git");
                if git_cache.is_dir() {
                    let git_view = home.join("git");
                    std::fs::create_dir(&git_view).map_err(io("create", &git_view))?;
                    for entry in std::fs::read_dir(&git_cache).map_err(io("read", &git_cache))? {
                        let entry = entry.map_err(io("read", &git_cache))?;
                        symlink(entry.path(), git_view.join(entry.file_name()))
                            .map_err(io("link", &git_view))?;
                    }
                }
            }
        }
        #[cfg(not(unix))]
        return Err(WorktreeError::Invalid(
            "trial merge offline Cargo cache needs a Linux sandbox".into(),
        ));
        Ok(home)
    }

    /// Writable cache roots for a command. Their common parent belongs
    /// exclusively to atomic, so a step cannot create a sibling target
    /// directory or replace one with a link.
    pub fn writable_roots(&self) -> [PathBuf; 2] {
        [self.dir.join("shared"), self.dir.join(&self.scope)]
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
        if let Some(e) = fail_for_tests(&self.dir) {
            return Err(e);
        }
        for dir in std::iter::once(self.dir.clone()).chain(self.writable_roots()) {
            match std::fs::symlink_metadata(&dir) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(WorktreeError::Invalid(format!(
                        "{} is a link, not the run's cache directory; atomic did not make it",
                        dir.display()
                    )));
                }
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(io("inspect", &dir)(e));
                }
                _ => {}
            }
            crate::state::private_dir(&dir).map_err(|e| match e {
                crate::state::StateError::Create { source, .. } => WorktreeError::Io {
                    what: "create",
                    path: dir,
                    source,
                },
                e => WorktreeError::Invalid(e.to_string()),
            })?;
        }
        Ok(())
    }

    /// [`create`](Self::create), as [`crate::runtime::wait_for_room`] reads
    /// it: a full disk or quota is a reason to wait, anything else is for
    /// good.
    pub(crate) fn create_for_room(&self) -> std::result::Result<(), crate::runtime::NotYet> {
        let Err(e) = self.create() else {
            return Ok(());
        };
        let why = format!("the run's build caches: {e}");
        if e.disk_full() {
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
        remove_tree(&self.dir)?;
        if let Some(legacy) = &self.legacy {
            remove_tree(legacy)?;
        }
        Ok(())
    }

    /// The environment that points common build tools at the caches: the
    /// XDG cache home, Cargo's target directory, and npm's, pip's and Go's
    /// caches, plus `ATOMIC_CACHE_DIR` for anything else, and `RUSTUP_HOME`
    /// at the person's toolchains when they were found. Cargo's home is the
    /// command's own `HOME`'s, never shared.
    pub fn env(&self) -> std::collections::BTreeMap<String, String> {
        let [shared, step] = self.writable_roots();
        let at = |sub: &str| step.join(sub).to_string_lossy().into_owned();
        [
            ("ATOMIC_CACHE_DIR", shared.to_string_lossy().into_owned()),
            ("XDG_CACHE_HOME", at("xdg")),
            ("CARGO_TARGET_DIR", at("cargo-target")),
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
    fn attempt_ref_ownership_uses_encoded_run_name() {
        let dotted = "demo.one";
        let reference = format!("refs/atomic/{}/steps/author/1", component(dotted));
        assert!(Worktrees::step_ref_belongs_to_run(dotted, &reference));
        assert!(!Worktrees::step_ref_belongs_to_run("demo_two", &reference));
        assert!(!Worktrees::step_ref_belongs_to_run(
            dotted,
            &format!("refs/atomic/{}/steps/author/0", component(dotted))
        ));
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
        assert_eq!(caches.dir(), Path::new("/state/cache/run-1-ab12.v2"));
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
        // worktree's, even for two names differing only in case.
        let tree = caches
            .in_worktree(&Path::new("/state/worktrees").join(worktree_name("run-1", "a")))
            .env();
        let targets: Vec<String> = ["a", "A", "b", "a.b", "_a"]
            .iter()
            .map(|turn| caches.for_exec(turn).env()["CARGO_TARGET_DIR"].clone())
            .chain([
                tree["CARGO_TARGET_DIR"].clone(),
                env["CARGO_TARGET_DIR"].clone(),
            ])
            .collect();
        let mut distinct: Vec<_> = targets.iter().map(|t| t.to_lowercase()).collect();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), targets.len(), "{targets:?}");
        let exec = caches.for_exec("a").env();
        for (name, value) in &exec {
            match name.as_str() {
                "ATOMIC_CACHE_DIR" | "RUSTUP_HOME" => assert_eq!(value, &tree[name], "{name}"),
                _ => {
                    assert_ne!(value, &tree[name], "{name}");
                    assert!(
                        !Path::new(value).starts_with(&exec["ATOMIC_CACHE_DIR"]),
                        "{name}"
                    );
                }
            }
        }
    }

    /// Only atomic's own file operation that found the disk or a quota
    /// full counts as a full disk; no other error does, and nothing git
    /// reports, which says what to do if the disk is why. A cache directory
    /// that cannot be made says why as it failed.
    #[test]
    fn only_a_full_disk_or_quota_counts_as_full() {
        use std::io::{Error, ErrorKind};
        let io = |source: Error| WorktreeError::Io {
            what: "create",
            path: "/state/cache".into(),
            source,
        };
        assert!(io(Error::from(ErrorKind::StorageFull)).disk_full());
        assert!(io(Error::from(ErrorKind::QuotaExceeded)).disk_full());
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::NotADirectory,
            ErrorKind::ReadOnlyFilesystem,
            ErrorKind::ResourceBusy,
            ErrorKind::WouldBlock,
            ErrorKind::Interrupted,
            ErrorKind::TimedOut,
            ErrorKind::BrokenPipe,
        ] {
            assert!(!io(Error::from(kind)).disk_full(), "{kind:?}");
        }
        for raw in [libc::EMFILE, libc::ENFILE] {
            assert!(!io(Error::from_raw_os_error(raw)).disk_full(), "{raw}");
        }
        let git = WorktreeError::Git(GitError::Failed {
            what: "worktree".into(),
            code: Some(128),
            stderr: "fatal: could not create 'No space left on device': Disk quota exceeded".into(),
        });
        assert!(!git.disk_full(), "what git prints is not read");
        assert!(
            git.described()
                .ends_with("; if the disk is full, free space and start the run again"),
            "{}",
            git.described()
        );
        let spawn = WorktreeError::Git(GitError::Spawn(Error::from(ErrorKind::StorageFull)));
        assert!(!spawn.disk_full());
        let invalid = WorktreeError::Invalid("No space left on device".into());
        assert!(!invalid.disk_full());
        assert_eq!(invalid.described(), "No space left on device", "no hint");

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
        assert!(!refused.disk_full());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Through the calls a run makes, [`Caches::create_waiting`] and
    /// [`Worktrees::run_step`]: a step whose caches, or whose worktrees'
    /// directory, atomic finds the disk full making waits, then goes on
    /// once they are made. A read-only file system, a directory it may not
    /// write or a file in the way fails the step at once, and so does a
    /// checkout git cannot finish, here a worktrees directory git may not
    /// write.
    #[cfg(unix)]
    #[test]
    fn only_atomics_own_writes_on_a_full_disk_wait() {
        use std::io::ErrorKind;
        use std::os::unix::fs::PermissionsExt;
        let fail = |dir: &Path, kind: ErrorKind, times: usize| {
            FAIL_FOR_TESTS
                .lock()
                .expect("fail")
                .push((dir.to_owned(), kind, times));
        };
        let left = |dir: &Path| {
            FAIL_FOR_TESTS
                .lock()
                .expect("fail")
                .iter()
                .filter(|(d, _, _)| d == dir)
                .map(|(_, _, left)| *left)
                .sum::<usize>()
        };
        let dir = crate::git::tests::scratch("room");
        let (repo, first, _) = crate::git::tests::history(&dir);
        let repo = crate::git::Git::find()
            .expect("git")
            .discover(&repo)
            .expect("repo");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            // Caches: full twice, then made.
            let caches = Caches::new(dir.join("state/cache/run-1-ab"));
            fail(caches.dir(), ErrorKind::StorageFull, 2);
            caches
                .create_waiting("implement-it", "run-1", "implement")
                .await
                .expect("made once there is room");
            assert!(caches.dir().is_dir());
            assert_eq!(left(caches.dir()), 0, "it waited through both");
            // Anything else, at once.
            for kind in [
                ErrorKind::ReadOnlyFilesystem,
                ErrorKind::PermissionDenied,
                ErrorKind::NotADirectory,
            ] {
                let other = Caches::new(dir.join(format!("state/cache/{kind:?}")));
                fail(other.dir(), kind, 1);
                fail(other.dir(), ErrorKind::StorageFull, 1);
                let refused = other
                    .create_waiting("implement-it", "run-1", "implement")
                    .await;
                assert!(
                    matches!(&refused, Err(crate::AtomError::Runtime(m)) if m.contains("the run's build caches")),
                    "{kind:?}: {refused:?}"
                );
                assert_eq!(left(other.dir()), 1, "{kind:?}: it did not try again");
            }

            // The worktrees' directory: full twice, then the step runs.
            let trees = dir.join("state/worktrees");
            let worktrees = Arc::new(Worktrees::new(repo, trees.clone()));
            let request = StepRequest {
                run: "run-1".into(),
                step: "implement".into(),
                atom: "implement-it".into(),
                base: first.clone(),
                message: "Implement it".into(),
                signature: Signature::default(),
            };
            let ran = std::sync::atomic::AtomicUsize::new(0);
            fail(&trees, ErrorKind::QuotaExceeded, 2);
            worktrees
                .run_step(request.clone(), |tree| {
                    ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async move { Ok::<_, String>(tree.is_dir()) }
                })
                .await
                .expect("ran once there was room");
            assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(left(&trees), 0, "it waited through both");
            // Anything else, at once.
            for kind in [
                ErrorKind::ReadOnlyFilesystem,
                ErrorKind::PermissionDenied,
                ErrorKind::NotADirectory,
            ] {
                fail(&trees, kind, 1);
                fail(&trees, ErrorKind::StorageFull, 1);
                let failed = worktrees
                    .run_step(request.clone(), |_| async { Ok::<_, String>(true) })
                    .await;
                assert!(
                    matches!(&failed, Err(StepError::Worktree(WorktreeError::Io { source, .. })) if source.kind() == kind),
                    "{kind:?}: {failed:?}"
                );
                assert_eq!(left(&trees), 1, "{kind:?}: it did not try again");
                FAIL_FOR_TESTS
                    .lock()
                    .expect("fail")
                    .retain(|(d, _, _)| *d != trees);
            }
            // A checkout git cannot finish fails the step at once, saying
            // what to do if the disk is full, even on a full disk.
            assert_ne!(
                crate::state::effective_uid(),
                0,
                "this test needs an unprivileged user: a privileged one may write \
                 the read-only directory it makes"
            );
            // A step of its own, so no earlier worktree is in the way.
            let request = StepRequest {
                step: "again".into(),
                ..request
            };
            std::fs::set_permissions(&trees, std::fs::Permissions::from_mode(0o500))
                .expect("read-only");
            let failed = worktrees
                .run_step(request, |_| async { Ok::<_, String>(true) })
                .await;
            std::fs::set_permissions(&trees, std::fs::Permissions::from_mode(0o700))
                .expect("writable");
            let Err(StepError::Worktree(e @ WorktreeError::Git(GitError::Failed { .. }))) = &failed
            else {
                panic!("{failed:?}");
            };
            assert!(
                e.described()
                    .contains("if the disk is full, free space and start the run again"),
                "{}",
                e.described()
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
        let root = dir.join("cache");
        let legacy = root.join("run-1-ab12");
        let caches = Caches::for_run(&root, "run-1", Some("ab12"), None)
            .expect("caches")
            .for_exec("build");
        let prior_target = legacy.join("cargo-target/build");
        std::fs::create_dir_all(&prior_target).expect("older cache");
        std::fs::write(prior_target.join("old"), "older build").expect("write");
        caches.create().expect("create");
        let env = caches.env();
        assert!(!Path::new(&env["CARGO_TARGET_DIR"]).starts_with(&legacy));
        assert!(!Path::new(&env["CARGO_TARGET_DIR"]).join("old").exists());
        let mode = std::fs::metadata(caches.dir())
            .expect("made")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        std::fs::write(caches.dir().join("built"), "x").expect("write");
        // A directory a tool made read-only, as Go's module cache is.
        let modules = Path::new(&env["GOMODCACHE"]);
        let locked = modules.join("m@v1");
        std::fs::create_dir_all(&locked).expect("locked");
        std::fs::write(locked.join("go.mod"), "module m\n").expect("file");
        for d in [locked.as_path(), modules] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o500)).expect("mode");
        }
        caches.remove().expect("remove");
        assert!(!caches.dir().exists());
        assert!(!legacy.exists());
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

    #[cfg(unix)]
    #[test]
    fn trial_cargo_links_the_trusted_registry_and_git_cache() {
        let dir = std::env::temp_dir().join(format!(
            "atomic-trial-registry-cache-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let registry = dir.join("trusted/registry");
        let index = registry.join("src/index.test");
        std::fs::create_dir_all(registry.join("index")).expect("registry index");
        std::fs::create_dir_all(registry.join("cache")).expect("registry archive cache");
        std::fs::create_dir_all(index.join("serde-1.0.0/src")).expect("serde source");
        std::fs::create_dir_all(dir.join("trusted/git")).expect("Git dependency cache");
        std::fs::write(dir.join("trusted/git/db"), "trusted Git cache")
            .expect("Git dependency marker");
        std::fs::write(index.join("serde-1.0.0/src/lib.rs"), "trusted serde")
            .expect("serde source");
        // macOS temp_dir may spell /private/var as /var. The production
        // caller supplies a canonical registry path.
        let registry = std::fs::canonicalize(&registry).expect("canonical registry");
        let scratch = dir.join("trial");
        std::fs::create_dir(&scratch).expect("trial scratch");
        let cache = Caches::new(dir.join("unused"))
            .for_trial(&scratch)
            .expect("trial cache");
        cache.create().expect("fresh cache");
        let home = cache
            .prepare_trial_cargo(&registry)
            .expect("trial Cargo view");
        assert!(home.starts_with(cache.writable_roots()[1].as_path()));
        for path in ["registry/index", "registry/cache", "git/db", "registry/src"] {
            assert!(
                std::fs::symlink_metadata(home.join(path))
                    .expect("trusted cache link")
                    .file_type()
                    .is_symlink(),
                "{path} must still come from the trusted cache"
            );
        }
        assert!(home.join("git").is_dir());
        assert!(
            !std::fs::symlink_metadata(home.join("git"))
                .expect("private Git view")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(home.join("registry/src/index.test/serde-1.0.0/src/lib.rs"))
                .expect("read trusted serde source"),
            "trusted serde"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn exec_cargo_preparation_never_reuses_a_previous_attempt_home() {
        let dir =
            std::env::temp_dir().join(format!("atomic-exec-cargo-attempts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let registry = dir.join("trusted/registry");
        for part in ["index", "cache", "src/index.test/serde-1.0.0"] {
            std::fs::create_dir_all(registry.join(part)).expect("trusted registry");
        }
        let registry = std::fs::canonicalize(&registry).expect("canonical registry");
        let cache = Caches::new(dir.join("private/run-cache"));
        cache.create().expect("private run cache");
        let first = cache.prepare_exec_cargo(&registry).expect("first attempt");
        assert!(first.starts_with(cache.writable_roots()[1].as_path()));
        std::fs::write(
            first.join("config.toml"),
            "[env]\nCARGO_NET_OFFLINE='false'\n",
        )
        .expect("earlier attempt write");
        let next = cache.prepare_exec_cargo(&registry).expect("next attempt");
        assert_ne!(first, next);
        assert_eq!(
            std::fs::read_to_string(next.join("config.toml")).expect("fresh config"),
            ""
        );
        assert!(next.join("registry/src/index.test/serde-1.0.0").is_dir());
        let _ = std::fs::remove_dir_all(dir);
    }
}
