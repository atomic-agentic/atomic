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
        state_dir: None,
        cwd: dir.join("cwd"),
        endpoint: ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
        },
        defaults: AtomConfig::default(),
        artifacts: Artifacts::none(),
        meter: crate::usage::Metering::none(),
    }
}

/// Both runtimes, on the same fake model.
fn runtimes(model: &FakeModel, tag: &str) -> Runtimes {
    let codex = runtime(model, tag);
    let lite = crate::lite::LiteRuntime::new(codex.endpoint.clone(), AtomConfig::default());
    Runtimes {
        codex,
        lite,
        clock: None,
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
            dir.join("atoms/ticket.atom.astro"),
            r#"---
import type { Props, Input, Output } from './.atomic/types/ticket';
import type { AtomConfig } from './.atomic/types/authoring';
const { input }: Props = Astro.props;
export const config: AtomConfig<Input, Output> = {
  description: 'Name the ticket a note is about.',
  validate: [({output}) => output.ticket !== 'ABC-0'],
};
---
Note: {input.text}
"#,
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
        let a = atom::load(&dir.join("atoms/ticket.atom.astro"), &mut registry).expect("loads");
        let compiled = compile::compile(&a, &registry).expect("compiles");

        // The first answer fits strict mode but breaks maxLength; the repaired
        // one fits.
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"ticket": "ABC-123456"}"#.into()),
            Reply::Text(r#"{"ticket": "ABC-12"}"#.into()),
        ])
        .await
        .expect("fake model");
        let rt = runtimes(&model, "decl");
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
        assert_eq!(
            model.paths(),
            ["/v1/chat/completions", "/v1/chat/completions"],
            "a lite atom runs as plain model calls, not on a Codex thread"
        );
        let first = model.requests()[0].to_string();
        assert!(
            first.contains("origin=\\\"external\\\""),
            "the input is wrapped in the user turn: {first}"
        );
        // Every request's schema mentions maxLength; only the repair turn
        // quotes the validator's complaint about this answer.
        let repair = model.requests()[1].to_string();
        assert!(
            repair.contains("did not fit the required output")
                && repair.contains("is longer than 6 characters"),
            "the repair quotes the broken constraint: {repair}"
        );

        // A validate rule failing twice: one repair, then the validation error.
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"ticket": "ABC-0"}"#.into()),
            Reply::Text(r#"{"ticket": "ABC-0"}"#.into()),
        ])
        .await
        .expect("fake model");
        let rt = runtimes(&model, "decl-rule");
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

        // The same atom declared `kind: codex` runs on a Codex thread.
        let file = dir.join("atoms/ticket.atom.astro");
        let text = std::fs::read_to_string(&file).expect("read");
        std::fs::write(
            &file,
            text.replace("description: 'Name", "kind: 'codex',\n  description: 'Name"),
        )
        .expect("write");
        let mut registry = Registry::new();
        let a = atom::load(&file, &mut registry).expect("loads");
        let compiled = compile::compile(&a, &registry).expect("compiles");
        let model = FakeModel::start(vec![Reply::Text(r#"{"ticket": "ABC-12"}"#.into())])
            .await
            .expect("fake model");
        let rt = runtimes(&model, "decl-codex");
        let (out, _) = run_declarative(
            &a,
            &compiled,
            &mut registry,
            &input,
            "run-3",
            "external",
            &rt,
        )
        .await
        .expect("runs");
        assert_eq!(out, json!({"ticket": "ABC-12"}));
        assert_eq!(model.paths(), ["/v1/responses"]);
    });
}

