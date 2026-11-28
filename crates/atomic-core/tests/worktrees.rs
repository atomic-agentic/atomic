// SPDX-License-Identifier: Apache-2.0

//! A git worktree per code-changing step, kept in the state directory:
//!
//! - a step commits on its base, and the commit's only parent is the base;
//! - every attempt starts from the base, whatever an earlier attempt left;
//! - a branch given as a run's base is pinned once, and later movement of
//!   the branch changes nothing;
//! - cleanup removes every worktree and attempt ref no accepted candidate
//!   names;
//! - nothing appears in the project's own tree.

#![allow(clippy::unwrap_used)]

mod support;

use atomic_core::git::{Git, Signature};
use atomic_core::worktree::{StepError, StepRequest, Worktrees};
use std::path::PathBuf;
use std::sync::Arc;
use support::{Scratch, git, write};

struct Fixture {
    _scratch: Scratch,
    project: PathBuf,
    state: PathBuf,
    worktrees: Arc<Worktrees>,
    first: String,
    second: String,
}

fn fixture(tag: &str) -> Fixture {
    let scratch = Scratch::new(tag);
    let (project, first, second) = support::project(scratch.path());
    let state = scratch.path().join("state");
    let repo = Git::find().unwrap().discover(&project).unwrap();
    let worktrees = Arc::new(Worktrees::new(repo, state.join("worktrees")));
    Fixture {
        _scratch: scratch,
        project,
        state,
        worktrees,
        first,
        second,
    }
}

fn status(f: &Fixture) -> String {
    git(&f.project, &["status", "--porcelain", "--ignored"])
}

#[test]
fn a_step_commits_on_its_base() {
    let f = fixture("commit");
    let wt = f.worktrees.prepare("run-1", "implement", &f.first).unwrap();
    assert!(wt.path.starts_with(f.state.canonicalize().unwrap()));
    assert_eq!(
        std::fs::read_to_string(wt.path.join("a.txt")).unwrap(),
        "one\ntwo\n"
    );
    write(&wt.path.join("a.txt"), "one\ntwo\nthree\n");
    write(&wt.path.join("src/new.rs"), "fn main() {}\n");
    write(&wt.path.join("ignored/build.log"), "noise\n");

    let commit = f
        .worktrees
        .commit(&wt, "Add a third line", &Signature::default())
        .unwrap();
    let repo = f.worktrees.repo();
    assert_eq!(repo.parents(&commit.commit).unwrap(), vec![f.first.clone()]);
    assert_eq!(commit.base, f.first);
    assert_eq!(commit.attempt, 1);
    assert_eq!(commit.reference, "refs/atomic/run-1/steps/implement/1");
    assert_eq!(
        repo.ref_target(&commit.reference).unwrap().as_deref(),
        Some(commit.commit.as_str())
    );
    // Ignored files stay out.
    assert_eq!(
        repo.changed_paths(&f.first, &commit.commit).unwrap(),
        ["a.txt", "src/new.rs"]
    );
    assert_eq!(
        support::show(&f.project, &commit.commit, "a.txt"),
        "one\ntwo\nthree\n"
    );
    let author = git(
        &f.project,
        &["log", "-1", "--format=%an <%ae>|%s", &commit.commit],
    );
    assert_eq!(author.trim(), "atomic <atomic@localhost>|Add a third line");
    // The project's branch, its checkout and its own tree are untouched.
    assert_eq!(repo.resolve_commit("main").unwrap(), f.second);
    assert_eq!(status(&f), "");
}

#[test]
fn trial_merge_refuses_info_attributes_that_select_union() {
    let f = fixture("trial-info-attributes");
    let before = status(&f);
    let wt = f.worktrees.prepare("run", "candidate", &f.first).unwrap();
    write(&wt.path.join("a.txt"), "one\nCANDIDATE\n");
    let candidate = f
        .worktrees
        .commit(&wt, "candidate", &Signature::default())
        .unwrap();
    write(
        &f.project.join(".git/info/attributes"),
        "a.txt merge=union\n",
    );
    let error = f
        .worktrees
        .trial_merge("run", "trial", &candidate, "main", None)
        .expect_err("info/attributes must not hide an overlapping conflict");
    assert!(error.to_string().contains("info/attributes"), "{error}");
    assert_eq!(status(&f), before, "source checkout remains unchanged");
}

