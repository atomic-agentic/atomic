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
        guard: crate::codex::StateGuard::NoProjectState,
        cwd: dir.join("cwd"),
        endpoint: ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
            sign_in: None,
        },
        defaults: AtomConfig::default(),
        artifacts: Artifacts::none(),
        meter: crate::usage::Metering::none(),
        backend: Default::default(),
        caches: None,
    }
}

/// Both runtimes, on the same fake model.
fn runtimes(model: &FakeModel, tag: &str) -> Runtimes {
    let codex = runtime(model, tag);
    let lite = crate::lite::LiteRuntime::new(codex.endpoint.clone(), AtomConfig::default());
    Runtimes {
        codex,
        lite,
        exec: Default::default(),
        exec_meter: Default::default(),
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
                    place: Place::of("run-1", "infrastructure-test"),
                    env: Default::default(),
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
                    sign_in: None,
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
                Arc::new(atomic_exec::LocalBackend::default()),
                Some(&clock),
                &Artifacts::none(),
                &crate::usage::Metering::none(),
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
            Arc::new(atomic_exec::LocalBackend::default()),
            None,
            &Artifacts::none(),
            &crate::usage::Metering::none(),
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
        ("kind: 'codex', env: { 'BAD-NAME': 'x' }", "variable name"),
        ("kind: 'codex', env: { RUSTUP_HOME: 1 }", "a string"),
        (
            "kind: 'codex', env: { LD_PRELOAD: '/x/evil.so' }",
            "load code",
        ),
        ("kind: 'codex', env: { LD_LIBRARY_PATH: '/x' }", "load code"),
        ("kind: 'codex', env: { GCONV_PATH: '/x' }", "load code"),
        (
            "kind: 'codex', env: { BASH_ENV: '/home/dev/.bashrc' }",
            "as it starts",
        ),
        (
            "kind: 'codex', env: { ENV: '/home/dev/.profile' }",
            "as it starts",
        ),
        (
            "kind: 'codex', env: { SHELLOPTS: 'xtrace' }",
            "as it starts",
        ),
        (
            "kind: 'codex', env: { PS4: '$(cat /home/dev/.netrc)' }",
            "as it starts",
        ),
        (
            "kind: 'codex', env: { PROMPT_COMMAND: 'cat /home/dev/.netrc' }",
            "as it starts",
        ),
        ("kind: 'codex', env: { HOME: '/x' }", "set by atomic"),
        ("kind: 'codex', env: { ZDOTDIR: '/x' }", "set by atomic"),
        ("kind: 'codex', env: { TMPDIR: '/x' }", "set by atomic"),
        (
            "kind: 'codex', env: { ATOMIC_TURN_TMPDIR: '/x' }",
            "set by atomic",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, env: { ATOMIC_TURN_TMPDIR: '/x' }",
            "set by atomic",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, env: { HOME: '/x' }",
            "set by atomic",
        ),
        (
            "kind: 'codex', exec: { command: ['x'] }",
            "for `kind: 'exec'`",
        ),
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
        // Network: only an exec atom's command, only public DNS names.
        (
            "kind: 'codex', network: { allow: ['api.example.com'] }",
            "for exec atoms",
        ),
        (
            "kind: 'lite', network: { allow: ['api.example.com'] }",
            "for exec atoms",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, network: { allow: ['localhost'] }",
            "public DNS names",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, network: { allow: ['127.0.0.1'] }",
            "an IP address",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, network: { allow: ['169.254.169.254'] }",
            "an IP address",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, network: { allow: ['metadata.google.internal'] }",
            "private network",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, network: { allow: ['*'] }",
            "public DNS names",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, network: { allow: ['unix:/var/run/docker.sock'] }",
            "Unix socket",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, network: { allow: ['api.example.com:443'] }",
            "no scheme, port",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, network: { allow: 'api.example.com' }",
            "a list of host names",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, network: { allow: ['a.example.com'], unix: true }",
            "unknown field `unix`",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, network: true",
            "is `false` or",
        ),
        ("kind: 'codex', network: true", "is `false` or"),
        (
            "kind: 'lite', network: { allow: 'x.example.com' }",
            "a list of host names",
        ),
        (
            "kind: 'codex', network: { unix: true }",
            "unknown field `unix`",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, network: { allow: ['0x7f.1'] }",
            "an IP address",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, network: { allow: ['8.8.8.0x8'] }",
            "an IP address",
        ),
        // Proxy variables are atomic's to set, in any case.
        (
            "kind: 'exec', exec: { command: ['x'] }, env: { https_proxy: 'http://evil.example.com' }",
            "names a proxy",
        ),
        (
            "kind: 'exec', exec: { command: ['x'] }, env: { NO_PROXY: '*' }",
            "names a proxy",
        ),
        (
            "kind: 'codex', env: { All_Proxy: 'socks5://x' }",
            "names a proxy",
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

/// An exec atom may declare public hosts for its command, in any case and
/// order, and they reach the command as one canonical list.
#[test]
fn an_exec_atom_declares_the_public_hosts_its_command_reaches() {
    let (_, a, _) = exec_project(
        "network-load",
        TOTAL,
        ", network: { allow: ['API.example.com', '*.cdn.example.net'] }",
    );
    assert_eq!(
        declared_network(&a),
        ["*.cdn.example.net", "api.example.com"]
    );
    let (_, none, _) = exec_project("network-none", TOTAL, ", network: false");
    assert!(declared_network(&none).is_empty());
    let (_, plain, _) = exec_project("network-plain", TOTAL, "");
    assert!(declared_network(&plain).is_empty());
}

/// A lite atom with [`Classify`]'s contract.
struct LiteClassify;

impl Atom for LiteClassify {
    type In = Note;
    type Out = Classified;
    const NAME: &'static str = "classify-note-lite";
    fn input_schema(&self) -> Value {
        Classify.input_schema()
    }
    fn output_schema(&self) -> Value {
        Classify.output_schema()
    }
    fn instructions(&self) -> Cow<'static, str> {
        Classify.instructions()
    }
    fn kind(&self) -> crate::typed::AtomKind {
        crate::typed::AtomKind::Lite
    }
}

/// An execution backend that records every call before passing it to the
/// local backend, and can refuse to open a workspace.
#[derive(Clone, Default)]
struct Recording {
    calls: Arc<std::sync::Mutex<Vec<String>>>,
    opened: Arc<std::sync::Mutex<Vec<(atomic_exec::Open, Workspace)>>>,
    refuse: bool,
    no_shell: bool,
    unavailable: Arc<std::sync::atomic::AtomicUsize>,
}

impl Recording {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    fn opened(&self) -> Vec<(atomic_exec::Open, Workspace)> {
        self.opened.lock().expect("opened").clone()
    }

    fn backend(&self) -> atomic_exec::Backend {
        atomic_exec::Backend::connect(self.clone()).expect("connects")
    }
}

impl ExecutionBackend for Recording {
    fn connect(&self) -> Result<atomic_exec::Statement, atomic_exec::ExecError> {
        Ok(atomic_exec::LocalBackend::STATEMENT)
    }

    fn open(&self, request: &atomic_exec::Open) -> Result<Workspace, atomic_exec::ExecError> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("open {}", request.label));
        if self.refuse {
            return Err(atomic_exec::ExecError::Workspace {
                label: request.label.clone(),
                message: "no room on this backend".into(),
            });
        }
        if self
            .unavailable
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |n| n.checked_sub(1),
            )
            .is_ok()
        {
            return Err(atomic_exec::ExecError::Unavailable {
                label: request.label.clone(),
                message: "no runner is free".into(),
            });
        }
        let mut ws = atomic_exec::LocalBackend::default().open(request)?;
        if self.no_shell {
            ws.shell = atomic_exec::Shell::None;
        }
        self.opened
            .lock()
            .expect("opened")
            .push((request.clone(), ws.clone()));
        Ok(ws)
    }

    fn run(
        &self,
        command: &atomic_exec::Command,
    ) -> Result<atomic_exec::Completed, atomic_exec::ExecError> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("run {}", command.program));
        atomic_exec::LocalBackend::default().run(command)
    }

    /// Recorded once the close has done its work, so a test that sees it
    /// sees what the close left; one that failed is recorded as failed.
    fn close(&self, workspace: &Workspace) -> Result<(), atomic_exec::ExecError> {
        let closed = atomic_exec::LocalBackend::default().close(workspace);
        let call = match &closed {
            Ok(()) => format!("close {}", workspace.label),
            Err(e) => format!("close {} failed: {e}", workspace.label),
        };
        self.calls.lock().expect("calls").push(call);
        closed
    }
}

