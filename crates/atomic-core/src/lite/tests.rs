// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::codex::ModelEndpoint;
use crate::runtime::run_lite_atom;
use crate::testing::{FakeModel, Reply};
use crate::typed::{Atom, AtomError, AtomKind};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Serialize)]
struct Note {
    text: String,
}

#[derive(Debug, Deserialize, PartialEq)]
struct Classified {
    category: String,
    summary: String,
    #[serde(default)]
    line: Option<u32>,
}

/// The classifier the Codex runtime tests use, as a lite atom.
struct Classify;

impl Atom for Classify {
    type In = Note;
    type Out = Classified;
    const NAME: &'static str = "classify-note";
    fn input_schema(&self) -> Value {
        json!({"type": "object", "required": ["text"], "properties": {"text": {"type": "string"}}})
    }
    fn output_schema(&self) -> Value {
        json!({
            "type": "object", "additionalProperties": false,
            "required": ["category", "summary"],
            "properties": {
                "category": {"enum": ["bug", "feature", "question"]},
                "summary": {"type": "string", "maxLength": 40},
                "line": {"type": "integer", "minimum": 1}
            }
        })
    }
    fn instructions(&self) -> Cow<'static, str> {
        "Classify the note as bug, feature or question.".into()
    }
    fn kind(&self) -> AtomKind {
        AtomKind::Lite
    }
}

const GOOD: &str = r#"{"category": "bug", "summary": "Export crashes.", "line": null}"#;

fn runtime(model: &FakeModel, wire: Wire) -> LiteRuntime {
    let mut rt = LiteRuntime::new(
        ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
        },
        AtomConfig::default(),
    );
    rt.wire = wire;
    rt
}

fn note() -> Note {
    Note {
        text: "Export crashes on é.".into(),
    }
}

#[tokio::test]
async fn a_lite_atom_returns_a_valid_out_with_exactly_one_model_call() {
    let model = FakeModel::start(vec![Reply::Text(GOOD.into())])
        .await
        .expect("fake model");
    let rt = runtime(&model, Wire::ChatCompletions);
    let (out, meta) = run_lite_atom(&Classify, &note(), &rt).await.expect("runs");
    assert_eq!(
        out,
        Classified {
            category: "bug".into(),
            summary: "Export crashes.".into(),
            line: None
        }
    );
    assert_eq!(model.calls(), 1, "exactly one model call");
    assert_eq!(model.paths(), ["/v1/chat/completions"]);
    assert_eq!((meta.model.as_str(), meta.turns), ("synthetic", 1));
    assert_eq!(
        (meta.input_tokens, meta.output_tokens, meta.total_tokens),
        (1, 1, 2)
    );
    assert_eq!(meta.thread_id, "chatcmpl_synthetic");
    assert_eq!(meta.prompt_hash.len(), 64);

    // The instructions are the system message, the input is the user message,
    // and the lowered schema constrains decoding, strictly.
    let request = &model.requests()[0];
    assert_eq!(request["stream"], json!(false));
    assert_eq!(
        request["messages"][0],
        json!({"role": "system", "content": "Classify the note as bug, feature or question."})
    );
    assert_eq!(request["messages"][1]["role"], json!("user"));
    assert!(
        request["messages"][1]["content"]
            .as_str()
            .is_some_and(|c| c.contains("Export crashes on é.")),
        "input sent: {request}"
    );
    let format = &request["response_format"];
    assert_eq!(format["type"], json!("json_schema"));
    assert_eq!(format["json_schema"]["strict"], json!(true));
    let schema = &format["json_schema"]["schema"];
    let mut required: Vec<&str> = schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    required.sort_unstable();
    assert_eq!(
        (required, &schema["properties"]["line"]["type"]),
        (
            vec!["category", "line", "summary"],
            &json!(["integer", "null"])
        ),
        "lowered: every property is required, the optional one nullable: {schema}"
    );
    assert!(
        schema["properties"]["summary"].get("maxLength").is_none(),
        "the generation profile drops maxLength: {schema}"
    );
}

#[tokio::test]
async fn the_responses_wire_makes_one_call_with_text_format() {
    let model = FakeModel::start(vec![Reply::Text(GOOD.into())])
        .await
        .expect("fake model");
    let rt = runtime(&model, Wire::Responses);
    let (out, meta) = run_lite_atom(&Classify, &note(), &rt).await.expect("runs");
    assert_eq!(out.category, "bug");
    assert_eq!(model.calls(), 1);
    assert_eq!(model.paths(), ["/v1/responses"]);
    assert_eq!(meta.thread_id, "resp_synthetic");
    assert_eq!((meta.turns, meta.total_tokens), (1, 2));
    let request = &model.requests()[0];
    assert_eq!(
        (&request["stream"], &request["store"]),
        (&json!(false), &json!(false))
    );
    assert_eq!(
        request["instructions"],
        json!("Classify the note as bug, feature or question.")
    );
    assert_eq!(request["input"][0]["role"], json!("user"));
    assert_eq!(request["text"]["format"]["type"], json!("json_schema"));
    assert_eq!(request["text"]["format"]["strict"], json!(true));
}