/// A trial's check is verified in the worktree the trial checked out, even
/// where the state directory is reached through a link, as macOS's
/// temporary directory under `/var` is: git records the worktree's real
/// path.
#[test]
fn a_trial_is_verified_where_the_state_directory_is_a_link() {
    let scratch = Scratch::new("trial-linked-state");
    let (project, first, _) = support::project(scratch.path());
    let real = scratch.path().join("real-state");
    std::fs::create_dir_all(&real).unwrap();
    let state = scratch.path().join("state");
    std::os::unix::fs::symlink(&real, &state).unwrap();
    let repo = Git::find().unwrap().discover(&project).unwrap();
    let worktrees = Worktrees::new(repo, state.join("worktrees"));
    let wt = worktrees.prepare("run", "candidate", &first).unwrap();
    write(&wt.path.join("trial.txt"), "candidate\n");
    let candidate = worktrees
        .commit(&wt, "candidate", &Signature::default())
        .unwrap();
    let (_, _, _, tree) = worktrees
        .trial_merge("run", "trial", &candidate, "main", None)
        .unwrap();
    let tree = tree.expect("a clean trial merge");
    assert_eq!(
        worktrees
            .trial_changed_tracked("run", "trial", &tree)
            .unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn every_attempt_starts_from_the_base() {
    let f = fixture("reset");
    let wt = f.worktrees.prepare("run", "step", &f.first).unwrap();
    // A crashed attempt's half-edit: changed, new, ignored and locked files.
    write(&wt.path.join("a.txt"), "half-edited\n");
    write(&wt.path.join("stray.txt"), "stray\n");
    write(&wt.path.join("ignored/cache.bin"), "cache\n");
    write(&wt.path.join("locked/inner.txt"), "locked\n");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            wt.path.join("locked"),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();
    }
    std::fs::remove_file(wt.path.join(".git")).unwrap();

    let again = f.worktrees.prepare("run", "step", &f.first).unwrap();
    assert_eq!(again.path, wt.path);
    let mut names: Vec<String> = std::fs::read_dir(&again.path)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, [".git", ".gitignore", "a.txt"]);
    assert_eq!(
        std::fs::read_to_string(again.path.join("a.txt")).unwrap(),
        "one\ntwo\n"
    );
    let commit = f
        .worktrees
        .commit(&again, "Nothing changed", &Signature::default())
        .unwrap();
    let repo = f.worktrees.repo();
    assert_eq!(repo.parents(&commit.commit).unwrap(), vec![f.first.clone()]);
    assert_eq!(
        repo.changed_paths(&f.first, &commit.commit).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn a_branch_base_is_pinned_once() {
    let f = fixture("pin");
    let pinned = f.worktrees.pin_base("run", "main").unwrap();
    assert_eq!(pinned, f.second);
    assert_eq!(
        f.worktrees.base("run").unwrap().as_deref(),
        Some(f.second.as_str())
    );

    // The branch moves on after admission.
    write(&f.project.join("a.txt"), "moved\n");
    git(&f.project, &["commit", "-q", "-am", "moved"]);
    let moved = git(&f.project, &["rev-parse", "HEAD"]).trim().to_owned();
    assert_ne!(moved, f.second);

    // Admission again, and the step, see the pinned commit.
    assert_eq!(f.worktrees.pin_base("run", "main").unwrap(), f.second);
    let wt = f.worktrees.prepare("run", "step", &pinned).unwrap();
    assert_eq!(
        std::fs::read_to_string(wt.path.join("a.txt")).unwrap(),
        "one\nTWO\n"
    );
    let commit = f
        .worktrees
        .commit(&wt, "On the pinned base", &Signature::default())
        .unwrap();
    assert_eq!(
        f.worktrees.repo().parents(&commit.commit).unwrap(),
        vec![f.second.clone()]
    );
    // Another run admitted now gets the branch as it is now.
    assert_eq!(f.worktrees.pin_base("later", "main").unwrap(), moved);
    // A commit ID, whole or abbreviated, pins that commit.
    assert_eq!(
        f.worktrees.pin_base("by-id", &f.first[..10]).unwrap(),
        f.first
    );
}

#[test]
fn cleanup_keeps_only_what_an_accepted_candidate_names() {
    let f = fixture("cleanup");
    let wt = f.worktrees.prepare("run", "step", &f.first).unwrap();
    write(&wt.path.join("a.txt"), "attempt one\n");
    let orphan = f
        .worktrees
        .commit(&wt, "attempt one", &Signature::default())
        .unwrap();
    // The step is retried: a crash came after its commit and before its
    // checkpoint.
    let wt = f.worktrees.prepare("run", "step", &f.first).unwrap();
    write(&wt.path.join("a.txt"), "attempt two\n");
    let accepted = f
        .worktrees
        .commit(&wt, "attempt two", &Signature::default())
        .unwrap();
    assert_eq!((orphan.attempt, accepted.attempt), (1, 2));
    let other = f.worktrees.prepare("run", "other", &f.first).unwrap();
    let kept_elsewhere = f
        .worktrees
        .prepare("another-run", "step", &f.first)
        .unwrap();
    f.worktrees.pin_base("run", &f.first).unwrap();

    let report = f
        .worktrees
        .cleanup("run", std::slice::from_ref(&accepted.reference))
        .unwrap();
    assert_eq!(report.refs, std::slice::from_ref(&orphan.reference));
    assert_eq!(report.worktrees.len(), 2);
    assert!(!wt.path.exists() && !other.path.exists());
    assert!(kept_elsewhere.path.exists(), "another run's worktree stays");
    let repo = f.worktrees.repo();
    let left: Vec<String> = repo
        .refs("refs/atomic/")
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(
        left,
        ["refs/atomic/run/base", "refs/atomic/run/steps/step/2"]
    );
    // git's own records of the removed worktrees are gone too.
    let listed = git(&f.project, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains(wt.path.to_str().unwrap()), "{listed}");
    assert!(
        listed.contains(kept_elsewhere.path.to_str().unwrap()),
        "{listed}"
    );
    // Cleaning up again finds nothing more.
    assert_eq!(
        f.worktrees.cleanup("run", &[accepted.reference]).unwrap(),
        Default::default()
    );
}

#[test]
fn cleanup_removes_an_orphan_ref_even_when_it_names_the_accepted_commit() {
    let f = fixture("cleanup-identical-commit");
    let wt = f.worktrees.prepare("run", "step", &f.first).unwrap();
    write(&wt.path.join("a.txt"), "same attempt result\n");
    let first = f
        .worktrees
        .commit(&wt, "same result", &Signature::default())
        .unwrap();
    let second = "refs/atomic/run/steps/step/2".to_string();
    f.worktrees
        .repo()
        .create_ref(&second, &first.commit)
        .unwrap();
    let report = f
        .worktrees
        .cleanup("run", std::slice::from_ref(&second))
        .unwrap();
    assert_eq!(report.refs, [first.reference]);
    let refs = f.worktrees.repo().refs("refs/atomic/run/steps/").unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, second);
    assert_eq!(refs[0].sha, first.commit);
}

#[test]
fn run_step_resets_runs_and_commits() {
    let f = fixture("run-step");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let request = StepRequest {
            run: "run".into(),
            step: "implement".into(),
            atom: "implement-it".into(),
            base: f.first.clone(),
            message: "Implement it".into(),
            signature: Signature::default(),
        };
        let (out, commit) = f
            .worktrees
            .run_step(request.clone(), |dir| async move {
                std::fs::write(dir.join("a.txt"), "implemented\n").map_err(|e| e.to_string())?;
                Ok::<_, String>(7)
            })
            .await
            .unwrap();
        assert_eq!(out, 7);
        assert_eq!(
            f.worktrees.repo().parents(&commit.commit).unwrap(),
            vec![f.first.clone()]
        );
        assert_eq!(
            support::show(&f.project, &commit.commit, "a.txt"),
            "implemented\n"
        );

        // A failing body commits nothing.
        let failed = f
            .worktrees
            .run_step(request, |dir| async move {
                std::fs::write(dir.join("a.txt"), "broken\n").unwrap();
                Err::<(), _>("the atom failed")
            })
            .await;
        assert!(matches!(failed, Err(StepError::Body("the atom failed"))));
        let attempts = f
            .worktrees
            .repo()
            .refs("refs/atomic/run/steps/implement/")
            .unwrap();
        assert_eq!(attempts.len(), 1);
    });
}
