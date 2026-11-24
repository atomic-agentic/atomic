// SPDX-License-Identifier: Apache-2.0

//! An epic work loop runs real file-defined atoms through the scripted model.

#![recursion_limit = "512"]

use atomic_app::model_policy::{LanePolicy, LanePolicySource};
use atomic_app::pipeline::{
    GRAPH_PIPELINE, GraphPipelineRun, LAYOUT_VERSION, PIPELINE, Pipeline, PipelineRun,
    ProjectWorktrees, Services, prepare, register_pipeline,
};
use atomic_core::artifact::LocalStore;
use atomic_core::budget::BudgetSettings;
use atomic_core::codex::{CodexPaths, ModelEndpoint};
use atomic_core::git::Git;
use atomic_core::lite::LiteRuntime;
use atomic_core::runtime::{Artifacts, CodexRuntime, Runtimes};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::workflow::Criterion;
use atomic_core::worktree::Worktrees;
use atomic_core::{AtomConfig, atom, workflow};
use atomic_durable::gate::{
    self, DecisionCommand, Deployment, GateFilter, HumanCredential, Principal, Verdict,
};
use atomic_durable::publication::{Acceptance, Candidate};
use atomic_durable::{Database, DurableRuntime, Durare, Param};
use atomic_graph::sqlite::SqliteStore;
use atomic_graph::store::{Actor, Draft, EdgeKind, GraphStore, Status};
use atomic_schema::registry::Registry;
use serde_json::{Value, json};
use sqlx::Connection;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[cfg(feature = "sandbox-tests")]
#[path = "support/mod.rs"]
mod support;

const EPIC: &str = "at-epic";
const RUN: &str = "graph-run";
const CHILDREN: [&str; 3] = ["at-first", "at-second", "at-third"];
const SECRET: &str = "graph work loop test credential";
const WORKFLOW: &str = "\
atomic: 1
name: fix-graph-item
input:
  type: object
  additionalProperties: false
  required: [base]
  properties:
    base: {type: string}
    item:
      type: object
      additionalProperties: false
      required: [id, title, body, kind, priority]
      properties:
        id: {type: string}
        title: {type: string}
        body: {type: string}
        kind: {type: string}
        priority: {type: integer}
output: { $ref: ../atoms/fix-item.output.schema.json }
steps:
  fix:
    atom: fix-item
    with: { title: input.item.title, base: input.base }
    worktree: true
returns: steps.fix.output
";

fn helper_host() {
    use atomic_core::helper_runtime::{CHILD_ARG, HostCommand, set_host_command};
    set_host_command(HostCommand {
        program: env!("CARGO_BIN_EXE_atomic").into(),
        args: vec![CHILD_ARG.to_owned()],
        env: Vec::new(),
    });
}

fn scratch(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "atomic-graph-loop-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    for sub in ["home", "cwd", "project/atoms", "project/workflows"] {
        std::fs::create_dir_all(root.join(sub)).expect("scratch directory");
    }
    root
}

fn git(project: &Path, args: &[&str]) -> String {
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
        .current_dir(project)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git command");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout)
        .expect("git UTF-8")
        .trim()
        .into()
}

async fn cli(root: &Path, project: &Path, url: &str, args: &[&str]) -> Output {
    let (root, project, url, args) = (
        root.to_owned(),
        project.to_owned(),
        url.to_owned(),
        args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
    );
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_atomic"))
            .args(args)
            .current_dir(project)
            .env("HOME", root.join("home"))
            .env("ATOMIC_HOME", root.join("state-home"))
            .env("ATOMIC_CREDENTIAL_STORE", "file")
            .env("USER", "graph-cli-test")
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
            .env("ATOMIC_MODEL_BASE_URL", url)
            .env("ATOMIC_MODEL", "synthetic")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .stdin(Stdio::null())
            .output()
            .expect("atomic command")
    })
    .await
    .expect("CLI worker")
}

#[track_caller]
fn cli_success(output: &Output) -> String {
    assert!(
        output.status.success(),
        "atomic CLI {:?}: stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).expect("CLI UTF-8")
}

fn cli_state_id(root: &Path, project: &Path) -> String {
    let label = project.file_name().expect("project name").to_string_lossy();
    std::fs::read_dir(root.join("state-home/projects"))
        .expect("project state")
        .map(|entry| {
            entry
                .expect("state entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .find(|id: &String| id.starts_with(&format!("{label}-")))
        .expect("state ID")
}

fn cli_database(root: &Path, project: &Path) -> PathBuf {
    root.join("state-home/private")
        .join(cli_state_id(root, project))
        .join("atomic.db")
}

async fn enroll_cli(root: &Path, project: &Path, url: &str) {
    cli_success(&cli(root, project, url, &["graph", "export"]).await);
    let id = cli_state_id(root, project);
    let secret = "graph CLI verified commit operator";
    let credentials =
        atomic_core::credential::FileSecrets::open(&root.join("state-home/credentials"), project)
            .expect("CLI file credentials");
    atomic_core::credential::SecretStore::save(
        &credentials,
        &format!("operator:graph-cli-test@{id}"),
        secret,
    )
    .expect("save CLI operator credential");
    let db = cli_database(root, project);
    let rt = Durare::sqlite(&db).await.expect("CLI durable database");
    atomic_durable::gate::enroll(
        &rt,
        &HumanCredential::new(Principal::local("graph-cli-test"), secret),
    )
    .await
    .expect("enroll CLI operator");
}

fn project(root: &Path) -> (PathBuf, String) {
    let project = root.join("project");
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: graph-loop\n").expect("manifest");
    std::fs::write(
        project.join("atoms/fix-item.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/fix-item';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'fix-item', kind: 'codex', sandbox: 'workspace-write' };\n\
         ---\nFix the graph item {input.title} at base {input.base}. Reply with done true.\n",
    )
    .expect("atom");
    std::fs::write(
        project.join("atoms/fix-item.input.schema.json"),
        json!({"type":"object","additionalProperties":false,"required":["title","base"],
            "properties":{"title":{"type":"string"},"base":{"type":"string"}}})
        .to_string(),
    )
    .expect("input schema");
    std::fs::write(
        project.join("atoms/fix-item.output.schema.json"),
        json!({"type":"object","additionalProperties":false,"required":["done"],
            "properties":{"done":{"type":"boolean"}}})
        .to_string(),
    )
    .expect("output schema");
    std::fs::write(project.join("workflows/fix.workflow.yaml"), WORKFLOW).expect("workflow");
    git(&project, &["init", "-q"]);
    git(&project, &["add", "."]);
    git(&project, &["commit", "-q", "-m", "base"]);
    let base = git(&project, &["rev-parse", "HEAD"]);
    (project, base)
}

fn pipeline(project: &Path) -> Pipeline {
    let mut registry = Registry::new();
    let file = project.join("atoms/fix-item.atom.astro");
    let atom = atom::load(&file, &mut registry).expect("atom loads");
    let mut atoms = BTreeMap::from([(atom.name.clone(), atom)]);
    let reviewer_file = project.join("atoms/check-commit.atom.astro");
    if reviewer_file.exists() {
        let reviewer = atom::load(&reviewer_file, &mut registry).expect("reviewer loads");
        atoms.insert(reviewer.name.clone(), reviewer);
    }
    let loaded = workflow::load(
        &project.join("workflows/fix.workflow.yaml"),
        &atoms,
        &mut registry,
    )
    .expect("workflow loads");
    prepare(
        &loaded,
        &atoms,
        &Deployment::Local {
            operator: Principal::local("owner"),
        },
    )
    .expect("workflow prepares")
}

fn review_project(project: &Path) {
    std::fs::write(project.join("atoms/check-commit.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/check-commit';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'check-commit', kind: 'codex', sandbox: 'read-only' };\n\
         ---\nReview commit {input.commit}. Reply with verdict reject for human review.\n",
    ).expect("reviewer atom");
    std::fs::write(
        project.join("atoms/check-commit.input.schema.json"),
        json!({"type":"object","additionalProperties":false,"required":["commit"],
            "properties":{"commit":{"type":"string"}}})
        .to_string(),
    )
    .expect("reviewer input schema");
    std::fs::write(project.join("atoms/check-commit.output.schema.json"),
        json!({"type":"object","additionalProperties":false,"required":["verdict"],
            "properties":{"verdict":{"type":"string","enum":["approve","reject","request_changes"]}}}).to_string(),
    ).expect("reviewer output schema");
    std::fs::write(project.join("workflows/fix.workflow.yaml"),
        WORKFLOW.replace(
            "    worktree: true\nreturns:",
            "    worktree: true\n    review:\n      reviewer: check-commit\n      with: { commit: candidate.metadata.commit }\n      rounds: 0\nreturns:",
        ),
    ).expect("reviewed workflow");
}

fn review_repair_project(project: &Path) {
    review_project(project);
    let workflow = std::fs::read_to_string(project.join("workflows/fix.workflow.yaml"))
        .expect("review workflow");
    std::fs::write(
        project.join("workflows/fix.workflow.yaml"),
        workflow.replace("      rounds: 0", "      rounds: 1"),
    )
    .expect("repair workflow");
}

fn panel_graph_project(project: &Path) -> String {
    let item_properties: serde_json::Map<String, Value> = (1..=10)
        .map(|n| (n.to_string(), json!({"type":"string"})))
        .collect();
    let decision_properties = json!({
        "point":{"type":"string"}, "allow_refuse":{"type":"string"},
        "not_found":{"type":"string"}, "permission_denied":{"type":"string"},
        "link_loop":{"type":"string"}, "not_directory":{"type":"string"},
        "other_io":{"type":"string"}, "parse_error":{"type":"string"},
        "timeout":{"type":"string"}, "empty_result":{"type":"string"}
    });
    let evidence = json!({
        "type":"object", "additionalProperties":false,
        "required":["items","decisions","controls"],
        "properties":{
            "items":{"type":"object","additionalProperties":false,"properties":item_properties},
            "decisions":{"type":"array","items":{"type":"object","additionalProperties":false,
                "required":decision_properties.as_object().expect("decision fields").keys().cloned().collect::<Vec<_>>(),
                "properties":decision_properties}},
            "controls":{"type":"array","items":{"type":"object","additionalProperties":false,
                "required":["row","result"],"properties":{"row":{"type":"string"},"result":{"type":"string"}}}}
        }
    });
    std::fs::create_dir_all(project.join(".atomic")).expect("decision inventory directory");
    std::fs::write(
        project.join(".atomic/coding-decision-inventory.json"),
        r#"{"version":1,"decision_points":["candidate publication"]}"#,
    )
    .expect("decision inventory");
    for name in ["self-check", "review-one", "review-two"] {
        std::fs::write(project.join(format!("atoms/{name}.atom.astro")),
            format!("---\nimport type {{ Props, Input, Output }} from '../.atomic/types/{name}';\nimport type {{ AtomConfig }} from '../.atomic/types/authoring';\nconst {{ input }}: Props = Astro.props;\nexport const config: AtomConfig<Input, Output> = {{ description: '{name}', kind: 'codex', sandbox: 'read-only' }};\n---\nReview {{input.diff}}.\n"))
            .expect("panel atom");
        let input = if name == "self-check" {
            json!({"type":"object","additionalProperties":false,"required":["diff"],
                "properties":{"diff":{"type":"string"}}})
        } else {
            json!({"type":"object","additionalProperties":false,"required":["diff","host_review"],
            "properties":{
                "diff":{"type":"string"}, "self_review":evidence,
                "host_review":{"type":"object","additionalProperties":false,
                    "required":["decision_points","selected_controls","control_results"],
                    "properties":{
                        "decision_points":{"type":"array","items":{"type":"string"}},
                        "selected_controls":{"type":"array","items":{"type":"string"}},
                        "control_results":{"type":"array","items":{"type":"object","additionalProperties":false,
                            "required":["row","result"],"properties":{"row":{"type":"string"},"result":{"type":"string"}}}}
                    }}
            }})
        };
        let output = if name == "self-check" {
            evidence.clone()
        } else {
            json!({"type":"object","additionalProperties":false,"required":["verdict"],
                "properties":{"verdict":{"type":"string","enum":["approve","request_changes"]}}})
        };
        std::fs::write(
            project.join(format!("atoms/{name}.input.schema.json")),
            input.to_string(),
        )
        .expect("panel input schema");
        std::fs::write(
            project.join(format!("atoms/{name}.output.schema.json")),
            output.to_string(),
        )
        .expect("panel output schema");
    }
    let source = WORKFLOW.replace(
        "    worktree: true\nreturns:",
        "    worktree: true\n    coding: true\n    review:\n      reviewers: [review-one, review-two]\n      require: all\n      with: { diff: candidate.metadata.diff }\n      trial_merge:\n        main: refs/heads/main\n        checks: [['true']]\n      self_review:\n        atom: self-check\n        with: { diff: candidate.metadata.diff }\n        decision_points: [candidate publication]\n      rounds: 0\nreturns:",
    );
    assert_ne!(source, WORKFLOW);
    std::fs::write(project.join("workflows/fix.workflow.yaml"), source).expect("panel workflow");
    git(project, &["add", "."]);
    git(project, &["commit", "-q", "-m", "panel review fixture"]);
    let base = git(project, &["rev-parse", "HEAD"]);
    git(project, &["update-ref", "refs/heads/main", &base]);
    base
}

fn panel_pipeline(project: &Path) -> Pipeline {
    let mut registry = Registry::new();
    let mut atoms = BTreeMap::new();
    for name in ["fix-item", "self-check", "review-one", "review-two"] {
        let loaded = atom::load(
            &project.join(format!("atoms/{name}.atom.astro")),
            &mut registry,
        )
        .expect("panel atom loads");
        atoms.insert(loaded.name.clone(), loaded);
    }
    let loaded = workflow::load(
        &project.join("workflows/fix.workflow.yaml"),
        &atoms,
        &mut registry,
    )
    .expect("panel workflow loads");
    prepare(
        &loaded,
        &atoms,
        &Deployment::Local {
            operator: Principal::local("owner"),
        },
    )
    .expect("panel workflow prepares")
}

async fn durable(root: &Path) -> Durare {
    Durare::open(
        &Database::Sqlite(root.join("atomic.db")),
        None,
        LAYOUT_VERSION,
        "graph-loop/test",
    )
    .await
    .expect("durable database")
}

async fn engine(root: &Path, url: &str, graph_registered: bool) -> Durare {
    engine_at(
        root,
        url,
        graph_registered,
        root.join("atomic.db"),
        "graph-loop/test",
    )
    .await
}

async fn engine_at(
    root: &Path,
    url: &str,
    graph_registered: bool,
    database: PathBuf,
    project_id: &str,
) -> Durare {
    engine_at_with_endpoint(root, url, graph_registered, database, project_id, true).await
}

async fn engine_at_with_endpoint(
    root: &Path,
    url: &str,
    graph_registered: bool,
    database: PathBuf,
    project_id: &str,
    model_endpoint_configured: bool,
) -> Durare {
    helper_host();
    let mut rt = Durare::open(
        &Database::Sqlite(database),
        None,
        LAYOUT_VERSION,
        project_id,
    )
    .await
    .expect("project durable database");
    if graph_registered {
        let endpoint = ModelEndpoint {
            base_url: url.into(),
            model: "synthetic".into(),
            api_key_env: None,
            sign_in: None,
        };
        let runtimes = Arc::new(Runtimes {
            codex: CodexRuntime {
                paths: CodexPaths::without_helpers(),
                codex_home: root.join("home"),
                guard: atomic_core::codex::StateGuard::NoProjectState,
                cwd: root.join("project"),
                endpoint: endpoint.clone(),
                defaults: AtomConfig::default(),
                artifacts: Artifacts::none(),
                meter: atomic_core::usage::Metering::none(),
                caches: None,
            },
            lite: LiteRuntime::new(endpoint, AtomConfig::default()),
            backend: atomic_core::testing::denying_nothing(Default::default()),
            exec_meter: Default::default(),
            clock: None,
        });
        register_pipeline(
            &mut rt,
            Services {
                runtimes,
                model_endpoint_configured,
                store: LocalStore::open(root.join("artifacts")).expect("artifacts"),
                deployment: Deployment::Local {
                    operator: Principal::local("owner"),
                },
                worktrees: ProjectWorktrees::at(
                    &root.join("project"),
                    root.join("state/worktrees"),
                ),
                caches: None,
                trial_cargo: None,
                budgets: Arc::new(BudgetSettings::default()),
                context: None,
            },
        );
    }
    rt.launch().await.expect("launch");
    rt
}

async fn seed(root: &Path) -> String {
    let (project, base) = project(root);
    assert!(project.join(".git").exists());
    let db = root.join("atomic.db");
    let rt = durable(root).await;
    let credential = HumanCredential::new(Principal::local("owner"), SECRET);
    atomic_durable::gate::enroll(&rt, &credential)
        .await
        .expect("enroll");
    drop(rt);
    let graph = SqliteStore::open_authorized(&db, SystemTime::now, credential).expect("graph");
    let mut epic = Draft::new(EPIC, "Graph epic");
    epic.kind = "epic".into();
    epic.status = Status::Pinned;
    graph
        .create_item(Actor::agent("seed"), "create-epic".into(), epic)
        .await
        .expect("epic");
    for (ordinal, id) in CHILDREN.iter().enumerate() {
        let mut child = Draft::new(id, &format!("Work item {}", ordinal + 1));
        child.parent = Some(EPIC.into());
        child.priority = ordinal as i32;
        graph
            .create_item(Actor::agent("seed"), format!("create-{id}"), child)
            .await
            .expect("child");
    }
    for ordinal in 1..CHILDREN.len() {
        graph
            .add_edge(
                Actor::agent("seed"),
                CHILDREN[ordinal].into(),
                EdgeKind::Blocks,
                CHILDREN[ordinal - 1].into(),
            )
            .await
            .expect("dependency");
    }
    graph
        .authorize_graph_run(EPIC, RUN)
        .await
        .expect("authorized graph run");
    base
}

async fn leave_one_work_child(root: &Path) {
    let rt = durable(root).await;
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.execute(
                "UPDATE atomic_graph_items SET status='closed' WHERE id IN (?,?)",
                &[
                    Param::Text(CHILDREN[1].into()),
                    Param::Text(CHILDREN[2].into()),
                ],
            )
            .await?;
            Ok(())
        })
    })
    .await
    .expect("leave one work child");
}

async fn wait_for_review_gate(rt: &Durare) -> gate::Gate {
    for _ in 0..600 {
        let open = gate::gates(
            rt,
            GateFilter {
                open_only: true,
                run: Some(RUN.into()),
            },
        )
        .await
        .expect("review gates");
        if let Some(gate) = open.into_iter().next() {
            return gate;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("graph review never opened a human gate");
}

async fn approve_review_gate(rt: &Durare, gate: &gate::Gate, command: &str) {
    gate::decide(
        rt,
        &HumanCredential::new(Principal::local("owner"), SECRET),
        DecisionCommand {
            command_id: command.into(),
            gate: gate.id.clone(),
            verdict: Verdict::Approve,
            comment: Some("approved after review".into()),
            proposal: gate.proposal.clone(),
            answer: None,
        },
    )
    .await
    .expect("human approval");
}

async fn released_review_handoff(rt: &Durare) -> (String, i64, Option<String>) {
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            let item = tx.query_one(
                "SELECT status FROM atomic_graph_items WHERE id=?",
                &[Param::Text(CHILDREN[0].into())],
            ).await?;
            let lease = tx.query_one(
                "SELECT COUNT(*) AS count FROM atomic_graph_leases WHERE item_id=?",
                &[Param::Text(CHILDREN[0].into())],
            ).await?;
            let note = tx.query_opt(
                "SELECT text,trust FROM atomic_graph_notes WHERE item_id=? ORDER BY ordinal DESC LIMIT 1",
                &[Param::Text(CHILDREN[0].into())],
            ).await?;
            let note = note.map(|note| {
                assert_eq!(note.text("trust")?, "agent");
                note.text("text")
            }).transpose()?;
            Ok((item.text("status")?, lease.int("count")?, note))
        })
    }).await.expect("review release handoff")
}