const CLASSIFIED: &str = r#"{"category": "bug", "summary": "Export crashes.", "line": null}"#;

/// Every kind of atom opens its workspace on its runtime's backend before
/// anything runs, and closes it after: a codex atom's in the runtime's
/// directory with a local shell, a lite atom's with no files or shell, and
/// an exec atom's in its project, where its command runs through the
/// backend with the workspace's scratch directory as its home.
#[test]
fn every_kind_of_atom_runs_through_its_execution_backend() {
    on_codex_runtime(async {
        let note = Note {
            text: "Export crashes on é.".into(),
        };
        let model = FakeModel::start(vec![
            Reply::Text(CLASSIFIED.into()),
            Reply::Text(CLASSIFIED.into()),
        ])
        .await
        .expect("fake model");

        let codex = Recording::default();
        let mut rt = runtime(&model, "seam-codex");
        rt.backend = codex.backend();
        run_codex_atom(&Classify, &note, &rt).await.expect("runs");
        assert_eq!(codex.calls(), ["open classify-note", "close classify-note"]);
        let (asked, given) = &codex.opened()[0];
        assert_eq!(asked.source.as_ref(), Some(&rt.cwd));
        assert_eq!(asked.sandbox, crate::typed::Sandbox::ReadOnly);
        assert_eq!(given.shell, atomic_exec::Shell::Local);
        assert!(
            !given.scratch.as_ref().expect("scratch").exists(),
            "closed when the run ended"
        );

        let lite = Recording::default();
        let mut lrt = crate::lite::LiteRuntime::new(rt.endpoint.clone(), AtomConfig::default());
        lrt.backend = lite.backend();
        run_lite_atom(&LiteClassify, &note, &lrt)
            .await
            .expect("runs");
        assert_eq!(
            lite.calls(),
            ["open classify-note-lite", "close classify-note-lite"]
        );
        let (asked, given) = &lite.opened()[0];
        assert_eq!(asked.source, None);
        assert_eq!(given.shell, atomic_exec::Shell::None);
        assert_eq!(model.calls(), 2);

        let exec = Recording::default();
        let mut all = runtimes(&model, "seam-exec");
        all.exec = exec.backend();
        let script = "import json, os, sys\norder = json.load(sys.stdin)\n\
            json.dump({'total': len(os.listdir(os.environ['HOME']))}, sys.stdout)\n";
        let (dir, a, compiled) = exec_project("seam-exec", script, "");
        let (out, _) = run_declarative(
            &a,
            &compiled,
            &mut Registry::new(),
            &json!({"cents": [1]}),
            "run-1",
            "external",
            &all,
        )
        .await
        .expect("runs");
        assert_eq!(out, json!({"total": 0}), "the scratch home starts empty");
        assert_eq!(exec.calls(), ["open total", "run python3", "close total"]);
        let (asked, given) = &exec.opened()[0];
        assert_eq!(asked.source.as_ref(), Some(&a.program.root));
        assert!(dir.join("atomic.yaml").is_file());
        assert!(!given.scratch.as_ref().expect("scratch").exists());
        assert_eq!(model.calls(), 2, "an exec atom calls no model");
    });
}

/// A backend that cannot open a workspace fails the run before anything is
/// sent to a model or run: no atom gets around the backend.
#[test]
fn a_run_whose_workspace_cannot_be_opened_sends_and_runs_nothing() {
    on_codex_runtime(async {
        let note = Note { text: "n".into() };
        let model = FakeModel::start(vec![Reply::Text(CLASSIFIED.into())])
            .await
            .expect("fake model");
        let refusing = Recording {
            refuse: true,
            ..Recording::default()
        };

        let mut rt = runtime(&model, "seam-refuse");
        rt.backend = refusing.backend();
        let r = run_codex_atom(&Classify, &note, &rt).await;
        assert!(
            matches!(&r, Err(AtomError::Runtime(m)) if m.contains("no room on this backend")),
            "{r:?}"
        );

        let mut lrt = crate::lite::LiteRuntime::new(rt.endpoint.clone(), AtomConfig::default());
        lrt.backend = refusing.backend();
        let r = run_lite_atom(&LiteClassify, &note, &lrt).await;
        assert!(matches!(&r, Err(AtomError::Runtime(_))), "{r:?}");

        let mut all = runtimes(&model, "seam-refuse-exec");
        all.exec = refusing.backend();
        let (_, a, compiled) = exec_project("seam-refuse", TOTAL, "");
        let r = run_declarative(
            &a,
            &compiled,
            &mut Registry::new(),
            &json!({"cents": [1]}),
            "run-1",
            "external",
            &all,
        )
        .await;
        assert!(matches!(&r, Err(AtomError::Runtime(_))), "{r:?}");

        assert_eq!(model.calls(), 0, "nothing reached the model");
        assert_eq!(
            refusing.calls(),
            [
                "open classify-note",
                "open classify-note-lite",
                "open total"
            ],
            "nothing ran and nothing was left open"
        );
    });
}

/// A run that fails still closes its workspace.
#[test]
fn a_failed_run_closes_its_workspace() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text("not json".into()),
            Reply::Text("still not json".into()),
        ])
        .await
        .expect("fake model");
        let codex = Recording::default();
        let mut rt = runtime(&model, "seam-fail");
        rt.backend = codex.backend();
        let r = run_codex_atom(&Classify, &Note { text: "n".into() }, &rt).await;
        assert!(matches!(&r, Err(AtomError::Schema { .. })), "{r:?}");
        assert_eq!(codex.calls(), ["open classify-note", "close classify-note"]);

        let exec = Recording::default();
        let mut all = runtimes(&model, "seam-fail-exec");
        all.exec = exec.backend();
        let (_, a, compiled) = exec_project("seam-fail", "import sys\nsys.exit(3)\n", "");
        let r = run_declarative(
            &a,
            &compiled,
            &mut Registry::new(),
            &json!({"cents": [1]}),
            "run-1",
            "external",
            &all,
        )
        .await;
        assert!(matches!(&r, Err(AtomError::Runtime(_))), "{r:?}");
        assert_eq!(exec.calls(), ["open total", "run python3", "close total"]);
        let (_, given) = &exec.opened()[0];
        assert!(!given.scratch.as_ref().expect("scratch").exists());
    });
}

/// A codex atom whose backend gives its turn no shell fails before any model
/// call, and its workspace is closed.
#[test]
fn a_codex_atom_whose_workspace_has_no_shell_sends_nothing() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(CLASSIFIED.into())])
            .await
            .expect("fake model");
        let shell_less = Recording {
            no_shell: true,
            ..Recording::default()
        };
        let mut rt = runtime(&model, "seam-no-shell");
        rt.backend = shell_less.backend();
        let r = run_codex_atom(&Classify, &Note { text: "n".into() }, &rt).await;
        assert!(
            matches!(&r, Err(AtomError::Runtime(m)) if m.contains("no shell")),
            "{r:?}"
        );
        assert_eq!(model.calls(), 0);
        assert_eq!(
            shell_less.calls(),
            ["open classify-note", "close classify-note"]
        );
    });
}