#[test]
fn an_astro_typed_validator_gets_exactly_one_repair() {
    use crate::{atom, compile};
    on_codex_runtime(async {
        let dir = scratch("decl-project");
        std::fs::create_dir_all(dir.join("atoms")).expect("mkdir");
        std::fs::write(
            dir.join("atoms/ticket.atom.astro"),
            r#"---
import type { Props, Input, Output } from './.atomic/types/ticket';
import type { AtomConfig } from './.atomic/types/authoring';
const { input }: Props = Astro.props;
export const config: AtomConfig<Input, Output> = {
  description: 'Name the ticket a note is about.',
  validate: [({output}) => output.ticket !== 'ABC-0'],
};
---
Note: {input.text}
"#,
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
        let a = atom::load(&dir.join("atoms/ticket.atom.astro"), &mut registry).expect("loads");
        let compiled = compile::compile(&a, &registry).expect("compiles");

        // The first answer fits the schema but breaks the validator; the
        // repaired one fits.
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"ticket": "ABC-0"}"#.into()),
            Reply::Text(r#"{"ticket": "ABC-12"}"#.into()),
        ])
        .await
        .expect("fake model");
        let rt = runtimes(&model, "decl");
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
        let repair = model.requests()[1].to_string();
        assert!(
            repair.contains("did not fit the required output")
                && repair.contains("output.ticket !== 'ABC-0'")
                && repair.contains("returned false"),
            "the repair quotes the broken rule: {repair}"
        );

        // A validate rule failing twice: one repair, then the validation error.
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"ticket": "ABC-0"}"#.into()),
            Reply::Text(r#"{"ticket": "ABC-0"}"#.into()),
        ])
        .await
        .expect("fake model");
        let rt = runtimes(&model, "decl-rule");
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

#[test]
fn a_registry_reload_cannot_change_a_compiled_atoms_contract() {
    use crate::{atom, compile};
    on_codex_runtime(async {
        let dir = scratch("decl-project");
        std::fs::create_dir_all(dir.join("atoms")).expect("mkdir");
        std::fs::write(
            dir.join("atoms/ticket.atom.astro"),
            r#"---
import type { Props, Input, Output } from './.atomic/types/ticket';
import type { AtomConfig } from './.atomic/types/authoring';
const { input }: Props = Astro.props;
export const config: AtomConfig<Input, Output> = {
  description: 'Name the ticket a note is about.',
  validate: [({output}) => output.ticket !== 'ABC-0'],
};
---
Note: {input.text}
"#,
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
        let a = atom::load(&dir.join("atoms/ticket.atom.astro"), &mut registry).expect("loads");
        let compiled = compile::compile(&a, &registry).expect("compiles");
        let entry = registry.get(&a.output).expect("entry").clone();
        let mut changed = entry.schema;
        changed["properties"]["ticket"]["maxLength"] = json!(100);
        registry
            .register(entry.file, changed)
            .expect("simulated reload");

        // The first answer fits strict mode but breaks maxLength; the repaired
        // one fits.
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"ticket": "ABC-123456"}"#.into()),
            Reply::Text(r#"{"ticket": "ABC-12"}"#.into()),
        ])
        .await
        .expect("fake model");
        let rt = runtimes(&model, "decl");
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
        // Every request's schema mentions maxLength; only the repair turn
        // quotes the validator's complaint about this answer.
        let repair = model.requests()[1].to_string();
        assert!(
            repair.contains("did not fit the required output")
                && repair.contains("is longer than 6 characters"),
            "the repair quotes the broken constraint: {repair}"
        );

        // A validate rule failing twice: one repair, then the validation error.
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"ticket": "ABC-0"}"#.into()),
            Reply::Text(r#"{"ticket": "ABC-0"}"#.into()),
        ])
        .await
        .expect("fake model");
        let rt = runtimes(&model, "decl-rule");
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

