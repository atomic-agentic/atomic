// SPDX-License-Identifier: Apache-2.0

//! `run_pipeline` against the scripted fake model, on workflow and atom files
//! written while the test runs, so nothing about them is compiled in:
//!
//! - a three-step workflow of file-defined atoms runs, each step on what the
//!   steps before it published, and returns its `returns` expression;
//! - a second workflow over the same atoms, written after the first ran, runs
//!   in the same process;
//! - a finished run's result is read back from the store by a new engine, and
//!   no atom runs again;
//! - an input that breaks the workflow's contract fails before any model call;
//! - a workflow the interpreter cannot run yet is refused before it starts;
//! - every model call is metered: one settled ledger attempt per call, a
//!   workflow budget with no room parks the run before any call, and a dollar
//!   cap on an unpriced model fails the run before any call;
//! - a gate stops the run until the local operator decides: an approval lets
//!   the steps after it run, a rejection fails the run before any of them,
//!   and a process killed while waiting leaves the gate waiting.

// Codex's turn futures nest deeply inside the workflow's; proving them Send
// needs more than the default query depth.
#![recursion_limit = "512"]

use atomic_app::pipeline::{
    PIPELINE, Pinned, Pipeline, PipelineRun, SNAPSHOT_MEDIA_TYPE, Services, prepare,
    register_pipeline,
};
use atomic_core::artifact::{ArtifactRef, ArtifactStore, LocalStore};
use atomic_core::codex::{CodexPaths, ModelEndpoint};
use atomic_core::git::Git;
use atomic_core::lite::LiteRuntime;
use atomic_core::runtime::{Artifacts, CodexRuntime, Runtimes};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::worktree::Worktrees;
use atomic_core::{AtomConfig, atom, workflow};
use atomic_durable::gate::{self, DecisionCommand, Deployment, GateFilter, Principal, Verdict};
use atomic_durable::{DurableRuntime, Durare, RunStatus};
use atomic_schema::registry::Registry;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn scratch(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("atomic-pipeline-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["home", "cwd", "project/atoms", "project/workflows"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
    dir
}

/// A lite atom with sidecar schemas, written into `project/atoms`.
fn write_atom(project: &Path, name: &str, body: &str, input: Value, output: Value) {
    let atoms = project.join("atoms");
    std::fs::write(
        atoms.join(format!("{name}.atom.astro")),
        format!(
            "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/{name}';\n\
             import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
             const {{ input }}: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = {{ description: '{name}' }};\n\
             ---\n{body}\n"
        ),
    )
    .expect("atom");
    for (role, schema) in [("input", input), ("output", output)] {
        std::fs::write(
            atoms.join(format!("{name}.{role}.schema.json")),
            schema.to_string(),
        )
        .expect("schema");
    }
}

fn object(properties: Value) -> Value {
    let required: Vec<String> = properties
        .as_object()
        .expect("properties")
        .keys()
        .cloned()
        .collect();
    json!({"type": "object", "additionalProperties": false, "required": required, "properties": properties})
}

/// A project of three atoms: read a note, rank it, write a one-line summary.
fn project(dir: &Path) -> PathBuf {
    let project = dir.join("project");
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: notes\n").expect("manifest");
    write_atom(
        &project,
        "read-note",
        "Give the note a short title and count its words.\nNote: {input.text}",
        object(json!({"text": {"type": "string"}})),
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
    );
    write_atom(
        &project,
        "rank-note",
        "Rank the note from 1 (urgent) to 3.\nTitle: {input.title}\nWords: {input.words}",
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
        object(json!({"priority": {"type": "integer", "minimum": 1, "maximum": 3}})),
    );
    write_atom(
        &project,
        "summarize-note",
        "Write one line for the triage board.\nTitle: {input.title}\nPriority: {input.priority}",
        object(json!({"title": {"type": "string"}, "priority": {"type": "integer"}})),
        object(json!({"line": {"type": "string"}})),
    );
    project
}

const TRIAGE: &str = "\
atomic: 1
name: triage-note
input: { $ref: ../atoms/read-note.input.schema.json }
output: { $ref: ../atoms/summarize-note.output.schema.json }
steps:
  read:
    atom: read-note
    with: { text: input.text }
  rank:
    after: [read]
    atom: rank-note
    with: { title: steps.read.output.title, words: steps.read.output.words }
  summarize:
    after: [rank]
    atom: summarize-note
    with: { title: steps.read.output.title, priority: steps.rank.output.priority }
returns: steps.summarize.output
";

/// Load the project's atoms and one workflow file, as `atomic check` does,
/// and prepare the workflow to run.
fn load(project: &Path, workflow_file: &str, text: &str) -> Pipeline {
    let file = project.join("workflows").join(workflow_file);
    std::fs::write(&file, text).expect("workflow");
    let mut registry = Registry::new();
    let mut atoms = BTreeMap::new();
    for entry in std::fs::read_dir(project.join("atoms")).expect("atoms") {
        let path = entry.expect("entry").path();
        if path.to_string_lossy().ends_with(".atom.astro") {
            let atom = atom::load(&path, &mut registry).expect("atom loads");
            atoms.insert(atom.name.clone(), atom);
        }
    }
    let loaded = workflow::load(&file, &atoms, &mut registry).expect("workflow loads");
    prepare(&loaded, &atoms, &local()).expect("prepares")
}

/// One developer on one machine.
fn local() -> Deployment {
    Deployment::Local {
        operator: Principal::local("dev"),
    }
}

fn runtimes(dir: &Path, base_url: &str) -> Arc<Runtimes> {
    runtimes_in(dir, &dir.join("cwd"), base_url)
}

fn runtimes_in(dir: &Path, cwd: &Path, base_url: &str) -> Arc<Runtimes> {
    let endpoint = ModelEndpoint {
        base_url: base_url.to_owned(),
        model: "synthetic".into(),
        api_key_env: None,
    };
    Arc::new(Runtimes {
        codex: CodexRuntime {
            paths: CodexPaths::without_helpers(),
            codex_home: dir.join("home"),
            cwd: cwd.to_owned(),
            endpoint: endpoint.clone(),
            defaults: AtomConfig::default(),
            artifacts: Artifacts::none(),
            meter: atomic_core::usage::Metering::none(),
        },
        lite: LiteRuntime::new(endpoint, AtomConfig::default()),
    })
}

async fn engine(dir: &Path, base_url: &str) -> Durare {
    let mut rt = Durare::sqlite(&dir.join("atomic.db"))
        .await
        .expect("engine");
    let store = LocalStore::open(dir.join("artifacts")).expect("artifact store");
    // A project in a git repository gets worktrees for its worktree steps.
    let project = dir.join("project");
    let (runtimes, worktrees) = if project.join(".git").exists() {
        let repo = Git::find()
            .expect("git")
            .discover(&project)
            .expect("repository");
        (
            runtimes_in(dir, &project, base_url),
            Some(Arc::new(Worktrees::new(repo, dir.join("state/worktrees")))),
        )
    } else {
        (runtimes(dir, base_url), None)
    };
    register_pipeline(
        &mut rt,
        Services {
            runtimes,
            store,
            deployment: local(),
            worktrees,
            budgets: Arc::default(),
        },
    );
    rt.launch().await.expect("launch");
    rt
}

const READ: &str = r#"{"title": "Export crashes on accents", "words": 7}"#;
const RANK: &str = r#"{"priority": 1}"#;
const SUMMARY: &str = r#"{"line": "P1: Export crashes on accents"}"#;

#[test]
fn a_three_step_workflow_of_file_defined_atoms_runs() {
    on_codex_runtime(async {
        let dir = scratch("three");
        let project = project(&dir);
        let pipeline = load(&project, "triage.workflow.yaml", TRIAGE);
        assert_eq!(
            pipeline.steps.iter().map(|s| s.id()).collect::<Vec<_>>(),
            ["read", "rank", "summarize"]
        );
        let model = FakeModel::start(vec![
            Reply::Text(READ.into()),
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
        ])
        .await
        .expect("fake model");
        let rt = engine(&dir, &model.base_url()).await;
        let run = PipelineRun {
            pipeline,
            input: json!({"text": "Exporting a file named café.txt crashes the app."}),
        };
        rt.start(PIPELINE, "triage-1", run).await.expect("start");
        let out: Value = rt.result("triage-1").await.expect("finishes");

        assert_eq!(out, json!({"line": "P1: Export crashes on accents"}));
        assert_eq!(model.calls(), 3, "one model call per step");
        // Each call was admitted and settled once in the usage ledger.
        let attempts = rt.ledger().attempts("triage-1").await.expect("attempts");
        assert_eq!(attempts.len(), 3, "{attempts:?}");
        assert!(
            attempts
                .iter()
                .all(|a| a.state == "settled" && a.usage == Some((1, 1))),
            "{attempts:?}"
        );
        let requests = model.requests();
        // Each step got what the steps before it published.
        assert!(requests[0].to_string().contains("café.txt"));
        assert!(
            requests[1]
                .to_string()
                .contains("Export crashes on accents")
        );
        let third = requests[2].to_string();
        assert!(third.contains("Export crashes on accents") && third.contains("Priority"));
        // Every step is recorded as its turn and its publication.
        let names: Vec<String> = rt
            .steps("triage-1")
            .await
            .expect("steps")
            .into_iter()
            .map(|s| s.name)
            .collect();
        for step in ["read", "rank", "summarize"] {
            for part in ["turn", "publish"] {
                let name = format!("{step}.{part}");
                assert!(names.contains(&name), "{name} in {names:?}");
            }
        }
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");

        // A different workflow over the same atoms, written now, runs in the
        // same process: the interpreter needs no rebuild.
        let shorter = TRIAGE
            .replace("name: triage-note", "name: quick-rank")
            .replace(
                "output: { $ref: ../atoms/summarize-note.output.schema.json }",
                "output: { $ref: ../atoms/rank-note.output.schema.json }",
            )
            .replace(
                "  summarize:\n    after: [rank]\n    atom: summarize-note\n    with: { title: steps.read.output.title, priority: steps.rank.output.priority }\n",
                "",
            )
            .replace("returns: steps.summarize.output", "returns: steps.rank.output");
        let pipeline = load(&project, "quick.workflow.yaml", &shorter);
        let model = FakeModel::start(vec![Reply::Text(READ.into()), Reply::Text(RANK.into())])
            .await
            .expect("fake model");
        let rt = engine(&dir, &model.base_url()).await;
        let run = PipelineRun {
            pipeline,
            input: json!({"text": "Exporting a file named café.txt crashes the app."}),
        };
        rt.start(PIPELINE, "quick-1", run).await.expect("start");
        let out: Value = rt.result("quick-1").await.expect("finishes");
        assert_eq!(out, json!({"priority": 1}));
        assert_eq!(model.calls(), 2);

        // A new engine on the same store reads the first run's result back,
        // and no atom runs again.
        let first: Value = rt.result("triage-1").await.expect("recorded");
        assert_eq!(first, json!({"line": "P1: Export crashes on accents"}));
        assert_eq!(rt.recover().await.expect("recover"), 0);
        assert_eq!(model.calls(), 2, "nothing reran");
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");
    });
}

#[test]
fn an_input_that_breaks_the_workflow_contract_fails_before_any_model_call() {
    on_codex_runtime(async {
        let dir = scratch("bad-input");
        let project = project(&dir);
        let pipeline = load(&project, "triage.workflow.yaml", TRIAGE);
        let model = FakeModel::start(vec![Reply::Text(READ.into())])
            .await
            .expect("fake model");
        let rt = engine(&dir, &model.base_url()).await;
        let run = PipelineRun {
            pipeline,
            input: json!({"text": 42}),
        };
        rt.start(PIPELINE, "bad-1", run).await.expect("start");
        let error = rt
            .result::<Value>("bad-1")
            .await
            .expect_err("refused")
            .to_string();
        assert!(error.contains("input does not fit its contract"), "{error}");
        assert_eq!(model.calls(), 0);
        assert!(matches!(
            rt.status("bad-1").await.expect("status"),
            Some(RunStatus::Failed(_))
        ));
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");
    });
}

/// The triage workflow with a gate between ranking and summarizing.
fn gated() -> String {
    TRIAGE.replace(
        "  summarize:\n    after: [rank]\n",
        "  check:\n    after: [rank]\n    gate:\n      title: \"'Check the rank of ' + steps.read.output.title\"\n      show: steps.rank.output\n      timeout: 24h\n  summarize:\n    after: [check]\n",
    )
}

/// Wait until `run` has an open gate, and return it.
async fn open_gate(rt: &Durare, run: &str) -> gate::Gate {
    for _ in 0..600 {
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
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{run} never opened a gate");
}

/// The local operator decides `g`.
async fn decide(rt: &Durare, g: &gate::Gate, verdict: Verdict, comment: &str) {
    gate::decide(
        rt,
        &Principal::local("dev"),
        DecisionCommand {
            command_id: format!("{}-{verdict:?}", g.id),
            gate: g.id.clone(),
            verdict,
            comment: Some(comment.into()),
            proposal: g.proposal.clone(),
        },
    )
    .await
    .expect("decided");
}

#[test]
fn a_gate_stops_the_run_until_it_is_approved() {
    on_codex_runtime(async {
        let dir = scratch("gate");
        let project = project(&dir);
        let pipeline = load(&project, "gated.workflow.yaml", &gated());
        assert_eq!(
            pipeline.steps.iter().map(|s| s.id()).collect::<Vec<_>>(),
            ["read", "rank", "check", "summarize"]
        );
        let model = FakeModel::start(vec![
            Reply::Text(READ.into()),
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
        ])
        .await
        .expect("fake model");
        let rt = engine(&dir, &model.base_url()).await;
        let run = PipelineRun {
            pipeline,
            input: json!({"text": "Exporting a file named café.txt crashes the app."}),
        };
        rt.start(PIPELINE, "gate-1", run).await.expect("start");

        let g = open_gate(&rt, "gate-1").await;
        assert_eq!(g.title, "Check the rank of Export crashes on accents");
        assert_eq!(g.show, json!({"priority": 1}));
        assert_eq!(g.workflow, "triage-note");
        assert_eq!(g.approvers, [Principal::local("dev")]);
        assert!(g.deadline.is_some(), "a 24h deadline");
        // Waiting at the gate: the step after it has not run.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(model.calls(), 2, "read and rank ran; summarize waits");
        assert_eq!(
            rt.status("gate-1").await.expect("status"),
            Some(RunStatus::Pending)
        );

        decide(&rt, &g, Verdict::Approve, "rank looks right").await;
        let out: Value = rt.result("gate-1").await.expect("finishes");
        assert_eq!(out, json!({"line": "P1: Export crashes on accents"}));
        assert_eq!(model.calls(), 3);
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");
    });
}

#[test]
fn a_rejected_gate_fails_the_run_before_the_steps_after_it() {
    on_codex_runtime(async {
        let dir = scratch("gate-reject");
        let project = project(&dir);
        let pipeline = load(&project, "gated.workflow.yaml", &gated());
        let model = FakeModel::start(vec![
            Reply::Text(READ.into()),
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
        ])
        .await
        .expect("fake model");
        let rt = engine(&dir, &model.base_url()).await;
        let run = PipelineRun {
            pipeline,
            input: json!({"text": "Exporting a file named café.txt crashes the app."}),
        };
        rt.start(PIPELINE, "reject-1", run).await.expect("start");
        let g = open_gate(&rt, "reject-1").await;
        decide(&rt, &g, Verdict::Reject, "this is a P2").await;
        let error = rt
            .result::<Value>("reject-1")
            .await
            .expect_err("rejected")
            .to_string();
        assert!(
            error.contains("gate `check`: rejected by local:dev: this is a P2"),
            "{error}"
        );
        assert_eq!(model.calls(), 2, "summarize never ran");
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");
    });
}

#[test]
fn a_gate_whose_approvers_do_not_resolve_is_refused_before_it_starts() {
    let dir = scratch("gate-approvers");
    let project = project(&dir);
    let file = project.join("workflows/gated.workflow.yaml");
    std::fs::write(&file, gated()).expect("workflow");
    let mut registry = Registry::new();
    let mut atoms = BTreeMap::new();
    for name in ["read-note", "rank-note", "summarize-note"] {
        let atom = atom::load(
            &project.join(format!("atoms/{name}.atom.astro")),
            &mut registry,
        )
        .expect("atom");
        atoms.insert(atom.name.clone(), atom);
    }
    let loaded = workflow::load(&file, &atoms, &mut registry).expect("a gate is valid");
    // A shared deployment with no default group: a gate naming none has no
    // one to decide it.
    let server = Deployment::Server {
        allowed: vec!["maintainers".into()],
        default: None,
    };
    let error = prepare(&loaded, &atoms, &server).expect_err("no approvers");
    assert!(error.message.contains("check"), "{error}");
}

#[test]
fn a_result_that_breaks_the_workflow_contract_fails_the_run() {
    on_codex_runtime(async {
        let dir = scratch("bad-result");
        let project = project(&dir);
        // The loader cannot prove a three-character limit either way, so the
        // run checks the result itself.
        let strict = TRIAGE.replace(
            "output: { $ref: ../atoms/summarize-note.output.schema.json }",
            "output: { type: object, required: [line], properties: { line: { type: string, maxLength: 3 } } }",
        );
        let pipeline = load(&project, "strict.workflow.yaml", &strict);
        let model = FakeModel::start(vec![
            Reply::Text(READ.into()),
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
        ])
        .await
        .expect("fake model");
        let rt = engine(&dir, &model.base_url()).await;
        let run = PipelineRun {
            pipeline,
            input: json!({"text": "Exporting a file named café.txt crashes the app."}),
        };
        rt.start(PIPELINE, "strict-1", run).await.expect("start");
        let error = rt
            .result::<Value>("strict-1")
            .await
            .expect_err("refused")
            .to_string();
        assert!(
            error.contains("result does not fit its contract"),
            "{error}"
        );
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");
    });
}

#[test]
fn a_worktree_for_an_atom_with_no_workspace_is_refused_before_it_starts() {
    let dir = scratch("worktree");
    let project = project(&dir);
    let file = project.join("workflows/worktree.workflow.yaml");
    std::fs::write(
        &file,
        TRIAGE.replace(
            "    with: { text: input.text }\n",
            "    with: { text: input.text }\n    worktree: true\n",
        ),
    )
    .expect("workflow");
    let mut registry = Registry::new();
    let mut atoms = BTreeMap::new();
    for name in ["read-note", "rank-note", "summarize-note"] {
        let atom = atom::load(
            &project.join(format!("atoms/{name}.atom.astro")),
            &mut registry,
        )
        .expect("atom");
        atoms.insert(atom.name.clone(), atom);
    }
    let loaded = workflow::load(&file, &atoms, &mut registry).expect("loads");
    // read-note is a codex atom; as an exec atom it has no workspace.
    let mut atoms = atoms;
    atoms.get_mut("read-note").expect("read-note").kind = "exec".into();
    let error = prepare(&loaded, &atoms, &local()).expect_err("no workspace");
    assert!(error.message.contains("only a codex atom"), "{error}");
}

const ENV_ROLE: &str = "ATOMIC_PIPELINE_ROLE";
const ENV_DIR: &str = "ATOMIC_PIPELINE_DIR";
const ENV_URL: &str = "ATOMIC_PIPELINE_URL";
const ENV_RUN: &str = "ATOMIC_PIPELINE_RUN";

/// Not a test on its own: the parent runs it in a child process.
#[test]
#[ignore = "run by a_killed_process_does_not_rerun_a_finished_step in a child process"]
fn pipeline_child() {
    let (Ok(role), Ok(dir), Ok(url)) = (
        std::env::var(ENV_ROLE),
        std::env::var(ENV_DIR),
        std::env::var(ENV_URL),
    ) else {
        return;
    };
    on_codex_runtime(async move {
        let dir = PathBuf::from(dir);
        let rt = engine(&dir, &url).await;
        if role.starts_with("start") {
            let project = dir.join("project");
            let run = match role.as_str() {
                "start" => PipelineRun {
                    pipeline: load(&project, "triage.workflow.yaml", TRIAGE),
                    input: json!({"text": "Exporting a file named café.txt crashes the app."}),
                },
                "start-sent" => PipelineRun {
                    pipeline: load(&project, "sent.workflow.yaml", &sent()),
                    input: json!({"text": "Exporting a file named café.txt crashes the app."}),
                },
                "start-gated" => PipelineRun {
                    pipeline: load(&project, "gated.workflow.yaml", &gated()),
                    input: json!({"text": "Exporting a file named café.txt crashes the app."}),
                },
                _ => PipelineRun {
                    pipeline: load(&project, "fix.workflow.yaml", FIX),
                    input: json!({"title": "Export crashes on accents", "base": plain_git(&project, &["rev-parse", "HEAD"])}),
                },
            };
            let id = std::env::var(ENV_RUN).expect("run id");
            rt.start(PIPELINE, &id, run).await.expect("start");
        } else {
            rt.recover().await.expect("recover");
        }
        let id = std::env::var(ENV_RUN).expect("run id");
        match rt.result::<Value>(&id).await {
            Ok(out) => {
                std::fs::write(dir.join("result.json"), out.to_string()).expect("write the result")
            }
            Err(e) => {
                std::fs::write(dir.join("error.txt"), e.to_string()).expect("write the error")
            }
        }
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");
    });
}

fn child(
    dir: &Path,
    url: &str,
    role: &str,
    run: &str,
    failpoint: Option<&str>,
) -> std::process::ExitStatus {
    let mut cmd = std::process::Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args(["--exact", "pipeline_child", "--ignored", "--nocapture"])
        .env(ENV_ROLE, role)
        .env(ENV_RUN, run)
        .env(ENV_DIR, dir)
        .env(ENV_URL, url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match failpoint {
        Some(f) => cmd.env("ATOMIC_FAILPOINT", f),
        None => cmd.env_remove("ATOMIC_FAILPOINT"),
    };
    cmd.status().expect("run the child")
}

#[test]
fn a_killed_process_does_not_rerun_a_finished_step() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(READ.into()),
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("killed");
        project(&dir);
        let url = model.base_url();

        // The first process dies right after the second step is published.
        let (d, u) = (dir.clone(), url.clone());
        let status = tokio::task::spawn_blocking(move || {
            child(&d, &u, "start", "killed-1", Some("pipeline:after:rank"))
        })
        .await
        .expect("child");
        assert!(!status.success(), "the first process was killed: {status}");
        assert_eq!(model.calls(), 2, "read and rank ran");
        assert!(!dir.join("result.json").exists());

        // A new process recovers the run and runs only the last step.
        let (d, u) = (dir.clone(), url.clone());
        let status =
            tokio::task::spawn_blocking(move || child(&d, &u, "recover", "killed-1", None))
                .await
                .expect("child");
        assert!(status.success(), "the second process finished: {status}");
        assert_eq!(model.calls(), 3, "read and rank were not run again");
        let out: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("result.json")).expect("result"),
        )
        .expect("decode");
        assert_eq!(out, json!({"line": "P1: Export crashes on accents"}));
    });
}