const ENV_ROOT: &str = "ATOMIC_GRAPH_LOOP_ROOT";
const ENV_URL: &str = "ATOMIC_GRAPH_LOOP_URL";
const ENV_BASE: &str = "ATOMIC_GRAPH_LOOP_BASE";
const ENV_ROLE: &str = "ATOMIC_GRAPH_LOOP_ROLE";

#[test]
#[ignore = "run in a child process by the graph work-loop tests"]
fn graph_loop_child() {
    let (Ok(root), Ok(url), Ok(base), Ok(role)) = (
        std::env::var(ENV_ROOT),
        std::env::var(ENV_URL),
        std::env::var(ENV_BASE),
        std::env::var(ENV_ROLE),
    ) else {
        return;
    };
    on_codex_runtime(async move {
        let root = PathBuf::from(root);
        if role == "recover-without-graph" {
            let rt = engine(&root, &url, false).await;
            let recovery = rt.recover().await;
            std::fs::write(root.join("without-graph.txt"), format!("{recovery:?}"))
                .expect("record recovery result");
            return;
        }
        let rt = engine(&root, &url, true).await;
        if role == "start" || role == "start-panel" || role == "start-routed" {
            let mut prepared = if role == "start-panel" {
                panel_pipeline(&root.join("project"))
            } else {
                pipeline(&root.join("project"))
            };
            if let Ok(frozen) = std::fs::read(root.join("graph-fixture.json")) {
                prepared.fixture = Some(serde_json::from_slice(&frozen).expect("frozen fixture"));
            }
            let model_source = if role == "start-routed" {
                let path = root.join("operator.yaml");
                let value: Value =
                    serde_json::from_slice(&std::fs::read(&path).expect("policy file"))
                        .expect("policy document");
                let policy: LanePolicy = serde_json::from_value(value["model"]["lanes"].clone())
                    .expect("operator lane policy");
                Some(LanePolicySource {
                    path,
                    endpoints: policy.endpoints,
                })
            } else {
                None
            };
            let run = GraphPipelineRun {
                run: PipelineRun {
                    pipeline: prepared,
                    input: json!({"base": base}),
                },
                epic: EPIC.into(),
                model_source,
                review_routing: true,
            };
            rt.start(GRAPH_PIPELINE, RUN, run)
                .await
                .expect("start graph run");
        } else {
            rt.recover().await.expect("recover graph run");
        }
        if role == "recover-park" {
            wait_for_park(&rt, None).await;
            rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
            return;
        }
        let outcome: Result<Value, _> = rt.result(RUN).await;
        std::fs::write(root.join("result.txt"), format!("{outcome:?}")).expect("record result");
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

fn child(
    root: &Path,
    url: &str,
    base: &str,
    role: &str,
    failpoint: Option<&str>,
) -> std::process::ExitStatus {
    child_command(root, url, base, role, failpoint)
        .status()
        .expect("child process")
}

fn child_command(
    root: &Path,
    url: &str,
    base: &str,
    role: &str,
    failpoint: Option<&str>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(std::env::current_exe().expect("test executable"));
    cmd.args(["--exact", "graph_loop_child", "--ignored", "--nocapture"])
        .env(ENV_ROOT, root)
        .env(ENV_URL, url)
        .env(ENV_BASE, base)
        .env(ENV_ROLE, role)
        .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env_remove("ATOMIC_TEST_HELPER_RUNTIME")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_BASE_URL")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match failpoint {
        Some(name) => {
            cmd.env("ATOMIC_FAILPOINT", name);
        }
        None => {
            cmd.env_remove("ATOMIC_FAILPOINT");
        }
    }
    cmd
}

async fn inspect(root: &Path) -> (Vec<(String, String, String)>, Vec<String>) {
    let rt = durable(root).await;
    let rows = rt
        .control_transaction(|mut tx| {
            Box::pin(async move {
                let mut result = Vec::new();
                for id in CHILDREN {
                    let item = tx
                        .query_one(
                            "SELECT status FROM atomic_graph_items WHERE id=?",
                            &[Param::Text(id.into())],
                        )
                        .await?;
                    let link = tx
                        .query_opt(
                            "SELECT sha FROM atomic_graph_commit_links WHERE item_id=?",
                            &[Param::Text(id.into())],
                        )
                        .await?;
                    result.push((
                        id.into(),
                        item.text("status")?,
                        link.map(|row| row.text("sha"))
                            .transpose()?
                            .unwrap_or_default(),
                    ));
                }
                Ok(result)
            })
        })
        .await
        .expect("graph rows");
    let steps = rt
        .steps(RUN)
        .await
        .expect("durable steps")
        .into_iter()
        .map(|step| step.name)
        .collect();
    (rows, steps)
}

#[test]
fn three_dependent_items_make_three_model_calls_and_close_with_host_commits() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into()); 3])
            .await
            .expect("scripted model");
        let root = scratch("three-items");
        let base = seed(&root).await;
        let url = model.base_url();
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let status = tokio::task::spawn_blocking(move || child(&r, &u, &b, "start", None))
            .await
            .expect("child");
        assert!(status.success(), "graph run: {status}");
        assert_eq!(model.calls(), 3, "one model turn per child");
        let (rows, steps) = inspect(&root).await;
        let mut parent = base;
        for (id, state, sha) in rows {
            assert_eq!(state, "closed", "{id}");
            assert_eq!(sha.len(), 40, "{id} has one host commit SHA");
            assert_eq!(
                git(&root.join("project"), &["rev-parse", &format!("{sha}^")]),
                parent
            );
            parent = sha;
        }
        for ordinal in 0..3 {
            for suffix in ["claim", "fix.turn", "fix.publish", "fix.close"] {
                assert!(
                    steps.contains(&format!("graph.{ordinal:05}.{suffix}")),
                    "{steps:?}"
                );
            }
        }
        assert!(steps.contains(&"epic.done".to_owned()));
        let graph = SqliteStore::open(&root.join("atomic.db"), SystemTime::now).expect("graph");
        let epic = graph
            .show(Actor::agent("inspect"), EPIC.into())
            .await
            .expect("show epic")
            .expect("epic exists");
        assert_eq!(epic.item.status, Status::Pinned);
    });
}

