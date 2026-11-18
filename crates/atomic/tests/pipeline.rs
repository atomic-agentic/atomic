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
//!   cap on an unpriced model fails the run before any call.

// Codex's turn futures nest deeply inside the workflow's; proving them Send
// needs more than the default query depth.
#![recursion_limit = "512"]

use atomic_app::pipeline::{
    PIPELINE, Pinned, Pipeline, PipelineRun, SNAPSHOT_MEDIA_TYPE, prepare, register_pipeline,
};
use atomic_core::artifact::{ArtifactRef, ArtifactStore, LocalStore};
use atomic_core::codex::{CodexPaths, ModelEndpoint};
use atomic_core::lite::LiteRuntime;
use atomic_core::runtime::{Artifacts, CodexRuntime, Runtimes};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::{AtomConfig, atom, workflow};
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
    prepare(&loaded, &atoms).expect("prepares")
}

fn runtimes(dir: &Path, base_url: &str) -> Arc<Runtimes> {
    let endpoint = ModelEndpoint {
        base_url: base_url.to_owned(),
        model: "synthetic".into(),
        api_key_env: None,
    };
    Arc::new(Runtimes {
        codex: CodexRuntime {
            paths: CodexPaths::without_helpers(),
            codex_home: dir.join("home"),
            cwd: dir.join("cwd"),
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
    register_pipeline(&mut rt, runtimes(dir, base_url), store, Arc::default());
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
            pipeline
                .steps
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
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

#[test]
fn a_workflow_with_a_gate_is_refused_before_it_starts() {
    let dir = scratch("gate");
    let project = project(&dir);
    let gated = TRIAGE.replace(
        "  summarize:\n    after: [rank]\n",
        "  check:\n    after: [rank]\n    gate:\n      title: \"'Check the rank'\"\n      show: steps.rank.output\n  summarize:\n    after: [check]\n",
    );
    let file = project.join("workflows/gated.workflow.yaml");
    std::fs::write(&file, &gated).expect("workflow");
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
    let error = prepare(&loaded, &atoms).expect_err("not runnable yet");
    assert!(error.message.contains("gate"), "{error}");
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
fn a_worktree_step_is_refused_before_it_starts() {
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
    let error = prepare(&loaded, &atoms).expect_err("not runnable yet");
    assert!(error.message.contains("worktree"), "{error}");
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
        if role == "start" {
            let pipeline = load(&dir.join("project"), "triage.workflow.yaml", TRIAGE);
            let run = PipelineRun {
                pipeline,
                input: json!({"text": "Exporting a file named café.txt crashes the app."}),
            };
            let id = std::env::var(ENV_RUN).expect("run id");
            rt.start(PIPELINE, &id, run).await.expect("start");
        } else {
            rt.recover().await.expect("recover");
        }
        let id = std::env::var(ENV_RUN).expect("run id");
        let out: Value = rt.result(&id).await.expect("finishes");
        std::fs::write(dir.join("result.json"), out.to_string()).expect("write the result");
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