#[test]
fn a_process_killed_at_a_gate_leaves_it_waiting_for_the_decision() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(READ.into()),
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("gate-killed");
        project(&dir);
        let url = model.base_url();

        // The first process dies just after the gate opens.
        let (d, u) = (dir.clone(), url.clone());
        let status = tokio::task::spawn_blocking(move || {
            child(
                &d,
                &u,
                "start-gated",
                "gate-killed-1",
                Some("gate:after-open"),
            )
        })
        .await
        .expect("child");
        assert!(!status.success(), "the first process was killed: {status}");
        assert_eq!(model.calls(), 2);

        // The gate is still open; the operator approves it while no process
        // is running the workflow.
        let rt = engine(&dir, &url).await;
        let g = open_gate(&rt, "gate-killed-1").await;
        decide(&rt, &g, Verdict::Approve, "fine").await;
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");

        // A new process recovers the run, finds the approval, and runs only
        // the step after the gate.
        let (d, u) = (dir.clone(), url.clone());
        let status =
            tokio::task::spawn_blocking(move || child(&d, &u, "recover", "gate-killed-1", None))
                .await
                .expect("child");
        assert!(status.success(), "the second process finished: {status}");
        assert_eq!(model.calls(), 3, "read and rank were not run again");
        let out: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("result.json")).expect("result"),
        )
        .expect("decode");
        assert_eq!(out, json!({"line": "P1: Export crashes on accents"}));
    });
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