#[test]
fn delivery_evidence_accepts_real_graph_panel_and_refuses_reopened_item() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(json!({
                "items": (1..=10).map(|n| (n.to_string(), json!(if n == 10 {
                    "N/A: no controls were selected".to_owned()
                } else { format!("evidence for item {n}") }))).collect::<serde_json::Map<String, Value>>(),
                "decisions": [{
                    "point":"candidate publication", "allow_refuse":"only validated evidence reaches review",
                    "not_found":"refuse", "permission_denied":"refuse", "link_loop":"refuse",
                    "not_directory":"refuse", "other_io":"refuse", "parse_error":"refuse",
                    "timeout":"refuse", "empty_result":"refuse"
                }],
                "controls": []
            }).to_string()),
            Reply::Text(r#"{"verdict":"approve"}"#.into()),
            Reply::Text(r#"{"verdict":"approve"}"#.into()),
        ]).await.expect("scripted coding panel");
        let root = std::fs::canonicalize(scratch("delivery-panel-source"))
            .expect("physical fixture path for Git trial worktree");
        let _ = seed(&root).await;
        leave_one_work_child(&root).await;
        let base = panel_graph_project(&root.join("project"));
        let url = model.base_url();
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let status = tokio::task::spawn_blocking(move || {
            child_command(&r, &u, &b, "start-panel", None)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .expect("graph child")
        })
        .await
        .expect("graph child");
        assert!(status.status.success(), "graph panel run: {status:?}");
        assert_eq!(model.calls(), 4, "author, self-review, and two reviewers");
        let rt = durable(&root).await;
        let source =
            atomic_app::delivery_evidence::approved_graph_item(&rt, RUN, "graph.00000.fix")
                .await
                .expect("reviewed graph item is a deliverable source");
        assert_eq!(source.item_id, CHILDREN[0]);
        assert_eq!(source.head.len(), 40);
        assert_eq!(source.occurrence, 0);
        rt.control_transaction({
            let source = source.clone();
            move |mut tx| {
                let source = source.clone();
                Box::pin(async move {
                    atomic_app::delivery_evidence::revalidate_delivery_source_in(&mut tx, &source)
                        .await
                        .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))
                })
            }
        })
        .await
        .expect("source remains valid at outbound slot");
        // Move a consistent snapshot of this real graph run into the normal
        // CLI project state. Delivery admission must read the recorded source
        // through that project's runtime and open an actual human gate.
        cli_success(&cli(&root, &root.join("project"), &url, &["graph", "export"]).await);
        let cli_db = cli_database(&root, &root.join("project"));
        std::fs::remove_file(&cli_db).expect("replace fresh CLI database");
        let mut source_db = sqlx::SqliteConnection::connect(&format!(
            "sqlite://{}?mode=rw",
            root.join("atomic.db").display()
        ))
        .await
        .expect("source database connection");
        sqlx::query("VACUUM main INTO ?")
            .bind(cli_db.to_string_lossy().as_ref())
            .execute(&mut source_db)
            .await
            .expect("consistent CLI source snapshot");
        let state_id = cli_state_id(&root, &root.join("project"));
        let cli_project = format!("graph-loop/{state_id}");
        let cli_rt = Durare::sqlite(&cli_db).await.expect("copied project store");
        cli_rt
            .control_transaction(move |mut tx| {
                let cli_project = cli_project.clone();
                Box::pin(async move {
                    tx.execute(
                        "UPDATE atomic_run_projects SET project=? WHERE run_id=?",
                        &[Param::Text(cli_project), Param::Text(RUN.into())],
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .expect("adopt source into CLI project state");
        let delivery_dir = cli_db.parent().expect("private state").join("delivery");
        std::fs::create_dir(&delivery_dir).expect("protected delivery directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&delivery_dir, std::fs::Permissions::from_mode(0o700))
                .expect("delivery directory mode");
        }
        std::fs::write(
            delivery_dir.join("adapter"),
            include_bytes!("support/delivery_remote.py"),
        )
        .expect("fake remote adapter");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                delivery_dir.join("adapter"),
                std::fs::Permissions::from_mode(0o700),
            )
            .expect("adapter mode");
        }
        let target = json!({"repository":"test/repo","branch_ref":"refs/heads/reviewed",
            "base_ref":"refs/heads/main","expected_old":null,"pull_request_number":null,
            "title":"Reviewed graph item","body":"Green trial and panel"});
        std::fs::write(delivery_dir.join("target.json"), target.to_string())
            .expect("delivery target");
        let started = cli_success(
            &cli(
                &root,
                &root.join("project"),
                &url,
                &[
                    "--json",
                    "delivery",
                    "start",
                    RUN,
                    "--step",
                    "graph.00000.fix",
                    "--run-id",
                    "delivery-panel-source",
                ],
            )
            .await,
        );
        let opened: Value = serde_json::from_str(&started).expect("delivery gate JSON");
        assert_eq!(opened["run"], "delivery-panel-source");
        assert!(opened["gate"].as_str().is_some(), "{opened}");
        assert_eq!(opened["plan"]["push"]["head"], source.head);
        let before = cli_rt
            .control_transaction(|mut tx| {
                Box::pin(async move {
                    tx.query_one(
                        "SELECT COUNT(*) AS n FROM atomic_gates WHERE run='delivery-panel-source'",
                        &[],
                    )
                    .await?
                    .int("n")
                })
            })
            .await
            .expect("one delivery gate");
        assert_eq!(before, 1);
        let mut changed = target;
        changed["title"] = json!("Changed after first gate");
        std::fs::write(delivery_dir.join("target.json"), changed.to_string())
            .expect("change protected target");
        let duplicate = cli(
            &root,
            &root.join("project"),
            &url,
            &[
                "--json",
                "delivery",
                "start",
                RUN,
                "--step",
                "graph.00000.fix",
                "--run-id",
                "delivery-panel-source",
            ],
        )
        .await;
        assert!(
            !duplicate.status.success(),
            "duplicate delivery ID was admitted"
        );
        assert!(
            String::from_utf8_lossy(&duplicate.stdout).contains("already exists"),
            "{duplicate:?}"
        );
        let after = cli_rt
            .control_transaction(|mut tx| {
                Box::pin(async move {
                    tx.query_one(
                        "SELECT COUNT(*) AS n FROM atomic_gates WHERE run='delivery-panel-source'",
                        &[],
                    )
                    .await?
                    .int("n")
                })
            })
            .await
            .expect("delivery gate count after duplicate");
        assert_eq!(after, before, "duplicate start opened another gate");
        rt.control_transaction(|mut tx| {
            Box::pin(async move {
                tx.execute(
                    "UPDATE atomic_graph_items SET status='open' WHERE id=?",
                    &[Param::Text(CHILDREN[0].into())],
                )
                .await?;
                Ok(())
            })
        })
        .await
        .expect("reopen source item");
        let stale = rt
            .control_transaction({
                let source = source.clone();
                move |mut tx| {
                    let source = source.clone();
                    Box::pin(async move {
                        atomic_app::delivery_evidence::revalidate_delivery_source_in(
                            &mut tx, &source,
                        )
                        .await
                        .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))
                    })
                }
            })
            .await
            .expect_err("reopened item cannot be delivered");
        assert!(stale.to_string().contains("no longer closed"), "{stale}");
        rt.control_transaction(|mut tx| Box::pin(async move {
            tx.execute("UPDATE atomic_graph_items SET status='closed',last_finish_epoch=last_finish_epoch+1 WHERE id=?",
                &[Param::Text(CHILDREN[0].into())]).await?;
            Ok(())
        })).await.expect("reclose under same claimant text but newer token");
        let reclosed = rt
            .control_transaction({
                let source = source.clone();
                move |mut tx| {
                    let source = source.clone();
                    Box::pin(async move {
                        atomic_app::delivery_evidence::revalidate_delivery_source_in(
                            &mut tx, &source,
                        )
                        .await
                        .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))
                    })
                }
            })
            .await
            .expect_err("new claim token cannot reuse historical commit link");
        assert!(
            reclosed.to_string().contains("no longer closed"),
            "{reclosed}"
        );
        rt.control_transaction(|mut tx| Box::pin(async move {
            tx.execute("UPDATE workflow_status SET application_version='atomic.pipeline.8' WHERE workflow_uuid=?",
                &[Param::Text(RUN.into())]).await?;
            Ok(())
        })).await.expect("mark legacy graph layout");
        let legacy =
            atomic_app::delivery_evidence::approved_graph_item(&rt, RUN, "graph.00000.fix")
                .await
                .expect_err("legacy source layout cannot qualify");
        assert!(
            legacy
                .to_string()
                .contains("predates current graph evidence layout"),
            "{legacy}"
        );
    });
}

#[test]
fn crash_after_graph_publish_has_already_closed_the_item_atomically() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("scripted author");
        let root = scratch("atomic-publish-close");
        let base = seed(&root).await;
        leave_one_work_child(&root).await;
        let url = model.base_url();
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let crashed = tokio::task::spawn_blocking(move || {
            child(&r, &u, &b, "start", Some("graph:after-verified-publish"))
        })
        .await
        .expect("crash child");
        assert!(!crashed.success(), "publish failpoint did not kill child");
        let (rows, steps) = inspect(&root).await;
        assert_eq!(
            rows[0].1, "closed",
            "publication committed without item close"
        );
        assert_eq!(rows[0].2.len(), 40, "closed item has no host commit link");
        assert!(steps.contains(&"graph.00000.fix.publish".into()));
        assert!(!steps.contains(&"graph.00000.fix.close".into()));
        let (r, u, b) = (root.clone(), url.clone(), base);
        let recovered = tokio::task::spawn_blocking(move || child(&r, &u, &b, "recover", None))
            .await
            .expect("recovery child");
        assert!(recovered.success(), "published graph recovery: {recovered}");
        assert_eq!(
            model.calls(),
            1,
            "recovery repeated a completed author turn"
        );
        let (rows, steps) = inspect(&root).await;
        assert_eq!(rows[0].1, "closed");
        assert!(steps.contains(&"graph.00000.fix.close".into()));
    });
}

#[test]
fn crash_after_atomic_finalize_replays_publish_and_close_projections_without_a_turn() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("scripted author");
        let root = scratch("finalize-projection-replay");
        let base = seed(&root).await;
        leave_one_work_child(&root).await;
        let url = model.base_url();
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let crashed = tokio::task::spawn_blocking(move || {
            child(&r, &u, &b, "start", Some("graph:after-finalize-applied"))
        })
        .await
        .expect("crash child");
        assert!(!crashed.success(), "finalize failpoint did not kill child");
        let (rows, steps) = inspect(&root).await;
        assert_eq!(rows[0].1, "closed", "finalize did not close atomically");
        assert!(steps.contains(&"graph.00000.fix.finalize.0".into()));
        assert!(!steps.contains(&"graph.00000.fix.publish".into()));
        let (r, u, b) = (root.clone(), url, base);
        let recovered = tokio::task::spawn_blocking(move || child(&r, &u, &b, "recover", None))
            .await
            .expect("recovery child");
        assert!(
            recovered.success(),
            "finalize projection recovery: {recovered}"
        );
        assert_eq!(
            model.calls(),
            1,
            "recovery repeated a completed author turn"
        );
        let (_, steps) = inspect(&root).await;
        assert!(steps.contains(&"graph.00000.fix.publish".into()));
        assert!(steps.contains(&"graph.00000.fix.close".into()));
    });
}

#[test]
fn offline_branch_proof_cannot_skip_a_checkpointed_proof_park_on_recovery() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("scripted author");
        let root = scratch("offline-proof-replay");
        let base = seed(&root).await;
        leave_one_work_child(&root).await;
        let rt = durable(&root).await;
        rt.control_transaction(|mut tx| {
            Box::pin(async move {
                tx.execute(
                    "INSERT INTO atomic_graph_item_policy (item_id,requires_commit,requires_verified_commit,authorized_branch,set_by,set_at) VALUES (?,1,1,?,'owner',1)",
                    &[Param::Text(CHILDREN[0].into()), Param::Text("refs/heads/main".into())],
                )
                .await?;
                Ok(())
            })
        })
        .await
        .expect("verified policy fixture");
        let url = model.base_url();
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let crashed = tokio::task::spawn_blocking(move || {
            child(&r, &u, &b, "start", Some("graph:after-proof-stage"))
        })
        .await
        .expect("crash child");
        assert!(
            !crashed.success(),
            "proof-stage failpoint did not kill child"
        );
        let checkpoint = rt
            .steps(RUN)
            .await
            .expect("graph checkpoints")
            .into_iter()
            .find(|step| step.name == "graph.00000.fix.finalize.0")
            .expect("missing proof decision was checkpointed");
        assert_eq!(
            checkpoint.output.expect("proof decision")["status"],
            "missing"
        );
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let first_recovery =
            tokio::task::spawn_blocking(move || child(&r, &u, &b, "recover-park", None))
                .await
                .expect("first recovery child");
        assert!(first_recovery.success(), "first recovery: {first_recovery}");
        let proof_park = wait_for_park(&rt, None).await;
        assert!(proof_park.reason.contains("host branch proof"));
        let candidate = rt
            .steps(RUN)
            .await
            .expect("graph checkpoints")
            .into_iter()
            .find(|step| step.name == "graph.00000.fix.turn")
            .and_then(|step| step.output)
            .expect("checkpointed candidate");
        let sha = candidate["Candidate"]["proposal"]["metadata"]["commit"]
            .as_str()
            .expect("host commit SHA")
            .to_owned();
        let changed = rt.control_transaction(move |mut tx| {
            let (sha, base) = (sha.clone(), base.clone());
            Box::pin(async move {
                let changed = tx.execute(
                    "UPDATE atomic_graph_commit_links SET indexed=1,on_branch=1,verified_branch=?,verified_head=?,pinned_base=? WHERE item_id=? AND sha=?",
                    &[Param::Text("refs/heads/main".into()), Param::Text(sha.clone()), Param::Text(base), Param::Text(CHILDREN[0].into()), Param::Text(sha)],
                )
                .await?;
                Ok(changed)
            })
        })
        .await
        .expect("proof fixture changed while atomic was offline");
        assert_eq!(changed, 1, "the staged host SHA received a proof fixture");
        let (r, u, b) = (
            root.clone(),
            url.clone(),
            git(&root.join("project"), &["rev-parse", "HEAD"]),
        );
        let second_recovery =
            tokio::task::spawn_blocking(move || child(&r, &u, &b, "recover-park", None))
                .await
                .expect("second recovery child");
        assert!(
            second_recovery.success(),
            "second recovery: {second_recovery}"
        );
        let pending = atomic_durable::release::waiting(&rt)
            .await
            .expect("waiting parks");
        assert!(pending.iter().any(|park| park.id == proof_park.id));
        assert_eq!(inspect(&root).await.0[0].1, "in_progress");
        assert_eq!(model.calls(), 1, "recovery repeated a completed turn");
        release(&rt, &proof_park, "release-offline-proof").await;
        let (r, u, b) = (
            root.clone(),
            url.clone(),
            git(&root.join("project"), &["rev-parse", "HEAD"]),
        );
        let after_release = tokio::task::spawn_blocking(move || {
            child(&r, &u, &b, "recover", Some("graph:after-proof-release"))
        })
        .await
        .expect("post-release crash child");
        assert!(
            !after_release.success(),
            "post-release failpoint did not kill child"
        );
        assert_eq!(
            model.calls(),
            1,
            "post-release crash repeated the author turn"
        );
        let (r, u, b) = (
            root.clone(),
            url.clone(),
            git(&root.join("project"), &["rev-parse", "HEAD"]),
        );
        let last_recovery = tokio::task::spawn_blocking(move || child(&r, &u, &b, "recover", None))
            .await
            .expect("final recovery child");
        assert!(last_recovery.success(), "final recovery: {last_recovery}");
        let (rows, steps) = inspect(&root).await;
        assert_eq!(rows[0].1, "closed");
        assert!(steps.contains(&"graph.00000.fix.finalize.1".into()));
        assert!(steps.contains(&"graph.00000.fix.publish".into()));
        assert_eq!(model.calls(), 1, "recovery repeated a completed turn");
    });
}

