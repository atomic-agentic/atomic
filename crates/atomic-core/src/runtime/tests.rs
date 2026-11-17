// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::testing::{FakeModel, Reply, on_codex_runtime};
use serde::{Deserialize, Serialize};
use serde_json::json;
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
    fn validate(&self, out: &Classified) -> Result<(), AtomError> {
        if out.summary.trim().is_empty() {
            return Err(AtomError::Validation("the summary is empty".into()));
        }
        Ok(())
    }
}

/// A fresh directory for one test's files, never shared with another test.
fn scratch(tag: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("atomic-runtime-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn runtime(model: &FakeModel, tag: &str) -> CodexRuntime {
    let dir = scratch(tag);
    std::fs::create_dir_all(dir.join("home")).expect("home");
    std::fs::create_dir_all(dir.join("cwd")).expect("cwd");
    CodexRuntime {
        paths: CodexPaths::without_helpers(),
        codex_home: dir.join("home"),
        cwd: dir.join("cwd"),
        endpoint: ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
        },
        defaults: AtomConfig::default(),
    }
}

#[test]
fn a_codex_atom_returns_a_valid_out_against_the_fake_model() {
    on_codex_runtime(async {
        // The model fills the lowered schema: `line` is required there and
        // nullable, and its null means "omitted".
        let model = FakeModel::start(vec![Reply::Text(
            r#"{"category": "bug", "summary": "Export crashes.", "line": null}"#.into(),
        )])
        .await
        .expect("fake model");
        let rt = runtime(&model, "valid");
        let (out, meta) = run_codex_atom(
            &Classify,
            &Note {
                text: "Export crashes on é.".into(),
            },
            &rt,
        )
        .await
        .expect("runs");
        assert_eq!(
            out,
            Classified {
                category: "bug".into(),
                summary: "Export crashes.".into(),
                line: None
            }
        );
        assert_eq!(model.calls(), 1);
        assert_eq!((meta.model.as_str(), meta.turns), ("synthetic", 1));
        // The fake model reports 1 input, 1 output and 2 total tokens a call.
        assert_eq!(
            (meta.input_tokens, meta.output_tokens, meta.total_tokens),
            (1, 1, 2)
        );
        assert!(!meta.thread_id.is_empty());
        assert_eq!(meta.prompt_hash.len(), 64);
        assert!(meta.human_actions.is_empty());

        let request = model.requests()[0].to_string();
        assert!(
            request.contains("Classify the note as bug, feature or question."),
            "instructions sent"
        );
        assert!(request.contains("Export crashes on é."), "input sent");
        assert!(
            request.contains("json_schema"),
            "the lowered output schema constrains the answer: {request}"
        );
    });
}

#[test]
fn answers_that_do_not_fit_fail_with_a_schema_error() {
    on_codex_runtime(async {
        for (answer, needle) in [
            ("not json", "not JSON"),
            (r#"{"category": "spam", "summary": "x"}"#, "/category"),
            (
                r#"{"category": "bug", "summary": "this summary is far longer than forty characters allows"}"#,
                "/summary",
            ),
        ] {
            let model =
                FakeModel::start(vec![Reply::Text(answer.into()), Reply::Text(answer.into())])
                    .await
                    .expect("fake model");
            let rt = runtime(&model, "bad");
            match run_codex_atom(&Classify, &Note { text: "n".into() }, &rt).await {
                Err(AtomError::Schema {
                    message,
                    answer: got,
                }) => {
                    assert!(message.contains(needle), "{answer}: {message}");
                    assert_eq!(got, answer);
                }
                other => panic!(
                    "{answer}: expected a schema error after {} model calls, got {other:?}",
                    model.calls()
                ),
            }
        }
        let blank = r#"{"category": "bug", "summary": "  "}"#;
        let model = FakeModel::start(vec![Reply::Text(blank.into()), Reply::Text(blank.into())])
            .await
            .expect("fake model");
        let rt = runtime(&model, "invalid");
        match run_codex_atom(&Classify, &Note { text: "n".into() }, &rt).await {
            Err(AtomError::Validation(_)) => {}
            other => panic!(
                "expected a validation error after {} model calls, got {other:?}",
                model.calls()
            ),
        }
    });
}

#[test]
fn a_bad_output_gets_one_repair_turn_then_fails_with_a_schema_error() {
    on_codex_runtime(async {
        // Repaired: a bad answer, then a good one, on the same thread.
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"category": "spam", "summary": "x"}"#.into()),
            Reply::Text(r#"{"category": "question", "summary": "x"}"#.into()),
        ])
        .await
        .expect("fake model");
        let rt = runtime(&model, "repair-ok");
        let (out, meta) = run_codex_atom(&Classify, &Note { text: "n".into() }, &rt)
            .await
            .expect("repaired");
        assert_eq!(out.category, "question");
        assert_eq!((model.calls(), meta.turns), (2, 2));
        assert_eq!(meta.total_tokens, 4, "two calls of 2 tokens each");
        let repair = model.requests()[1].to_string();
        assert!(
            repair.contains("did not fit the required output"),
            "{repair}"
        );
        assert!(
            repair.contains("/category"),
            "the repair quotes the error: {repair}"
        );

        // Not repaired: exactly one repair turn, then the schema error.
        let model = FakeModel::start(vec![
            Reply::Text("not json".into()),
            Reply::Text("still not json".into()),
            Reply::Text(r#"{"category": "bug", "summary": "never asked for"}"#.into()),
        ])
        .await
        .expect("fake model");
        let rt = runtime(&model, "repair-fails");
        match run_codex_atom(&Classify, &Note { text: "n".into() }, &rt).await {
            Err(AtomError::Schema { answer, .. }) => assert_eq!(answer, "still not json"),
            other => panic!("expected a schema error, got {other:?}"),
        }
        assert_eq!(model.calls(), 2, "one repair, no more");
    });
}