/// A project in a git repository, with an atom that works on a base commit
/// and one that reviews a commit, and a workflow that runs the first in a
/// worktree and hands its commit to the second.
fn repository_project(dir: &Path) -> (PathBuf, String) {
    let project = project(dir);
    write_atom(
        &project,
        "fix-note",
        "Fix the note.\nTitle: {input.title}\nBase: {input.base}",
        object(json!({"title": {"type": "string"}, "base": {"type": "string"}})),
        object(json!({"done": {"type": "boolean"}})),
    );
    // It changes code, so it is a codex atom that may write its workspace.
    let file = project.join("atoms/fix-note.atom.astro");
    let text = std::fs::read_to_string(&file).expect("atom");
    std::fs::write(
        &file,
        text.replace(
            "description: 'fix-note'",
            "description: 'fix-note', kind: 'codex', sandbox: 'workspace-write'",
        ),
    )
    .expect("atom");
    write_atom(
        &project,
        "check-fix",
        "Check the fix.\nCommit: {input.commit}",
        object(json!({"commit": {"type": "string"}})),
        object(json!({"ok": {"type": "boolean"}})),
    );
    plain_git(&project, &["init", "-q"]);
    plain_git(&project, &["add", "."]);
    plain_git(&project, &["commit", "-q", "-m", "base"]);
    let base = plain_git(&project, &["rev-parse", "HEAD"]);
    (project, base)
}