#[test]
fn failed_graph_item_criterion_repairs_without_calling_an_independent_reviewer() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into()); 2])
            .await
            .expect("scripted author and repair");
        let root = scratch("graph-criterion-blocks-review");
        let _ = seed(&root).await;
        leave_one_work_child(&root).await;
        let project = root.join("project");
        review_repair_project(&project);
        git(&project, &["add", "."]);
        git(&project, &["commit", "-q", "-m", "review fixture"]);
        let base = git(&project, &["rev-parse", "HEAD"]);
        let store = SqliteStore::open_authorized(
            &root.join("atomic.db"),
            SystemTime::now,
            HumanCredential::new(Principal::local("owner"), SECRET),
        )
        .expect("authorized graph store");
        store
            .set_item_criteria(
                Actor::human("owner"),
                CHILDREN[0].into(),
                vec![Criterion::Cel {
                    expr: "false".into(),
                }],
            )
            .await
            .expect("operator criteria");
        let url = model.base_url();
        let (r, u, b) = (root.clone(), url, base);
        let finished = tokio::task::spawn_blocking(move || child(&r, &u, &b, "start", None))
            .await
            .expect("graph child");
        assert!(finished.success(), "graph child returned: {finished}");
        let result = std::fs::read_to_string(root.join("result.txt")).expect("run result");
        assert!(result.contains("criteria"), "{result}");
        assert_eq!(
            model.calls(),
            2,
            "reviewer was called after failed criteria"
        );
        let (rows, steps) = inspect(&root).await;
        assert_eq!(rows[0].1, "open", "failed item remained claimed or closed");
        assert!(steps.contains(&"graph.00000.fix.criteria.0.0".into()));
        assert!(steps.contains(&"graph.00000.fix.criteria.1.0".into()));
        assert!(
            !steps.iter().any(|step| step.contains(".review.verdict")),
            "{steps:?}"
        );
        assert!(!steps.contains(&"graph.00000.fix.publish".into()));
        let rt = durable(&root).await;
        let leases = rt
            .control_transaction(|mut tx| {
                Box::pin(async move {
                    tx.query_one(
                        "SELECT COUNT(*) AS count FROM atomic_graph_leases WHERE item_id=?",
                        &[Param::Text(CHILDREN[0].into())],
                    )
                    .await?
                    .int("count")
                })
            })
            .await
            .expect("remaining lease count");
        assert_eq!(
            leases, 0,
            "terminal criterion failure stranded a live lease"
        );
    });
}

#[test]
fn imported_beads_epic_can_be_pinned_and_runs_three_items_with_acceptance_text() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into()); 3])
            .await
            .expect("scripted model");
        let root = scratch("import-pin-three");
        let (project, _) = project(&root);
        let workflow = WORKFLOW
            .replace(
                "priority: {type: integer}\n",
                "priority: {type: integer}\n        acceptance_criteria: {type: string}\n",
            )
            .replace(
                "with: { title: input.item.title, base: input.base }",
                "with: { title: input.item.acceptance_criteria, base: input.base }",
            );
        std::fs::write(project.join("workflows/fix.workflow.yaml"), workflow)
            .expect("workflow includes imported acceptance text");
        git(&project, &["add", "."]);
        git(
            &project,
            &["commit", "-q", "-m", "acceptance input fixture"],
        );
        let base = git(&project, &["rev-parse", "HEAD"]);
        let rt = durable(&root).await;
        let credential = HumanCredential::new(Principal::local("owner"), SECRET);
        gate::enroll(&rt, &credential)
            .await
            .expect("enroll operator");
        drop(rt);
        let graph =
            SqliteStore::open_authorized(&root.join("atomic.db"), SystemTime::now, credential)
                .expect("authorized graph store");
        let mut rows = vec![json!({
            "id": EPIC, "title": "Imported epic", "description": "Three imported items",
            "status": "open", "priority": 1, "issue_type": "epic",
            "created_at": "2026-01-01T00:00:00Z"
        })];
        for (index, id) in CHILDREN.iter().enumerate() {
            rows.push(json!({
                "id": id, "title": format!("Imported child {}", index + 1),
                "description": "Imported work item",
                "acceptance_criteria": format!("acceptance text for {id}"),
                "status": "open", "priority": index,
                "issue_type": "task", "parent_issue": EPIC,
                "created_at": "2026-01-02T00:00:00Z"
            }));
        }
        let jsonl = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        graph
            .import(Actor::human("owner"), jsonl, false)
            .await
            .expect("import Beads JSONL");
        assert!(
            graph.authorize_graph_run(EPIC, RUN).await.is_err(),
            "ordinary open imported epic was admitted"
        );
        graph
            .set_status(Actor::human("owner"), EPIC.into(), Status::Pinned)
            .await
            .expect("operator pins imported epic");
        graph
            .authorize_graph_run(EPIC, RUN)
            .await
            .expect("authorize run");
        drop(graph);
        let url = model.base_url();
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let status = tokio::task::spawn_blocking(move || child(&r, &u, &b, "start", None))
            .await
            .expect("child");
        assert!(status.success(), "imported graph run: {status}");
        assert_eq!(model.calls(), 3, "one model turn per imported child");
        for (index, request) in model.requests().iter().enumerate() {
            assert!(
                request
                    .to_string()
                    .contains(&format!("acceptance text for {}", CHILDREN[index])),
                "imported acceptance text did not reach model call {index}: {request}"
            );
        }
        let (items, _) = inspect(&root).await;
        assert!(
            items
                .iter()
                .all(|(_, status, sha)| status == "closed" && sha.len() == 40)
        );
    });
}

#[test]
fn graph_loop_evaluates_the_workflow_returns_expression_for_each_item() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into()); 3])
            .await
            .expect("scripted model");
        let root = scratch("returns-mapping");
        let base = seed(&root).await;
        let rt = engine(&root, &model.base_url(), true).await;
        let mut prepared = pipeline(&root.join("project"));
        prepared.returns = "steps.fix.output.done".into();
        prepared.output = json!({"type":"boolean"});
        rt.start(
            GRAPH_PIPELINE,
            RUN,
            GraphPipelineRun {
                run: PipelineRun {
                    pipeline: prepared,
                    input: json!({"base": base}),
                },
                epic: EPIC.into(),
                model_source: None,
                review_routing: true,
            },
        )
        .await
        .expect("start mapped graph run");
        let result: Value = rt.result(RUN).await.expect("mapped run succeeds");
        assert_eq!(result, json!(true));
        assert_eq!(model.calls(), 3);
        let (rows, _) = inspect(&root).await;
        assert!(
            rows.iter()
                .all(|(_, status, sha)| status == "closed" && sha.len() == 40)
        );
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

#[test]
fn empty_epic_cannot_return_null_under_an_object_output_contract() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("scripted model");
        let root = scratch("empty-output-contract");
        let base = seed(&root).await;
        let database = durable(&root).await;
        database
            .control_transaction(|mut tx| {
                Box::pin(async move {
                    tx.execute(
                        "UPDATE atomic_graph_items SET status='closed' WHERE parent=?",
                        &[Param::Text(EPIC.into())],
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .expect("close all work children before run");
        drop(database);
        let rt = engine(&root, &model.base_url(), true).await;
        rt.start(
            GRAPH_PIPELINE,
            RUN,
            GraphPipelineRun {
                run: PipelineRun {
                    pipeline: pipeline(&root.join("project")),
                    input: json!({"base": base}),
                },
                epic: EPIC.into(),
                model_source: None,
                review_routing: true,
            },
        )
        .await
        .expect("start empty graph run");
        let error = rt
            .result::<Value>(RUN)
            .await
            .expect_err("object output rejects null");
        assert!(error.to_string().contains("empty graph result"), "{error}");
        assert_eq!(model.calls(), 0);
        assert!(
            !rt.steps(RUN)
                .await
                .expect("steps")
                .iter()
                .any(|step| step.name == "epic.done")
        );
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

#[test]
fn process_crashes_at_each_graph_boundary_recover_without_repeating_completed_turns() {
    on_codex_runtime(async {
        for (boundary, calls_before) in [
            ("checkpoint:after:graph.00000.claim", 0),
            ("checkpoint:after:graph.00000.fix.turn", 1),
            ("checkpoint:after:graph.00000.fix.publish", 1),
            ("checkpoint:after:graph.00000.fix.close", 1),
            ("pipeline:after:graph.00000.fix", 1),
        ] {
            let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into()); 3])
                .await
                .expect("scripted model");
            let root = scratch(&boundary.replace(':', "-"));
            let base = seed(&root).await;
            let url = model.base_url();
            let (r, u, b) = (root.clone(), url.clone(), base.clone());
            let status =
                tokio::task::spawn_blocking(move || child(&r, &u, &b, "start", Some(boundary)))
                    .await
                    .expect("crash child");
            assert!(!status.success(), "{boundary} did not kill the child");
            assert_eq!(
                model.calls(),
                calls_before,
                "{boundary} model calls before crash"
            );
            let (r, u, b) = (root.clone(), url.clone(), base.clone());
            let status = tokio::task::spawn_blocking(move || child(&r, &u, &b, "recover", None))
                .await
                .expect("recovery child");
            assert!(status.success(), "{boundary} recovery: {status}");
            assert_eq!(model.calls(), 3, "{boundary} repeated a completed turn");
            let (rows, _) = inspect(&root).await;
            assert!(
                rows.iter()
                    .all(|(_, status, sha)| status == "closed" && sha.len() == 40),
                "{boundary}: {rows:?}"
            );
        }
    });
}

#[test]
fn takeover_after_turn_refuses_publication_and_close() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into()); 3])
            .await
            .expect("scripted model");
        let root = scratch("takeover-after-turn");
        let base = seed(&root).await;
        let url = model.base_url();
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let status = tokio::task::spawn_blocking(move || {
            child(
                &r,
                &u,
                &b,
                "start",
                Some("checkpoint:after:graph.00000.fix.turn"),
            )
        })
        .await
        .expect("crash child");
        assert!(!status.success());
        assert_eq!(model.calls(), 1);
        let rt = durable(&root).await;
        rt.control_transaction(|mut tx| Box::pin(async move {
            tx.execute(
                "UPDATE atomic_graph_leases SET claimant='other', epoch=epoch+1, acquisition='takeover' WHERE item_id=?",
                &[Param::Text(CHILDREN[0].into())],
            ).await?;
            tx.execute(
                "UPDATE atomic_graph_items SET claim_epoch=claim_epoch+1 WHERE id=?",
                &[Param::Text(CHILDREN[0].into())],
            ).await?;
            Ok(())
        })).await.expect("take over claim");
        let mut recovered = child_command(&root, &url, &base, "recover", None)
            .spawn()
            .expect("recover stale graph run");
        let parked = wait_for_park(&rt, None).await;
        assert!(parked.reason.contains(CHILDREN[0]), "{parked:?}");
        recovered.kill().expect("stop parked stale run");
        recovered.wait().expect("reap parked stale run");
        assert_eq!(model.calls(), 1, "stale run called the model again");
        let (rows, steps) = inspect(&root).await;
        assert_eq!(rows[0].1, "in_progress");
        assert!(rows[0].2.is_empty(), "stale turn linked a commit");
        assert!(
            !steps.contains(&"graph.00000.fix.publish".to_owned()),
            "stale candidate published"
        );
        assert!(
            !steps.contains(&"graph.00000.fix.close".to_owned()),
            "stale holder closed item"
        );
    });
}

async fn wait_for_park(rt: &Durare, previous: Option<&str>) -> atomic_durable::Park {
    for _ in 0..500 {
        let parks = atomic_durable::release::waiting(rt)
            .await
            .expect("waiting parks");
        if let Some(park) = parks
            .into_iter()
            .find(|park| park.run == RUN && previous.is_none_or(|old| park.id != old))
        {
            return park;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("graph run did not park");
}

async fn release(rt: &Durare, park: &atomic_durable::Park, id: &str) {
    atomic_durable::release::release_park(
        rt,
        &HumanCredential::new(Principal::local("owner"), SECRET),
        atomic_durable::release::ReleaseCommand {
            command_id: id.into(),
            park: park.id.clone(),
            raise: None,
            comment: None,
        },
    )
    .await
    .expect("operator released graph park");
}

#[test]
fn pending_epic_creates_distinct_parks_and_resumes_after_human_release() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into()); 3])
            .await
            .expect("scripted model");
        let root = scratch("pending");
        let base = seed(&root).await;
        let url = model.base_url();
        let graph = SqliteStore::open_authorized(
            &root.join("atomic.db"),
            SystemTime::now,
            HumanCredential::new(Principal::local("owner"), SECRET),
        )
        .expect("authorized graph");
        graph
            .set_status(Actor::human("owner"), CHILDREN[1].into(), Status::Deferred)
            .await
            .expect("defer second item");
        let mut process = child_command(&root, &url, &base, "start", None)
            .spawn()
            .expect("start graph process");
        let rt = durable(&root).await;
        let first = wait_for_park(&rt, None).await;
        assert!(
            first.id.ends_with(":1"),
            "first pending occurrence: {}",
            first.id
        );
        assert_eq!(
            model.calls(),
            1,
            "first child completed before pending park"
        );
        assert!(
            matches!(rt.run_record(RUN).await.expect("run record"), Some(record)
            if record.status == atomic_durable::RunStatus::Pending)
        );
        let (rows, _) = inspect(&root).await;
        assert_eq!(rows[0].1, "closed");
        assert_eq!(rows[1].1, "deferred");

        // Releasing without resolving readiness must create a new occurrence.
        release(&rt, &first, "release-while-blocked").await;
        let second = wait_for_park(&rt, Some(&first.id)).await;
        assert!(
            second.id.ends_with(":2"),
            "second pending occurrence: {}",
            second.id
        );
        assert_ne!(first.id, second.id);
        assert_eq!(model.calls(), 1, "no completed turn repeated at a new park");

        graph
            .set_status(Actor::human("owner"), CHILDREN[1].into(), Status::Open)
            .await
            .expect("make second item ready");
        release(&rt, &second, "release-after-ready").await;
        let mut status = None;
        for _ in 0..1000 {
            if let Some(done) = process.try_wait().expect("poll graph process") {
                status = Some(done);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if status.is_none() {
            let _ = process.kill();
            let _ = process.wait();
            panic!("graph process did not finish after release");
        }
        assert!(
            status.expect("status").success(),
            "graph process failed after release"
        );
        assert_eq!(model.calls(), 3, "each child called the model once");
        let (rows, steps) = inspect(&root).await;
        assert!(
            rows.iter()
                .all(|(_, state, sha)| state == "closed" && sha.len() == 40)
        );
        assert!(
            steps.contains(&"graph.00001.claim.park".to_owned()),
            "first park checkpoint"
        );
        assert!(
            steps.contains(&"graph.00002.claim.park".to_owned()),
            "fresh park checkpoint"
        );
        assert!(steps.contains(&"epic.done".to_owned()));
    });
}