/// While the backend has no room the run waits, trying again, and runs once
/// there is room; a run that waits sends nothing, and stopping it stops the
/// wait.
#[test]
fn an_unavailable_workspace_is_waited_for() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(CLASSIFIED.into())])
            .await
            .expect("fake model");
        let busy = Recording {
            unavailable: Arc::new(std::sync::atomic::AtomicUsize::new(4)),
            ..Recording::default()
        };
        let mut rt = runtime(&model, "seam-busy");
        rt.backend = busy.backend();
        run_codex_atom(&Classify, &Note { text: "n".into() }, &rt)
            .await
            .expect("runs once the backend has room");
        let mut expected = vec!["open classify-note"; 5];
        expected.push("close classify-note");
        assert_eq!(busy.calls(), expected);
        assert_eq!(model.calls(), 1);

        let full = Recording {
            unavailable: Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX)),
            ..Recording::default()
        };
        rt.backend = full.backend();
        let waiting = tokio::spawn(async move {
            run_codex_atom(&Classify, &Note { text: "n".into() }, &rt).await
        });
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(!waiting.is_finished(), "it waits");
        assert!(
            full.calls().len() >= 3,
            "and tries again: {:?}",
            full.calls()
        );
        waiting.abort();
        assert!(waiting.await.expect_err("stopped").is_cancelled());
        assert_eq!(model.calls(), 1, "nothing more was sent");
    });
}

/// An atom that panics after its workspace opened still closes it.
#[test]
fn a_run_that_panics_closes_its_workspace() {
    struct Panics;
    impl Atom for Panics {
        type In = Note;
        type Out = Classified;
        const NAME: &'static str = "panics";
        fn input_schema(&self) -> Value {
            Classify.input_schema()
        }
        fn output_schema(&self) -> Value {
            Classify.output_schema()
        }
        fn instructions(&self) -> Cow<'static, str> {
            Classify.instructions()
        }
        fn validate(&self, _: &Classified) -> Result<(), AtomError> {
            panic!("the atom's own check panicked")
        }
    }
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(CLASSIFIED.into())])
            .await
            .expect("fake model");
        let recorded = Recording::default();
        let mut rt = runtime(&model, "seam-panic");
        rt.backend = recorded.backend();
        let r = contained(
            async move { run_codex_atom(&Panics, &Note { text: "n".into() }, &rt).await },
        )
        .await;
        assert!(matches!(&r, Err(AtomError::Panicked(_))), "{r:?}");
        let scratch = recorded.opened()[0].1.scratch.clone().expect("scratch");
        for _ in 0..200 {
            if recorded.calls().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(recorded.calls(), ["open panics", "close panics"]);
        // Recorded as the close starts; the removal follows.
        for _ in 0..200 {
            if !scratch.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!scratch.exists());
    });
}

/// A run that is cancelled after its workspace opened still closes it.
#[test]
fn a_cancelled_run_closes_its_workspace() {
    on_codex_runtime(async {
        let model = FakeModel::start(Vec::new()).await.expect("fake model");
        let recorded = Recording::default();
        let mut all = runtimes(&model, "seam-cancel");
        all.exec = recorded.backend();
        // The command writes in its HOME, the scratch directory, after a
        // while: its workspace must still be there.
        let (_, a, compiled) = exec_project(
            "seam-cancel",
            "import os, time\ntime.sleep(2)\nopen(os.path.join(os.environ['HOME'], 'late'), 'w').write('x')\n",
            "",
        );
        let task = tokio::spawn(async move {
            run_declarative(
                &a,
                &compiled,
                &mut Registry::new(),
                &json!({"cents": [1]}),
                "run-1",
                "external",
                &all,
            )
            .await
        });
        for _ in 0..500 {
            if recorded.calls().iter().any(|c| c.starts_with("run ")) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(recorded.calls(), ["open total", "run python3"]);
        task.abort();
        assert!(task.await.expect_err("cancelled").is_cancelled());
        // The command still runs, so its workspace stays open...
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let scratch = recorded.opened()[0].1.scratch.clone().expect("scratch");
        assert_eq!(recorded.calls(), ["open total", "run python3"]);
        assert!(scratch.is_dir(), "closed under a running command");
        // ...until it ends.
        for _ in 0..1000 {
            if recorded.calls().len() == 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            recorded.calls(),
            ["open total", "run python3", "close total"]
        );
        assert!(!scratch.exists());
    });
}

/// A codex atom declares the environment its turn's commands get, such as
/// the tool homes its build finds through HOME otherwise: it reaches the
/// workspace the run opens.
#[test]
fn a_codex_atom_passes_its_declared_tool_homes_to_its_workspace() {
    use crate::{atom, compile};
    on_codex_runtime(async {
        let dir = scratch("codex-env");
        std::fs::create_dir_all(dir.join("atoms")).expect("atoms");
        std::fs::write(
            dir.join("atoms/build.atom.astro"),
            "---\nimport type { Props, Input, Output } from './.atomic/types/build';\n\
             import type { AtomConfig } from './.atomic/types/authoring';\n\
             const { input }: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = { description: 'Build.', kind: 'codex', \
             env: { RUSTUP_HOME: '/opt/rustup', CARGO_HOME: '/opt/cargo', PYENV_ROOT: '/opt/pyenv' } };\n---\nBuild {input.text}\n",
        )
        .expect("atom");
        for role in ["input", "output"] {
            std::fs::write(
                dir.join(format!("atoms/build.{role}.schema.yaml")),
                "type: object\nadditionalProperties: false\nrequired: [text]\nproperties:\n  text: {type: string}\n",
            )
            .expect("schema");
        }
        let mut registry = Registry::new();
        let a = atom::load(&dir.join("atoms/build.atom.astro"), &mut registry).expect("loads");
        let compiled = compile::compile(&a, &registry).expect("compiles");
        let model = FakeModel::start(vec![Reply::Text(r#"{"text": "built"}"#.into())])
            .await
            .expect("fake model");
        let recorded = Recording::default();
        let mut all = runtimes(&model, "codex-env");
        all.codex.backend = recorded.backend();
        run_declarative(
            &a,
            &compiled,
            &mut registry,
            &json!({"text": "it"}),
            "run-1",
            "external",
            &all,
        )
        .await
        .expect("runs");
        let (asked, given) = &recorded.opened()[0];
        for (name, value) in [
            ("RUSTUP_HOME", "/opt/rustup"),
            ("CARGO_HOME", "/opt/cargo"),
            ("PYENV_ROOT", "/opt/pyenv"),
        ] {
            assert_eq!(asked.env.get(name).map(String::as_str), Some(value));
            assert_eq!(given.env.get(name).map(String::as_str), Some(value));
        }
    });
}

/// Where the sandbox cannot run, the backend still connects and an atom
/// that needs no sandbox, such as a lite atom, runs; an exec atom, or a
/// codex atom whose turn would run commands, is refused before anything
/// runs as unconfinable, with what the sandbox needs, which a durable run
/// stops on without recording anything. A failed check is not kept: each
/// such atom checks again.
#[test]
fn only_atoms_that_run_commands_need_the_sandbox() {
    use crate::codex::{CodexPaths, ExecSandbox, StateGuard};
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(CLASSIFIED.into())])
            .await
            .expect("fake model");
        // Not under /tmp, which connect refuses.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/atomic-core-tests")
            .join(format!("no-sandbox-{}", std::process::id()));
        let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Counting(ExecSandbox, Arc<std::sync::atomic::AtomicUsize>);
        impl atomic_exec::Confine for Counting {
            fn confine(
                &self,
                command: &atomic_exec::Command,
                tmp: &std::path::Path,
            ) -> Result<atomic_exec::Command, atomic_exec::ExecError> {
                self.0.confine(command, tmp)
            }
            fn check(&self, tmp: &std::path::Path) -> Result<(), atomic_exec::ExecError> {
                self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.0.check(tmp)
            }
        }
        // A helper that fails as bubblewrap does on a machine without user
        // namespaces, and one that fails as it does on a full disk.
        let helper = |name: &str, says: &str| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(&root).expect("root");
            let path = root.join(name);
            std::fs::write(&path, format!("#!/bin/sh\necho '{says}' >&2\nexit 1\n"))
                .expect("helper");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            CodexPaths::with_helpers(path.clone(), path)
        };
        let paths = helper(
            "refusing",
            "bwrap: No permissions to create a new namespace, likely because the kernel does \
             not allow non-privileged user namespaces.",
        );
        let full = helper(
            "full",
            "error building bubblewrap command: No space left on device (os error 28)",
        );
        let tmp = root.join("check-tmp");
        std::fs::create_dir_all(&tmp).expect("tmp");
        let passing = ExecSandbox::new(&full, StateGuard::NoProjectState, root.join("codex"))
            .expect("a helper");
        assert!(
            matches!(
                atomic_exec::Confine::check(&passing, &tmp),
                Err(atomic_exec::ExecError::Unavailable { ref message, .. })
                    if message.contains("No space left on device")
            ),
            "a failure that may pass waits"
        );
        let sandbox = ExecSandbox::new(&paths, StateGuard::NoProjectState, root.join("codex"))
            .expect("a helper");
        let backend = atomic_exec::Backend::connect(
            atomic_exec::LocalBackend::in_dir(root.join("scratch"))
                .confined(Arc::new(Counting(sandbox, checks.clone()))),
        )
        .expect("connects without checking the sandbox");
        assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 0);

        let mut all = runtimes(&model, "no-sandbox");
        all.codex.backend = backend.clone();
        all.lite.backend = backend.clone();
        all.exec = backend;
        run_lite_atom(&LiteClassify, &Note { text: "n".into() }, &all.lite)
            .await
            .expect("a lite atom needs no sandbox");
        assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 0);

        let (_, a, compiled) = exec_project("no-sandbox", TOTAL, "");
        let r = run_declarative(
            &a,
            &compiled,
            &mut Registry::new(),
            &json!({"cents": [1]}),
            "run-1",
            "external",
            &all,
        )
        .await;
        let refused = |e: Option<&AtomError>| {
            matches!(e, Some(AtomError::Unconfinable(m))
                if m.contains("cannot start here")
                    && m.contains("bubblewrap")
                    && m.contains("unprivileged user namespaces")
                    && m.contains("--security-opt"))
        };
        assert!(refused(r.as_ref().err()), "{r:?}");
        let r = run_codex_atom(&Classify, &Note { text: "n".into() }, &all.codex).await;
        assert!(refused(r.as_ref().err()), "{r:?}");
        assert_eq!(
            checks.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a failed check is made again"
        );
        assert_eq!(model.calls(), 1, "only the lite atom called the model");
    });
}

