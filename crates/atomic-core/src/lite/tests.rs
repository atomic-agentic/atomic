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
            sign_in: None,
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

// ---------------------------------------------------------------------------
// Metering: what each call settles as, and the limit it is held to.

use crate::usage::{Call, Limit, Meter, MeterFuture, Metering, Outcome, Ticket, Usage};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A meter that admits every call with `limit` and records what each was
/// admitted with and settled as.
#[derive(Default)]
struct Recorder {
    limit: Limit,
    calls: Mutex<Vec<Call>>,
    settled: Mutex<Vec<Outcome>>,
}

impl Meter for Recorder {
    fn admit(&self, call: Call) -> MeterFuture<'_, Ticket> {
        Box::pin(async move {
            let ticket = Ticket {
                request: format!("r:{}", call.index),
                attempt: 1,
                model: call.model.clone(),
                admitted: Instant::now(),
                limit: self.limit,
                bounded_by: None,
            };
            self.calls.lock().expect("calls").push(call);
            Ok(ticket)
        })
    }

    fn settle(&self, _: Ticket, outcome: Outcome) -> MeterFuture<'_, ()> {
        Box::pin(async move {
            self.settled.lock().expect("settled").push(outcome);
            Ok(())
        })
    }
}

fn metered(rt: &mut LiteRuntime, limit: Limit) -> Arc<Recorder> {
    let recorder = Arc::new(Recorder {
        limit,
        ..Recorder::default()
    });
    rt.meter = Metering::new(recorder.clone());
    recorder
}

fn settled(recorder: &Recorder) -> Vec<Outcome> {
    recorder.settled.lock().expect("settled").clone()
}

#[tokio::test]
async fn a_reply_without_usage_settles_as_unknown_never_as_free() {
    for wire in [Wire::ChatCompletions, Wire::Responses] {
        let model = FakeModel::start(vec![Reply::WithoutUsage(GOOD.into())])
            .await
            .expect("fake model");
        let mut rt = runtime(&model, wire);
        let recorder = metered(&mut rt, Limit::default());
        let (_, meta) = run_lite_atom(&Classify, &note(), &rt).await.expect("runs");
        assert_eq!(settled(&recorder), [Outcome::Unknown], "{wire:?}");
        assert_eq!((meta.input_tokens, meta.output_tokens), (0, 0));
        // With usage, it settles as reported.
        let model = FakeModel::start(vec![Reply::Text(GOOD.into())])
            .await
            .expect("fake model");
        let mut rt = runtime(&model, wire);
        let recorder = metered(&mut rt, Limit::default());
        run_lite_atom(&Classify, &note(), &rt).await.expect("runs");
        assert_eq!(
            settled(&recorder),
            [Outcome::Reported(Usage {
                input_tokens: 1,
                output_tokens: 1
            })]
        );
    }
}

#[test]
fn usage_with_a_missing_or_malformed_count_is_unknown() {
    let read = |v: Value| usage_of(&v, "prompt_tokens", "completion_tokens");
    assert_eq!(read(json!({})), None);
    assert_eq!(read(json!({"usage": null})), None);
    assert_eq!(read(json!({"usage": {"prompt_tokens": 5}})), None);
    assert_eq!(
        read(json!({"usage": {"input_tokens": 5, "output_tokens": 2}})),
        None,
        "another wire's names"
    );
    assert_eq!(
        read(json!({"usage": {"prompt_tokens": -1, "completion_tokens": 2}})),
        None
    );
    assert_eq!(
        read(json!({"usage": {"prompt_tokens": 5, "completion_tokens": 2}})),
        Some(Usage {
            input_tokens: 5,
            output_tokens: 2
        })
    );
}

#[tokio::test]
async fn a_call_is_sent_with_its_reserved_output_limit() {
    // The fake model is a local server: `max_tokens` on Chat Completions.
    for (wire, field) in [
        (Wire::ChatCompletions, "max_tokens"),
        (Wire::Responses, "max_output_tokens"),
    ] {
        let model = FakeModel::start(vec![Reply::Text(GOOD.into())])
            .await
            .expect("fake model");
        let mut rt = runtime(&model, wire);
        let recorder = metered(
            &mut rt,
            Limit {
                output_tokens: Some(321),
                time: None,
            },
        );
        run_lite_atom(&Classify, &note(), &rt).await.expect("runs");
        assert_eq!(model.requests()[0][field], json!(321), "{wire:?}");
        // The request's size bounds its input tokens.
        let calls = recorder.calls.lock().expect("calls").clone();
        let size = serde_json::to_vec(&model.requests()[0])
            .expect("encode")
            .len() as u64;
        assert!(
            calls[0].input_bound.is_some_and(|b| b >= size),
            "{:?} for a {size}-byte request",
            calls[0].input_bound
        );
    }
    // Unmetered, no limit is sent.
    let model = FakeModel::start(vec![Reply::Text(GOOD.into())])
        .await
        .expect("fake model");
    run_lite_atom(&Classify, &note(), &runtime(&model, Wire::ChatCompletions))
        .await
        .expect("runs");
    assert!(model.requests()[0].get("max_tokens").is_none());
}