#[test]
fn reviewed_graph_gate_releases_with_handoff_and_reclaims_after_kill_restart() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"reject"}"#.into()),
        ])
        .await
        .expect("scripted author and reviewer");
        let root = scratch("review-gate-recovery");
        let base = seed(&root).await;
        leave_one_work_child(&root).await;
        review_project(&root.join("project"));
        let url = model.base_url();
        let mut first = child_command(&root, &url, &base, "start", None)
            .spawn()
            .expect("start reviewed graph run");
        let rt = durable(&root).await;
        let gate = wait_for_review_gate(&rt).await;
        assert_eq!(model.calls(), 2, "one author and one reviewer turn");
        let (status, leases, handoff) = released_review_handoff(&rt).await;
        if status != "open" || leases != 0 {
            first.kill().expect("stop process missing review release");
            first.wait().expect("reap process missing review release");
        }
        assert_eq!(status, "open", "human gate must release its graph item");
        assert_eq!(leases, 0, "human gate cannot retain a process-local lease");
        assert!(
            handoff
                .as_deref()
                .is_some_and(|note| note.contains("released and must be re-claimed")),
            "review release must record an agent-trust handoff"
        );
        assert!(
            first.try_wait().expect("poll waiting process").is_none(),
            "human gate wait ended at a model turn deadline"
        );
        first.kill().expect("kill at review gate");
        first.wait().expect("reap killed process");
        let mut recovered = child_command(&root, &url, &base, "recover", None)
            .spawn()
            .expect("recover reviewed graph run");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            recovered.try_wait().expect("poll recovery").is_none(),
            "recovered graph did not resume the human gate"
        );
        assert_eq!(model.calls(), 2, "recovery repeated a completed model turn");
        approve_review_gate(&rt, &gate, "approve-review-after-restart").await;
        let mut status = None;
        for _ in 0..1000 {
            if let Some(done) = recovered.try_wait().expect("poll recovered graph") {
                status = Some(done);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(status.expect("recovered graph finished").success());
        assert_eq!(model.calls(), 2, "approval repeated a completed model turn");
        let (rows, steps) = inspect(&root).await;
        assert_eq!(rows[0].1, "closed");
        assert_eq!(rows[0].2.len(), 40);
        assert!(steps.contains(&"graph.00000.fix.review.0.publish".into()));
        assert!(steps.contains(&"graph.00000.fix.review.gate".into()));
        assert!(steps.contains(&"graph.00000.fix.publish".into()));
        assert!(steps.contains(&"graph.00000.fix.close".into()));
    });
}

#[test]
fn a_policy_edit_routes_only_later_graph_lanes_and_records_each_choice() {
    on_codex_runtime(async {
        let first = FakeModel::start(vec![
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"reject"}"#.into()),
        ])
        .await
        .expect("first endpoint");
        let later = FakeModel::start(vec![
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"reject"}"#.into()),
        ])
        .await
        .expect("later endpoint");
        let root = scratch("route-later-lanes");
        let base = seed(&root).await;
        review_project(&root.join("project"));
        let policy_path = root.join("operator.yaml");
        let mut policy: LanePolicy = serde_json::from_value(json!({
            "endpoints": {
                "first": {"base_url": first.base_url(), "name": "first-model"},
                "later": {"base_url": later.base_url(), "name": "later-model"}
            },
            "default": "first"
        }))
        .expect("operator policy");
        let write_policy = |policy: &LanePolicy| {
            std::fs::write(
                &policy_path,
                json!({"model": {"lanes": policy}}).to_string(),
            )
            .expect("operator route file");
        };
        write_policy(&policy);
        let source = LanePolicySource {
            path: policy_path.clone(),
            endpoints: policy.endpoints.clone(),
        };
        let rt = engine(&root, &first.base_url(), true).await;
        rt.control_transaction(|mut tx| {
            Box::pin(async move {
                tx.execute(
                    "UPDATE atomic_graph_items SET status='closed' WHERE id=?",
                    &[Param::Text(CHILDREN[2].into())],
                )
                .await?;
                Ok(())
            })
        })
        .await
        .expect("leave two graph children");
        rt.start(
            GRAPH_PIPELINE,
            RUN,
            GraphPipelineRun {
                run: PipelineRun {
                    pipeline: pipeline(&root.join("project")),
                    input: json!({"base": base}),
                },
                epic: EPIC.into(),
                model_source: Some(source),
                review_routing: true,
            },
        )
        .await
        .expect("start routed graph run");
        let gate = wait_for_review_gate(&rt).await;
        assert_eq!(
            first.calls(),
            2,
            "first author and reviewer use first route"
        );
        assert_eq!(later.calls(), 0);
        policy.default = "later".into();
        write_policy(&policy);
        approve_review_gate(&rt, &gate, "approve-first-routed-lane").await;
        let second = wait_for_review_gate(&rt).await;
        assert_eq!(first.calls(), 2, "completed lane kept its model");
        assert_eq!(later.calls(), 2, "later author and reviewer use new route");
        let steps = rt.steps(RUN).await.expect("durable graph steps");
        for (ordinal, endpoint, model) in [(0, "first", "first-model"), (1, "later", "later-model")]
        {
            let choice = format!("graph.{ordinal:05}.model_choice");
            let recorded = steps
                .iter()
                .find(|step| step.name == choice)
                .and_then(|step| step.output.as_ref())
                .expect("checkpointed model choice");
            assert_eq!(recorded["endpoint"], endpoint);
            let turn = format!("graph.{ordinal:05}.fix.turn");
            let candidate: Candidate = serde_json::from_value(
                steps
                    .iter()
                    .find(|step| step.name == turn)
                    .and_then(|step| step.output.as_ref())
                    .and_then(|output| output.get("Candidate"))
                    .cloned()
                    .expect("checkpointed author candidate"),
            )
            .expect("author candidate");
            assert_eq!(candidate.proposal.metadata["lane_endpoint"], endpoint);
            assert_eq!(candidate.proposal.metadata["model"], model);
            let review = format!("graph.{ordinal:05}.fix.review.0.turn");
            let reviewer: Candidate = serde_json::from_value(
                steps
                    .iter()
                    .find(|step| step.name == review)
                    .and_then(|step| step.output.as_ref())
                    .and_then(|output| output.get("Candidate"))
                    .cloned()
                    .expect("checkpointed reviewer candidate"),
            )
            .expect("reviewer candidate");
            assert_eq!(reviewer.proposal.metadata["lane_endpoint"], endpoint);
            assert_eq!(reviewer.proposal.metadata["model"], model);
        }
        approve_review_gate(&rt, &second, "approve-second-routed-lane").await;
        rt.result::<Value>(RUN).await.expect("routed graph result");
        assert_eq!(first.calls(), 2);
        assert_eq!(later.calls(), 2);
    });
}

#[test]
fn a_crash_after_model_choice_keeps_that_lane_on_its_recorded_endpoint() {
    on_codex_runtime(async {
        let first = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("first endpoint");
        let later = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into()); 2])
            .await
            .expect("later endpoint");
        let root = scratch("routed-choice-recovery");
        let base = seed(&root).await;
        let path = root.join("operator.yaml");
        let mut policy: LanePolicy = serde_json::from_value(json!({
            "endpoints": {
                "first": {"base_url": first.base_url(), "name": "first-model"},
                "later": {"base_url": later.base_url(), "name": "later-model"}
            },
            "default": "first"
        }))
        .expect("operator policy");
        let write = |policy: &LanePolicy| {
            std::fs::write(&path, json!({"model": {"lanes": policy}}).to_string())
                .expect("operator route file");
        };
        write(&policy);
        let stopped = child(
            &root,
            &first.base_url(),
            &base,
            "start-routed",
            Some("checkpoint:after:graph.00000.model_choice"),
        );
        assert!(!stopped.success(), "failpoint must stop the first lane");
        assert_eq!(first.calls(), 0, "choice checkpoint precedes model call");
        policy.default = "later".into();
        write(&policy);
        assert!(
            child(&root, &first.base_url(), &base, "recover", None).success(),
            "recover routed graph run"
        );
        assert_eq!(first.calls(), 1, "completed checkpoint froze first lane");
        assert_eq!(later.calls(), 2, "later lanes saw changed policy");
        let rt = durable(&root).await;
        let steps = rt.steps(RUN).await.expect("durable steps");
        for (ordinal, endpoint) in [(0, "first"), (1, "later"), (2, "later")] {
            let name = format!("graph.{ordinal:05}.model_choice");
            let choice = steps
                .iter()
                .find(|step| step.name == name)
                .and_then(|step| step.output.as_ref())
                .expect("model choice checkpoint");
            assert_eq!(choice["endpoint"], endpoint);
        }
    });
}

#[test]
fn an_ordinary_run_cannot_use_a_graph_only_model_endpoint() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("scripted model");
        let root = scratch("ordinary-with-graph-only-model");
        let (_project, base) = project(&root);
        let rt = engine_at_with_endpoint(
            &root,
            &model.base_url(),
            true,
            root.join("atomic.db"),
            "graph-loop/test",
            false,
        )
        .await;
        rt.start(
            PIPELINE,
            RUN,
            PipelineRun {
                pipeline: pipeline(&root.join("project")),
                input: json!({"base": base}),
            },
        )
        .await
        .expect("start ordinary run");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            rt.status(RUN).await.expect("ordinary run status"),
            Some(atomic_durable::RunStatus::Pending),
            "missing default endpoint must leave a resumable run"
        );
        assert!(
            rt.steps(RUN)
                .await
                .expect("ordinary run checkpoints")
                .is_empty(),
            "refusal must precede the first durable step"
        );
        assert_eq!(model.calls(), 0, "ordinary run reached a model endpoint");
    });
}

#[test]
fn takeover_during_review_gate_cannot_publish_or_close() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"reject"}"#.into()),
        ])
        .await
        .expect("scripted author and reviewer");
        let root = scratch("review-gate-takeover");
        let base = seed(&root).await;
        leave_one_work_child(&root).await;
        review_project(&root.join("project"));
        let url = model.base_url();
        let mut process = child_command(&root, &url, &base, "start", None)
            .spawn()
            .expect("start reviewed graph run");
        let rt = durable(&root).await;
        let gate = wait_for_review_gate(&rt).await;
        assert_eq!(model.calls(), 2);
        let (status, leases, _) = released_review_handoff(&rt).await;
        assert_eq!((status.as_str(), leases), ("open", 0));
        let graph =
            SqliteStore::open(&root.join("atomic.db"), SystemTime::now).expect("graph store");
        let other = graph
            .claim(
                Actor::agent("other"),
                "other".into(),
                CHILDREN[0].into(),
                Duration::from_secs(900),
            )
            .await
            .expect("take over released review item")
            .expect("other claimant won");
        assert_ne!(other.claimant, RUN);
        let operator = SqliteStore::open_authorized(
            &root.join("atomic.db"),
            SystemTime::now,
            HumanCredential::new(Principal::local("owner"), SECRET),
        )
        .expect("operator graph store");
        operator
            .set_status(Actor::human("owner"), CHILDREN[0].into(), Status::Open)
            .await
            .expect("rival released item again");
        approve_review_gate(&rt, &gate, "approve-stale-review").await;
        let parked = wait_for_park(&rt, None).await;
        assert!(parked.reason.contains(CHILDREN[0]), "{parked:?}");
        process.kill().expect("stop parked stale review");
        process.wait().expect("reap parked stale review");
        assert_eq!(model.calls(), 2);
        let (rows, steps) = inspect(&root).await;
        assert_eq!(rows[0].1, "open");
        assert!(rows[0].2.is_empty());
        assert!(!steps.contains(&"graph.00000.fix.publish".into()));
        assert!(!steps.contains(&"graph.00000.fix.close".into()));
    });
}

#[test]
fn closed_epic_during_review_gate_cannot_publish_child() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"reject"}"#.into()),
        ])
        .await
        .expect("scripted author and reviewer");
        let root = scratch("review-gate-closed-epic");
        let base = seed(&root).await;
        leave_one_work_child(&root).await;
        review_project(&root.join("project"));
        let url = model.base_url();
        let mut process = child_command(&root, &url, &base, "start", None)
            .spawn()
            .expect("start reviewed graph run");
        let rt = durable(&root).await;
        let gate = wait_for_review_gate(&rt).await;
        assert_eq!(model.calls(), 2);
        let (status, leases, _) = released_review_handoff(&rt).await;
        assert_eq!((status.as_str(), leases), ("open", 0));
        let operator = SqliteStore::open_authorized(
            &root.join("atomic.db"),
            SystemTime::now,
            HumanCredential::new(Principal::local("owner"), SECRET),
        )
        .expect("operator graph store");
        operator
            .set_status(Actor::human("owner"), EPIC.into(), Status::Closed)
            .await
            .expect("operator closed the formerly pinned epic");
        approve_review_gate(&rt, &gate, "approve-after-epic-closed").await;
        let parked = wait_for_park(&rt, None).await;
        assert!(parked.reason.contains(CHILDREN[0]), "{parked:?}");
        process.kill().expect("stop parked stale review");
        process.wait().expect("reap parked stale review");
        assert_eq!(model.calls(), 2, "stale approval repeated a model turn");
        let (rows, steps) = inspect(&root).await;
        assert_eq!(rows[0].1, "open");
        assert!(rows[0].2.is_empty());
        assert!(!steps.contains(&"graph.00000.fix.publish".into()));
        assert!(!steps.contains(&"graph.00000.fix.close".into()));
    });
}