/// A sandbox check that fails for a reason that may pass (no room, no
/// process to spare) makes the run wait, as a backend with no room does,
/// and once it passes it is kept.
#[test]
fn a_check_that_may_pass_is_waited_for_and_kept_once_it_passes() {
    struct Flaky(Arc<std::sync::atomic::AtomicUsize>);
    impl atomic_exec::Confine for Flaky {
        fn confine(
            &self,
            command: &atomic_exec::Command,
            _: &std::path::Path,
        ) -> Result<atomic_exec::Command, atomic_exec::ExecError> {
            Ok(command.clone())
        }
        fn check(&self, _: &std::path::Path) -> Result<(), atomic_exec::ExecError> {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                return Err(atomic_exec::ExecError::Unavailable {
                    label: "the sandbox check".into(),
                    message: "no process to spare".into(),
                });
            }
            Ok(())
        }
    }
    on_codex_runtime(async {
        let model = FakeModel::start(Vec::new()).await.expect("fake model");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/atomic-core-tests")
            .join(format!("flaky-check-{}", std::process::id()));
        let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let backend = atomic_exec::Backend::connect(
            atomic_exec::LocalBackend::in_dir(root.join("scratch"))
                .confined(Arc::new(Flaky(checks.clone()))),
        )
        .expect("connects");
        let mut all = runtimes(&model, "flaky-check");
        all.exec = backend;
        let (_, a, compiled) = exec_project("flaky-check", TOTAL, "");
        for run in ["run-1", "run-2"] {
            let (out, _) = run_declarative(
                &a,
                &compiled,
                &mut Registry::new(),
                &json!({"cents": [1, 2]}),
                run,
                "external",
                &all,
            )
            .await
            .expect("runs once the check passes");
            assert_eq!(out, json!({"total": 3}));
        }
        assert_eq!(
            checks.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "two refusals, one pass, then kept"
        );
    });
}

// An exec atom's output is held to the artifact store like a model's answer.
#[test]
fn an_exec_atom_output_names_only_artifacts_the_store_holds() {
    use crate::artifact::{ArtifactStore, LocalStore, Provenance};
    use crate::{atom, compile};
    on_codex_runtime(async {
        let store = LocalStore::open(scratch("exec-artifacts")).expect("store");
        let put = |bytes: &[u8], media: &str| {
            store
                .put(
                    &mut &bytes[..],
                    Provenance {
                        media_type: media.into(),
                        workflow: None,
                        step: None,
                        origin: None,
                        labels: Default::default(),
                    },
                )
                .expect("put")
                .artifact
                .uri()
        };
        let diff = put(b"--- a\n+++ b\n", "text/x-diff");
        let text = put(b"a note", "text/plain");
        let missing = format!("artifact://sha256/{}", "0".repeat(64));
        for (reference, want) in [
            (&diff, None),
            (&text, Some("text/x-diff")),
            (&missing, Some("no artifact")),
        ] {
            let script = format!(
                "import json, sys\njson.load(sys.stdin)\njson.dump({{'total': 1, 'patch': '{reference}'}}, sys.stdout)\n"
            );
            let (dir, _, _) = exec_project("exec-artifact", &script, "");
            std::fs::write(
                dir.join("atoms/total.output.schema.yaml"),
                "type: object\nadditionalProperties: false\nrequired: [total, patch]\nproperties:\n  total: {type: integer}\n  patch: {$ref: 'atomic:artifact', mediaType: text/x-diff}\n",
            )
            .expect("output");
            let mut registry = Registry::new();
            let a = atom::load(&dir.join("atoms/total.atom.astro"), &mut registry).expect("loads");
            let compiled = compile::compile(&a, &registry).expect("compiles");
            let r = run_exec(
                &a,
                &compiled,
                &json!({"cents": [1]}),
                &EffectIdentity {
                    run_id: "run-1".into(),
                    step_id: "total".into(),
                },
                Arc::new(atomic_exec::LocalBackend::default()),
                None,
                &Artifacts::new(store.clone()),
                &crate::usage::Metering::none(),
            )
            .await;
            match want {
                None => assert!(r.is_ok(), "{r:?}"),
                Some(says) => assert!(
                    matches!(&r, Err(AtomError::Schema { message, .. }) if message.contains(says)),
                    "{r:?}"
                ),
            }
        }
    });
}

// A number that validates but that CEL cannot read as its contract types it
// is a schema error at acceptance: repaired, and never checkpointed.
#[test]
fn an_answer_cel_cannot_read_is_repaired() {
    on_codex_runtime(async {
        let wedging = Reply::Text(
            r#"{"category": "bug", "summary": "s", "line": 1.0000000000000000000001}"#.into(),
        );
        let good = Reply::Text(r#"{"category": "bug", "summary": "s", "line": 1}"#.into());
        let model = FakeModel::start(vec![wedging.clone(), good])
            .await
            .expect("fake model");
        let rt = runtime(&model, "cel-repair");
        let (out, _) = run_codex_atom(&Classify, &Note { text: "n".into() }, &rt)
            .await
            .expect("repaired");
        assert_eq!(out.line, Some(1));
        assert_eq!(model.calls(), 2, "one repair");
        let model = FakeModel::start(vec![wedging.clone(), wedging])
            .await
            .expect("fake model");
        let rt = runtime(&model, "cel-repair-fails");
        let r = run_codex_atom(&Classify, &Note { text: "n".into() }, &rt).await;
        assert!(
            matches!(&r, Err(AtomError::Schema { message, .. }) if message.contains("integer")),
            "{r:?}"
        );
    });
}

struct Hidden;

impl Atom for Hidden {
    type In = Note;
    type Out = Value;
    const NAME: &'static str = "hidden-artifact";
    fn input_schema(&self) -> Value {
        Classify.input_schema()
    }
    fn output_schema(&self) -> Value {
        json!({"type": "object", "properties": {
            "files": {"type": "array", "contains": {"$ref": "atomic:artifact"}}}})
    }
    fn instructions(&self) -> Cow<'static, str> {
        "List the files.".into()
    }
}

