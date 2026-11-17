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

fn runtime(model: &FakeModel, tag: &str) -> CodexRuntime {
    let dir = std::env::temp_dir().join(format!("atomic-runtime-{tag}-{}", std::process::id()));
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
            let model = FakeModel::start(vec![Reply::Text(answer.into())])
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
                other => panic!("{answer}: expected a schema error, got {other:?}"),
            }
        }
        let model = FakeModel::start(vec![Reply::Text(
            r#"{"category": "bug", "summary": "  "}"#.into(),
        )])
        .await
        .expect("fake model");
        let rt = runtime(&model, "invalid");
        assert!(matches!(
            run_codex_atom(&Classify, &Note { text: "n".into() }, &rt).await,
            Err(AtomError::Validation(_))
        ));
    });
}