#[test]
fn closed_then_reopened_child_during_review_gate_cannot_publish() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"reject"}"#.into()),
        ])
        .await
        .expect("scripted author and reviewer");
        let root = scratch("review-gate-reopened-child");
        let base = seed(&root).await;
        leave_one_work_child(&root).await;
        review_project(&root.join("project"));
        let url = model.base_url();
        let mut process = child_command(&root, &url, &base, "start", None)
            .spawn()
            .expect("start reviewed graph run");
        let rt = durable(&root).await;
        let gate = wait_for_review_gate(&rt).await;
        let (status, leases, _) = released_review_handoff(&rt).await;
        assert_eq!((status.as_str(), leases), ("open", 0));
        let operator = SqliteStore::open_authorized(
            &root.join("atomic.db"),
            SystemTime::now,
            HumanCredential::new(Principal::local("owner"), SECRET),
        )
        .expect("operator graph store");
        operator
            .set_status(Actor::human("owner"), CHILDREN[0].into(), Status::Closed)
            .await
            .expect("operator closed gate-released item");
        operator
            .set_status(Actor::human("owner"), CHILDREN[0].into(), Status::Open)
            .await
            .expect("operator reopened item");
        approve_review_gate(&rt, &gate, "approve-after-child-reopened").await;
        let parked = wait_for_park(&rt, None).await;
        assert!(parked.reason.contains(CHILDREN[0]), "{parked:?}");
        process.kill().expect("stop parked stale review");
        process.wait().expect("reap parked stale review");
        assert_eq!(model.calls(), 2, "stale approval repeated a model turn");
        let (rows, steps) = inspect(&root).await;
        assert_eq!(rows[0].1, "open");
        assert!(rows[0].2.is_empty());
        assert!(!steps.contains(&"graph.00000.fix.publish".into()));
        assert!(!steps.contains(&"graph.00000.fix.close".into()));
    });
}

#[test]
fn closing_epic_after_reclaim_crash_fences_completed_review() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"reject"}"#.into()),
        ])
        .await
        .expect("scripted author and reviewer");
        let root = scratch("review-reclaim-epic-close");
        let base = seed(&root).await;
        leave_one_work_child(&root).await;
        review_project(&root.join("project"));
        let url = model.base_url();
        let mut first = child_command(
            &root,
            &url,
            &base,
            "start",
            Some("graph:after-review-reclaim"),
        )
        .spawn()
        .expect("start reviewed graph run");
        let rt = durable(&root).await;
        let gate = wait_for_review_gate(&rt).await;
        let (status, leases, _) = released_review_handoff(&rt).await;
        assert_eq!((status.as_str(), leases), ("open", 0));
        approve_review_gate(&rt, &gate, "approve-before-reclaim-crash").await;
        let mut crashed = None;
        for _ in 0..1000 {
            if let Some(done) = first.try_wait().expect("poll reclaim crash") {
                crashed = Some(done);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !crashed.expect("reclaim failpoint reached").success(),
            "reclaim checkpoint did not crash the child"
        );
        let steps = rt.steps(RUN).await.expect("checkpointed graph steps");
        assert!(
            steps.iter().any(|step| {
                step.name == "graph.00000.fix.review.claim.reclaim.0"
                    && step.output.as_ref().is_some_and(|output| !output.is_null())
            }),
            "reclaim must be durable before the process dies"
        );
        let operator = SqliteStore::open_authorized(
            &root.join("atomic.db"),
            SystemTime::now,
            HumanCredential::new(Principal::local("owner"), SECRET),
        )
        .expect("operator graph store");
        operator
            .set_status(Actor::human("owner"), EPIC.into(), Status::Closed)
            .await
            .expect("operator closed epic after re-claim");
        let recovery_log =
            std::fs::File::create(root.join("recovery.log")).expect("create recovery log");
        let mut recovered = child_command(&root, &url, &base, "recover", None)
            .stderr(recovery_log)
            .spawn()
            .expect("recover after invalidated epic");
        let parked = wait_for_park(&rt, None).await;
        assert!(parked.reason.contains(CHILDREN[0]), "{parked:?}");
        recovered.kill().expect("stop parked stale run");
        recovered.wait().expect("reap parked stale run");
        assert_eq!(model.calls(), 2, "recovery repeated a completed model turn");
        let (rows, steps) = inspect(&root).await;
        assert_eq!(rows[0].1, "open");
        assert!(rows[0].2.is_empty());
        assert!(!steps.contains(&"graph.00000.fix.publish".into()));
        assert!(!steps.contains(&"graph.00000.fix.close".into()));
    });
}

#[test]
fn takeover_after_reviewer_turn_cannot_publish_its_verdict_or_the_author_candidate() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"approve"}"#.into()),
        ])
        .await
        .expect("scripted author and reviewer");
        let root = scratch("reviewer-turn-takeover");
        let base = seed(&root).await;
        leave_one_work_child(&root).await;
        review_project(&root.join("project"));
        let url = model.base_url();
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let crashed = tokio::task::spawn_blocking(move || {
            child(
                &r,
                &u,
                &b,
                "start",
                Some("checkpoint:after:graph.00000.fix.review.0.turn"),
            )
        })
        .await
        .expect("crash after reviewer turn");
        assert!(!crashed.success());
        assert_eq!(model.calls(), 2);
        let rt = durable(&root).await;
        rt.control_transaction(|mut tx| Box::pin(async move {
            tx.execute("UPDATE atomic_graph_leases SET claimant='other',epoch=epoch+1,acquisition='takeover' WHERE item_id=?",
                &[Param::Text(CHILDREN[0].into())]).await?;
            tx.execute("UPDATE atomic_graph_items SET claim_epoch=claim_epoch+1 WHERE id=?",
                &[Param::Text(CHILDREN[0].into())]).await?;
            Ok(())
        })).await.expect("take over before reviewer publication");
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let _ = tokio::task::spawn_blocking(move || child(&r, &u, &b, "recover", None))
            .await
            .expect("recovery process");
        assert_eq!(
            model.calls(),
            2,
            "recovery repeated a completed reviewer turn"
        );
        let (rows, steps) = inspect(&root).await;
        assert_eq!(rows[0].1, "in_progress");
        assert!(rows[0].2.is_empty());
        assert!(steps.contains(&"graph.00000.fix.review.0.turn".into()));
        assert!(!steps.contains(&"graph.00000.fix.review.0.publish".into()));
        assert!(!steps.contains(&"graph.00000.fix.publish".into()));
        assert!(!steps.contains(&"graph.00000.fix.close".into()));
    });
}

#[test]
fn graph_repair_renews_each_turn_and_counts_four_model_calls() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"request_changes"}"#.into()),
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"approve"}"#.into()),
        ])
        .await
        .expect("scripted author, reviewer and repair");
        let root = scratch("review-repair");
        let base = seed(&root).await;
        leave_one_work_child(&root).await;
        review_repair_project(&root.join("project"));
        let (r, u, b) = (root.clone(), model.base_url(), base);
        let output = tokio::task::spawn_blocking(move || {
            child_command(&r, &u, &b, "start", None)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .expect("repair child process")
        })
        .await
        .expect("repair child");
        assert!(output.status.success(), "repair graph child: {output:?}");
        assert_eq!(model.calls(), 4, "one author, reviewer, repair, reviewer");
        let (items, steps) = inspect(&root).await;
        assert_eq!(items[0].1, "closed");
        for step in [
            "graph.00000.fix.review.0.turn",
            "graph.00000.fix.repair.1.turn",
            "graph.00000.fix.review.1.turn",
        ] {
            assert!(steps.contains(&step.to_owned()), "missing {step}");
        }
    });
}

#[test]
fn recovery_without_graph_registrar_cannot_dispatch_unfinished_graph_run() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into()); 3])
            .await
            .expect("scripted model");
        let root = scratch("missing-registrar");
        let base = seed(&root).await;
        let url = model.base_url();
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let status = tokio::task::spawn_blocking(move || {
            child(
                &r,
                &u,
                &b,
                "start",
                Some("checkpoint:after:graph.00000.claim"),
            )
        })
        .await
        .expect("crash child");
        assert!(!status.success());
        let before = inspect(&root).await;
        assert_eq!(model.calls(), 0);
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let status =
            tokio::task::spawn_blocking(move || child(&r, &u, &b, "recover-without-graph", None))
                .await
                .expect("unregistered recovery child");
        if status.success() {
            let result = std::fs::read_to_string(root.join("without-graph.txt"))
                .expect("unregistered recovery result");
            assert_eq!(
                result, "Ok(0)",
                "unregistered recovery dispatched work: {result}"
            );
        }
        assert_eq!(
            model.calls(),
            0,
            "unregistered recovery dispatched a model turn"
        );
        assert_eq!(
            inspect(&root).await,
            before,
            "unregistered recovery mutated graph run"
        );
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let status = tokio::task::spawn_blocking(move || child(&r, &u, &b, "recover", None))
            .await
            .expect("registered recovery child");
        assert!(status.success(), "registered graph recovery: {status}");
        assert_eq!(model.calls(), 3);
    });
}

#[test]
fn cli_verifies_only_the_recorded_graph_attempt_on_its_authorized_branch() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("scripted model");
        let root = scratch("cli-verified-commit");
        let (project, base) = project(&root);
        let url = model.base_url();
        enroll_cli(&root, &project, &url).await;
        cli_success(
            &cli(
                &root,
                &project,
                &url,
                &[
                    "graph", "create", "Epic", "--id", EPIC, "--kind", "epic", "--pinned",
                ],
            )
            .await,
        );
        cli_success(
            &cli(
                &root,
                &project,
                &url,
                &["graph", "create", "Work", "--id", CHILDREN[0]],
            )
            .await,
        );
        cli_success(
            &cli(
                &root,
                &project,
                &url,
                &["graph", "link", CHILDREN[0], "parent-child", EPIC],
            )
            .await,
        );
        cli_success(
            &cli(
                &root,
                &project,
                &url,
                &[
                    "graph",
                    "policy",
                    "set",
                    CHILDREN[0],
                    "--require",
                    "verified-commit",
                    "--branch",
                    "refs/heads/main",
                ],
            )
            .await,
        );
        let database = cli_database(&root, &project);
        let credential = HumanCredential::new(
            Principal::local("graph-cli-test"),
            "graph CLI verified commit operator",
        );
        let graph = SqliteStore::open_authorized(&database, SystemTime::now, credential)
            .expect("authorized CLI graph store");
        graph
            .authorize_graph_run(EPIC, RUN)
            .await
            .expect("authorized run");
        let state_id = cli_state_id(&root, &project);
        let snapshot = root
            .join("state-home/projects")
            .join(&state_id)
            .join("fixtures/cli.git");
        std::fs::create_dir_all(snapshot.parent().expect("fixtures directory"))
            .expect("fixtures directory");
        git(
            &project,
            &[
                "clone",
                "--bare",
                project.to_str().expect("source"),
                snapshot.to_str().expect("snapshot"),
            ],
        );
        let fixture = atomic_core::fixture::FixtureBinding {
            source: project.clone(),
            rev: base.clone(),
            commit: base.clone(),
            tree: git(&project, &["rev-parse", "HEAD^{tree}"]),
            snapshot: snapshot.clone(),
            worktrees: root
                .join("state-home/projects")
                .join(&state_id)
                .join("worktrees"),
        };
        let mut prepared = pipeline(&project);
        prepared.fixture = Some(fixture);
        let rt = engine_at(
            &root,
            &url,
            true,
            database.clone(),
            &format!("graph-loop/{state_id}"),
        )
        .await;
        rt.start(
            GRAPH_PIPELINE,
            RUN,
            GraphPipelineRun {
                run: PipelineRun {
                    pipeline: prepared,
                    input: json!({"base":base}),
                },
                epic: EPIC.into(),
                model_source: None,
                review_routing: true,
            },
        )
        .await
        .expect("start graph fixture run");
        let proof_park = wait_for_park(&rt, None).await;
        assert!(
            proof_park.reason.contains("host branch proof"),
            "{proof_park:?}"
        );
        assert_eq!(model.calls(), 1, "the candidate was generated once");
        let record = rt
            .run_record(RUN)
            .await
            .expect("run record")
            .expect("run exists");
        let frozen: GraphPipelineRun = serde_json::from_value(record.input).expect("frozen run");
        let snapshot = frozen
            .run
            .pipeline
            .fixture
            .expect("frozen fixture")
            .snapshot;
        let steps = rt.steps(RUN).await.expect("graph checkpoints");
        let candidate: Value = steps
            .iter()
            .find(|step| step.name == "graph.00000.fix.turn")
            .and_then(|step| step.output.as_ref())
            .and_then(|output| output.get("Candidate"))
            .cloned()
            .expect("checkpointed candidate");
        let sha = candidate["proposal"]["metadata"]["commit"]
            .as_str()
            .expect("host commit SHA")
            .to_owned();
        assert_eq!(sha.len(), 40);
        git(&snapshot, &["update-ref", "refs/heads/main", &sha]);
        // An alias attempt can reconstruct the same commit and base from Git.
        // The checkpoint's exact commit ref must still bind the attestation
        // to attempt 1 rather than accepting this forged attempt 2.
        git(
            &snapshot,
            &[
                "update-ref",
                &format!("refs/atomic/{RUN}/steps/graph.00000.fix/2"),
                &sha,
            ],
        );
        for (item, step, attempt) in [
            (CHILDREN[0], "graph.00001.fix", "1"),
            (CHILDREN[0], "graph.00000.fix", "2"),
            ("at-other", "graph.00000.fix", "1"),
        ] {
            let forged = cli(
                &root,
                &project,
                &url,
                &[
                    "graph",
                    "verify-commit",
                    item,
                    "--run",
                    RUN,
                    "--step",
                    step,
                    "--attempt",
                    attempt,
                ],
            )
            .await;
            assert!(
                !forged.status.success(),
                "forged {item}/{step}/{attempt} was accepted"
            );
        }
        let success: Value = serde_json::from_str(&cli_success(
            &cli(
                &root,
                &project,
                &url,
                &[
                    "--json",
                    "graph",
                    "verify-commit",
                    CHILDREN[0],
                    "--run",
                    RUN,
                    "--step",
                    "graph.00000.fix",
                    "--attempt",
                    "1",
                ],
            )
            .await,
        ))
        .expect("verified JSON");
        assert_eq!(success["sha"], sha);
        assert_eq!(success["verified"], true);
        let closed: Value = serde_json::from_str(&cli_success(
            &cli(
                &root,
                &project,
                &url,
                &["--json", "graph", "close", CHILDREN[0]],
            )
            .await,
        ))
        .expect("closed JSON");
        assert_eq!(closed["status"], "closed");
        // Attestation and an independent human close do not silently finish
        // the graph run waiting at its proof park.
        let run_state: Value = serde_json::from_str(&cli_success(
            &cli(&root, &project, &url, &["--json", "runs", "show", RUN]).await,
        ))
        .expect("parked run JSON");
        assert_eq!(run_state["status"], "unfinished");
        assert_eq!(run_state["graph"]["binding"], "active");
        let link = rt.control_transaction(move |mut tx| {
            let sha = sha.clone();
            Box::pin(async move {
            tx.query_one(
                "SELECT verified_branch,verified_head,pinned_base FROM atomic_graph_commit_links WHERE item_id=? AND sha=?",
                &[Param::Text(CHILDREN[0].into()), Param::Text(sha)],
            ).await
            })
        }).await.expect("host proof row");
        assert_eq!(
            link.text("verified_branch").expect("branch"),
            "refs/heads/main"
        );
        assert!(link.text_opt("verified_head").expect("head").is_some());
        assert_eq!(link.text("pinned_base").expect("base"), base);
    });
}