// A contract written in code whose artifact references cannot be checked is
// refused before any model call.
#[test]
fn a_typed_contract_that_cannot_be_checked_is_refused_before_the_model() {
    on_codex_runtime(async {
        let model = FakeModel::start(Vec::new()).await.expect("fake model");
        let rt = runtime(&model, "hidden");
        let r = run_codex_atom(&Hidden, &Note { text: "n".into() }, &rt).await;
        assert!(
            matches!(&r, Err(AtomError::Runtime(message)) if message.contains("contains")),
            "{r:?}"
        );
        assert_eq!(model.calls(), 0);
    });
}

// An exec atom's output CEL cannot read is a schema error, as a model's is.
#[test]
fn an_exec_output_cel_cannot_read_fails() {
    on_codex_runtime(async {
        let script = "import sys\nsys.stdin.read()\nsys.stdout.write('{\"total\": 1.0000000000000000000001}')\n";
        let (_, a, compiled) = exec_project("exec-cel", script, "");
        let r = run_exec(
            &a,
            &compiled,
            &json!({"cents": [1]}),
            &EffectIdentity {
                run_id: "run-1".into(),
                step_id: "total".into(),
            },
            Arc::new(atomic_exec::LocalBackend::default()),
            None,
            &Artifacts::none(),
            &crate::usage::Metering::none(),
        )
        .await;
        assert!(
            matches!(&r, Err(AtomError::Schema { message, .. }) if message.contains("integer")),
            "{r:?}"
        );
    });
}

/// A ChatGPT sign-in, as Codex's login keeps it in a file store, with a
/// token no real backend would take, refreshed now.
fn signed_in(dir: &std::path::Path, backend: &str) -> crate::signin::SignIn {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_secs();
    signed_in_at(dir, backend, now)
}

/// The same, last refreshed `refreshed` seconds after the epoch. Its issuer
/// is the test backend's host, so a refresh, on a stale sign-in or a 401,
/// goes there and never to OpenAI's.
fn signed_in_at(dir: &std::path::Path, backend: &str, refreshed: u64) -> crate::signin::SignIn {
    std::fs::create_dir_all(dir).expect("auth home");
    let id_token = crate::testing::fake_id_token("dev@example.invalid", "account-1");
    let issuer = backend.trim_end_matches("/backend-api");
    std::fs::write(
        dir.join("auth.json"),
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": id_token,
                "access_token": "synthetic-access-token",
                "refresh_token": "synthetic-refresh-token",
                "account_id": "account-1",
                "oauth_provenance": {"issuer": issuer, "client_id": codex_login::CLIENT_ID},
            },
            "last_refresh": chrono_like(refreshed),
        })
        .to_string(),
    )
    .expect("auth.json");
    crate::signin::SignIn::in_files(dir).with_backend(backend)
}

/// `secs` since the epoch as an RFC 3339 UTC time.
fn chrono_like(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// Sign-ins are kept, and used, on Linux only.
#[cfg(target_os = "linux")]
#[test]
fn a_codex_atom_runs_on_a_chatgpt_sign_in_with_no_api_key() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(
            r#"{"category": "bug", "summary": "Export crashes.", "line": null}"#.into(),
        )])
        .await
        .expect("fake model");
        let mut rt = runtime(&model, "signed-in");
        let backend = model.base_url().replace("/v1", "/backend-api");
        let auth = rt.codex_home.with_file_name("auth");
        rt.endpoint = ModelEndpoint {
            base_url: "unused".into(),
            model: "synthetic".into(),
            api_key_env: None,
            sign_in: Some(signed_in(&auth, &backend)),
        };
        // A key in the environment does not replace the sign-in. Only this
        // test reads or writes CODEX_API_KEY.
        // SAFETY: as above; nothing else in this binary touches it.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("CODEX_API_KEY", "sk-from-the-environment");
        }
        let (out, _) = run_codex_atom(
            &Classify,
            &Note {
                text: "Export crashes on é.".into(),
            },
            &rt,
        )
        .await
        .expect("runs on the sign-in");
        assert_eq!(out.category, "bug");
        assert_eq!(model.calls(), 1);
        assert_eq!(model.paths(), ["/backend-api/codex/responses"]);
        let headers = &model.headers()[0];
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(
            header("authorization"),
            Some("Bearer synthetic-access-token")
        );
        assert_eq!(header("chatgpt-account-id"), Some("account-1"));
        // Everything the thread asked of the backend: the model, and nothing
        // that reaches the account's apps, connectors or plugins.
        eprintln!("every request: {:?}", model.every_request());
        assert_eq!(model.every_request(), expected_backend_requests());
        // SAFETY: as above.
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("CODEX_API_KEY");
        }
    });
}

/// A stale sign-in is refreshed at its own issuer, here the test backend,
/// before the thread uses it: nothing reaches OpenAI's.
#[cfg(target_os = "linux")]
#[test]
fn a_stale_sign_in_is_refreshed_at_the_test_issuer() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(
            r#"{"category": "bug", "summary": "Export crashes.", "line": null}"#.into(),
        )])
        .await
        .expect("fake model");
        let mut rt = runtime(&model, "stale-sign-in");
        let backend = model.base_url().replace("/v1", "/backend-api");
        let auth = rt.codex_home.with_file_name("auth");
        rt.endpoint = ModelEndpoint {
            base_url: "unused".into(),
            model: "synthetic".into(),
            api_key_env: None,
            sign_in: Some(signed_in_at(&auth, &backend, 0)),
        };
        // The fake has no token endpoint, so the refresh fails; whether the
        // turn then runs on the old token is Codex's to decide.
        let _ = run_codex_atom(&Classify, &Note { text: "n".into() }, &rt).await;
        let asked = model.every_request();
        assert!(
            asked.contains(&("POST".to_owned(), "/oauth/token".to_owned())),
            "{asked:?}"
        );
    });
}

/// Codex's login restrictions hold for the sign-in when a thread starts: a
/// workspace other than the sign-in's, or API keys only, refuse it before
/// any model call.
#[cfg(target_os = "linux")]
#[test]
fn codex_login_restrictions_refuse_a_sign_in_they_do_not_allow() {
    on_codex_runtime(async {
        for (tag, config, said) in [
            (
                "workspace",
                "forced_chatgpt_workspace_id = \"another-workspace\"\n",
                "allow ChatGPT workspace(s) another-workspace only",
            ),
            (
                "method",
                "forced_login_method = \"api\"\n",
                "do not allow a chatgpt sign-in",
            ),
        ] {
            let model = FakeModel::start(Vec::new()).await.expect("fake model");
            let mut rt = runtime(&model, &format!("restricted-{tag}"));
            std::fs::write(rt.codex_home.join("config.toml"), config).expect("config");
            let backend = model.base_url().replace("/v1", "/backend-api");
            let auth = rt.codex_home.with_file_name("auth");
            rt.endpoint = ModelEndpoint {
                base_url: "unused".into(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: Some(signed_in(&auth, &backend)),
            };
            let refused = run_codex_atom(&Classify, &Note { text: "n".into() }, &rt)
                .await
                .expect_err("refused");
            assert!(refused.to_string().contains(said), "{tag}: {refused}");
            assert_eq!(model.calls(), 0, "{tag}");
        }
    });
}

/// The requests a signed-in thread makes of the Codex backend, in order.
fn expected_backend_requests() -> Vec<(String, String)> {
    // The model catalog, then the turn.
    [
        ("GET", "/backend-api/codex/models"),
        ("POST", "/backend-api/codex/responses"),
    ]
    .into_iter()
    .map(|(m, p)| (m.to_owned(), p.to_owned()))
    .collect()
}

#[test]
fn a_lite_atom_says_a_sign_in_is_not_for_it() {
    on_codex_runtime(async {
        let model = FakeModel::start(Vec::new()).await.expect("fake model");
        let codex = runtime(&model, "lite-signed-in");
        let auth = codex.codex_home.with_file_name("auth");
        let endpoint = ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
            sign_in: Some(signed_in(
                &auth,
                &model.base_url().replace("/v1", "/backend-api"),
            )),
        };
        let lite = crate::lite::LiteRuntime::new(endpoint, AtomConfig::default());
        let refused = run_lite_atom(
            &Patch,
            &Note {
                text: "Export crashes.".into(),
            },
            &lite,
        )
        .await
        .expect_err("refused");
        let said = refused.to_string();
        assert!(said.contains("serves Codex atoms only"), "{said}");
        assert!(said.contains("ATOMIC_MODEL_BASE_URL"), "{said}");
        assert_eq!(model.calls(), 0);
    });
}

