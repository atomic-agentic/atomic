// SPDX-License-Identifier: Apache-2.0

//! The golden test: every built-in atom runs against the scripted fake model,
//! with no provider and no network, on the runtime its kind names.
//!
//! Each atom has a directory under `tests/golden/` holding `input.json`, the
//! `answer.json` the model is scripted to give (in the lowered form the
//! generation profile asks for, nulls included), and the `output.json` the
//! atom must return.

use atomic_atoms::{ChangeRequest, DraftTask, ReviewPatch, ReviewRequest};
use atomic_core::codex::{CodexPaths, ModelEndpoint};
use atomic_core::lite::LiteRuntime;
use atomic_core::runtime::{CodexRuntime, run_codex_atom, run_lite_atom};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::{Atom, AtomConfig, AtomError, AtomKind};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::{Path, PathBuf};

fn golden(atom: &str, file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(atom)
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn golden_json(atom: &str, file: &str) -> Value {
    serde_json::from_str(&golden(atom, file)).unwrap_or_else(|e| panic!("{atom}/{file}: {e}"))
}

/// A fresh directory for one run's Codex home and working directory.
fn scratch(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("atomic-golden-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["home", "cwd"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch directory");
    }
    dir
}

fn endpoint(model: &FakeModel) -> ModelEndpoint {
    ModelEndpoint {
        base_url: model.base_url(),
        model: "synthetic".into(),
        api_key_env: None,
    }
}

/// Run `atom` on its golden input against `script`, on its kind's runtime.
async fn run<A>(atom: &A, script: Vec<Reply>) -> (FakeModel, Result<A::Out, AtomError>)
where
    A: Atom,
    A::In: DeserializeOwned,
{
    let input: A::In = serde_json::from_str(&golden(A::NAME, "input.json")).expect("input");
    run_on(atom, &input, script).await
}

/// Run `atom` on `input` against `script`, on its kind's runtime.
async fn run_on<A: Atom>(
    atom: &A,
    input: &A::In,
    script: Vec<Reply>,
) -> (FakeModel, Result<A::Out, AtomError>) {
    let model = FakeModel::start(script).await.expect("fake model");
    let result = match atom.kind() {
        AtomKind::Lite => {
            let rt = LiteRuntime::new(endpoint(&model), AtomConfig::default());
            run_lite_atom(atom, input, &rt).await
        }
        AtomKind::Codex => {
            let dir = scratch(A::NAME);
            let rt = CodexRuntime {
                paths: CodexPaths::without_helpers(),
                codex_home: dir.join("home"),
                guard: atomic_core::codex::StateGuard::NoProjectState,
                cwd: dir.join("cwd"),
                endpoint: endpoint(&model),
                defaults: AtomConfig::default(),
                artifacts: atomic_core::runtime::Artifacts::none(),
                meter: atomic_core::usage::Metering::none(),
                // Its turns run without Codex's sandbox helper, so nothing may be
                // denied to them: see `denying_nothing`.
                backend: atomic_core::testing::denying_nothing(Default::default()),
                caches: None,
            };
            run_codex_atom(atom, input, &rt).await
        }
        AtomKind::Exec => panic!("{} calls no model", A::NAME),
    };
    (model, result.map(|(out, _)| out))
}

/// The golden answer gives the golden output, in exactly one model call, and
/// both golden files fit the atom's full contracts.
async fn check_golden<A>(atom: A, path: &str)
where
    A: Atom,
    A::In: DeserializeOwned,
    A::Out: Serialize + std::fmt::Debug,
{
    let name = A::NAME;
    for (file, schema) in [
        ("input.json", atom.input_schema()),
        ("output.json", atom.output_schema()),
    ] {
        let validator = jsonschema::validator_for(&schema).expect("valid schema");
        let value = golden_json(name, file);
        let errors: Vec<String> = validator
            .iter_errors(&value)
            .map(|e| e.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "{name}/{file} breaks its contract: {errors:?}"
        );
    }
    let (model, result) = run(&atom, vec![Reply::Text(golden(name, "answer.json"))]).await;
    let out = result.unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(
        serde_json::to_value(&out).expect("serializes"),
        golden_json(name, "output.json"),
        "{name}: {out:?}"
    );
    assert_eq!(model.calls(), 1, "{name}: exactly one model call");
    assert_eq!(model.paths(), [path], "{name}: on its kind's runtime");
    // The instructions and the input reach the model.
    let request = model.requests()[0].to_string();
    let first_line = atom.instructions();
    let first_line = first_line.lines().next().unwrap_or_default();
    assert!(request.contains(first_line), "{name}: instructions sent");
    let input = golden_json(name, "input.json");
    let probe = match name {
        "draft-task" => input["request"].as_str().map(str::to_owned),
        _ => input["item"]["title"].as_str().map(str::to_owned),
    }
    .unwrap_or_default();
    assert!(request.contains(&probe), "{name}: input sent");
}