#[tokio::test]
async fn a_call_is_cut_off_when_its_reserved_time_runs_out() {
    let model = FakeModel::start(vec![Reply::Delayed {
        after: Duration::from_secs(5),
        reply: Box::new(Reply::Text(GOOD.into())),
    }])
    .await
    .expect("fake model");
    let mut rt = runtime(&model, Wire::ChatCompletions);
    let recorder = metered(
        &mut rt,
        Limit {
            output_tokens: None,
            time: Some(Duration::from_millis(300)),
        },
    );
    let began = Instant::now();
    match run_lite_atom(&Classify, &note(), &rt).await {
        Err(AtomError::Model(message)) => assert!(message.contains("cut off"), "{message}"),
        other => panic!("expected the call to be cut off, got {other:?}"),
    }
    assert!(began.elapsed() < Duration::from_secs(4), "cut off in time");
    // It may have reached the provider: charged at its reservation.
    assert_eq!(settled(&recorder), [Outcome::Unknown]);
}

#[tokio::test]
async fn a_failed_call_settles_by_whether_it_could_have_been_sent() {
    // An error from the provider: it was sent, and what it spent is unknown.
    let model = FakeModel::start(vec![Reply::Http {
        status: 500,
        body: "{}".into(),
    }])
    .await
    .expect("fake model");
    let mut rt = runtime(&model, Wire::ChatCompletions);
    let recorder = metered(&mut rt, Limit::default());
    assert!(run_lite_atom(&Classify, &note(), &rt).await.is_err());
    assert_eq!(settled(&recorder), [Outcome::Unknown]);
    // No connection was ever made: nothing was sent.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = closed.local_addr().expect("addr").port();
    drop(closed);
    let mut rt = LiteRuntime::new(
        ModelEndpoint {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            model: "synthetic".into(),
            api_key_env: None,
            sign_in: None,
        },
        AtomConfig::default(),
    );
    let recorder = metered(&mut rt, Limit::default());
    assert!(run_lite_atom(&Classify, &note(), &rt).await.is_err());
    assert_eq!(settled(&recorder), [Outcome::NotDispatched]);
}

#[test]
fn the_output_limit_goes_in_the_field_its_endpoint_reads() {
    use LimitField::*;
    for (wire, base, field) in [
        (Wire::ChatCompletions, "http://127.0.0.1:8080/v1", MaxTokens),
        (
            Wire::ChatCompletions,
            "http://localhost:11434/v1",
            MaxTokens,
        ),
        (
            Wire::ChatCompletions,
            "https://api.openai.com/v1",
            MaxCompletionTokens,
        ),
        (
            Wire::ChatCompletions,
            "https://example.openai.azure.com/openai/v1",
            MaxCompletionTokens,
        ),
        (
            Wire::ChatCompletions,
            "https://example.cognitiveservices.azure.com/openai/v1",
            MaxCompletionTokens,
        ),
        (
            Wire::ChatCompletions,
            "https://eu.api.openai.com/v1",
            MaxCompletionTokens,
        ),
        (
            Wire::ChatCompletions,
            "https://us.api.openai.com/v1",
            MaxCompletionTokens,
        ),
        (
            Wire::ChatCompletions,
            "https://example.services.ai.azure.com/openai/v1",
            MaxCompletionTokens,
        ),
        (
            Wire::ChatCompletions,
            "https://example.openai.azure.us/openai/v1",
            MaxCompletionTokens,
        ),
        (
            Wire::ChatCompletions,
            "https://example.openai.azure.cn/openai/v1",
            MaxCompletionTokens,
        ),
        (
            Wire::ChatCompletions,
            "https://notapi.openai.com.example/v1",
            MaxTokens,
        ),
        (
            Wire::Responses,
            "https://api.openai.com/v1",
            MaxOutputTokens,
        ),
        (Wire::Responses, "http://127.0.0.1:8080/v1", MaxOutputTokens),
    ] {
        assert_eq!(
            LimitField::for_endpoint(wire, base),
            field,
            "{wire:?} {base}"
        );
    }
    assert_eq!(MaxCompletionTokens.name(), "max_completion_tokens");
}

