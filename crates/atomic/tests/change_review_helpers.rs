// SPDX-License-Identifier: Apache-2.0

//! A change-review project that uses every part of captured TypeScript
//! helpers (`tests/fixtures/change-review-helpers`), on production atomic:
//!
//! - its atoms and components check against shared schema types, with
//!   helpers, frontmatter locals, template literals and named slots;
//! - ticket and evidence text that tries to give instructions stays data,
//!   through helpers, components and template literals, while a helper
//!   computed from authored literals alone supplies instructions;
//! - policy routed through a helper that also reads the ticket is external
//!   data, and policy bound directly keeps the team origin its workflow
//!   binding gives it;
//! - the workflow runs on the scripted fake model, and a validator that calls
//!   a helper sends a missing-evidence answer back for one repair turn.

#![recursion_limit = "512"]

use atomic_app::pipeline::{PIPELINE, PipelineRun, Services, prepare, register_pipeline};
use atomic_core::artifact::LocalStore;
use atomic_core::codex::{CodexPaths, ModelEndpoint};
use atomic_core::compile;
use atomic_core::lite::LiteRuntime;
use atomic_core::render::{RunInput, render};
use atomic_core::runtime::{Artifacts, CodexRuntime, Runtimes};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::{AtomConfig, atom, workflow};
use atomic_durable::gate::{Deployment, Principal};
use atomic_durable::{DurableRuntime, Durare};
use atomic_schema::registry::Registry;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SERVICE_INJECTION: &str = "SERVICE-NAME-INJECTION";
const EVIDENCE_INJECTION: &str = "EVIDENCE-INJECTION";
const POLICY_RULE: &str = "POLICY-RULE: start in one region";

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("directory");
    for entry in std::fs::read_dir(source).expect("source") {
        let entry = entry.expect("entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// Helper calls run in a child process: the atomic binary under test.
fn helper_host() {
    use atomic_core::helper_runtime::{CHILD_ARG, HostCommand, set_host_command};
    set_host_command(HostCommand {
        program: env!("CARGO_BIN_EXE_atomic").into(),
        args: vec![CHILD_ARG.to_owned()],
        env: Vec::new(),
    });
}

fn project(tag: &str) -> PathBuf {
    helper_host();
    let dir =
        std::env::temp_dir().join(format!("atomic-change-review-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/change-review-helpers");
    copy_tree(&fixture, &dir.join("project"));
    std::fs::create_dir_all(dir.join("home")).expect("home");
    std::fs::create_dir_all(dir.join("cwd")).expect("cwd");
    dir
}

fn atoms(project: &Path) -> (BTreeMap<String, atom::Atom>, Registry) {
    let mut registry = Registry::new();
    let mut atoms = BTreeMap::new();
    for entry in std::fs::read_dir(project.join("atoms")).expect("atoms") {
        let path = entry.expect("entry").path();
        if path.to_string_lossy().ends_with(".atom.astro") {
            let atom = atom::load(&path, &mut registry).expect("the atom checks");
            atoms.insert(atom.name.clone(), atom);
        }
    }
    (atoms, registry)
}

fn input(project: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(project.join("input.json")).expect("input"))
        .expect("json")
}

/// Renders `name` with the policy from team configuration and the change and
/// its evidence from outside, as the workflow binds them.
fn rendered(project: &Path, name: &str) -> atomic_core::render::Rendered {
    let (atoms, registry) = atoms(project);
    let atom = &atoms[name];
    let compiled = compile::compile(atom, &registry).expect("compiles");
    let origins: BTreeMap<String, String> = [
        ("policy", "team"),
        ("change", "external"),
        ("evidence", "external"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    render(
        &compiled,
        &RunInput {
            run_id: "run-1",
            input: &input(project),
            frontmatter: &atom.frontmatter,
            origin: "external",
            origins: Some(&origins),
            step: None,
        },
    )
    .expect("renders")
}

fn origin_of(user: &str, needle: &str) -> Option<String> {
    user.lines()
        .find(|line| line.contains(needle))
        .and_then(|line| line.split("origin=\"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .map(str::to_owned)
}

#[test]
fn ticket_and_evidence_text_never_reach_instructions() {
    let dir = project("render");
    let out = rendered(&dir.join("project"), "assess-change");
    for injection in [SERVICE_INJECTION, EVIDENCE_INJECTION] {
        assert!(
            !out.developer.contains(injection),
            "{injection} in instructions:\n{}",
            out.developer
        );
        assert_eq!(
            origin_of(&out.user, injection).as_deref(),
            Some("external"),
            "{injection}"
        );
    }
    // A helper computed from authored literals alone supplies instructions,
    // and an absent slot forwarded through the layout renders its fallback.
    assert!(
        out.developer
            .contains("Stage: RISK ASSESSMENT. Recommendations are advisory")
    );
    assert!(
        out.developer
            .contains("Escalate anything the policy does not cover.")
    );
    // Control flow on data selects authored text: rollback failed, and
    // observability evidence is missing.
    assert!(
        out.developer
            .contains("Missing or unsuccessful evidence categories:")
    );
    assert!(
        out.developer
            .contains("This report does not demonstrate a successful check.")
    );
}

#[test]
fn policy_through_a_helper_that_reads_the_ticket_is_external() {
    let dir = project("policy");
    // effectivePolicy(change, policy) reads the external change, so all of
    // its result is external, team policy included.
    let routed = rendered(&dir.join("project"), "assess-change");
    assert_eq!(
        origin_of(&routed.user, POLICY_RULE).as_deref(),
        Some("external")
    );
    // Bound directly, the policy keeps its team origin, and only the
    // approvers computed from the change are external. Team data is still
    // data: it never becomes instructions.
    let split = rendered(&dir.join("project"), "assess-change-split");
    assert_eq!(origin_of(&split.user, POLICY_RULE).as_deref(), Some("team"));
    assert_eq!(
        origin_of(&split.user, ">eu-operations<").as_deref(),
        Some("external")
    );
    assert!(!split.developer.contains(POLICY_RULE));
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
        clock: None,
    })
}

fn system_prompt(request: &Value) -> String {
    request["messages"]
        .as_array()
        .and_then(|messages| messages.iter().find(|m| m["role"] == "system"))
        .and_then(|m| m["content"].as_str())
        .or_else(|| request["instructions"].as_str())
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn the_workflow_runs_and_a_helper_validator_asks_for_one_repair() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            // Missing evidence, yet recommended for review: the validator's
            // helper rejects it.
            Reply::Text(r#"{"verdict": "review", "cited": ["EV-TEST"]}"#.into()),
            Reply::Text(r#"{"verdict": "hold", "cited": ["EV-ROLLBACK"]}"#.into()),
        ])
        .await
        .expect("fake model");
        let dir = project("run");
        let project = dir.join("project");
        let (atoms, mut registry) = atoms(&project);
        let file = project.join("workflows/review.workflow.yaml");
        let loaded = workflow::load(&file, &atoms, &mut registry).expect("the workflow checks");
        let deployment = Deployment::Local {
            operator: Principal::local("dev"),
        };
        let pipeline = prepare(&loaded, &atoms, &deployment).expect("prepares");

        let mut rt = Durare::sqlite(&dir.join("atomic.db"))
            .await
            .expect("engine");
        let store = LocalStore::open(dir.join("artifacts")).expect("artifact store");
        register_pipeline(
            &mut rt,
            Services {
                runtimes: runtimes(&dir, &model.base_url()),
                store,
                deployment,
                worktrees: None,
                budgets: Arc::default(),
                context: None,
            },
        );
        rt.launch().await.expect("launch");
        let given = input(&project);
        rt.start(
            PIPELINE,
            "review-1",
            PipelineRun {
                pipeline,
                input: json!({ "change": given["change"], "evidence": given["evidence"] }),
            },
        )
        .await
        .expect("start");
        let out: Value = rt.result("review-1").await.expect("result");
        assert_eq!(out, json!({"verdict": "hold", "cited": ["EV-ROLLBACK"]}));
        assert_eq!(model.calls(), 2, "one repair turn");
        let requests = model.requests();
        let first = &requests[0];
        let system = system_prompt(first);
        assert!(!system.is_empty(), "{first}");
        for injection in [SERVICE_INJECTION, EVIDENCE_INJECTION, POLICY_RULE] {
            assert!(
                !system.contains(injection),
                "{injection} in instructions: {system}"
            );
        }
        // The workflow wrote the policy, so it arrives as team data.
        let text = first.to_string();
        assert!(text.contains(r#"origin=\"team\">POLICY-RULE"#), "{text}");
        // The repair turn quotes the rule the answer broke.
        assert!(
            requests[1]
                .to_string()
                .contains("missingEvidence(input).length === 0")
        );
        rt.shutdown(std::time::Duration::from_secs(5))
            .await
            .expect("shutdown");
    });
}
