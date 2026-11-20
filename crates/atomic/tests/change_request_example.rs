// SPDX-License-Identifier: Apache-2.0

//! The change-request example (`examples/02-change-request`), as written,
//! through `run_pipeline` against the scripted fake model, under Codex's
//! real Linux sandbox:
//!
//! - the run stops at `approve-plan` after planning, and nothing after it
//!   runs until the local operator approves;
//! - `implement` runs in its own worktree at the run's base, where the
//!   model's command edits the code under `workspace-write`; atomic commits
//!   the edit on the base after the turn and hands the commit to `review`;
//! - the run stops again at `accept-change`, showing that commit and the
//!   review, and returns the review once it is approved.
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot start
//! fails the test.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![recursion_limit = "512"]

mod support;

use atomic_app::pipeline::{PIPELINE, PipelineRun, Services, prepare, register_pipeline};
use atomic_core::artifact::LocalStore;
use atomic_core::codex::{CodexPaths, ModelEndpoint, StateGuard};
use atomic_core::git::Git;
use atomic_core::lite::LiteRuntime;
use atomic_core::runtime::{Artifacts, CodexRuntime, Runtimes};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::worktree::Worktrees;
use atomic_core::{AtomConfig, atom, collections, project, workflow};
use atomic_durable::gate::{self, DecisionCommand, Deployment, GateFilter, Principal, Verdict};
use atomic_durable::{DurableRuntime, Durare, RunStatus};
use atomic_schema::registry::Registry;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/02-change-request")
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("directory");
    for entry in std::fs::read_dir(source).expect("source") {
        let entry = entry.expect("entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            // `atomic check` writes the generated declarations afresh.
            if entry.file_name() == "types" && source.ends_with(".atomic") {
                continue;
            }
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy");
        }
    }
}

fn plain_git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new(Git::find().expect("git").program())
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
        .expect("git");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout)
        .expect("UTF-8")
        .trim()
        .to_owned()
}

/// The example and its seed code in one new repository, outside the system
/// temporary directory (which `workspace-write` leaves writable), with one
/// commit: the run's base.
fn repository(dir: &Path, layout: &str) -> (PathBuf, String) {
    let root = dir.join("project");
    let project = if layout == "member" {
        root.join("member")
    } else {
        root.clone()
    };
    copy_tree(&example(), &project);
    copy_tree(&example().join("fixtures/repo"), &project);
    let check = std::process::Command::new(env!("CARGO_BIN_EXE_atomic"))
        .arg("check")
        .current_dir(&project)
        .env("ATOMIC_HOME", dir.join("atomic-home"))
        // None of the developer's own configuration, and no model found.
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("atomic check");
    assert!(
        check.status.success(),
        "atomic check: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    plain_git(&root, &["init", "-q"]);
    plain_git(&root, &["add", "."]);
    plain_git(&root, &["commit", "-q", "-m", "baseline"]);
    let base = plain_git(&root, &["rev-parse", "HEAD"]);
    if layout == "primary" {
        return (project, base);
    }
    let linked = if layout == "nested" {
        root.join(".worktrees/source")
    } else {
        dir.join("linked")
    };
    plain_git(
        &root,
        &[
            "worktree",
            "add",
            "--detach",
            linked.to_str().expect("path"),
            &base,
        ],
    );
    let project = if layout == "member" {
        linked.join("member")
    } else {
        linked
    };
    (project, base)
}

/// The workflow, loaded and checked as `atomic check` does, and prepared.
fn pipeline(project: &Path) -> atomic_app::pipeline::Pipeline {
    let found = project::discover(project).expect("project");
    let files = collections::collect(&found).expect("collections");
    let mut registry = Registry::new();
    for schema in &files["schemas"] {
        registry.load_file(&schema.path).expect("schema");
    }
    let mut atoms = BTreeMap::new();
    for source in &files["atoms"] {
        let atom = atom::load(&source.path, &mut registry).expect("atom");
        atoms.insert(atom.name.clone(), atom);
    }
    let file = project.join("workflows/change-request.workflow.yaml");
    let loaded = workflow::load(&file, &atoms, &mut registry).expect("workflow");
    prepare(&loaded, &atoms, &local()).expect("prepares")
}

fn local() -> Deployment {
    Deployment::Local {
        operator: Principal::local("dev"),
    }
}

async fn engine(dir: &Path, project: &Path, base_url: &str) -> Durare {
    let endpoint = ModelEndpoint {
        base_url: base_url.to_owned(),
        model: "synthetic".into(),
        api_key_env: None,
    };
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_atomic"),
        dir.join("bin/codex-linux-sandbox"),
    )
    .expect("sandbox helper");
    let runtimes = Arc::new(Runtimes {
        codex: CodexRuntime {
            paths: CodexPaths::with_helpers(
                PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
                dir.join("bin/codex-linux-sandbox"),
            ),
            codex_home: dir.join("codex-home"),
            guard: StateGuard::deny_state_dir(dir.join("state")).expect("guard"),
            cwd: project.to_owned(),
            endpoint: endpoint.clone(),
            defaults: AtomConfig::default(),
            artifacts: Artifacts::none(),
            meter: atomic_core::usage::Metering::none(),
        },
        lite: LiteRuntime::new(endpoint, AtomConfig::default()),
        backend: Default::default(),
        clock: None,
    });
    let repo = Git::find()
        .expect("git")
        .discover(project)
        .expect("repository");
    let mut rt = Durare::sqlite(&dir.join("state/atomic.db"))
        .await
        .expect("engine");
    register_pipeline(
        &mut rt,
        Services {
            runtimes,
            store: LocalStore::open(dir.join("state/artifacts")).expect("artifact store"),
            deployment: local(),
            worktrees: Some(Arc::new(Worktrees::new(repo, dir.join("state/worktrees")))),
            budgets: Arc::default(),
            context: None,
        },
    );
    rt.launch().await.expect("launch");
    rt
}