#[test]
fn draft_task_returns_its_golden_output_in_one_lite_call() {
    on_codex_runtime(check_golden(DraftTask, "/v1/chat/completions"));
}

#[test]
fn review_patch_returns_its_golden_output_in_one_codex_turn() {
    on_codex_runtime(check_golden(ReviewPatch, "/v1/responses"));
}

/// A forced bad output gets one repair, then fails with `AtomError::Schema`.
#[test]
fn a_forced_bad_output_gets_one_repair_then_a_schema_error() {
    on_codex_runtime(async {
        let bad = r#"{"title": "", "priority": 9, "criteria": []}"#;
        let (model, result) = run(
            &DraftTask,
            vec![Reply::Text(bad.into()), Reply::Text(bad.into())],
        )
        .await;
        assert!(
            matches!(result, Err(AtomError::Schema { .. })),
            "draft-task: {result:?}"
        );
        assert_eq!(model.calls(), 2, "draft-task: one call and one repair");

        let bad = r#"{"verdict": "maybe", "summary": "x", "findings": []}"#;
        let (model, result) = run(
            &ReviewPatch,
            vec![Reply::Text(bad.into()), Reply::Text(bad.into())],
        )
        .await;
        assert!(
            matches!(result, Err(AtomError::Schema { .. })),
            "review-patch: {result:?}"
        );
        assert_eq!(model.calls(), 2, "review-patch: one turn and one repair");
    });
}

/// An approval that carries a blocker fits the schema but not the reviewer's
/// own check: it is repaired once, then fails validation.
#[test]
fn an_approval_with_a_blocker_fails_the_reviewers_check() {
    on_codex_runtime(async {
        let bad = r#"{"verdict": "approve", "summary": "Looks fine.", "findings": [{"file": "pricing.py", "line": 2, "severity": "blocker", "message": "Two units total 125."}]}"#;
        let (model, result) = run(
            &ReviewPatch,
            vec![Reply::Text(bad.into()), Reply::Text(bad.into())],
        )
        .await;
        assert!(
            matches!(&result, Err(AtomError::Validation(m)) if m.contains("Blocker")),
            "{result:?}"
        );
        assert_eq!(model.calls(), 2);
    });
}

/// An input that breaks its contract fails before anything reaches the model.
#[test]
fn invalid_inputs_never_reach_the_model() {
    on_codex_runtime(async {
        let good = || Reply::Text(golden(DraftTask::NAME, "answer.json"));
        // minLength.
        let request = ChangeRequest {
            request: String::new(),
        };
        let (model, result) = run_on(&DraftTask, &request, vec![good()]).await;
        assert!(
            matches!(&result, Err(AtomError::Input(m)) if m.contains("/request")),
            "{result:?}"
        );
        assert_eq!(model.calls(), 0);

        let review: ReviewRequest =
            serde_json::from_str(&golden(ReviewPatch::NAME, "input.json")).expect("input");
        let good = || Reply::Text(golden(ReviewPatch::NAME, "answer.json"));
        // pattern: a branch name is not a commit.
        let mut by_name = review.clone();
        by_name.commit = "HEAD".into();
        // maximum.
        let mut urgent = review.clone();
        urgent.item.priority = 9;
        // minItems.
        let mut uncheckable = review;
        uncheckable.item.criteria.clear();
        for (input, at) in [
            (by_name, "/commit"),
            (urgent, "/item/priority"),
            (uncheckable, "/item/criteria"),
        ] {
            let (model, result) = run_on(&ReviewPatch, &input, vec![good()]).await;
            assert!(
                matches!(&result, Err(AtomError::Input(m)) if m.contains(at)),
                "{at}: {result:?}"
            );
            assert_eq!(model.calls(), 0, "{at}: no model call");
        }
    });
}