#[tokio::test]
async fn a_judged_answer_still_settles_the_usage_it_reported() {
    let usage = json!({"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10});
    for body in [
        // A refusal.
        json!({"id": "c", "choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": null, "refusal": "No."}}], "usage": usage}),
        // No content.
        json!({"id": "c", "choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": null}}], "usage": usage}),
    ] {
        let model = FakeModel::start(vec![Reply::Json(body.clone())])
            .await
            .expect("fake model");
        let mut rt = runtime(&model, Wire::ChatCompletions);
        let recorder = metered(&mut rt, Limit::default());
        assert!(
            run_lite_atom(&Classify, &note(), &rt).await.is_err(),
            "{body}"
        );
        assert_eq!(
            settled(&recorder),
            [Outcome::Reported(Usage {
                input_tokens: 7,
                output_tokens: 3
            })],
            "{body}"
        );
    }
}

#[tokio::test]
async fn an_answer_cut_off_at_its_output_limit_is_its_own_error_with_no_repair() {
    for (wire, body) in [
        (
            Wire::ChatCompletions,
            json!({"id": "c", "choices": [{"index": 0, "finish_reason": "length",
                "message": {"role": "assistant", "content": "{\"category\": \"bu"}}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 50, "total_tokens": 57}}),
        ),
        (
            Wire::Responses,
            json!({"id": "r", "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
                "output": [{"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "{\"category\": \"bu"}]}],
                "usage": {"input_tokens": 7, "output_tokens": 50, "total_tokens": 57}}),
        ),
    ] {
        let model = FakeModel::start(vec![Reply::Json(body), Reply::Text(GOOD.into())])
            .await
            .expect("fake model");
        let mut rt = runtime(&model, wire);
        let recorder = metered(&mut rt, Limit::default());
        match run_lite_atom(&Classify, &note(), &rt).await {
            Err(AtomError::Model(message)) => {
                assert!(
                    message.contains("cut off at the call's output limit"),
                    "{message}"
                );
            }
            other => panic!("{wire:?}: expected a truncation error, got {other:?}"),
        }
        assert_eq!(model.calls(), 1, "{wire:?}: no repair turn");
        assert_eq!(
            settled(&recorder),
            [Outcome::Reported(Usage {
                input_tokens: 7,
                output_tokens: 50
            })]
        );
    }
}

/// A runtime on `base_url` speaking Chat Completions.
fn chat_at(base_url: &str) -> LiteRuntime {
    let mut rt = LiteRuntime::new(
        ModelEndpoint {
            base_url: base_url.into(),
            model: "m".into(),
            api_key_env: None,
            sign_in: None,
        },
        AtomConfig::default(),
    );
    rt.wire = Wire::ChatCompletions;
    rt
}

#[test]
fn a_request_body_carries_its_limit_in_the_endpoints_field_or_the_one_set() {
    let exchange = [(Role::User, "hi".to_owned())];
    let body = |rt: &LiteRuntime| request_body(rt, "m", None, &exchange, None, Some(99));
    for base in [
        "https://api.openai.com/v1",
        "https://example.openai.azure.com/openai/v1",
    ] {
        let sent = body(&chat_at(base));
        assert_eq!(sent["max_completion_tokens"], json!(99), "{base}");
        assert!(sent.get("max_tokens").is_none(), "{base}");
    }
    let sent = body(&chat_at("http://127.0.0.1:8080/v1"));
    assert_eq!(sent["max_tokens"], json!(99));
    // A gateway the URL does not identify: the setting chooses.
    let mut gateway = chat_at("https://llm-gateway.example/v1");
    assert_eq!(body(&gateway)["max_tokens"], json!(99));
    gateway.limit_field = Some(LimitField::MaxCompletionTokens);
    let sent = body(&gateway);
    assert_eq!(sent["max_completion_tokens"], json!(99));
    assert!(sent.get("max_tokens").is_none());
}

#[tokio::test]
async fn an_answer_a_content_filter_stopped_is_not_a_truncation() {
    let body = json!({"id": "r", "status": "incomplete",
        "incomplete_details": {"reason": "content_filter"},
        "output": [{"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": "{"}]}],
        "usage": {"input_tokens": 7, "output_tokens": 1, "total_tokens": 8}});
    let model = FakeModel::start(vec![Reply::Json(body), Reply::Text(GOOD.into())])
        .await
        .expect("fake model");
    let mut rt = runtime(&model, Wire::Responses);
    let recorder = metered(&mut rt, Limit::default());
    match run_lite_atom(&Classify, &note(), &rt).await {
        Err(AtomError::Model(message)) => {
            assert!(message.contains("content_filter"), "{message}");
            assert!(!message.contains("output limit"), "{message}");
        }
        other => panic!("expected the provider's stop, got {other:?}"),
    }
    assert_eq!(model.calls(), 1, "no repair turn");
    assert_eq!(
        settled(&recorder),
        [Outcome::Reported(Usage {
            input_tokens: 7,
            output_tokens: 1
        })]
    );
}