#[test]
fn cli_verifies_only_the_accepted_no_review_criteria_repair_commit() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done":false}"#.into()),
            Reply::Text(r#"{"done":true}"#.into()),
        ])
        .await
        .expect("scripted author and criteria repair");
        let root = scratch("cli-criteria-repair-proof");
        let (project, base) = project(&root);
        let url = model.base_url();
        enroll_cli(&root, &project, &url).await;
        for args in [
            vec![
                "graph", "create", "Epic", "--id", EPIC, "--kind", "epic", "--pinned",
            ],
            vec!["graph", "create", "Work", "--id", CHILDREN[0]],
            vec!["graph", "link", CHILDREN[0], "parent-child", EPIC],
            vec![
                "graph",
                "policy",
                "set",
                CHILDREN[0],
                "--require",
                "verified-commit",
                "--branch",
                "refs/heads/main",
            ],
        ] {
            cli_success(&cli(&root, &project, &url, &args).await);
        }
        let database = cli_database(&root, &project);
        let graph = SqliteStore::open_authorized(
            &database,
            SystemTime::now,
            HumanCredential::new(
                Principal::local("graph-cli-test"),
                "graph CLI verified commit operator",
            ),
        )
        .expect("authorized CLI graph store");
        graph
            .set_item_criteria(
                Actor::human("graph-cli-test"),
                CHILDREN[0].into(),
                vec![Criterion::Cel {
                    expr: "candidate.output.done".into(),
                }],
            )
            .await
            .expect("operator criterion");
        graph
            .authorize_graph_run(EPIC, RUN)
            .await
            .expect("authorize run");
        let state_id = cli_state_id(&root, &project);
        let snapshot = root
            .join("state-home/projects")
            .join(&state_id)
            .join("fixtures/cli-criteria-repair.git");
        std::fs::create_dir_all(snapshot.parent().expect("fixture directory"))
            .expect("fixture directory");
        git(
            &project,
            &[
                "clone",
                "--bare",
                project.to_str().expect("source"),
                snapshot.to_str().expect("snapshot"),
            ],
        );
        let fixture = atomic_core::fixture::FixtureBinding {
            source: project.clone(),
            rev: base.clone(),
            commit: base.clone(),
            tree: git(&project, &["rev-parse", "HEAD^{tree}"]),
            snapshot: snapshot.clone(),
            worktrees: root
                .join("state-home/projects")
                .join(&state_id)
                .join("worktrees"),
        };
        let mut prepared = pipeline(&project);
        prepared.fixture = Some(fixture);
        let rt = engine_at(
            &root,
            &url,
            true,
            database,
            &format!("graph-loop/{state_id}"),
        )
        .await;
        rt.start(
            GRAPH_PIPELINE,
            RUN,
            GraphPipelineRun {
                run: PipelineRun {
                    pipeline: prepared,
                    input: json!({"base":base}),
                },
                epic: EPIC.into(),
                model_source: None,
                review_routing: true,
            },
        )
        .await
        .expect("start graph fixture run");
        let proof_park = wait_for_park(&rt, None).await;
        assert!(proof_park.reason.contains("host branch proof"));
        assert_eq!(model.calls(), 2, "one author and one criteria repair");
        let steps = rt.steps(RUN).await.expect("durable steps");
        let accepted: Acceptance = serde_json::from_value(
            steps
                .iter()
                .find(|step| step.name == "graph.00000.fix.accepted")
                .and_then(|step| step.output.clone())
                .expect("accepted candidate receipt"),
        )
        .expect("acceptance receipt");
        let repair: Candidate = serde_json::from_value(
            steps
                .iter()
                .find(|step| step.name == "graph.00000.fix.repair.1.turn")
                .and_then(|step| step.output.as_ref())
                .and_then(|output| output.get("Candidate"))
                .cloned()
                .expect("repair candidate"),
        )
        .expect("repair candidate value");
        assert_eq!(accepted.candidate, repair.id);
        assert_eq!(accepted.digest, repair.digest);
        let sha = repair.proposal.metadata["commit"]
            .as_str()
            .expect("repair host commit SHA");
        git(&snapshot, &["update-ref", "refs/heads/main", sha]);
        let rejected = cli(
            &root,
            &project,
            &url,
            &[
                "graph",
                "verify-commit",
                CHILDREN[0],
                "--run",
                RUN,
                "--step",
                "graph.00000.fix",
                "--attempt",
                "1",
            ],
        )
        .await;
        assert!(
            !rejected.status.success(),
            "unaccepted first commit was attested"
        );
        let verified: Value = serde_json::from_str(&cli_success(
            &cli(
                &root,
                &project,
                &url,
                &[
                    "--json",
                    "graph",
                    "verify-commit",
                    CHILDREN[0],
                    "--run",
                    RUN,
                    "--step",
                    "graph.00000.fix.repair.1",
                    "--attempt",
                    "1",
                ],
            )
            .await,
        ))
        .expect("verified repair JSON");
        assert_eq!(verified["sha"], sha);
        assert_eq!(verified["verified"], true);
    });
}

#[test]
fn cli_verifies_repaired_review_candidate_only_after_its_branch_and_live_claim() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"request_changes"}"#.into()),
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"approve"}"#.into()),
        ])
        .await
        .expect("scripted reviewed repair");
        let root = scratch("cli-repaired-commit");
        let (project, base) = project(&root);
        review_repair_project(&project);
        let url = model.base_url();
        enroll_cli(&root, &project, &url).await;
        for args in [
            vec![
                "graph", "create", "Epic", "--id", EPIC, "--kind", "epic", "--pinned",
            ],
            vec!["graph", "create", "Work", "--id", CHILDREN[0]],
            vec!["graph", "link", CHILDREN[0], "parent-child", EPIC],
            vec![
                "graph",
                "policy",
                "set",
                CHILDREN[0],
                "--require",
                "verified-commit",
                "--branch",
                "refs/heads/main",
            ],
        ] {
            cli_success(&cli(&root, &project, &url, &args).await);
        }
        let database = cli_database(&root, &project);
        let credential = HumanCredential::new(
            Principal::local("graph-cli-test"),
            "graph CLI verified commit operator",
        );
        let graph = SqliteStore::open_authorized(&database, SystemTime::now, credential)
            .expect("authorized CLI graph store");
        graph
            .authorize_graph_run(EPIC, RUN)
            .await
            .expect("authorize run");
        let state_id = cli_state_id(&root, &project);
        let snapshot = root
            .join("state-home/projects")
            .join(&state_id)
            .join("fixtures/cli-repair.git");
        std::fs::create_dir_all(snapshot.parent().expect("fixtures directory"))
            .expect("fixtures directory");
        git(
            &project,
            &[
                "clone",
                "--bare",
                project.to_str().expect("source"),
                snapshot.to_str().expect("snapshot"),
            ],
        );
        let fixture = atomic_core::fixture::FixtureBinding {
            source: project.clone(),
            rev: base.clone(),
            commit: base.clone(),
            tree: git(&project, &["rev-parse", "HEAD^{tree}"]),
            snapshot: snapshot.clone(),
            worktrees: root
                .join("state-home/projects")
                .join(&state_id)
                .join("worktrees"),
        };
        let mut prepared = pipeline(&project);
        prepared.fixture = Some(fixture);
        let rt = engine_at(
            &root,
            &url,
            true,
            database.clone(),
            &format!("graph-loop/{state_id}"),
        )
        .await;
        rt.start(
            GRAPH_PIPELINE,
            RUN,
            GraphPipelineRun {
                run: PipelineRun {
                    pipeline: prepared,
                    input: json!({"base":base}),
                },
                epic: EPIC.into(),
                model_source: None,
                review_routing: true,
            },
        )
        .await
        .expect("start repaired graph run");
        let proof_park = wait_for_park(&rt, None).await;
        assert!(
            proof_park.reason.contains("host branch proof"),
            "{proof_park:?}"
        );
        assert_eq!(model.calls(), 4, "author, reviewer, repair, reviewer");
        let steps = rt.steps(RUN).await.expect("graph checkpoints");
        let candidate = |name: &str| -> Value {
            steps
                .iter()
                .find(|step| step.name == name)
                .and_then(|step| step.output.as_ref())
                .and_then(|output| output.get("Candidate"))
                .cloned()
                .unwrap_or_else(|| panic!("no candidate at {name}"))
        };
        let initial = candidate("graph.00000.fix.turn");
        let repaired = candidate("graph.00000.fix.repair.1.turn");
        let initial_sha = initial["proposal"]["metadata"]["commit"]
            .as_str()
            .expect("initial SHA");
        let repaired_sha = repaired["proposal"]["metadata"]["commit"]
            .as_str()
            .expect("repaired SHA");
        assert_ne!(
            initial_sha, repaired_sha,
            "repair has a distinct recorded attempt"
        );
        let verify = |step: &'static str| {
            [
                "graph",
                "verify-commit",
                CHILDREN[0],
                "--run",
                RUN,
                "--step",
                step,
                "--attempt",
                "1",
            ]
        };
        let repair_step = "graph.00000.fix.repair.1";
        let before_branch = cli(&root, &project, &url, &verify(repair_step)).await;
        assert!(
            !before_branch.status.success(),
            "unreachable repair accepted"
        );
        // The initial attempt is reachable here, so its refusal must come
        // from the durable acceptance checkpoint rather than branch policy.
        git(&snapshot, &["update-ref", "refs/heads/main", initial_sha]);
        let stale = cli(&root, &project, &url, &verify("graph.00000.fix")).await;
        assert!(
            !stale.status.success(),
            "reachable initial candidate was accepted after repair"
        );
        git(&snapshot, &["update-ref", "refs/heads/main", repaired_sha]);
        let wrong_run = cli(
            &root,
            &project,
            &url,
            &[
                "graph",
                "verify-commit",
                CHILDREN[0],
                "--run",
                "other-run",
                "--step",
                repair_step,
                "--attempt",
                "1",
            ],
        )
        .await;
        assert!(!wrong_run.status.success());
        let verified: Value = serde_json::from_str(&cli_success(
            &cli(
                &root,
                &project,
                &url,
                &[
                    "--json",
                    "graph",
                    "verify-commit",
                    CHILDREN[0],
                    "--run",
                    RUN,
                    "--step",
                    repair_step,
                    "--attempt",
                    "1",
                ],
            )
            .await,
        ))
        .expect("repaired verification JSON");
        assert_eq!(verified["sha"], repaired_sha);
        let credentials = atomic_core::credential::FileSecrets::open(
            &root.join("state-home/credentials"),
            &project,
        )
        .expect("CLI credentials");
        atomic_core::credential::SecretStore::save(
            &credentials,
            &format!("operator:graph-cli-test@{state_id}"),
            "forged secret",
        )
        .expect("replace credential with forgery");
        let forged = cli(&root, &project, &url, &verify(repair_step)).await;
        assert!(
            !forged.status.success(),
            "forged operator credential was accepted"
        );
        atomic_core::credential::SecretStore::save(
            &credentials,
            &format!("operator:graph-cli-test@{state_id}"),
            "graph CLI verified commit operator",
        )
        .expect("restore operator credential");
        rt.control_transaction(|mut tx| Box::pin(async move {
            tx.execute("UPDATE atomic_graph_leases SET claimant='other',epoch=epoch+1,acquisition='takeover' WHERE item_id=?",
                &[Param::Text(CHILDREN[0].into())]).await?;
            tx.execute("UPDATE atomic_graph_items SET claim_epoch=claim_epoch+1 WHERE id=?",
                &[Param::Text(CHILDREN[0].into())]).await?;
            Ok(())
        })).await.expect("take over graph claim");
        let takeover = cli(&root, &project, &url, &verify(repair_step)).await;
        assert!(
            !takeover.status.success(),
            "old reviewed claim verified after takeover"
        );
    });
}

