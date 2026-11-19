// SPDX-License-Identifier: Apache-2.0

//! A code-changing step on durare, in its own worktree, against the scripted
//! fake model:
//!
//! - killing the process mid-atom, after the model answered and the atom
//!   half-edited its worktree, then recovering: the retried step starts from
//!   the base commit, not the half-edit, and yields one accepted candidate
//!   whose commit's only parent is the base;
//! - killing it after the commit and before the step's checkpoint: the retry
//!   commits again, one candidate is accepted, and retention cleanup removes
//!   the orphaned attempt's ref and every worktree;
//! - the commit is recorded in the candidate's metadata by atomic.
//!
//! The fake model counts calls: every attempt spends one, and a finished
//! step is never run again.

#![allow(clippy::unwrap_used)]

use atomic_atoms::{ChangeRequest, DraftTask};
use atomic_core::AtomConfig;
use atomic_core::codex::ModelEndpoint;
use atomic_core::git::{Git, Signature};
use atomic_core::lite::LiteRuntime;
use atomic_core::runtime::run_lite_atom;
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::worktree::{StepError, StepRequest, Worktrees};
use atomic_durable::{
    DurableError, DurableRuntime, Durare, Proposal, Published, WorkflowContext, failpoint, run_atom,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const WORKFLOW: &str = "test.worktree-step";
const DRAFT: &str = r#"{"title": "Charge for every unit in the cart", "priority": 1, "criteria": ["Two units at 125 cents total 250 cents."]}"#;
const BASE_TEXT: &str = "fn total() -> u32 { 125 }\n";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Run {
    base: String,
}

/// What the step's atom reports: what it found when it started, and what it
/// wrote.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Implemented {
    started_clean: bool,
    title: String,
}

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