/// A chat span says of a provider's error its status and the type of error
/// it names, none of the provider's own text; any other error is cut. The
/// error itself keeps the start of the body.
#[test]
fn a_chat_spans_error_is_the_status_and_type_not_the_body() {
    let body = format!(
        "{{\"error\":{{\"message\":\"{}\",\"type\":\"invalid_request_error\"}}}}",
        "the provider's own words ".repeat(200)
    );
    let value: Value = serde_json::from_str(&body).expect("JSON");
    let error = anyhow::Error::from(ProviderError {
        url: "http://127.0.0.1:9/v1/chat/completions".into(),
        status: "401 Unauthorized".into(),
        code: 401,
        kind: error_kind(&value),
        excerpt: excerpt(&body),
    })
    .context("rank-note");
    let (message, status) = crate::usage::span_error(&error);
    assert_eq!(message, "401 Unauthorized: invalid_request_error");
    assert_eq!(status, Some(401));
    assert!(format!("{error:#}").contains("the provider's own words"));
    assert_eq!(
        error_kind(&json!({"error": {"code": "rate_limit_exceeded"}})).as_deref(),
        Some("rate_limit_exceeded")
    );
    // A type must read as one; a key, or the provider's words, does not.
    let kinds_kept = [
        "invalid_request_error",
        "overloaded",
        "server_error",
        "error_429",
        "invalid_base64",
        &"a".repeat(40),
    ];
    let wrong: Vec<&str> = kinds_kept
        .iter()
        .copied()
        .filter(|k| !is_error_kind(k))
        .collect();
    assert!(wrong.is_empty(), "refused: {wrong:?}");
    let kinds_refused = [
        "",
        "see the words here",
        &"a".repeat(41),
        "Invalid",
        "4xx_error",
        "abc123def456",
        concat!("sk-", "abcdefghijklmnop0123"),
        concat!("ghp_", "AbCdEfGhIjKlMnOp0123456789"),
        concat!("AKIA", "1234567890ABCDEF"),
        "deadbeef00112233",
        concat!("key_", "0123456789abcdef0123456789ab"),
        concat!("tok_", "9f8e7d6c5b4a39281706f5e4d3c2b1a0"),
        concat!("api_key_live_", "3f9a1c7e5b2d4680"),
        "a1b2c3d4_e5f6_4a7b_8c9d_0e1f2a3b4c5d",
    ];
    let wrong: Vec<usize> = kinds_refused
        .iter()
        .enumerate()
        .filter(|(_, k)| is_error_kind(k))
        .map(|(i, _)| i)
        .collect();
    assert!(wrong.is_empty(), "kept cases {wrong:?}");
    // With no type, the status alone.
    let bare = anyhow::Error::from(ProviderError {
        url: "http://127.0.0.1:9/v1/chat/completions".into(),
        status: "401 Unauthorized".into(),
        code: 401,
        kind: None,
        excerpt: excerpt("key rejected"),
    });
    assert_eq!(crate::usage::span_error(&bare).0, "401 Unauthorized");
    // A type of letters alone that is a key reads as one, and the summary's
    // redaction catches it.
    const SAMPLE_TYPE: &str = concat!("sk_live_", "abcdefghijklmnopqrst");
    assert!(is_error_kind(SAMPLE_TYPE));
    let keyed = anyhow::Error::from(ProviderError {
        url: "http://127.0.0.1:9/v1/chat/completions".into(),
        status: "401 Unauthorized".into(),
        code: 401,
        kind: Some(SAMPLE_TYPE.into()),
        excerpt: excerpt("key rejected"),
    });
    assert_eq!(
        crate::usage::span_error(&keyed).0,
        "401 Unauthorized: [REDACTED]"
    );
    // Any other error is redacted, then cut to the span's bound, ellipsis
    // included: a key across the cut shows none of itself, as it would if
    // it were cut first, or not redacted.
    const SAMPLE: &str = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789");
    let across = anyhow!("{} {SAMPLE} and more", "x".repeat(150));
    let (message, status) = crate::usage::span_error(&across);
    assert!(
        message.chars().count() <= crate::usage::MAX_SPAN_ERROR,
        "{} characters",
        message.chars().count()
    );
    assert!(!message.contains("ghp_"), "the key showed");
    assert!(message.contains("[REDACTED]"), "the key was not redacted");
    assert_eq!(status, None);
    // A connection's failure keeps what failed and its innermost cause.
    let refused = anyhow!("Connection refused (os error 111)")
        .context("error sending request for url (http://127.0.0.1:9/v1/chat/completions)")
        .context("POST http://127.0.0.1:9/v1/chat/completions");
    assert_eq!(
        crate::usage::span_error(&refused).0,
        "POST http://127.0.0.1:9/v1/chat/completions: Connection refused (os error 111)"
    );
}