const FIX: &str = "\
atomic: 1
name: fix-and-check
input:
  type: object
  additionalProperties: false
  required: [title, base]
  properties:
    title: {type: string}
    base: {type: string}
output: { $ref: ../atoms/check-fix.output.schema.json }
steps:
  fix:
    atom: fix-note
    with: { title: input.title, base: input.base }
    worktree: true
  check:
    after: [fix]
    atom: check-fix
    with: { commit: steps.fix.metadata.commit }
returns: steps.check.output
";

#[test]
fn a_worktree_step_runs_at_its_base_and_atomic_commits_what_it_left() {
    on_codex_runtime(async {
        let dir = scratch("worktree-step");
        let (project, base) = repository_project(&dir);
        let pipeline = load(&project, "fix.workflow.yaml", FIX);
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done": true}"#.into()),
            Reply::Text(r#"{"ok": true}"#.into()),
        ])
        .await
        .expect("fake model");
        let rt = engine(&dir, &model.base_url()).await;
        let run = PipelineRun {
            pipeline,
            input: json!({"title": "Export crashes on accents", "base": base}),
        };
        rt.start(PIPELINE, "fix-1", run).await.expect("start");
        let out: Value = rt.result("fix-1").await.expect("finishes");
        assert_eq!(out, json!({"ok": true}));
        assert_eq!(model.calls(), 2);

        // Where the turn ran is not in the model's request at this Codex
        // revision; the sandboxed change-request test shows the model's edit
        // landing in the worktree and not in the project.
        let worktrees = dir.join("state/worktrees");
        let requests = model.requests();
        // atomic committed on the base, recorded it in the step's metadata,
        // and the next step received that commit.
        let fix = rt
            .steps("fix-1")
            .await
            .expect("steps")
            .into_iter()
            .find(|s| s.name == "fix.turn")
            .expect("fix's turn");
        let candidate = fix.output.expect("the turn's candidate");
        // The turn records either the candidate or that it parked.
        let metadata = &candidate["Candidate"]["proposal"]["metadata"];
        let commit = metadata["commit"].as_str().expect("a commit").to_owned();
        assert_eq!(metadata["base"], json!(base));
        assert_eq!(
            metadata["commit_ref"],
            json!("refs/atomic/fix-1/steps/fix/1")
        );
        assert_eq!(
            plain_git(&project, &["rev-parse", &format!("{commit}^")]),
            base
        );
        assert!(requests[1].to_string().contains(&commit));
        // The finished run's worktree is gone; its accepted commit keeps its ref.
        let left: Vec<_> = std::fs::read_dir(&worktrees)
            .map(|d| d.collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "worktrees left: {left:?}");
        assert_eq!(
            plain_git(&project, &["rev-parse", "refs/atomic/fix-1/steps/fix/1"]),
            commit
        );
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");
    });
}