#[test]
fn compiler_failures_never_spend_a_repair_call() {
    on_codex_runtime(async {
        for failure in [
            "Deno missing",
            "compiler worker exceeded 30 seconds",
            "Broken pipe",
        ] {
            let model = FakeModel::start(vec![Reply::Text(r#"{"ok":true}"#.into())])
                .await
                .expect("fake model");
            let rt = runtime(&model, "compiler-infrastructure");
            let backend = Backend::Codex(&rt);
            let contract = json!({"type":"object","additionalProperties":false,"required":["ok"],"properties":{"ok":{"type":"boolean"}}});
            let validator = jsonschema::validator_for(&contract).expect("schema");
            let check = |_: Value| -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), AtomError>> + Send>,
            > {
                Box::pin(std::future::ready(validation_result(Err(failure.into()))))
            };
            let error = run_contract(
                backend,
                Request {
                    developer: "Test".into(),
                    prompt: "Test".into(),
                    model: None,
                    sandbox: crate::typed::Sandbox::ReadOnly,
                    contract: &contract,
                    validator: &validator,
                    name: "infrastructure-test",
                },
                &check,
            )
            .await
            .expect_err("infrastructure failure");
            assert!(
                matches!(error, AtomError::Runtime(ref message) if message == failure),
                "{error}"
            );
            assert_eq!(model.calls(), 1, "{failure}");
        }
        assert!(matches!(
            validation_result(Ok(json!({}))),
            Err(AtomError::Runtime(_))
        ));
        assert!(matches!(
            validation_result(Ok(json!({"valid":false}))),
            Err(AtomError::Validation(_))
        ));
    });
}

#[tokio::test]
async fn no_declared_validators_need_no_compiler_process() {
    let program = crate::astro::Program {
        file: "/missing/atom.atom.astro".into(),
        root: "/missing".into(),
        sources: Default::default(),
        source_paths: Default::default(),
        contracts: Value::Null,
        helpers: Default::default(),
    };
    validate_output(&program, false, json!({}), None)
        .await
        .expect("no worker needed");
    assert!(matches!(
        validate_output(&program, true, json!({}), None).await,
        Err(AtomError::Runtime(_))
    ));
}

#[test]
fn a_declarative_atom_rejects_an_input_that_breaks_its_contract() {
    use crate::{atom, compile};
    on_codex_runtime(async {
        let dir = scratch("decl-input");
        std::fs::create_dir_all(dir.join("atoms")).expect("mkdir");
        std::fs::write(
            dir.join("atoms/count.atom.astro"),
            "---\nimport type { Props, Input, Output } from './.atomic/types/count';\nimport type { AtomConfig } from './.atomic/types/authoring';\nconst { input }: Props = Astro.props;\nexport const config: AtomConfig<Input, Output> = { description: 'Count the words in a note.', kind: 'lite' };\n---\nNote: {input.text}\n",
        )
        .expect("atom");
        std::fs::write(
            dir.join("atoms/count.input.schema.yaml"),
            "type: object\nrequired: [text, limit]\nproperties:\n  text: {type: string, maxLength: 10}\n  limit: {type: integer, minimum: 1}\n",
        )
        .expect("input");
        std::fs::write(
            dir.join("atoms/count.output.schema.yaml"),
            "type: object\nadditionalProperties: false\nrequired: [words]\nproperties:\n  words: {type: integer}\n",
        )
        .expect("output");
        let mut registry = Registry::new();
        let a = atom::load(&dir.join("atoms/count.atom.astro"), &mut registry).expect("loads");
        let compiled = compile::compile(&a, &registry).expect("compiles");
        for (input, at) in [
            (json!({"text": "two words"}), "limit"),
            (
                json!({"text": "far more than ten characters", "limit": 3}),
                "/text",
            ),
            (json!({"text": "short", "limit": 0}), "/limit"),
        ] {
            let model = FakeModel::start(vec![Reply::Text(r#"{"words": 2}"#.into())])
                .await
                .expect("fake model");
            let rt = runtimes(&model, "decl-input");
            let r = run_declarative(
                &a,
                &compiled,
                &mut registry,
                &input,
                "run-1",
                "external",
                &rt,
            )
            .await;
            assert!(
                matches!(&r, Err(AtomError::Input(m)) if m.contains(at)),
                "{at}: {r:?}"
            );
            assert_eq!(model.calls(), 0, "{at}: no model call");
        }
    });
}

/// The model resolves as the atom's own, then the runtime default, then the
/// endpoint's, for lite and Codex declarative atoms alike.
#[test]
fn a_declarative_atom_takes_its_model_from_itself_then_the_defaults_then_the_endpoint() {
    use crate::{atom, compile};
    on_codex_runtime(async {
        let dir = scratch("decl-model");
        std::fs::create_dir_all(dir.join("atoms")).expect("mkdir");
        std::fs::write(
            dir.join("atoms/echo.input.schema.yaml"),
            "type: object\nrequired: [text]\nproperties:\n  text: {type: string}\n",
        )
        .expect("input");
        std::fs::write(
            dir.join("atoms/echo.output.schema.yaml"),
            "type: object\nadditionalProperties: false\nrequired: [text]\nproperties:\n  text: {type: string}\n",
        )
        .expect("output");
        for kind in ["lite", "codex"] {
            for (own, default, expected) in [
                (Some("own-model"), Some("default-model"), "own-model"),
                (None, Some("default-model"), "default-model"),
                (None, None, "synthetic"),
            ] {
                let model_field = own.map(|m| format!(", model: '{m}'")).unwrap_or_default();
                std::fs::write(
                    dir.join("atoms/echo.atom.astro"),
                    format!(
                        "---\nimport type {{ Props, Input, Output }} from './.atomic/types/echo';\nimport type {{ AtomConfig }} from './.atomic/types/authoring';\nconst {{ input }}: Props = Astro.props;\nexport const config: AtomConfig<Input, Output> = {{ description: 'Echo.', kind: '{kind}'{model_field} }};\n---\nSay {{input.text}}\n"
                    ),
                )
                .expect("atom");
                let mut registry = Registry::new();
                let a =
                    atom::load(&dir.join("atoms/echo.atom.astro"), &mut registry).expect("loads");
                let compiled = compile::compile(&a, &registry).expect("compiles");
                let model = FakeModel::start(vec![Reply::Text(r#"{"text": "hi"}"#.into())])
                    .await
                    .expect("fake model");
                let mut rt = runtimes(&model, "decl-model");
                rt.codex.defaults.model = default.map(str::to_owned);
                rt.lite.defaults.model = default.map(str::to_owned);
                let (_, meta) = run_declarative(
                    &a,
                    &compiled,
                    &mut registry,
                    &json!({"text": "hi"}),
                    "run-1",
                    "external",
                    &rt,
                )
                .await
                .expect("runs");
                let case = format!("{kind}, own {own:?}, default {default:?}");
                assert_eq!(meta.model, expected, "{case}: metadata");
                assert_eq!(
                    model.requests()[0]["model"],
                    json!(expected),
                    "{case}: the request"
                );
            }
        }
    });
}

/// An atom that returns a patch and supporting notes, all stored artifacts.
struct Patch;

impl Atom for Patch {
    type In = Note;
    type Out = Value;
    const NAME: &'static str = "write-patch";
    fn input_schema(&self) -> Value {
        Classify.input_schema()
    }
    fn output_schema(&self) -> Value {
        json!({
            "type": "object", "additionalProperties": false, "required": ["patch", "notes"],
            "properties": {
                "patch": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
                "notes": {"type": "array", "items": {"$ref": "#/$defs/note"}}
            },
            "$defs": {"note": {"$ref": "atomic:artifact"}}
        })
    }
    fn instructions(&self) -> Cow<'static, str> {
        "Write the patch.".into()
    }
    fn kind(&self) -> crate::typed::AtomKind {
        crate::typed::AtomKind::Lite
    }
}

#[test]
fn an_output_must_reference_artifacts_that_exist_with_the_right_type() {
    use crate::artifact::{ArtifactStore, LocalStore, Provenance};
    on_codex_runtime(async {
        let store = LocalStore::open(scratch("artifacts")).expect("store");
        let put = |content: &[u8], media_type: &str| {
            store
                .put(
                    &mut &content[..],
                    Provenance {
                        media_type: media_type.into(),
                        ..Provenance::default()
                    },
                )
                .expect("put")
                .artifact
                .uri()
        };
        let diff = put(b"--- a\n+++ b\n", "text/x-diff");
        let text = put(b"a note", "text/plain");
        let missing = format!("artifact://sha256/{}", "0".repeat(64));
        let answer = |patch: &str, notes: &[&str]| {
            Reply::Text(json!({"patch": patch, "notes": notes}).to_string())
        };
        let run = |script: Vec<Reply>, store: Option<LocalStore>| async move {
            let model = FakeModel::start(script).await.expect("fake model");
            let mut rt = crate::lite::LiteRuntime::new(
                ModelEndpoint {
                    base_url: model.base_url(),
                    model: "synthetic".into(),
                    api_key_env: None,
                },
                AtomConfig::default(),
            );
            if let Some(store) = store {
                rt.artifacts = Artifacts::new(store);
            }
            let r = crate::runtime::run_lite_atom(&Patch, &Note { text: "n".into() }, &rt).await;
            (model.calls(), r)
        };

        // Every reference holds: accepted in one call.
        let (calls, r) = run(vec![answer(&diff, &[&text, &diff])], Some(store.clone())).await;
        assert_eq!(calls, 1);
        assert!(r.is_ok(), "{r:?}");

        // A well-formed reference to nothing, at the top or nested in a list:
        // repaired once, then a schema error naming the place.
        for (bad, at) in [
            (answer(&missing, &[&text]), "/patch"),
            (answer(&diff, &[&text, &missing]), "/notes/1"),
            (answer(&text, &[]), "/patch"),
        ] {
            let (calls, r) = run(vec![bad.clone(), bad], Some(store.clone())).await;
            assert_eq!(calls, 2, "{at}: one repair");
            match r {
                Err(AtomError::Schema { message, .. }) => {
                    assert!(message.starts_with(at), "{at}: {message}");
                }
                other => panic!("{at}: expected a schema error, got {other:?}"),
            }
        }

        // The repair turn can fix it.
        let (calls, r) = run(
            vec![answer(&missing, &[]), answer(&diff, &[])],
            Some(store.clone()),
        )
        .await;
        assert_eq!(calls, 2);
        assert!(r.is_ok(), "{r:?}");

        // A runtime with no store accepts no reference at all.
        let (_, r) = run(vec![answer(&diff, &[]), answer(&diff, &[])], None).await;
        assert!(matches!(r, Err(AtomError::Schema { .. })), "{r:?}");
    });
}

#[test]
fn a_declarative_output_keeps_its_artifact_references_for_the_check() {
    use crate::{atom, compile};
    on_codex_runtime(async {
        let dir = scratch("decl-artifact");
        std::fs::create_dir_all(dir.join("atoms")).expect("mkdir");
        std::fs::write(
            dir.join("atoms/patch.atom.astro"),
            "---\nimport type { Props, Input, Output } from './.atomic/types/patch';\nimport type { AtomConfig } from './.atomic/types/authoring';\nconst { input }: Props = Astro.props;\nexport const config: AtomConfig<Input, Output> = { description: 'Write a patch.', kind: 'lite' };\n---\nPatch {input.text}\n",
        )
        .expect("atom");
        std::fs::write(
            dir.join("atoms/patch.input.schema.yaml"),
            "type: object\nrequired: [text]\nproperties:\n  text: {type: string}\n",
        )
        .expect("input");
        std::fs::write(
            dir.join("atoms/patch.output.schema.yaml"),
            "type: object\nadditionalProperties: false\nrequired: [patch]\nproperties:\n  patch: {$ref: \"atomic:artifact\", mediaType: text/x-diff}\n",
        )
        .expect("output");
        let mut registry = Registry::new();
        let a = atom::load(&dir.join("atoms/patch.atom.astro"), &mut registry).expect("loads");
        let compiled = compile::compile(&a, &registry).expect("compiles");
        let missing = format!("artifact://sha256/{}", "0".repeat(64));
        let bad = Reply::Text(json!({"patch": missing}).to_string());
        let model = FakeModel::start(vec![bad.clone(), bad])
            .await
            .expect("fake model");
        let rt = runtimes(&model, "decl-artifact");
        let r = run_declarative(
            &a,
            &compiled,
            &mut registry,
            &json!({"text": "x"}),
            "run-1",
            "external",
            &rt,
        )
        .await;
        assert!(
            matches!(&r, Err(AtomError::Schema { message, .. }) if message.contains("no artifact")),
            "{r:?}"
        );
        assert_eq!(model.calls(), 2);
    });
}

#[test]
fn an_answer_that_is_one_fenced_json_block_is_read_as_its_contents() {
    assert_eq!(unfence("```json\n{\"a\": 1}\n```"), "{\"a\": 1}");
    assert_eq!(unfence("  ```\n{\"a\": 1}\n```\n"), "{\"a\": 1}");
    assert_eq!(unfence("```JSON\n[1]\n```"), "[1]");
    assert_eq!(unfence("{\"a\": 1}"), "{\"a\": 1}");
    // Anything else stays as it is, and fails to parse.
    for kept in [
        "Here it is:\n```json\n{}\n```",
        "```json\n{}\n```\nDone.",
        "```python\n{}\n```",
        "```json\n{}\n```\n```json\n{}\n```",
        "```json {}```",
    ] {
        assert_eq!(unfence(kept), kept.trim(), "{kept:?}");
    }

    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(
            "```json\n{\"category\": \"bug\", \"summary\": \"Export crashes.\", \"line\": null}\n```".into(),
        )])
        .await
        .expect("fake model");
        let rt = runtime(&model, "fenced");
        let (out, meta) = run_codex_atom(&Classify, &Note { text: "n".into() }, &rt)
            .await
            .expect("accepted");
        assert_eq!(out.category, "bug");
        assert_eq!((model.calls(), meta.turns), (1, 1), "no repair turn");
    });
}

#[test]
fn a_codex_atom_off_openai_states_its_schema_in_its_instructions() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(
            r#"{"category": "bug", "summary": "Export crashes.", "line": null}"#.into(),
        )])
        .await
        .expect("fake model");
        let rt = runtime(&model, "schema-in-prompt");
        run_codex_atom(&Classify, &Note { text: "n".into() }, &rt)
            .await
            .expect("runs");
        let request = model.requests()[0].to_string();
        assert!(
            request.contains("follows this JSON Schema"),
            "the fake model is not OpenAI's API, so the schema is stated: {request}"
        );
        // The lowered schema, which lists the optional `line` as required.
        assert!(
            request.contains(r#"\"required\": [\n    \"category\",\n    \"line\""#),
            "{request}"
        );
    });
    assert!(crate::lite::is_openai_api("https://api.openai.com/v1"));
    assert!(!crate::lite::is_openai_api("http://127.0.0.1:13305/api/v1"));
}