/// Wait until `run` has an open gate, and return it.
async fn open_gate(rt: &Durare, run: &str) -> gate::Gate {
    for _ in 0..1200 {
        let open = gate::gates(
            rt,
            GateFilter {
                open_only: true,
                run: Some(run.to_owned()),
            },
        )
        .await
        .expect("gates");
        if let Some(g) = open.into_iter().next() {
            return g;
        }
        assert_eq!(
            rt.status(run).await.expect("status"),
            Some(RunStatus::Pending),
            "{run} ended before its gate opened"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{run} never opened a gate");
}

async fn approve(rt: &Durare, g: &gate::Gate) {
    let credential = gate::HumanCredential::new(Principal::local("dev"), "the operator's secret");
    gate::enroll(rt, &credential).await.expect("enroll");
    gate::decide(
        rt,
        &credential,
        DecisionCommand {
            command_id: format!("{}-approve", g.id),
            gate: g.id.clone(),
            verdict: Verdict::Approve,
            comment: None,
            proposal: g.proposal.clone(),
        },
    )
    .await
    .expect("approved");
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("JSON")
}

const RUN: &str = "change-1";

async fn run_example(layout: &str) {
    support::require_sandbox();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "atomic-change-request-{layout}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["bin", "codex-home", "state"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
    let dir = std::fs::canonicalize(&dir).expect("canonical");
    let (project, base) = repository(&dir, layout);
    let fixtures = example().join("fixtures");
    let mut task = read(&fixtures.join("plan-task/quantity.input.json"))["task"].clone();
    task["base"] = json!(base);
    let plan = read(&fixtures.join("plan-task/quantity.output.json"));
    let implemented = read(&fixtures.join("implement-step/quantity.output.json"));
    let review = json!({
        "verdict": "approve",
        "summary": "Two units now total 250 cents; one unit is unchanged.",
        "findings": []
    });
    let model = FakeModel::start(vec![
        Reply::Text(plan.to_string()),
        // The implementing model fixes the bug with a command in its
        // working directory, the step's worktree.
        Reply::Tool {
            name: "exec_command".into(),
            arguments: json!({
                "cmd": "sed -i 's/return unit_cents$/return unit_cents * quantity/' pricing.py",
                "yield_time_ms": 10000
            }),
        },
        Reply::Text(implemented.to_string()),
        Reply::Text(review.to_string()),
    ])
    .await
    .expect("fake model");
    let rt = engine(&dir, &project, &model.base_url()).await;
    rt.start(
        PIPELINE,
        RUN,
        PipelineRun {
            pipeline: pipeline(&project),
            input: task,
        },
    )
    .await
    .expect("start");

    // Planned, then stopped at the first gate.
    let g = open_gate(&rt, RUN).await;
    assert_eq!(g.step, "approve-plan");
    assert_eq!(g.title, "Approve bounded plan");
    assert_eq!(g.show, plan);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(model.calls(), 1, "nothing after the gate has run");
    approve(&rt, &g).await;

    // Implemented in a worktree, reviewed, then stopped at the second.
    let g = open_gate(&rt, RUN).await;
    assert_eq!(g.step, "accept-change");
    assert_eq!(model.calls(), 4);
    let commit = g.show["commit"].as_str().expect("the candidate").to_owned();
    assert_eq!(g.show["review"], review);
    assert_eq!(
        plain_git(&project, &["rev-parse", &format!("{commit}^")]),
        base
    );
    let path = if layout == "member" {
        "member/pricing.py"
    } else {
        "pricing.py"
    };
    let fixed = plain_git(&project, &["show", &format!("{commit}:{path}")]);
    assert!(
        fixed.contains("return unit_cents * quantity"),
        "the commit holds the model's edit: {fixed}"
    );
    // The project's own checkout is untouched.
    let own = std::fs::read_to_string(project.join("pricing.py")).expect("pricing.py");
    assert!(own.contains("return unit_cents\n"), "{own}");
    // The reviewer was given atomic's commit, not one a model wrote.
    assert!(model.requests()[3].to_string().contains(&commit));
    approve(&rt, &g).await;

    let out: Value = rt.result(RUN).await.expect("finishes");
    assert_eq!(out, review);
    assert_eq!(model.calls(), 4);
    rt.shutdown(std::time::Duration::from_secs(5))
        .await
        .expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_change_request_example_stops_at_its_gates_and_continues_after_approval() {
    on_codex_runtime(run_example("primary"));
}

#[test]
fn a_nested_linked_checkout_runs_in_its_attempt_root() {
    on_codex_runtime(run_example("nested"));
}

#[test]
fn a_member_project_in_a_linked_checkout_runs_in_its_attempt_subdirectory() {
    on_codex_runtime(run_example("member"));
}
