// SPDX-License-Identifier: Apache-2.0

//! The change-request workflow on durare, against the scripted fake model:
//!
//! - both atoms run, the second on the first one's typed output, and both
//!   published results are recorded;
//! - killing the process after the first atom's checkpoint, then restarting,
//!   does not run the first atom again: the fake model sees one call for it;
//! - a panicking atom fails its own step, and a workflow running beside it in
//!   the same process finishes.

// Codex's turn futures nest deeply inside the workflow's; proving them Send
// needs more than the default query depth.
#![recursion_limit = "512"]

use atomic_app::workflows::{
    CHANGE_REQUEST, ChangeRequestResult, ChangeRequestRun, propose, register_change_request,
};
use atomic_core::codex::{CodexPaths, ModelEndpoint};
use atomic_core::lite::LiteRuntime;
use atomic_core::runtime::{Artifacts, CodexRuntime, RunMetadata, Runtimes};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::{AtomConfig, AtomError};
use atomic_durable::{DurableError, DurableRuntime, Durare, RunStatus, WorkflowContext, run_atom};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const DRAFT: &str = r#"{"title": "Charge for every unit in the cart", "priority": 1, "criteria": ["Two units at 125 cents total 250 cents."]}"#;
const REVIEW: &str =
    r#"{"verdict": "approve", "summary": "The two-unit criterion holds.", "findings": []}"#;

fn input() -> ChangeRequestRun {
    ChangeRequestRun {
        request: "Two units at 1.25 should cost 2.50.".into(),
        base: "1".repeat(40),
        commit: "2".repeat(40),
    }
}

fn scratch(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "atomic-change-request-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["home", "cwd"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
    dir
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
            guard: atomic_core::codex::StateGuard::NoProjectState,
            cwd: dir.join("cwd"),
            endpoint: endpoint.clone(),
            defaults: AtomConfig::default(),
            artifacts: Artifacts::none(),
            meter: atomic_core::usage::Metering::none(),
        },
        lite: LiteRuntime::new(endpoint, AtomConfig::default()),
        backend: Default::default(),
        clock: None,
    })
}

async fn engine(dir: &Path, base_url: &str) -> Durare {
    let mut rt = Durare::sqlite(&dir.join("atomic.db"))
        .await
        .expect("engine");
    register_change_request(&mut rt, runtimes(dir, base_url), Arc::default())
        .expect("the default budgets resolve");
    rt
}

#[test]
fn both_atoms_run_and_the_second_consumes_the_first() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(DRAFT.into()), Reply::Text(REVIEW.into())])
            .await
            .expect("fake model");
        let dir = scratch("two");
        let rt = engine(&dir, &model.base_url()).await;
        rt.launch().await.expect("launch");
        rt.start(CHANGE_REQUEST, "cr-two", input())
            .await
            .expect("start");
        let out: ChangeRequestResult = rt.result("cr-two").await.expect("finishes");

        assert_eq!(out.task.title, "Charge for every unit in the cart");
        assert_eq!(out.verdict.summary, "The two-unit criterion holds.");
        // One call per atom, the lite one first, then the Codex turn.
        assert_eq!(model.paths(), ["/v1/chat/completions", "/v1/responses"]);
        // The review was asked about the drafted task.
        assert!(
            model.requests()[1]
                .to_string()
                .contains("Two units at 125 cents total 250 cents."),
            "the second atom got the first one's output"
        );
        // Both published results, with their run metadata.
        assert_eq!(out.steps.len(), 2);
        let meta: RunMetadata =
            serde_json::from_value(out.steps[1].metadata.clone()).expect("metadata");
        assert!(!meta.thread_id.is_empty());
        let names: Vec<String> = rt
            .steps("cr-two")
            .await
            .expect("steps")
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            names,
            [
                "draft.turn",
                "draft.publish",
                "review.turn",
                "review.publish"
            ]
        );
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

const ENV_ROLE: &str = "ATOMIC_CR_ROLE";
const ENV_DIR: &str = "ATOMIC_CR_DIR";
const ENV_URL: &str = "ATOMIC_CR_URL";