#[tokio::test]
async fn a_worktree_steps_turn_finds_its_runs_caches_and_only_it_may_write_them() {
    let model = FakeModel::start(vec![]).await.expect("model");
    let mut rt = runtime(&model, "caches");
    let contract = json!({"type": "object"});
    let validator = jsonschema::validator_for(&contract).expect("schema");
    let request = |sandbox, env: &[(&str, &str)]| Request {
        developer: String::new(),
        prompt: String::new(),
        model: None,
        sandbox,
        contract: &contract,
        validator: &validator,
        name: "fix",
        place: Place::of("run-1", "fix"),
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    };
    use crate::typed::Sandbox::{ReadOnly, WorkspaceWrite};
    // Outside a worktree step, nothing of the caches.
    let open = Backend::Codex(&rt).open(&request(WorkspaceWrite, &[]));
    assert!(
        open.env.is_empty() && open.writable_roots.is_empty(),
        "{open:?}"
    );
    let caches = crate::worktree::Caches::new(std::env::temp_dir().join("atomic-caches-x"));
    rt.caches = Some(caches.clone());
    let open = Backend::Codex(&rt).open(&request(WorkspaceWrite, &[("CARGO_TARGET_DIR", "/mine")]));
    assert!(
        !open.env.contains_key("CARGO_HOME"),
        "cargo's home is the attempt's own: {open:?}"
    );
    for name in [
        "PIP_CACHE_DIR",
        "GOMODCACHE",
        "XDG_CACHE_HOME",
        "ATOMIC_CACHE_DIR",
    ] {
        let at = open
            .env
            .get(name)
            .unwrap_or_else(|| panic!("{name}: {open:?}"));
        assert!(
            std::path::Path::new(at).starts_with(caches.dir()),
            "{name}={at}"
        );
    }
    assert!(!open.env.contains_key("HOME"), "HOME is the attempt's own");
    assert_eq!(
        open.env["CARGO_TARGET_DIR"], "/mine",
        "what the atom declares wins"
    );
    assert_eq!(open.writable_roots, [caches.dir().to_owned()]);
    // A read-only turn finds them, and writes nothing.
    let open = Backend::Codex(&rt).open(&request(ReadOnly, &[]));
    assert!(open.env.contains_key("XDG_CACHE_HOME"));
    assert!(open.writable_roots.is_empty(), "{open:?}");
}