/// A project with one exec atom whose command runs `script` with python3.
fn exec_project(
    tag: &str,
    script: &str,
    extra: &str,
) -> (std::path::PathBuf, crate::atom::Atom, Compiled) {
    use crate::{atom, compile};
    let dir = scratch(tag);
    std::fs::create_dir_all(dir.join("atoms")).expect("atoms");
    std::fs::create_dir_all(dir.join("tools")).expect("tools");
    std::fs::write(dir.join("atomic.yaml"), "atomic: 1\nname: exec-test\n").expect("manifest");
    std::fs::write(dir.join("tools/total.py"), script).expect("script");
    std::fs::write(
        dir.join("atoms/total.atom.astro"),
        format!(
            "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/total';\n\
             import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
             const {{ input }}: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = {{ description: 'Total an order.', kind: 'exec', exec: {{ command: ['python3', 'tools/total.py'] }}{extra} }};\n---\n"
        ),
    )
    .expect("atom");
    std::fs::write(
        dir.join("atoms/total.input.schema.yaml"),
        "type: object\nadditionalProperties: false\nrequired: [cents]\nproperties:\n  cents: {type: array, items: {type: integer}}\n",
    )
    .expect("input");
    std::fs::write(
        dir.join("atoms/total.output.schema.yaml"),
        "type: object\nadditionalProperties: false\nrequired: [total]\nproperties:\n  total: {type: integer, minimum: 0}\n",
    )
    .expect("output");
    let mut registry = Registry::new();
    let a = atom::load(&dir.join("atoms/total.atom.astro"), &mut registry).expect("loads");
    let compiled = compile::compile(&a, &registry).expect("compiles");
    (dir, a, compiled)
}