#[tokio::test]
async fn a_misfit_gets_one_repair_call_then_a_schema_error() {
    for wire in [Wire::ChatCompletions, Wire::Responses] {
        let bad = r#"{"category": "spam", "summary": "x", "line": null}"#;
        let model = FakeModel::start(vec![Reply::Text(bad.into()), Reply::Text(bad.into())])
            .await
            .expect("fake model");
        let rt = runtime(&model, wire);
        match run_lite_atom(&Classify, &note(), &rt).await {
            Err(AtomError::Schema { message, answer }) => {
                assert!(message.contains("/category"), "{wire:?}: {message}");
                assert_eq!(answer, bad);
            }
            other => panic!("{wire:?}: expected a schema error, got {other:?}"),
        }
        assert_eq!(model.calls(), 2, "{wire:?}: one call and one repair");
        // The repair call carries the first exchange and the correction.
        let repair = &model.requests()[1];
        let turns = match wire {
            Wire::ChatCompletions => repair["messages"].clone(),
            Wire::Responses => repair["input"].clone(),
        };
        let turns = turns.as_array().cloned().unwrap_or_default();
        let roles: Vec<&str> = turns.iter().filter_map(|t| t["role"].as_str()).collect();
        let expected: &[&str] = match wire {
            Wire::ChatCompletions => &["system", "user", "assistant", "user"],
            Wire::Responses => &["user", "assistant", "user"],
        };
        assert_eq!(roles, expected, "{wire:?}: {repair}");
        let last = turns.len() - 1;
        assert_eq!(turns[last - 1]["content"], json!(bad));
        assert!(
            turns[last]["content"]
                .as_str()
                .is_some_and(|c| c.contains("did not fit") && c.contains("/category")),
            "{wire:?}: the repair quotes the problem: {repair}"
        );
    }
}

#[tokio::test]
async fn a_repaired_answer_is_accepted() {
    let model = FakeModel::start(vec![
        Reply::Text("not json".into()),
        Reply::Text(GOOD.into()),
    ])
    .await
    .expect("fake model");
    let rt = runtime(&model, Wire::ChatCompletions);
    let (out, meta) = run_lite_atom(&Classify, &note(), &rt)
        .await
        .expect("the repair turn saves it");
    assert_eq!(out.summary, "Export crashes.");
    assert_eq!(model.calls(), 2);
    assert_eq!(
        (
            meta.turns,
            meta.input_tokens,
            meta.output_tokens,
            meta.total_tokens
        ),
        (2, 2, 2, 4),
        "tokens are summed over both calls"
    );
}

#[tokio::test]
async fn a_provider_error_is_a_model_error_with_no_retry() {
    let model = FakeModel::start(vec![Reply::Http {
        status: 503,
        body: r#"{"error": "loading model"}"#.into(),
    }])
    .await
    .expect("fake model");
    let rt = runtime(&model, Wire::ChatCompletions);
    match run_lite_atom(&Classify, &note(), &rt).await {
        Err(AtomError::Model(message)) => {
            assert!(
                message.contains("503") && message.contains("loading model"),
                "{message}"
            );
        }
        other => panic!("expected a model error, got {other:?}"),
    }
    assert_eq!(model.calls(), 1);
}

#[tokio::test]
async fn a_missing_api_key_fails_before_any_call() {
    let model = FakeModel::start(vec![Reply::Text(GOOD.into())])
        .await
        .expect("fake model");
    let mut rt = runtime(&model, Wire::ChatCompletions);
    rt.endpoint.api_key_env = Some("ATOMIC_TEST_UNSET_API_KEY_VARIABLE".into());
    match run_lite_atom(&Classify, &note(), &rt).await {
        Err(AtomError::Model(message)) => {
            assert!(
                message.contains("ATOMIC_TEST_UNSET_API_KEY_VARIABLE"),
                "{message}"
            );
        }
        other => panic!("expected a model error, got {other:?}"),
    }
    assert_eq!(model.calls(), 0);
}

#[tokio::test]
async fn an_atom_runs_only_as_its_own_kind() {
    struct AsCodex;
    impl Atom for AsCodex {
        type In = Note;
        type Out = Classified;
        const NAME: &'static str = "classify-on-codex";
        fn input_schema(&self) -> Value {
            Classify.input_schema()
        }
        fn output_schema(&self) -> Value {
            Classify.output_schema()
        }
        fn instructions(&self) -> Cow<'static, str> {
            Classify.instructions()
        }
    }
    let model = FakeModel::start(vec![Reply::Text(GOOD.into())])
        .await
        .expect("fake model");
    let rt = runtime(&model, Wire::ChatCompletions);
    match run_lite_atom(&AsCodex, &note(), &rt).await {
        Err(AtomError::Runtime(message)) => {
            assert!(message.contains("classify-on-codex"), "{message}");
        }
        other => panic!("expected a runtime error, got {other:?}"),
    }
    assert_eq!(model.calls(), 0);
}

#[test]
fn the_wire_follows_the_endpoint() {
    for (url, wire) in [
        ("https://api.openai.com/v1", Wire::Responses),
        ("https://API.openai.com:443/v1/", Wire::Responses),
        ("http://127.0.0.1:13305/api/v1", Wire::ChatCompletions),
        ("http://localhost:8000/v1", Wire::ChatCompletions),
        (
            "https://api.openai.com.example.net/v1",
            Wire::ChatCompletions,
        ),
        ("api.openai.com/v1", Wire::Responses),
    ] {
        assert_eq!(Wire::for_base_url(url), wire, "{url}");
    }
}

#[test]
fn long_error_bodies_are_cut_on_a_character_boundary() {
    let body = "é".repeat(600);
    let cut = excerpt(&body);
    assert_eq!(cut.chars().count(), 501);
    assert!(cut.ends_with('…'));
    assert_eq!(excerpt("short"), "short");
}