#[test]
fn a_turns_cargo_home_is_its_own_and_its_toolchains_are_only_read() {
    use crate::typed::Sandbox::WorkspaceWrite;
    let dir = std::env::temp_dir().join(format!("atomic-turn-in-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["tree", "scratch", "tmp", "caches", "toolchains"] {
        std::fs::create_dir_all(dir.join(sub)).expect("dir");
    }
    let workspace = |rustup: &std::path::Path| atomic_exec::Workspace {
        label: "fix".into(),
        root: Some(dir.join("tree")),
        scratch: Some(dir.join("scratch")),
        tmp: Some(dir.join("tmp")),
        shell: Shell::Local,
        sandbox: WorkspaceWrite,
        writable_roots: vec![dir.join("caches")],
        env: [("RUSTUP_HOME".to_owned(), rustup.display().to_string())].into(),
    };
    let mut spec = TurnSpec::default();
    turn_in(&workspace(&dir.join("toolchains")), &mut spec).expect("toolchains apart");
    assert_eq!(
        spec.env["CARGO_HOME"],
        dir.join("scratch/.cargo").display().to_string()
    );
    // Toolchains inside, or holding, anything the turn may write.
    for inside in ["caches/rustup", "tree/rustup", "scratch/.rustup", ""] {
        let mut spec = TurnSpec::default();
        let refused = turn_in(&workspace(&dir.join(inside)), &mut spec).expect_err(inside);
        assert!(refused.to_string().contains("RUSTUP_HOME"), "{refused}");
    }
    // A cache an atom points into the temporary directory every turn
    // shares.
    let mut shared = workspace(&dir.join("toolchains"));
    shared.env.insert(
        "GOMODCACHE".into(),
        dir.join("tmp/go").display().to_string(),
    );
    let refused = turn_in(&shared, &mut TurnSpec::default()).expect_err("shared");
    assert!(refused.to_string().contains("GOMODCACHE"), "{refused}");
    // Through a link to it, too.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.join("tmp"), dir.join("alias")).expect("link");
        let mut linked = workspace(&dir.join("toolchains"));
        linked.env.insert(
            "XDG_CACHE_HOME".into(),
            dir.join("alias/xdg").display().to_string(),
        );
        let refused = turn_in(&linked, &mut TurnSpec::default()).expect_err("linked");
        assert!(refused.to_string().contains("XDG_CACHE_HOME"), "{refused}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// A turn's commands get a TMPDIR of their own turn's on every platform: on
// Linux by way of atomic's sandbox helper, which takes it from
// ATOMIC_TURN_TMPDIR while Codex's helper keeps the backend's shared
// directory for its registry; elsewhere directly, with no handoff variable
// left for the command to see. The shared directory is never a turn's
// writable root, and a Linux turn without one is refused.
#[test]
fn a_turns_tmpdir_is_its_own_on_every_platform() {
    let base = std::env::temp_dir().join(format!("atomic-turn-tmpdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (scratch, shared) = (base.join("attempt"), base.join(".tmp"));
    std::fs::create_dir_all(&scratch).expect("scratch");
    std::fs::create_dir_all(&shared).expect("shared");
    let mut declared = std::collections::BTreeMap::new();
    declared.insert(codex::TURN_TMPDIR.to_owned(), "/elsewhere".to_owned());
    let workspace = Workspace {
        label: "t".into(),
        root: Some(base.clone()),
        scratch: Some(scratch.clone()),
        tmp: Some(shared.clone()),
        sandbox: crate::typed::Sandbox::WorkspaceWrite,
        env: declared,
        ..Workspace::default()
    };
    let own = scratch.join("tmp").display().to_string();
    let built = |linux: bool, workspace: &Workspace| {
        let mut spec = TurnSpec {
            env: workspace.env.clone(),
            writable_roots: vec![scratch.clone()],
            ..TurnSpec::default()
        };
        turn_tmpdir(workspace, &mut spec, linux).map(|()| spec)
    };

    let linux = built(true, &workspace).expect("linux");
    assert_eq!(linux.env["TMPDIR"], shared.display().to_string());
    assert_eq!(linux.env[codex::TURN_TMPDIR], own);
    assert!(!linux.writable_roots.contains(&shared));

    let other = built(false, &workspace).expect("elsewhere");
    assert_eq!(other.env["TMPDIR"], own);
    assert!(
        !other.env.contains_key(codex::TURN_TMPDIR),
        "{:?}",
        other.env
    );
    assert!(!other.writable_roots.contains(&shared));

    assert!(std::path::Path::new(&own).is_dir(), "{own} was not made");
    let without = Workspace {
        tmp: None,
        ..workspace.clone()
    };
    assert!(
        matches!(built(true, &without), Err(AtomError::Runtime(m)) if m.contains("shared temporary directory"))
    );
    assert_eq!(
        built(false, &without).expect("elsewhere").env["TMPDIR"],
        own
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A store that, when `at` is reached (`"open"` as an input is copied in,
/// `"stage"` as a handed-over file is read), says so and waits until it is
/// let go.
#[cfg(unix)]
#[derive(Clone)]
struct Stalling {
    inner: crate::artifact::LocalStore,
    at: &'static str,
    state: Arc<(std::sync::Mutex<(bool, bool)>, std::sync::Condvar)>,
}

#[cfg(unix)]
impl Stalling {
    fn stall(&self, at: &str) {
        if at != self.at {
            return;
        }
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().expect("state");
        state.0 = true;
        wake.notify_all();
        while !state.1 {
            state = wake.wait(state).expect("state");
        }
    }

    fn reached(&self) -> bool {
        self.state.0.lock().expect("state").0
    }

    fn let_go(&self) {
        let (lock, wake) = &*self.state;
        if let Ok(mut state) = lock.lock() {
            state.1 = true;
        }
        wake.notify_all();
    }
}

/// Lets a [`Stalling`] store go when dropped, as when a test fails, so the
/// runtime does not wait forever on the thread it holds.
#[cfg(unix)]
struct LetGo(Stalling);

#[cfg(unix)]
impl Drop for LetGo {
    fn drop(&mut self) {
        self.0.let_go();
    }
}

#[cfg(unix)]
impl crate::artifact::ArtifactStore for Stalling {
    fn stage(
        &self,
        content: &mut dyn std::io::Read,
    ) -> Result<crate::artifact::Staged, crate::artifact::ArtifactError> {
        self.stall("stage");
        self.inner.stage(content)
    }
    fn commit(
        &self,
        staged: crate::artifact::Staged,
        provenance: &[crate::artifact::Provenance],
    ) -> Result<Vec<crate::artifact::Manifest>, crate::artifact::ArtifactError> {
        self.inner.commit(staged, provenance)
    }
    fn discard(&self, staged: crate::artifact::Staged) {
        self.inner.discard(staged)
    }
    fn contains(
        &self,
        artifact: &crate::artifact::ArtifactRef,
    ) -> Result<bool, crate::artifact::ArtifactError> {
        self.inner.contains(artifact)
    }
    fn open(
        &self,
        artifact: &crate::artifact::ArtifactRef,
    ) -> Result<Box<dyn std::io::Read + Send>, crate::artifact::ArtifactError> {
        self.stall("open");
        self.inner.open(artifact)
    }
    fn manifests(
        &self,
        artifact: &crate::artifact::ArtifactRef,
    ) -> Result<Vec<crate::artifact::Manifest>, crate::artifact::ArtifactError> {
        self.inner.manifests(artifact)
    }
}

/// A run cancelled while its command's input files are copied in, or while
/// the files it handed over are read, keeps its workspace open until that
/// ends.
#[cfg(unix)]
#[test]
fn a_cancelled_run_waits_for_its_files_to_be_copied_and_read() {
    use crate::artifact::{ArtifactStore, LocalStore, Provenance};
    use crate::{atom, compile};
    on_codex_runtime(async {
        for (at, before) in [
            ("open", &["open total"][..]),
            ("stage", &["open total", "run python3"][..]),
        ] {
            let store = LocalStore::open(scratch("exec-stall-store")).expect("store");
            let given = store
                .put(
                    &mut &b"--- a\n+++ b\n"[..],
                    Provenance {
                        media_type: "text/x-diff".into(),
                        ..Default::default()
                    },
                )
                .expect("put")
                .artifact
                .uri();
            let script = "import json, os, sys\njson.load(sys.stdin)\n\
                 open(os.path.join(os.environ['ATOMIC_OUTPUTS'], 'out.diff'), 'w').write('x')\n\
                 json.dump({'total': 1, 'patch': {'$file': 'out.diff', 'mediaType': 'text/x-diff'}}, sys.stdout)\n";
            let (dir, _, _) = exec_project("exec-stall", script, "");
            std::fs::write(
                dir.join("atoms/total.input.schema.yaml"),
                "type: object\nadditionalProperties: false\nrequired: [cents, patch]\nproperties:\n  cents: {type: array, items: {type: integer}}\n  patch: {$ref: 'atomic:artifact', mediaType: text/x-diff}\n",
            )
            .expect("input");
            std::fs::write(
                dir.join("atoms/total.output.schema.yaml"),
                "type: object\nadditionalProperties: false\nrequired: [total, patch]\nproperties:\n  total: {type: integer}\n  patch: {$ref: 'atomic:artifact', mediaType: text/x-diff}\n",
            )
            .expect("output");
            let mut registry = Registry::new();
            let a = atom::load(&dir.join("atoms/total.atom.astro"), &mut registry).expect("loads");
            let compiled = compile::compile(&a, &registry).expect("compiles");
            let stalling = Stalling {
                inner: store.clone(),
                at,
                state: Default::default(),
            };
            let artifacts = Artifacts(
                Arc::new(store.clone()),
                Some(Arc::new(stalling.clone())),
                Default::default(),
            );
            let _let_go = LetGo(stalling.clone());
            let recorded = Recording::default();
            let backend: Arc<dyn atomic_exec::ExecutionBackend> = Arc::new(recorded.clone());
            let task = tokio::spawn(async move {
                run_exec(
                    &a,
                    &compiled,
                    &json!({"cents": [1], "patch": given}),
                    &EffectIdentity {
                        run_id: "run-1".into(),
                        step_id: "total".into(),
                    },
                    backend,
                    None,
                    &artifacts,
                    &crate::usage::Metering::none(),
                )
                .await
            });
            for _ in 0..1000 {
                if stalling.reached() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(stalling.reached(), "{at}: never reached the store");
            task.abort();
            assert!(task.await.expect_err("cancelled").is_cancelled());
            // The copy or the read still goes on, so the workspace stays...
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let scratch = recorded.opened()[0].1.scratch.clone().expect("scratch");
            assert_eq!(recorded.calls(), before, "{at}");
            assert!(scratch.is_dir(), "{at}: closed under the store");
            // ...until it ends.
            stalling.let_go();
            for _ in 0..1000 {
                if recorded.calls().len() == before.len() + 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                recorded.calls().last().map(String::as_str),
                Some("close total")
            );
            for _ in 0..200 {
                if !scratch.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(!scratch.exists(), "{at}");
        }
    });
}

/// A store held by a sweep while a command's files are stored is waited
/// out a few times; held past them, it leaves the step's outcome unknown,
/// as a crash there would, not failed, for recovery to take up through the
/// step's attempt guard.
#[cfg(unix)]
#[test]
fn a_store_held_while_files_are_stored_leaves_the_outcome_unknown() {
    use crate::artifact::LocalStore;
    use crate::{atom, compile};
    on_codex_runtime(async {
        let root = scratch("exec-locked-store");
        let store = LocalStore::open(&root)
            .expect("store")
            .waiting_at_most(std::time::Duration::from_millis(100));
        let script = "import json, os, sys\njson.load(sys.stdin)\n\
             open(os.path.join(os.environ['ATOMIC_OUTPUTS'], 'out.diff'), 'w').write('x')\n\
             json.dump({'total': 1, 'patch': {'$file': 'out.diff', 'mediaType': 'text/x-diff'}}, sys.stdout)\n";
        let (dir, _, _) = exec_project("exec-locked", script, "");
        std::fs::write(
            dir.join("atoms/total.output.schema.yaml"),
            "type: object\nadditionalProperties: false\nrequired: [total, patch]\nproperties:\n  total: {type: integer}\n  patch: {$ref: 'atomic:artifact', mediaType: text/x-diff}\n",
        )
        .expect("output");
        let mut registry = Registry::new();
        let a = atom::load(&dir.join("atoms/total.atom.astro"), &mut registry).expect("loads");
        let compiled = compile::compile(&a, &registry).expect("compiles");
        let artifacts = Artifacts::store(store, Default::default());
        // A sweep's hold on the store.
        let hold = || {
            let held = std::fs::File::options()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(root.join("lock"))
                .expect("the store's lock");
            held.lock().expect("held");
            held
        };
        let identity = EffectIdentity {
            run_id: "run-1".into(),
            step_id: "total".into(),
        };
        let input = json!({"cents": [1]});
        let meter = crate::usage::Metering::none();
        let run = || {
            run_exec(
                &a,
                &compiled,
                &input,
                &identity,
                Arc::new(atomic_exec::LocalBackend::default()),
                None,
                &artifacts,
                &meter,
            )
        };
        let held = hold();
        let r = run().await;
        assert!(
            matches!(&r, Err(AtomError::OutcomeUnknown(m)) if m.contains("lock")),
            "{r:?}"
        );
        drop(held);
        // A sweep that lets go while the gather waits.
        let held = hold();
        let sweep = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            drop(held);
        });
        let (out, _) = run().await.expect("stored once the sweep let go");
        assert!(
            out["patch"]
                .as_str()
                .is_some_and(|r| r.starts_with("artifact://sha256/")),
            "{out}"
        );
        sweep.join().expect("sweep");
    });
}

/// A local backend whose open waits, once it has begun, until it is let
/// go, and that counts its closes.
#[derive(Clone, Default)]
struct HeldOpen {
    state: Arc<(std::sync::Mutex<(bool, bool)>, std::sync::Condvar)>,
    closes: Arc<std::sync::atomic::AtomicUsize>,
    opened: Arc<std::sync::Mutex<Option<Workspace>>>,
}

impl HeldOpen {
    fn begun(&self) -> bool {
        self.state.0.lock().expect("state").0
    }

    fn let_go(&self) {
        let (lock, wake) = &*self.state;
        if let Ok(mut state) = lock.lock() {
            state.1 = true;
        }
        wake.notify_all();
    }

    fn closes(&self) -> usize {
        self.closes.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Lets a [`HeldOpen`] go when dropped, as when a test fails while the
/// open is held, so the test fails rather than hangs.
struct LetOpenGo(HeldOpen);

impl Drop for LetOpenGo {
    fn drop(&mut self) {
        self.0.let_go();
    }
}

impl ExecutionBackend for HeldOpen {
    fn connect(&self) -> Result<atomic_exec::Statement, atomic_exec::ExecError> {
        Ok(atomic_exec::LocalBackend::STATEMENT)
    }

    fn open(&self, request: &atomic_exec::Open) -> Result<Workspace, atomic_exec::ExecError> {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().expect("state");
        state.0 = true;
        wake.notify_all();
        while !state.1 {
            state = wake.wait(state).expect("state");
        }
        drop(state);
        let ws = atomic_exec::LocalBackend::default().open(request)?;
        *self.opened.lock().expect("opened") = Some(ws.clone());
        Ok(ws)
    }

    fn run(
        &self,
        command: &atomic_exec::Command,
    ) -> Result<atomic_exec::Completed, atomic_exec::ExecError> {
        atomic_exec::LocalBackend::default().run(command)
    }

    fn close(&self, workspace: &Workspace) -> Result<(), atomic_exec::ExecError> {
        self.closes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        atomic_exec::LocalBackend::default().close(workspace)
    }
}

/// A run cancelled while its workspace is still opening leaves nothing
/// open: the open finishes, and the workspace it made is closed, once.
#[test]
fn a_workspace_that_opens_after_its_run_was_cancelled_is_closed() {
    on_codex_runtime(async {
        let dir = scratch("cancelled-open");
        std::fs::create_dir_all(&dir).expect("source");
        let backend = HeldOpen::default();
        let _let_go = LetOpenGo(backend.clone());
        let shared: Arc<dyn ExecutionBackend> = Arc::new(backend.clone());
        let request = atomic_exec::Open {
            label: "held".into(),
            run: "run-1".into(),
            step: "held".into(),
            attempt: "attempt-1".into(),
            source: Some(dir.clone()),
            ..atomic_exec::Open::default()
        };
        let opening = tokio::spawn(open_workspace(shared, request));
        for _ in 0..500 {
            if backend.begun() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(backend.begun(), "the open never began");
        opening.abort();
        assert!(matches!(opening.await, Err(e) if e.is_cancelled()));
        // The open succeeds once no one waits for it.
        backend.let_go();
        for _ in 0..500 {
            if backend.closes() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(backend.closes(), 1, "the workspace was left open");
        let scratch = backend
            .opened
            .lock()
            .expect("opened")
            .as_ref()
            .and_then(|ws| ws.scratch.clone())
            .expect("a scratch directory");
        for _ in 0..200 {
            if !scratch.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!scratch.exists(), "its scratch directory is left");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(backend.closes(), 1, "closed more than once");
    });
}

/// Run `body` on a runtime of its own on a thread of its own, then drop the
/// runtime: whether the drop returned within `within`, and did not hang.
fn runtime_drop_returns(
    within: std::time::Duration,
    body: impl FnOnce(&tokio::runtime::Runtime) + Send + 'static,
) -> bool {
    let (done, dropped) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        body(&rt);
        drop(rt);
        let _ = done.send(());
    });
    dropped.recv_timeout(within).is_ok()
}

/// A runtime dropped while a workspace is still opening shuts down: the
/// open finishes as the blocking pool winds down, and the workspace it
/// made is left for the backend to reclaim rather than closed from there.
#[test]
fn a_runtime_dropped_while_a_workspace_opens_shuts_down() {
    let backend = HeldOpen::default();
    let _let_go = LetOpenGo(backend.clone());
    let held = backend.clone();
    let returned = runtime_drop_returns(std::time::Duration::from_secs(30), move |rt| {
        let dir = scratch("dropped-open");
        std::fs::create_dir_all(&dir).expect("source");
        let shared: Arc<dyn ExecutionBackend> = Arc::new(held.clone());
        rt.spawn(open_workspace(
            shared,
            atomic_exec::Open {
                label: "held".into(),
                run: "run-1".into(),
                step: "held".into(),
                attempt: "attempt-1".into(),
                source: Some(dir),
                ..atomic_exec::Open::default()
            },
        ));
        for _ in 0..500 {
            if held.begun() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(held.begun(), "the open never began");
        // Let go once the runtime is shutting down.
        let late = held.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            late.let_go();
        });
    });
    assert!(returned, "the runtime's drop hung");
}

/// A runtime dropped while a command runs in its workspace, or while a task
/// holds an open workspace, shuts down.
#[test]
fn a_runtime_dropped_under_a_running_command_or_a_held_workspace_shuts_down() {
    // A command that holds its workspace open until it ends.
    let (_, a, compiled) = exec_project(
        "dropped-command",
        "import json, sys, time\njson.load(sys.stdin)\ntime.sleep(1)\njson.dump({'total': 1}, sys.stdout)\n",
        "",
    );
    let recorded = Recording::default();
    let calls = recorded.clone();
    let returned = runtime_drop_returns(std::time::Duration::from_secs(30), move |rt| {
        let backend: Arc<dyn ExecutionBackend> = Arc::new(calls.clone());
        rt.spawn(async move {
            let _ = run_exec(
                &a,
                &compiled,
                &json!({"cents": [1]}),
                &EffectIdentity {
                    run_id: "run-1".into(),
                    step_id: "total".into(),
                },
                backend,
                None,
                &Artifacts::none(),
                &crate::usage::Metering::none(),
            )
            .await;
        });
        for _ in 0..500 {
            if calls.calls().iter().any(|c| c.starts_with("run ")) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            calls.calls().iter().any(|c| c.starts_with("run ")),
            "{:?}",
            calls.calls()
        );
    });
    assert!(returned, "the runtime's drop hung under a running command");
    // A task that holds an open workspace and never lets go of it.
    let holding = Recording::default();
    let opened = holding.clone();
    let returned = runtime_drop_returns(std::time::Duration::from_secs(30), move |rt| {
        let dir = scratch("dropped-guard");
        std::fs::create_dir_all(&dir).expect("source");
        let backend: Arc<dyn ExecutionBackend> = Arc::new(opened.clone());
        rt.spawn(async move {
            let guard = open_workspace(
                backend,
                atomic_exec::Open {
                    label: "kept".into(),
                    run: "run-1".into(),
                    step: "kept".into(),
                    attempt: "attempt-1".into(),
                    source: Some(dir),
                    ..atomic_exec::Open::default()
                },
            )
            .await;
            std::future::pending::<()>().await;
            drop(guard);
        });
        for _ in 0..500 {
            if !opened.opened().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!opened.opened().is_empty(), "the workspace never opened");
    });
    assert!(returned, "the runtime's drop hung under a held workspace");
}