#[test]
fn a_worktree_step_killed_after_its_commit_commits_again_and_keeps_one() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done": true}"#.into()),
            Reply::Text(r#"{"done": true}"#.into()),
            Reply::Text(r#"{"ok": true}"#.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("worktree-killed");
        let (project, base) = repository_project(&dir);
        let url = model.base_url();

        // The first process dies after atomic committed the step's work and
        // before the step's checkpoint.
        let (d, u) = (dir.clone(), url.clone());
        let status = tokio::task::spawn_blocking(move || {
            child(
                &d,
                &u,
                "start-fix",
                "fix-killed",
                Some("pipeline:after-commit"),
            )
        })
        .await
        .expect("child");
        assert!(!status.success(), "the first process was killed: {status}");
        assert_eq!(model.calls(), 1);
        let orphan = plain_git(
            &project,
            &["rev-parse", "refs/atomic/fix-killed/steps/fix/1"],
        );

        // The retry starts from the base again and makes a second commit;
        // that one is accepted, and cleanup drops the orphan's ref.
        let (d, u) = (dir.clone(), url.clone());
        let status =
            tokio::task::spawn_blocking(move || child(&d, &u, "recover", "fix-killed", None))
                .await
                .expect("child");
        assert!(status.success(), "the second process finished: {status}");
        assert_eq!(model.calls(), 3);
        let accepted = plain_git(
            &project,
            &["rev-parse", "refs/atomic/fix-killed/steps/fix/2"],
        );
        assert_eq!(
            plain_git(&project, &["rev-parse", &format!("{accepted}^")]),
            base
        );
        assert!(model.requests()[2].to_string().contains(&accepted));
        let refs = plain_git(
            &project,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/atomic/fix-killed/steps/",
            ],
        );
        assert_eq!(
            refs, "refs/atomic/fix-killed/steps/fix/2",
            "{orphan} was dropped"
        );
    });
}