const TOTAL: &str = "import json, sys\norder = json.load(sys.stdin)\njson.dump({'total': sum(order['cents'])}, sys.stdout)\n";

#[test]
fn an_exec_atom_runs_its_command_with_typed_json_and_no_model() {
    on_codex_runtime(async {
        let (_, a, compiled) = exec_project("exec-ok", TOTAL, "");
        let model = FakeModel::start(Vec::new()).await.expect("fake model");
        let rt = runtimes(&model, "exec-ok");
        let (out, meta) = run_declarative(
            &a,
            &compiled,
            &mut Registry::new(),
            &json!({"cents": [125, 125, 50]}),
            "run-1",
            "external",
            &rt,
        )
        .await
        .expect("runs");
        assert_eq!(out, json!({"total": 300}));
        assert_eq!((meta.model.as_str(), meta.turns), ("exec", 0));
        assert_eq!(model.calls(), 0, "no model call");

        // Input that breaks the contract never reaches the command.
        let r = run_declarative(
            &a,
            &compiled,
            &mut Registry::new(),
            &json!({"cents": "a lot"}),
            "run-2",
            "external",
            &rt,
        )
        .await;
        assert!(matches!(r, Err(AtomError::Input(_))), "{r:?}");
    });
}

// An exec atom's validators follow the step's recorded clock, as a model
// atom's do, so a retried step reaches the same decision.
#[test]
fn exec_validators_follow_the_steps_clock() {
    use crate::{atom, compile};
    on_codex_runtime(async {
        let (dir, _, _) = exec_project("exec-clock", TOTAL, "");
        std::fs::create_dir_all(dir.join("atoms/helpers")).expect("helpers");
        std::fs::write(
            dir.join("atoms/helpers/clock.ts"),
            "export const recorded = (total: number): boolean => total > 0 && Date.now() === 1700000000000;\n",
        )
        .expect("helper");
        std::fs::write(
            dir.join("atoms/total.atom.astro"),
            "---\nimport type { Props, Input, Output } from '../.atomic/types/total';\n\
             import type { AtomConfig } from '../.atomic/types/authoring';\n\
             import { recorded } from './helpers/clock';\n\
             const { input }: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = { description: 'Total an order.', kind: 'exec', exec: { command: ['python3', 'tools/total.py'] }, validate: [({ output }) => recorded(output.total)] };\n---\n",
        )
        .expect("atom");
        let mut registry = Registry::new();
        let a = atom::load(&dir.join("atoms/total.atom.astro"), &mut registry).expect("loads");
        let compiled = compile::compile(&a, &registry).expect("compiles");
        let identity = EffectIdentity {
            run_id: "run-1".into(),
            step_id: "total".into(),
        };
        let clock = crate::astro::StepClock {
            now_ms: 1_700_000_000_000,
            seed: "00".repeat(32),
        };
        let input = json!({"cents": [1, 2]});
        for _ in 0..2 {
            let (out, _) = run_exec(
                &a,
                &compiled,
                &input,
                &identity,
                Arc::new(atomic_exec::LocalBackend),
                Some(&clock),
            )
            .await
            .expect("the recorded clock passes");
            assert_eq!(out, json!({"total": 3}));
        }
        let fresh = run_exec(
            &a,
            &compiled,
            &input,
            &identity,
            Arc::new(atomic_exec::LocalBackend),
            None,
        )
        .await;
        assert!(matches!(fresh, Err(AtomError::Validation(_))), "{fresh:?}");
    });
}

