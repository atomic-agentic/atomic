// SPDX-License-Identifier: Apache-2.0

//! A replay of a recorded workflow-file run reuses its checkpoints and
//! spends nothing: no model call, no attempt in the usage ledger, and no
//! charge to the replay's allowances or to the recorded run's.

// Codex's turn futures nest deeply inside the workflow's; proving them Send
// needs more than the default query depth.
#![recursion_limit = "512"]

use atomic_app::pipeline::{PIPELINE, Pipeline, PipelineRun, REPLAY, Services, prepare};
use atomic_app::pipeline::{register_pipeline, replay_of};
use atomic_core::artifact::LocalStore;
use atomic_core::budget::{BudgetSettings, CallBound};
use atomic_core::codex::{CodexPaths, ModelEndpoint};
use atomic_core::lite::LiteRuntime;
use atomic_core::runtime::{Artifacts, CodexRuntime, Runtimes};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::{AtomConfig, atom, workflow};
use atomic_durable::gate::{Deployment, Principal};
use atomic_durable::usage::Charge;
use atomic_durable::{DurableRuntime, Durare};
use atomic_schema::registry::Registry;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("atomic-replay-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["home", "cwd", "project/atoms", "project/workflows"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
    dir
}

fn object(properties: Value) -> Value {
    let required: Vec<&String> = properties.as_object().expect("properties").keys().collect();
    json!({"type": "object", "additionalProperties": false, "required": required, "properties": properties})
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

/// Two model steps under a run budget: read a note, then rank it.
const WORKFLOW: &str = "\
atomic: 1
name: rank-note
input: { $ref: ../atoms/read-note.input.schema.json }
output: { $ref: ../atoms/rank-note.output.schema.json }
budget:
  tokens: 1000
steps:
  read:
    atom: read-note
    with: { text: input.text }
  rank:
    after: [read]
    atom: rank-note
    with: { title: steps.read.output.title }
returns: steps.rank.output
";

const READ: &str = r#"{"title": "Export crashes on accents"}"#;
const RANK: &str = r#"{"priority": 1}"#;

fn pipeline(dir: &Path) -> Pipeline {
    let project = dir.join("project");
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: notes\n").expect("manifest");
    write_atom(
        &project,
        "read-note",
        "Give the note a short title.\nNote: {input.text}",
        object(json!({"text": {"type": "string"}})),
        object(json!({"title": {"type": "string"}})),
    );
    write_atom(
        &project,
        "rank-note",
        "Rank the note from 1 (urgent) to 3.\nTitle: {input.title}",
        object(json!({"title": {"type": "string"}})),
        object(json!({"priority": {"type": "integer", "minimum": 1, "maximum": 3}})),
    );
    let file = project.join("workflows/rank.workflow.yaml");
    std::fs::write(&file, WORKFLOW).expect("workflow");
    let mut registry = Registry::new();
    let mut atoms = BTreeMap::new();
    for name in ["read-note", "rank-note"] {
        let path = project.join(format!("atoms/{name}.atom.astro"));
        let atom = atom::load(&path, &mut registry).expect("atom loads");
        atoms.insert(atom.name.clone(), atom);
    }
    let loaded = workflow::load(&file, &atoms, &mut registry).expect("workflow loads");
    prepare(&loaded, &atoms, &local()).expect("prepares")
}

fn local() -> Deployment {
    Deployment::Local {
        operator: Principal::local("dev"),
    }
}

/// An engine whose model calls are metered under a small call bound.
async fn engine(dir: &Path, base_url: &str) -> Durare {
    let endpoint = ModelEndpoint {
        base_url: base_url.to_owned(),
        model: "synthetic".into(),
        api_key_env: None,
    };
    let runtimes = Arc::new(Runtimes {
        codex: CodexRuntime {
            paths: CodexPaths::without_helpers(),
            codex_home: dir.join("home"),
            guard: atomic_core::codex::StateGuard::NoProjectState,
            cwd: dir.join("cwd"),
            endpoint: endpoint.clone(),
            defaults: AtomConfig::default(),
            artifacts: Artifacts::none(),
            meter: atomic_core::usage::Metering::none(),
            backend: Default::default(),
        },
        lite: LiteRuntime::new(endpoint, AtomConfig::default()),
        exec: Default::default(),
        clock: None,
    });
    let budgets = BudgetSettings {
        call: CallBound {
            input_tokens: 100,
            output_tokens: 50,
            minutes: 1.0,
        },
        ..BudgetSettings::default()
    };
    let mut rt = Durare::sqlite(&dir.join("atomic.db"))
        .await
        .expect("engine");
    register_pipeline(
        &mut rt,
        Services {
            runtimes,
            store: LocalStore::open(dir.join("artifacts")).expect("artifact store"),
            deployment: local(),
            worktrees: None,
            budgets: Arc::new(budgets),
            context: None,
        },
    );
    rt.launch().await.expect("launch");
    rt
}

async fn charged(rt: &Durare, scope: &str) -> Charge {
    rt.ledger()
        .allowance(scope)
        .await
        .expect("allowance")
        .map_or_else(Charge::default, |a| a.charged)
}

#[test]
fn a_replayed_run_reuses_its_checkpoints_and_spends_nothing() {
    on_codex_runtime(async {
        let dir = scratch("spend");
        // Answers for a second run too, so a replay that ran live would
        // finish and be caught by what it spent.
        let model = FakeModel::start(vec![
            Reply::Text(READ.into()),
            Reply::Text(RANK.into()),
            Reply::Text(READ.into()),
            Reply::Text(RANK.into()),
        ])
        .await
        .expect("fake model");
        let rt = engine(&dir, &model.base_url()).await;
        let run = PipelineRun {
            pipeline: pipeline(&dir),
            input: json!({"text": "Exporting a file named café.txt crashes the app."}),
        };
        rt.start(PIPELINE, "recorded", run).await.expect("start");
        let recorded: Value = rt.result("recorded").await.expect("finishes");

        // The recorded run made and paid for its two calls.
        assert_eq!(model.calls(), 2);
        let attempts_recorded = rt.ledger().attempts("recorded").await.expect("attempts");
        assert_eq!(attempts_recorded.len(), 2, "{attempts_recorded:?}");
        let spent = charged(&rt, "run:recorded").await;
        assert_eq!(
            spent.tokens, 4,
            "two calls of one input and one output token"
        );

        // Its replay is served each step's checkpoint: the same result, no
        // model call, no attempt, and nothing charged anywhere.
        let replay = replay_of(&rt, "recorded").await.expect("a replay");
        rt.start(REPLAY, "replayed", replay).await.expect("start");
        let replayed: Value = rt.result("replayed").await.expect("the replay finishes");
        assert_eq!(replayed, recorded);
        let attempts = rt.ledger().attempts("replayed").await.expect("attempts");
        assert!(attempts.is_empty(), "the replay was admitted: {attempts:?}");
        assert_eq!(charged(&rt, "run:replayed").await, Charge::default());
        // Each recorded call's atom allowance, `atom:<run>:<position>`, has
        // no counterpart charged in the replay.
        for a in &attempts_recorded {
            let position = a
                .request
                .rsplit(':')
                .nth(1)
                .expect("<run>:<position>:<call>");
            assert_ne!(
                charged(&rt, &format!("atom:recorded:{position}")).await,
                Charge::default()
            );
            assert_eq!(
                charged(&rt, &format!("atom:replayed:{position}")).await,
                Charge::default(),
                "position {position}"
            );
        }
        assert_eq!(
            charged(&rt, "run:recorded").await,
            spent,
            "the replay charged the run it replays"
        );
        assert_eq!(model.calls(), 2, "the replay made no model call");
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");
    });
}