/// An exec atom whose command reaches outside atomic: each run appends a
/// line to `sent.log` in the scratch directory, then reports on the note.
/// `external` is its `effects.external` declaration, if any.
fn write_send_note(dir: &Path, external: Option<&str>) {
    let project = dir.join("project");
    std::fs::create_dir_all(project.join("tools")).expect("tools");
    std::fs::write(
        project.join("tools/send.py"),
        "import json, os, sys\nnote = json.load(sys.stdin)\nwith open(os.environ['SENT_LOG'], 'a') as log:\n    log.write('sent\\n')\njson.dump({'title': 'Export crashes on accents', 'words': len(note['text'].split())}, sys.stdout)\n",
    )
    .expect("script");
    let effects = external
        .map(|e| format!(", effects: {{ external: '{e}' }}"))
        .unwrap_or_default();
    let atoms = project.join("atoms");
    std::fs::write(
        atoms.join("send-note.atom.astro"),
        format!(
            "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/send-note';\n\
             import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
             const {{ input }}: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = {{ description: 'Send the note.', kind: 'exec', exec: {{ command: ['python3', 'tools/send.py'] }}, env: {{ SENT_LOG: '{}' }}{effects} }};\n---\n",
            dir.join("sent.log").display()
        ),
    )
    .expect("atom");
    for role in ["input", "output"] {
        std::fs::copy(
            atoms.join(format!("read-note.{role}.schema.json")),
            atoms.join(format!("send-note.{role}.schema.json")),
        )
        .expect("schema");
    }
}

fn sent() -> String {
    TRIAGE
        .replace("name: triage-note", "name: sent-triage")
        .replace("atom: read-note", "atom: send-note")
}

fn sends(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("sent.log"))
        .map(|log| log.lines().count())
        .unwrap_or(0)
}

/// Kill a run of the sent-triage workflow just after its exec command ran
/// and before the step's checkpoint, then recover it in a new process.
async fn kill_after_the_command_and_recover(dir: &Path, url: &str, run: &'static str) {
    let (d, u) = (dir.to_owned(), url.to_owned());
    let status = tokio::task::spawn_blocking(move || {
        child(&d, &u, "start-sent", run, Some("pipeline:ran:read"))
    })
    .await
    .expect("child");
    assert!(!status.success(), "the first process was killed: {status}");
    assert_eq!(sends(dir), 1, "the command ran once before the kill");
    let (d, u) = (dir.to_owned(), url.to_owned());
    let status = tokio::task::spawn_blocking(move || child(&d, &u, "recover", run, None))
        .await
        .expect("child");
    assert!(status.success(), "the second process ended: {status}");
}

#[test]
fn an_interrupted_exec_step_with_no_effects_declaration_parks() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("parked");
        project(&dir);
        write_send_note(&dir, None);
        kill_after_the_command_and_recover(&dir, &model.base_url(), "parked-1").await;

        let error = std::fs::read_to_string(dir.join("error.txt")).expect("the run failed");
        assert!(error.contains("step `read`: outcome_unknown"), "{error}");
        assert_eq!(sends(&dir), 1, "the command did not run again");
        assert_eq!(model.calls(), 0, "no step after it ran");
    });
}