#[test]
fn exec_output_that_breaks_its_schema_fails_the_step() {
    on_codex_runtime(async {
        let model = FakeModel::start(Vec::new()).await.expect("fake model");
        let rt = runtimes(&model, "exec-bad");
        for (script, expected) in [
            ("print('{\"total\": -5}')\n", "minimum"),
            ("print('not json')\n", "not one JSON value"),
        ] {
            let (_, a, compiled) = exec_project("exec-bad", script, "");
            let r = run_declarative(
                &a,
                &compiled,
                &mut Registry::new(),
                &json!({"cents": [1]}),
                "run-1",
                "external",
                &rt,
            )
            .await;
            assert!(
                matches!(&r, Err(AtomError::Schema { message, .. }) if message.contains(expected)),
                "{expected}: {r:?}"
            );
        }
        assert_eq!(model.calls(), 0);
    });
}

#[test]
fn a_failing_exec_command_fails_the_step_with_its_stderr() {
    on_codex_runtime(async {
        let model = FakeModel::start(Vec::new()).await.expect("fake model");
        let rt = runtimes(&model, "exec-fail");
        let (_, a, compiled) = exec_project(
            "exec-fail",
            "import sys\nprint('the order has no lines', file=sys.stderr)\nsys.exit(4)\n",
            "",
        );
        let r = run_declarative(
            &a,
            &compiled,
            &mut Registry::new(),
            &json!({"cents": []}),
            "run-1",
            "external",
            &rt,
        )
        .await;
        assert!(
            matches!(&r, Err(AtomError::Runtime(m)) if m.contains("exited with code 4") && m.contains("the order has no lines")),
            "{r:?}"
        );
    });
}