/// Not a test on its own: the parent runs it in a child process.
#[test]
#[ignore = "run by a_killed_process_does_not_rerun_a_finished_atom in a child process"]
fn change_request_child() {
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
        rt.launch().await.expect("launch");
        if role == "start" {
            rt.start(CHANGE_REQUEST, "cr-killed", input())
                .await
                .expect("start");
        } else {
            rt.recover().await.expect("recover");
        }
        let out: ChangeRequestResult = rt.result("cr-killed").await.expect("finishes");
        std::fs::write(
            dir.join("result.json"),
            serde_json::to_string(&out).expect("encode"),
        )
        .expect("write the result");
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

fn child(dir: &Path, url: &str, role: &str, failpoint: Option<&str>) -> std::process::ExitStatus {
    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args([
        "--exact",
        "change_request_child",
        "--ignored",
        "--nocapture",
    ])
    .env(ENV_ROLE, role)
    .env(ENV_DIR, dir)
    .env(ENV_URL, url)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    match failpoint {
        Some(f) => cmd.env("ATOMIC_FAILPOINT", f),
        None => cmd.env_remove("ATOMIC_FAILPOINT"),
    };
    cmd.status().expect("run the child")
}

#[test]
fn a_killed_process_does_not_rerun_a_finished_atom() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(DRAFT.into()), Reply::Text(REVIEW.into())])
            .await
            .expect("fake model");
        let dir = scratch("killed");
        let url = model.base_url();

        // The first process dies right after the draft atom is published.
        let (d, u) = (dir.clone(), url.clone());
        let status = tokio::task::spawn_blocking(move || {
            child(&d, &u, "start", Some("change-request:after-draft"))
        })
        .await
        .expect("child");
        assert!(!status.success(), "the first process was killed: {status}");
        assert_eq!(
            model.paths(),
            ["/v1/chat/completions"],
            "only the draft ran"
        );
        assert!(!dir.join("result.json").exists());

        // A new process recovers the workflow and finishes it.
        let (d, u) = (dir.clone(), url.clone());
        let status = tokio::task::spawn_blocking(move || child(&d, &u, "recover", None))
            .await
            .expect("child");
        assert!(status.success(), "the second process finished: {status}");
        assert_eq!(
            model.paths(),
            ["/v1/chat/completions", "/v1/responses"],
            "the draft atom was not run again: one call for it, then the review"
        );
        let out: ChangeRequestResult = serde_json::from_str(
            &std::fs::read_to_string(dir.join("result.json")).expect("result"),
        )
        .expect("decode");
        assert_eq!(out.task.title, "Charge for every unit in the cart");
        assert_eq!(out.verdict.summary, "The two-unit criterion holds.");
    });
}

#[test]
fn a_panicking_atom_fails_its_step_and_spares_the_other_workflows() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(DRAFT.into()), Reply::Text(REVIEW.into())])
            .await
            .expect("fake model");
        let dir = scratch("panic");
        let mut rt = engine(&dir, &model.base_url()).await;
        rt.register(
            "explodes",
            |ctx: atomic_durable::DurareContext, (): ()| async move {
                let workflow = ctx.workflow_id().to_owned();
                run_atom(
                    &ctx,
                    "boom",
                    move || {
                        let workflow = workflow.clone();
                        propose::<Value, _>(async move {
                            if !workflow.is_empty() {
                                panic!("the atom exploded");
                            }
                            Err(AtomError::Runtime("unreachable".into()))
                        })
                    },
                    Vec::new(),
                )
                .await
            },
        );
        rt.launch().await.expect("launch");
        rt.start("explodes", "boom-1", ()).await.expect("start");
        rt.start(CHANGE_REQUEST, "cr-beside", input())
            .await
            .expect("start");

        let boom = rt.result::<Value>("boom-1").await;
        assert!(
            matches!(&boom, Err(DurableError::Failed(m)) if m.contains("the atom panicked: the atom exploded")),
            "{boom:?}"
        );
        assert!(matches!(
            rt.status("boom-1").await,
            Ok(Some(RunStatus::Failed(_)))
        ));
        let steps = rt.steps("boom-1").await.expect("steps");
        assert_eq!(steps.len(), 1, "{steps:?}");
        assert_eq!(steps[0].name, "boom.turn");
        assert!(
            steps[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("the atom panicked: the atom exploded")),
            "the step records the panic: {steps:?}"
        );

        let beside: ChangeRequestResult = rt.result("cr-beside").await.expect("finishes");
        assert_eq!(beside.verdict.summary, "The two-unit criterion holds.");
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
        let _ = json!(null);
    });
}