async fn interrupted_declared_exec_runs_again(external: &str) {
    let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
        .await
        .expect("fake model");
    let dir = scratch(external);
    project(&dir);
    write_send_note(&dir, Some(external));
    kill_after_the_command_and_recover(&dir, &model.base_url(), "declared-1").await;

    assert!(!dir.join("error.txt").exists());
    let out: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("result.json")).expect("result"))
            .expect("decode");
    assert_eq!(out, json!({"line": "P1: Export crashes on accents"}));
    assert_eq!(sends(&dir), 2, "the command ran again after the crash");
    assert_eq!(model.calls(), 2);
}

#[test]
fn an_interrupted_exec_step_declared_idempotent_runs_again() {
    on_codex_runtime(interrupted_declared_exec_runs_again("idempotent"));
}

#[test]
fn an_interrupted_exec_step_declared_read_only_runs_again() {
    on_codex_runtime(interrupted_declared_exec_runs_again("read-only"));
}

#[test]
fn an_exec_step_that_was_not_interrupted_runs_once_whatever_it_declares() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("uninterrupted");
        let project = project(&dir);
        write_send_note(&dir, None);
        let rt = engine(&dir, &model.base_url()).await;
        let run = PipelineRun {
            pipeline: load(&project, "sent.workflow.yaml", &sent()),
            input: json!({"text": "Exporting a file named café.txt crashes the app."}),
        };
        rt.start(PIPELINE, "once-1", run).await.expect("start");
        let out: Value = rt.result("once-1").await.expect("finishes");
        assert_eq!(out, json!({"line": "P1: Export crashes on accents"}));
        assert_eq!(sends(&dir), 1);
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");
    });
}

#[test]
fn an_atom_edited_mid_run_does_not_change_the_run_in_flight() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(READ.into()),
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("pinned");
        let project = project(&dir);
        let url = model.base_url();

        // The first process dies right after the first step is published.
        let (d, u) = (dir.clone(), url.clone());
        let status = tokio::task::spawn_blocking(move || {
            child(&d, &u, "start", "pinned-1", Some("pipeline:after:read"))
        })
        .await
        .expect("child");
        assert!(!status.success(), "the first process was killed: {status}");
        assert_eq!(model.calls(), 1);

        // Someone edits the next atom while the run waits for recovery.
        let rank = project.join("atoms/rank-note.atom.astro");
        let original = std::fs::read_to_string(&rank).expect("read");
        std::fs::write(
            &rank,
            original.replace("Rank the note", "EDITED: rank the note"),
        )
        .expect("edit");

        // Recovery runs the rank step as it was when the run started.
        let (d, u) = (dir.clone(), url.clone());
        let status =
            tokio::task::spawn_blocking(move || child(&d, &u, "recover", "pinned-1", None))
                .await
                .expect("child");
        assert!(status.success(), "the second process finished: {status}");
        assert_eq!(model.calls(), 3);
        let asked = model.requests()[1].to_string();
        assert!(
            asked.contains("Rank the note") && !asked.contains("EDITED"),
            "the run in flight used its pinned atom: {asked}"
        );

        // The run recorded where its definitions are, and the snapshot holds
        // the atom as it was, with the hash of each file it came from.
        let rt = engine(&dir, &url).await;
        let pin = rt
            .steps("pinned-1")
            .await
            .expect("steps")
            .into_iter()
            .find(|s| s.name == "pin")
            .and_then(|s| s.output)
            .expect("a pin step");
        let pinned: Pinned = serde_json::from_value(pin).expect("pinned");
        let store = LocalStore::open(dir.join("artifacts")).expect("store");
        let snapshot: ArtifactRef = pinned.snapshot.parse().expect("reference");
        assert_eq!(
            store.manifests(&snapshot).expect("manifests")[0].media_type,
            SNAPSHOT_MEDIA_TYPE
        );
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut store.open(&snapshot).expect("open"), &mut bytes)
            .expect("read");
        let stored: Pipeline = serde_json::from_slice(&bytes).expect("a pipeline");
        let source = stored.atoms["rank-note"]
            .program
            .sources
            .iter()
            .find(|(path, _)| path.ends_with("rank-note.atom.astro"))
            .map(|(path, text)| (path.clone(), text.clone()))
            .expect("the rank atom's source");
        assert!(!source.1.contains("EDITED"));
        assert_eq!(pinned.files[&source.0], {
            use sha2::Digest;
            format!("{:x}", sha2::Sha256::digest(original.as_bytes()))
        });
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");

        // A run prepared now gets the edited atom.
        let edited = load(&project, "triage.workflow.yaml", TRIAGE);
        assert!(
            edited.atoms["rank-note"]
                .program
                .sources
                .values()
                .any(|text| text.contains("EDITED"))
        );
    });
}

#[test]
fn an_exec_step_runs_its_command_and_only_model_steps_call_the_model() {
    on_codex_runtime(async {
        let dir = scratch("exec");
        let project = project(&dir);
        std::fs::create_dir_all(project.join("tools")).expect("tools");
        std::fs::write(
            project.join("tools/count.py"),
            "import json, sys\nnote = json.load(sys.stdin)\njson.dump({'title': note['text'][:20], 'words': len(note['text'].split())}, sys.stdout)\n",
        )
        .expect("script");
        let atoms = project.join("atoms");
        std::fs::write(
            atoms.join("count-note.atom.astro"),
            "---\nimport type { Props, Input, Output } from '../.atomic/types/count-note';\n\
             import type { AtomConfig } from '../.atomic/types/authoring';\n\
             const { input }: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = { description: 'Count words.', kind: 'exec', exec: { command: ['python3', 'tools/count.py'] } };\n---\n",
        )
        .expect("atom");
        for role in ["input", "output"] {
            std::fs::copy(
                atoms.join(format!("read-note.{role}.schema.json")),
                atoms.join(format!("count-note.{role}.schema.json")),
            )
            .expect("schema");
        }
        let workflow = TRIAGE
            .replace("name: triage-note", "name: counted-triage")
            .replace("atom: read-note", "atom: count-note");
        let pipeline = load(&project, "counted.workflow.yaml", &workflow);
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let rt = engine(&dir, &model.base_url()).await;
        let run = PipelineRun {
            pipeline,
            input: json!({"text": "Export crashes on accented names"}),
        };
        rt.start(PIPELINE, "counted-1", run).await.expect("start");
        let out: Value = rt.result("counted-1").await.expect("finishes");
        assert_eq!(out, json!({"line": "P1: Export crashes on accents"}));
        assert_eq!(model.calls(), 2, "the exec step made no model call");
        let rank = model.requests()[0].to_string();
        assert!(
            rank.contains("Export crashes on ac") && rank.contains('5'),
            "{rank}"
        );
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");
    });
}