#[test]
fn an_exec_command_sees_only_its_declared_environment_from_the_project_root() {
    on_codex_runtime(async {
        let model = FakeModel::start(Vec::new()).await.expect("fake model");
        let rt = runtimes(&model, "exec-env");
        let script = "import json, os, sys\njson.load(sys.stdin)\n\
            names = sorted(n for n in os.environ if n != 'LC_CTYPE')  # Python's own locale coercion\n\
            assert names == ['ATOMIC_EFFECT_KEY', 'ATOMIC_RUN_ID', 'ATOMIC_STEP_ID', 'HOME', 'REGION', 'TMPDIR'], names\n\
            assert os.environ['REGION'] == 'eu'\n\
            assert os.environ['ATOMIC_RUN_ID'] == 'run-1' and os.environ['ATOMIC_STEP_ID'] == 'total', dict(os.environ)\n\
            assert len(os.environ['ATOMIC_EFFECT_KEY']) == 64\n\
            assert os.path.isfile('atomic.yaml'), os.getcwd()\n\
            assert os.environ['HOME'] == os.environ['TMPDIR'] and os.path.isdir(os.environ['HOME'])\n\
            json.dump({'total': 1}, sys.stdout)\n";
        let (_, a, compiled) = exec_project("exec-env", script, ", env: { REGION: 'eu' }");
        let (out, _) = run_declarative(
            &a,
            &compiled,
            &mut Registry::new(),
            &json!({"cents": [1]}),
            "run-1",
            "external",
            &rt,
        )
        .await
        .expect("the environment is exactly the declared one");
        assert_eq!(out, json!({"total": 1}));
    });
}