fn scratch(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("atomic-worktree-step-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let project = dir.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(dir.join("state")).unwrap();
    plain_git(&project, &["init", "-q"]);
    std::fs::write(project.join("lib.rs"), BASE_TEXT).unwrap();
    plain_git(&project, &["add", "lib.rs"]);
    plain_git(&project, &["commit", "-q", "-m", "base"]);
    std::fs::canonicalize(dir).unwrap()
}

fn worktrees(dir: &Path) -> Arc<Worktrees> {
    let repo = Git::find().unwrap().discover(&dir.join("project")).unwrap();
    Arc::new(Worktrees::new(repo, dir.join("state/worktrees")))
}

fn failed(e: impl std::fmt::Display) -> DurableError {
    DurableError::Failed(e.to_string())
}

/// One code-changing step: in its worktree, the atom checks what it starts
/// from, half-edits a file, asks the model, then writes its change. atomic
/// commits after the atom ends and records the commit in the candidate.
fn register(rt: &mut Durare, dir: &Path, base_url: &str) {
    let worktrees = worktrees(dir);
    let lite = LiteRuntime::new(
        ModelEndpoint {
            base_url: base_url.to_owned(),
            model: "synthetic".into(),
            api_key_env: None,
        },
        AtomConfig::default(),
    );
    rt.register(
        WORKFLOW,
        move |ctx: atomic_durable::DurareContext, run: Run| {
            let (worktrees, lite) = (worktrees.clone(), lite.clone());
            async move {
                let request = StepRequest {
                    run: ctx.workflow_id().to_owned(),
                    step: "implement".into(),
                    base: run.base,
                    message: "Implement the change".into(),
                    signature: Signature::default(),
                };
                run_atom(
                    &ctx,
                    "implement",
                    move || {
                        let (worktrees, lite, request) =
                            (worktrees.clone(), lite.clone(), request.clone());
                        async move {
                            let step = worktrees.run_step(request, |tree| async move {
                                let started_clean = !tree.join("half-edit.txt").exists()
                                    && std::fs::read_to_string(tree.join("lib.rs")).ok().as_deref()
                                        == Some(BASE_TEXT);
                                std::fs::write(tree.join("half-edit.txt"), "half")
                                    .map_err(failed)?;
                                std::fs::write(tree.join("lib.rs"), "broken").map_err(failed)?;
                                let ask = ChangeRequest {
                                    request: "Two units at 1.25 should cost 2.50.".into(),
                                };
                                let (task, metadata) = run_lite_atom(&DraftTask, &ask, &lite)
                                    .await
                                    .map_err(failed)?;
                                failpoint("worktree-step:mid-atom");
                                std::fs::remove_file(tree.join("half-edit.txt")).map_err(failed)?;
                                std::fs::write(
                                    tree.join("lib.rs"),
                                    format!(
                                        "// {}\nfn total(n: u32) -> u32 {{ 125 * n }}\n",
                                        task.title
                                    ),
                                )
                                .map_err(failed)?;
                                Ok::<_, DurableError>((
                                    Implemented {
                                        started_clean,
                                        title: task.title,
                                    },
                                    metadata,
                                ))
                            });
                            let ((out, metadata), commit) = step.await.map_err(|e| match e {
                                StepError::Body(e) => e,
                                StepError::Worktree(e) => failed(e),
                            })?;
                            failpoint("worktree-step:after-commit");
                            let mut metadata = serde_json::to_value(metadata).map_err(failed)?;
                            commit.record(&mut metadata);
                            Ok(Proposal {
                                output: serde_json::to_value(out).map_err(failed)?,
                                metadata,
                                writes: Vec::new(),
                            })
                        }
                    },
                    Vec::new(),
                )
                .await
            }
        },
    );
}

async fn engine(dir: &Path, base_url: &str) -> Durare {
    let mut rt = Durare::sqlite(&dir.join("state/atomic.db"))
        .await
        .expect("engine");
    register(&mut rt, dir, base_url);
    rt
}

const ENV_ROLE: &str = "ATOMIC_WT_ROLE";
const ENV_DIR: &str = "ATOMIC_WT_DIR";
const ENV_URL: &str = "ATOMIC_WT_URL";
const ENV_BASE: &str = "ATOMIC_WT_BASE";
const RUN: &str = "wt-run";

/// Not a test on its own: the parent runs it in a child process.
#[test]
#[ignore = "run by the kill tests in a child process"]
fn worktree_step_child() {
    let (Ok(role), Ok(dir), Ok(url), Ok(base)) = (
        std::env::var(ENV_ROLE),
        std::env::var(ENV_DIR),
        std::env::var(ENV_URL),
        std::env::var(ENV_BASE),
    ) else {
        return;
    };
    on_codex_runtime(async move {
        let dir = PathBuf::from(dir);
        let rt = engine(&dir, &url).await;
        rt.launch().await.expect("launch");
        if role == "start" {
            rt.start(WORKFLOW, RUN, Run { base }).await.expect("start");
        } else {
            rt.recover().await.expect("recover");
        }
        let out: Published = rt.result(RUN).await.expect("finishes");
        std::fs::write(
            dir.join("result.json"),
            serde_json::to_string(&out).expect("encode"),
        )
        .expect("write the result");
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

fn child(dir: &Path, url: &str, base: &str, role: &str, failpoint: Option<&str>) -> bool {
    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args(["--exact", "worktree_step_child", "--ignored", "--nocapture"])
        .env(ENV_ROLE, role)
        .env(ENV_DIR, dir)
        .env(ENV_URL, url)
        .env(ENV_BASE, base)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(dir.join(format!("{role}.log"))).expect("log"));
    match failpoint {
        Some(f) => cmd.env("ATOMIC_FAILPOINT", f),
        None => cmd.env_remove("ATOMIC_FAILPOINT"),
    };
    cmd.status().expect("run the child").success()
}

/// Kill a first process at `failpoint`, recover in a second, and return the
/// published result, the model's calls and the worktrees.
async fn kill_and_recover(tag: &str, failpoint: &str) -> (PathBuf, String, Published, usize) {
    let model = FakeModel::start(vec![Reply::Text(DRAFT.into()), Reply::Text(DRAFT.into())])
        .await
        .expect("fake model");
    let dir = scratch(tag);
    let url = model.base_url();
    let wts = worktrees(&dir);
    let base = wts.pin_base(RUN, "main").unwrap();

    let (d, u, b, f) = (dir.clone(), url.clone(), base.clone(), failpoint.to_owned());
    let survived = tokio::task::spawn_blocking(move || child(&d, &u, &b, "start", Some(&f)))
        .await
        .unwrap();
    assert!(!survived, "the first process was killed at {failpoint}");
    assert_eq!(
        model.paths().len(),
        1,
        "the first attempt asked the model once: {}",
        std::fs::read_to_string(dir.join("start.log")).unwrap_or_default()
    );
    assert!(!dir.join("result.json").exists());

    let (d, u, b) = (dir.clone(), url.clone(), base.clone());
    let finished = tokio::task::spawn_blocking(move || child(&d, &u, &b, "recover", None))
        .await
        .unwrap();
    assert!(
        finished,
        "the second process finished: {}",
        std::fs::read_to_string(dir.join("recover.log")).unwrap_or_default()
    );
    let published: Published =
        serde_json::from_str(&std::fs::read_to_string(dir.join("result.json")).expect("result"))
            .expect("decode");
    (dir, base, published, model.paths().len())
}

/// The step's recorded operations and its published candidates.
async fn record(dir: &Path) -> (Vec<String>, i64) {
    let rt = Durare::sqlite(&dir.join("state/atomic.db"))
        .await
        .expect("engine");
    let names = rt
        .steps(RUN)
        .await
        .expect("steps")
        .into_iter()
        .map(|s| s.name)
        .collect();
    use sqlx::Connection;
    let mut db = sqlx::SqliteConnection::connect(&format!(
        "sqlite://{}",
        dir.join("state/atomic.db").display()
    ))
    .await
    .expect("open the store");
    let publications: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM atomic_publications WHERE publication_key LIKE ?")
            .bind(format!("{RUN}:%"))
            .fetch_one(&mut db)
            .await
            .expect("count publications");
    (names, publications)
}

fn check_commit(dir: &Path, base: &str, published: &Published) -> String {
    let out: Implemented = serde_json::from_value(published.output.clone()).unwrap();
    assert!(
        out.started_clean,
        "the retried attempt started from the base"
    );
    let commit = published.metadata["commit"].as_str().unwrap().to_owned();
    assert_eq!(published.metadata["base"], base);
    let repo = worktrees(dir).repo().clone();
    assert_eq!(repo.parents(&commit).unwrap(), vec![base.to_owned()]);
    assert_eq!(repo.changed_paths(base, &commit).unwrap(), ["lib.rs"]);
    let project = dir.join("project");
    let text = plain_git(&project, &["show", &format!("{commit}:lib.rs")]);
    assert_eq!(
        text,
        "// Charge for every unit in the cart\nfn total(n: u32) -> u32 { 125 * n }\n"
    );
    commit
}

fn attempt_refs(dir: &Path) -> Vec<(String, String)> {
    worktrees(dir)
        .repo()
        .refs(&format!("refs/atomic/{RUN}/steps/"))
        .unwrap()
        .into_iter()
        .map(|r| (r.name, r.sha))
        .collect()
}

#[test]
fn a_kill_mid_atom_retries_from_the_base_and_commits_once() {
    on_codex_runtime(async {
        let (dir, base, published, calls) =
            kill_and_recover("mid-atom", "worktree-step:mid-atom").await;
        assert_eq!(calls, 2, "one model call per attempt");
        let commit = check_commit(&dir, &base, &published);
        let (steps, publications) = record(&dir).await;
        assert_eq!(steps, ["implement.turn", "implement.publish"]);
        assert_eq!(publications, 1, "one accepted candidate");
        // The killed attempt never committed: one attempt, one ref.
        assert_eq!(
            attempt_refs(&dir),
            [(format!("refs/atomic/{RUN}/steps/implement/1"), commit)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_kill_after_the_commit_leaves_an_orphan_that_cleanup_removes() {
    on_codex_runtime(async {
        let (dir, base, published, calls) =
            kill_and_recover("after-commit", "worktree-step:after-commit").await;
        assert_eq!(calls, 2, "one model call per attempt");
        let commit = check_commit(&dir, &base, &published);
        let (_, publications) = record(&dir).await;
        assert_eq!(publications, 1, "one accepted candidate");
        let refs = attempt_refs(&dir);
        assert_eq!(
            refs.len(),
            2,
            "the killed attempt's commit is an orphan: {refs:?}"
        );
        assert_eq!(
            refs[1],
            (
                format!("refs/atomic/{RUN}/steps/implement/2"),
                commit.clone()
            )
        );

        // Retention: only what the accepted candidate names stays.
        let wts = worktrees(&dir);
        let report = wts.cleanup(RUN, std::slice::from_ref(&commit)).unwrap();
        assert_eq!(report.refs, [refs[0].0.clone()]);
        assert_eq!(report.worktrees, [wts.path(RUN, "implement")]);
        assert_eq!(attempt_refs(&dir), [refs[1].clone()]);
        assert_eq!(wts.base(RUN).unwrap().as_deref(), Some(base.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    });
}