#[test]
fn cli_verifies_reviewed_candidate_with_the_post_gate_reclaim_token() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done":true}"#.into()),
            Reply::Text(r#"{"verdict":"reject"}"#.into()),
        ])
        .await
        .expect("scripted author and reviewer");
        let root = scratch("cli-post-gate-commit");
        let (project, base) = project(&root);
        review_project(&project);
        let url = model.base_url();
        enroll_cli(&root, &project, &url).await;
        for args in [
            vec![
                "graph", "create", "Epic", "--id", EPIC, "--kind", "epic", "--pinned",
            ],
            vec!["graph", "create", "Work", "--id", CHILDREN[0]],
            vec!["graph", "link", CHILDREN[0], "parent-child", EPIC],
            vec![
                "graph",
                "policy",
                "set",
                CHILDREN[0],
                "--require",
                "verified-commit",
                "--branch",
                "refs/heads/main",
            ],
        ] {
            cli_success(&cli(&root, &project, &url, &args).await);
        }
        let database = cli_database(&root, &project);
        let secret = "graph CLI verified commit operator";
        let credential = HumanCredential::new(Principal::local("graph-cli-test"), secret);
        let graph = SqliteStore::open_authorized(&database, SystemTime::now, credential.clone())
            .expect("authorized CLI graph store");
        graph
            .authorize_graph_run(EPIC, RUN)
            .await
            .expect("authorize run");
        let state_id = cli_state_id(&root, &project);
        let snapshot = root
            .join("state-home/projects")
            .join(&state_id)
            .join("fixtures/cli-gate.git");
        std::fs::create_dir_all(snapshot.parent().expect("fixtures directory"))
            .expect("fixtures directory");
        git(
            &project,
            &[
                "clone",
                "--bare",
                project.to_str().expect("source"),
                snapshot.to_str().expect("snapshot"),
            ],
        );
        let fixture = atomic_core::fixture::FixtureBinding {
            source: project.clone(),
            rev: base.clone(),
            commit: base.clone(),
            tree: git(&project, &["rev-parse", "HEAD^{tree}"]),
            snapshot: snapshot.clone(),
            worktrees: root
                .join("state-home/projects")
                .join(&state_id)
                .join("worktrees"),
        };
        let mut prepared = pipeline(&project);
        prepared.fixture = Some(fixture);
        let rt = engine_at(
            &root,
            &url,
            true,
            database,
            &format!("graph-loop/{state_id}"),
        )
        .await;
        let approver = HumanCredential::new(Principal::local("owner"), SECRET);
        gate::enroll(&rt, &approver)
            .await
            .expect("enroll review approver");
        rt.start(
            GRAPH_PIPELINE,
            RUN,
            GraphPipelineRun {
                run: PipelineRun {
                    pipeline: prepared,
                    input: json!({"base":base}),
                },
                epic: EPIC.into(),
                model_source: None,
                review_routing: true,
            },
        )
        .await
        .expect("start reviewed graph run");
        let gate = wait_for_review_gate(&rt).await;
        let (status, leases, handoff) = released_review_handoff(&rt).await;
        assert_eq!((status.as_str(), leases), ("open", 0));
        assert!(
            handoff
                .as_deref()
                .is_some_and(|note| note.contains("re-claimed"))
        );
        gate::decide(
            &rt,
            &approver,
            DecisionCommand {
                command_id: "approve-cli-post-gate".into(),
                gate: gate.id.clone(),
                verdict: Verdict::Approve,
                comment: Some("accept reviewed candidate".into()),
                proposal: gate.proposal.clone(),
                answer: None,
            },
        )
        .await
        .expect("operator approves review");
        let proof_park = wait_for_park(&rt, None).await;
        assert!(
            proof_park.reason.contains("host branch proof"),
            "{proof_park:?}"
        );
        let staged = rt
            .control_transaction(|mut tx| {
                Box::pin(async move {
                    let link = tx.query_one(
                "SELECT indexed,on_branch FROM atomic_graph_commit_links WHERE item_id=?",
                &[Param::Text(CHILDREN[0].into())],
            ).await?;
                    let publication = tx
                        .query_one("SELECT COUNT(*) AS count FROM atomic_publications", &[])
                        .await?;
                    Ok((
                        link.int("indexed")?,
                        link.int("on_branch")?,
                        publication.int("count")?,
                    ))
                })
            })
            .await
            .expect("staged unverified link");
        assert_eq!(
            staged,
            (0, 0, 1),
            "proof wait published the author candidate or claimed verification; the reviewer has one publication"
        );
        release(&rt, &proof_park, "release-without-branch-proof").await;
        let second_proof_park = wait_for_park(&rt, Some(&proof_park.id)).await;
        assert!(second_proof_park.reason.contains("host branch proof"));
        assert_eq!(
            model.calls(),
            2,
            "empty proof release repeated a model turn"
        );
        assert_eq!(
            model.calls(),
            2,
            "gate wait and proof did not repeat model turns"
        );
        let steps = rt.steps(RUN).await.expect("graph checkpoints");
        let claim: Value = steps
            .iter()
            .find(|step| step.name == "graph.00000.claim")
            .and_then(|step| step.output.clone())
            .expect("original claim");
        let original: atomic_graph::store::Lease =
            serde_json::from_value(claim["Claimed"]["lease"].clone()).expect("original lease");
        let renewed: atomic_graph::store::Lease = serde_json::from_value(
            steps
                .iter()
                .find(|step| step.name == "graph.00000.fix.review.claim.reclaim.0")
                .and_then(|step| step.output.clone())
                .expect("post-gate re-claim"),
        )
        .expect("post-gate lease");
        assert!(
            renewed.epoch > original.epoch,
            "re-claim must mint a fresh epoch"
        );
        let candidate: Value = steps
            .iter()
            .find(|step| step.name == "graph.00000.fix.turn")
            .and_then(|step| step.output.as_ref())
            .and_then(|output| output.get("Candidate"))
            .cloned()
            .expect("checkpointed candidate");
        let sha = candidate["proposal"]["metadata"]["commit"]
            .as_str()
            .expect("host commit SHA");
        git(&snapshot, &["update-ref", "refs/heads/main", sha]);
        let verified: Value = serde_json::from_str(&cli_success(
            &cli(
                &root,
                &project,
                &url,
                &[
                    "--json",
                    "graph",
                    "verify-commit",
                    CHILDREN[0],
                    "--run",
                    RUN,
                    "--step",
                    "graph.00000.fix",
                    "--attempt",
                    "1",
                ],
            )
            .await,
        ))
        .expect("post-gate proof JSON");
        assert_eq!(verified["sha"], sha);
        assert_eq!(verified["verified"], true);
        release(&rt, &second_proof_park, "release-after-branch-proof").await;
        rt.result::<Value>(RUN)
            .await
            .expect("verified item closes after proof");
        assert_eq!(model.calls(), 2, "release repeated author or reviewer");
        let closed = graph
            .show(Actor::agent("inspect"), CHILDREN[0].into())
            .await
            .expect("show verified child")
            .expect("verified child");
        assert_eq!(closed.item.status, Status::Closed);
        let linked = rt
            .control_transaction(|mut tx| {
                Box::pin(async move {
                    let row = tx
                        .query_one(
                            "SELECT sha FROM atomic_graph_commit_links WHERE item_id=?",
                            &[Param::Text(CHILDREN[0].into())],
                        )
                        .await?;
                    row.text("sha")
                })
            })
            .await
            .expect("verified commit link");
        assert_eq!(linked, sha);
        let steps = rt
            .steps(RUN)
            .await
            .expect("verified graph steps")
            .into_iter()
            .map(|step| step.name)
            .collect::<Vec<_>>();
        assert!(steps.contains(&"graph.00000.fix.publish".into()));
        assert!(steps.contains(&"graph.00000.fix.close".into()));
    });
}

// needs_sandbox
#[cfg(feature = "sandbox-tests")]
#[test]
fn cli_run_graph_epic_completes_one_work_item_with_a_real_fixture() {
    support::require_sandbox();
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("scripted model");
        let root = scratch("cli-graph-run");
        let (project, base) = project(&root);
        let url = model.base_url();
        enroll_cli(&root, &project, &url).await;
        cli_success(
            &cli(
                &root,
                &project,
                &url,
                &[
                    "graph", "create", "Epic", "--id", EPIC, "--kind", "epic", "--pinned",
                ],
            )
            .await,
        );
        cli_success(
            &cli(
                &root,
                &project,
                &url,
                &["graph", "create", "Work", "--id", CHILDREN[0]],
            )
            .await,
        );
        cli_success(
            &cli(
                &root,
                &project,
                &url,
                &["graph", "link", CHILDREN[0], "parent-child", EPIC],
            )
            .await,
        );
        let input = json!({"base":base}).to_string();
        cli_success(
            &cli(
                &root,
                &project,
                &url,
                &[
                    "run",
                    "workflows/fix.workflow.yaml",
                    "--graph-epic",
                    EPIC,
                    "--run-id",
                    RUN,
                    "--data",
                    &input,
                    "--fixture",
                    project.to_str().expect("fixture path"),
                    "--fixture-rev",
                    &base,
                ],
            )
            .await,
        );
        assert_eq!(
            model.calls(),
            1,
            "one CLI graph work item called the model once"
        );
        let shown: Value = serde_json::from_str(&cli_success(
            &cli(&root, &project, &url, &["--json", "runs", "show", RUN]).await,
        ))
        .expect("run JSON");
        assert_eq!(shown["status"], "succeeded");
        assert_eq!(shown["graph"]["binding"], "done");
        assert_eq!(shown["graph"]["epic_status"], "pinned");
        let database = cli_database(&root, &project);
        let rt = Durare::sqlite(&database).await.expect("CLI database");
        let link = rt.control_transaction(|mut tx| Box::pin(async move {
            tx.query_one("SELECT i.status,l.sha FROM atomic_graph_items i JOIN atomic_graph_commit_links l ON l.item_id=i.id WHERE i.id=?",
                &[Param::Text(CHILDREN[0].into())]).await
        })).await.expect("closed work item with host commit");
        assert_eq!(link.text("status").expect("status"), "closed");
        assert_eq!(link.text("sha").expect("sha").len(), 40);
    });
}

#[test]
fn mismatched_existing_graph_base_ref_refuses_before_the_model_turn() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into())])
            .await
            .expect("scripted model");
        let root = scratch("mismatched-base-ref");
        let base = seed(&root).await;
        let project = root.join("project");
        std::fs::write(project.join("later.txt"), "another base\n").expect("later commit file");
        git(&project, &["add", "later.txt"]);
        git(&project, &["commit", "-q", "-m", "later"]);
        let later = git(&project, &["rev-parse", "HEAD"]);
        let snapshot = root.join("snapshot.git");
        git(
            &project,
            &[
                "clone",
                "--bare",
                project.to_str().expect("source"),
                snapshot.to_str().expect("snapshot"),
            ],
        );
        let repo = Git::find()
            .expect("git")
            .repo(&snapshot)
            .without_filters()
            .expect("snapshot repo");
        let worktrees = Worktrees::new(repo, root.join("state/worktrees"));
        assert_eq!(
            worktrees.pin_base(RUN, &later).expect("pre-pin wrong base"),
            later
        );
        let mut prepared = pipeline(&project);
        prepared.fixture = Some(atomic_core::fixture::FixtureBinding {
            source: project.clone(),
            rev: base.clone(),
            commit: base.clone(),
            tree: git(&project, &["rev-parse", &format!("{base}^{{tree}}")]),
            snapshot,
            worktrees: root.join("state/worktrees"),
        });
        let rt = engine(&root, &model.base_url(), true).await;
        rt.start(
            GRAPH_PIPELINE,
            RUN,
            GraphPipelineRun {
                run: PipelineRun {
                    pipeline: prepared,
                    input: json!({"base":base}),
                },
                epic: EPIC.into(),
                model_source: None,
                review_routing: true,
            },
        )
        .await
        .expect("start graph run");
        let refused = rt
            .result::<Value>(RUN)
            .await
            .expect_err("mismatched pinned base");
        assert!(
            refused.to_string().contains("pinned Git base differs"),
            "{refused}"
        );
        assert_eq!(model.calls(), 0, "mismatched base reached the model");
        assert!(
            !rt.steps(RUN)
                .await
                .expect("steps")
                .iter()
                .any(|step| step.name.ends_with(".claim"))
        );
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

#[test]
fn recovery_refuses_a_tampered_graph_base_before_repeating_a_claim_or_turn() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(r#"{"done":true}"#.into()); 3])
            .await
            .expect("scripted model");
        let root = scratch("tampered-base-recovery");
        let base = seed(&root).await;
        let project = root.join("project");
        std::fs::write(project.join("later.txt"), "tamper target\n").expect("later file");
        git(&project, &["add", "later.txt"]);
        git(&project, &["commit", "-q", "-m", "later"]);
        let later = git(&project, &["rev-parse", "HEAD"]);
        let snapshot = root.join("snapshot.git");
        git(
            &project,
            &[
                "clone",
                "--bare",
                project.to_str().expect("source"),
                snapshot.to_str().expect("snapshot"),
            ],
        );
        let fixture = atomic_core::fixture::FixtureBinding {
            source: project.clone(),
            rev: base.clone(),
            commit: base.clone(),
            tree: git(&project, &["rev-parse", &format!("{base}^{{tree}}")]),
            snapshot: snapshot.clone(),
            worktrees: root.join("state/worktrees"),
        };
        std::fs::write(
            root.join("graph-fixture.json"),
            serde_json::to_vec(&fixture).expect("serialize fixture"),
        )
        .expect("frozen fixture file");
        let url = model.base_url();
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let crashed = tokio::task::spawn_blocking(move || {
            child(
                &r,
                &u,
                &b,
                "start",
                Some("checkpoint:after:graph.00000.claim"),
            )
        })
        .await
        .expect("crash process");
        assert!(!crashed.success());
        assert_eq!(model.calls(), 0);
        assert_eq!(
            git(
                &snapshot,
                &["rev-parse", &format!("refs/atomic/{RUN}/base")]
            ),
            base
        );
        git(
            &snapshot,
            &["update-ref", &format!("refs/atomic/{RUN}/base"), &later],
        );
        let (r, u, b) = (root.clone(), url.clone(), base.clone());
        let _ = tokio::task::spawn_blocking(move || child(&r, &u, &b, "recover", None))
            .await
            .expect("recovery process");
        assert_eq!(model.calls(), 0, "tampered recovery reached the model");
        let (_, steps) = inspect(&root).await;
        assert_eq!(
            steps
                .iter()
                .filter(|step| *step == "graph.00000.claim")
                .count(),
            1
        );
        assert!(
            !steps
                .iter()
                .any(|step| step.ends_with(".turn") || step == "graph.00001.claim")
        );
        let outcome = std::fs::read_to_string(root.join("result.txt")).expect("recovery outcome");
        assert!(outcome.contains("pinned Git base differs"), "{outcome}");
    });
}