#[test]
fn exec_declarations_are_checked_at_load() {
    use crate::atom;
    for (config, problem) in [
        ("kind: 'exec'", "needs `exec"),
        ("kind: 'exec', exec: { command: [] }", "nonempty list"),
        (
            "kind: 'exec', exec: { script: './x.ts' }",
            "managed runtime",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, env: { 'BAD-NAME': 'x' }",
            "variable name",
        ),
        ("kind: 'lite', env: { REGION: 'eu' }", "for `kind: 'exec'`"),
        (
            "kind: 'exec', exec: { command: ['x'] }, effects: { external: 'sometimes' }",
            "`read-only` (the command only reads) or `idempotent`",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, effects: { caps: {} }",
            "`effects.caps` is not supported yet",
        ),
        (
            "kind: 'lite', effects: { external: 'read-only' }",
            "for `kind: 'exec'`",
        ),
    ] {
        let dir = scratch("exec-load");
        std::fs::create_dir_all(dir.join("atoms")).expect("atoms");
        std::fs::write(
            dir.join("atoms/x.atom.astro"),
            format!(
                "---\nimport type {{ Props, Input, Output }} from './.atomic/types/x';\n\
                 import type {{ AtomConfig }} from './.atomic/types/authoring';\n\
                 const {{ input }}: Props = Astro.props;\n\
                 export const config: AtomConfig<Input, Output> = {{ description: 'x', {config} }};\n---\nX {{input.text}}\n"
            ),
        )
        .expect("atom");
        for role in ["input", "output"] {
            std::fs::write(
                dir.join(format!("atoms/x.{role}.schema.yaml")),
                "type: object\nadditionalProperties: false\nrequired: [text]\nproperties:\n  text: {type: string}\n",
            )
            .expect("schema");
        }
        let error =
            atom::load(&dir.join("atoms/x.atom.astro"), &mut Registry::new()).expect_err(problem);
        assert!(error.message.contains(problem), "{config}: {error}");
    }
}