#[test]
fn an_atom_that_panics_fails_its_run_and_the_process_keeps_going() {
    on_codex_runtime(async {
        let r: Result<u32, AtomError> = contained(async { panic!("the atom blew up") }).await;
        assert_eq!(r, Err(AtomError::Panicked("the atom blew up".into())));
        let r: Result<u32, AtomError> =
            contained(async { panic!("{}", String::from("formatted")) }).await;
        assert_eq!(r, Err(AtomError::Panicked("formatted".into())));
        // The runtime, and the process, carry on.
        assert_eq!(contained(async { Ok::<_, AtomError>(7) }).await, Ok(7));
    });
}

#[test]
fn a_declarative_atom_breaking_a_contract_only_constraint_gets_exactly_one_repair() {
    use crate::{atom, compile};
    on_codex_runtime(async {
        let dir = scratch("decl-project");
        std::fs::create_dir_all(dir.join("atoms")).expect("mkdir");
        std::fs::write(
            dir.join("atoms/ticket.atom.md"),
            "---\natomic: 1\ndescription: Name the ticket a note is about.\nkind: lite\n\
             validate:\n  - \"output.ticket != 'ABC-0'\"\n---\nNote: {% $input.text %}\n",
        )
        .expect("atom");
        std::fs::write(
            dir.join("atoms/ticket.input.schema.yaml"),
            "type: object\nrequired: [text]\nproperties:\n  text: {type: string}\n",
        )
        .expect("input");
        // maxLength is not in the strict profile: only the full contract
        // enforces it.
        std::fs::write(
            dir.join("atoms/ticket.output.schema.yaml"),
            "type: object\nadditionalProperties: false\nrequired: [ticket]\nproperties:\n  ticket: {type: string, maxLength: 6}\n",
        )
        .expect("output");
        let mut registry = Registry::new();
        let a = atom::load(&dir.join("atoms/ticket.atom.md"), &mut registry).expect("loads");
        let compiled = compile::compile(&a, &registry).expect("compiles");

        // The first answer fits strict mode but breaks maxLength; the repaired
        // one fits.
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"ticket": "ABC-123456"}"#.into()),
            Reply::Text(r#"{"ticket": "ABC-12"}"#.into()),
        ])
        .await
        .expect("fake model");
        let rt = runtime(&model, "decl");
        let input = json!({"text": "Export crashes, see ABC-12."});
        let (out, meta) = run_declarative(
            &a,
            &compiled,
            &mut registry,
            &input,
            "run-1",
            "external",
            &rt,
        )
        .await
        .expect("repaired");
        assert_eq!(out, json!({"ticket": "ABC-12"}));
        assert_eq!((model.calls(), meta.turns), (2, 2));
        let first = model.requests()[0].to_string();
        assert!(
            first.contains("origin=\\\"external\\\""),
            "the input is wrapped in the user turn: {first}"
        );
        assert!(
            model.requests()[1].to_string().contains("maxLength"),
            "the repair quotes the broken constraint"
        );

        // A validate rule failing twice: one repair, then the validation error.
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"ticket": "ABC-0"}"#.into()),
            Reply::Text(r#"{"ticket": "ABC-0"}"#.into()),
        ])
        .await
        .expect("fake model");
        let rt = runtime(&model, "decl-rule");
        let r = run_declarative(
            &a,
            &compiled,
            &mut registry,
            &input,
            "run-2",
            "external",
            &rt,
        )
        .await;
        assert!(matches!(r, Err(AtomError::Validation(_))), "{r:?}");
        assert_eq!(model.calls(), 2);
    });
}