#[test]
fn a_workflow_budget_parks_or_fails_the_run_before_any_model_call() {
    on_codex_runtime(async {
        let dir = scratch("budget");
        let project = project(&dir);
        let model = FakeModel::start(vec![Reply::Text(READ.into())])
            .await
            .expect("fake model");
        let rt = engine(&dir, &model.base_url()).await;
        let input = json!({"text": "Exporting a file named café.txt crashes the app."});

        // Ten tokens leave no room for one call's bound: the run parks.
        let tight = load(
            &project,
            "tight.workflow.yaml",
            &format!("{TRIAGE}budget:\n  tokens: 10\n"),
        );
        let run = PipelineRun {
            pipeline: tight,
            input: input.clone(),
        };
        rt.start(PIPELINE, "tight", run).await.expect("start");
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let park = loop {
            if let Some(p) = atomic_durable::parks(&rt, "tight")
                .await
                .expect("parks")
                .into_iter()
                .next()
            {
                break p;
            }
            assert!(std::time::Instant::now() < until, "the run parked");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        assert!(park.reason.contains("run:tight"), "{}", park.reason);
        assert_eq!(park.step, "read.park");
        assert_eq!(model.calls(), 0);
        assert_eq!(
            rt.status("tight").await.expect("status"),
            Some(RunStatus::Pending)
        );

        // A dollar cap on a model the price table does not price fails the
        // run before anything is called.
        let priced = load(
            &project,
            "priced.workflow.yaml",
            &format!("{TRIAGE}budget:\n  cost_usd: 1\n"),
        );
        let run = PipelineRun {
            pipeline: priced,
            input,
        };
        rt.start(PIPELINE, "priced", run).await.expect("start");
        let failed = rt.result::<Value>("priced").await;
        assert!(
            matches!(&failed, Err(atomic_durable::DurableError::Failed(m)) if m.contains("cost_usd needs reliable pricing")),
            "{failed:?}"
        );
        assert_eq!(model.calls(), 0);
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");
    });
}

/// A failed candidate checkpoint must not resend an unknown external effect,
/// even when the engine recovers it without restarting the process.
async fn failed_exec_checkpoint(reopen: bool) {
    let dir = scratch(if reopen {
        "checkpoint-reopen"
    } else {
        "checkpoint-effect"
    });
    let project = project(&dir);
    write_send_note(&dir, None);
    let model = FakeModel::start(Vec::new()).await.expect("fake model");
    let mut rt = engine(&dir, &model.base_url()).await;
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute(
                "CREATE TRIGGER reject_candidate BEFORE INSERT ON operation_outputs \
                 WHEN NEW.function_name = 'read.turn' \
                 BEGIN SELECT RAISE(FAIL, 'candidate checkpoint unavailable'); END",
                &[],
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("checkpoint fault");
    let run = PipelineRun {
        pipeline: load(&project, "sent.workflow.yaml", &sent()),
        input: json!({"text": "A command with an unknown external effect."}),
    };
    rt.start(PIPELINE, "checkpoint-effect", run)
        .await
        .expect("start");
    // Wait for an automatic recovery claim, proving the first command
    // returned and its checkpoint failed, rather than racing its dispatch.
    let mut recovered = false;
    for _ in 0..500 {
        let attempts: i64 = rt
            .control_transaction(|mut tx| {
                Box::pin(async move {
                    tx.query_one(
                        "SELECT recovery_attempts AS attempts FROM workflow_status \
                         WHERE workflow_uuid = ?",
                        &[atomic_durable::Param::Text("checkpoint-effect".into())],
                    )
                    .await?
                    .int("attempts")
                })
            })
            .await
            .expect("recovery count");
        if attempts > 0 {
            recovered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(recovered, "the failed checkpoint must trigger recovery");
    if reopen {
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("stop the first engine");
        drop(rt);
        rt = engine(&dir, &model.base_url()).await;
    }
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute("DROP TRIGGER reject_candidate", &[]).await?;
            Ok(())
        })
    })
    .await
    .expect("restore checkpoint writes");
    if reopen {
        rt.recover().await.expect("recover in the new engine");
    }
    let mut terminal = None;
    for _ in 0..500 {
        let status = rt.status("checkpoint-effect").await.expect("status");
        if !matches!(status, Some(RunStatus::Pending)) {
            terminal = status;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    rt.shutdown(std::time::Duration::from_secs(5))
        .await
        .expect("shutdown");
    assert_eq!(
        sends(&dir),
        1,
        "automatic recovery must not resend the command"
    );
    assert!(
        matches!(terminal, Some(RunStatus::Failed(ref message)) if message.contains("outcome_unknown")),
        "{terminal:?}"
    );
    assert_eq!(model.calls(), 0, "nothing downstream may run");
}

#[test]
fn a_failed_exec_checkpoint_parks_in_the_same_process() {
    on_codex_runtime(failed_exec_checkpoint(false));
}

#[test]
fn a_reopened_engine_in_the_same_process_does_not_repeat_an_exec_effect() {
    on_codex_runtime(failed_exec_checkpoint(true));
}
