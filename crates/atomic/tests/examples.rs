// SPDX-License-Identifier: Apache-2.0

//! The v0.1 examples (01 to 05) under atomic's own loader: each one passes
//! `atomic check`, every golden fixture fits its atom's full contracts and
//! passes the atom's validators, every model atom renders its prompt for its
//! fixture input, and every exec atom, run for real, turns its fixture input
//! into exactly its fixture output.

use atomic_core::render::{self, RunInput};
use atomic_core::{atom, collections, compile, eval, project, runtime};
use atomic_schema::registry::Registry;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const V0_1: &[&str] = &[
    "01-classify",
    "02-change-request",
    "03-order-report",
    "04-embedded-review",
    "05-support-draft",
];

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("directory");
    for entry in std::fs::read_dir(source).expect("source") {
        let entry = entry.expect("entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            if entry.file_name() == "types" && source.ends_with(".atomic") {
                continue;
            }
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// A copy of `example` outside the repository, so `atomic check` writes its
/// generated declarations there. Each call gets its own directory: tests
/// that copy the same example run in parallel in one process.
fn copy(example: &str) -> PathBuf {
    static COPIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = COPIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "atomic-example-{example}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    copy_tree(&repository().join("examples").join(example), &dir);
    dir
}

fn read(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("JSON")
}

fn fits(what: &str, contract: &Value, value: &Value) {
    let validator = Registry::new().validator_for(contract).expect("contract");
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{what}: {errors:?}");
}

#[test]
fn every_v0_1_example_passes_the_real_load_and_check() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    for example in V0_1 {
        let dir = copy(example);
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_atomic"))
            .arg("check")
            .current_dir(&dir)
            .env("ATOMIC_HOME", dir.join(".state"))
            .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
            .env_remove("OPENAI_API_KEY")
            .env("XDG_CONFIG_HOME", dir.join(".state/config"))
            .output()
            .expect("atomic check");
        assert!(
            output.status.success(),
            "{example}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let project = project::discover(&dir).expect("project");
        let files = collections::collect(&project).expect("collections");
        let mut registry = Registry::new();
        for schema in &files["schemas"] {
            registry.load_file(&schema.path).expect("schema");
        }
        let mut fixtures = 0;
        let mut eval_cases = 0;
        for source in &files["atoms"] {
            let atom = atom::load(&source.path, &mut registry).expect("atom");
            let compiled = compile::compile(&atom, &registry).expect("compiles");
            let stem = source
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".atom.astro"))
                .expect("atom file name");
            // Its eval cases load, and each one's assertions hold against
            // the golden output beside it: a YAML case `<name>.yaml` against
            // `fixtures/<atom>/<name>.output.json`, line N of a JSONL file
            // `<file>.jsonl` against line N of `<file>.outputs.jsonl`.
            let settings = eval::settings(&atom, &project).expect("eval settings");
            let cases =
                eval::load_cases(&atom, &settings).unwrap_or_else(|e| panic!("{example}: {e}"));
            let drifted = drift(&project.dir.join("fixtures").join(stem), &cases);
            assert!(drifted.is_empty(), "{example}/{stem}: {drifted:?}");
            let contracts = eval::Contracts::of(&atom).expect("contracts");
            for case in &cases {
                let golden = match case.id.split_once(".jsonl:") {
                    Some((file, line)) => {
                        let path = project
                            .dir
                            .join("fixtures")
                            .join(stem)
                            .join(format!("{file}.outputs.jsonl"));
                        let text = std::fs::read_to_string(&path)
                            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                        let n: usize = line.parse().expect("a line number");
                        serde_json::from_str(text.lines().nth(n - 1).expect("a golden row"))
                            .expect("a golden output")
                    }
                    None => read(
                        &project
                            .dir
                            .join("fixtures")
                            .join(stem)
                            .join(format!("{}.output.json", case.id.trim_end_matches(".yaml"))),
                    ),
                };
                let failures = eval::check(case, &golden, &contracts);
                assert!(
                    failures.is_empty(),
                    "{example}/{stem}/{}: {failures:?}",
                    case.id
                );
                eval_cases += 1;
            }
            let Ok(entries) = std::fs::read_dir(project.dir.join("fixtures").join(stem)) else {
                continue;
            };
            for entry in entries {
                let path = entry.expect("entry").path();
                let Some(case) = path
                    .to_str()
                    .and_then(|p| p.strip_suffix(".input.json"))
                    .map(str::to_owned)
                else {
                    continue;
                };
                let name = Path::new(&case)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let what = format!("{example}/{stem}/{name}");
                let input = read(&path);
                let golden = read(Path::new(&format!("{case}.output.json")));
                let contracts = &atom.program.contracts;
                fits(&format!("{what} input"), &contracts["Input"], &input);
                fits(&format!("{what} output"), &contracts["Output"], &golden);
                let checks = atom
                    .frontmatter
                    .get("validate")
                    .and_then(Value::as_array)
                    .is_some_and(|v| !v.is_empty());
                if checks {
                    let result = atom
                        .program
                        .invoke(
                            "validate",
                            json!({"values": {"input": input, "output": golden}}),
                        )
                        .expect("validators run");
                    assert_eq!(result["valid"], true, "{what}: {result}");
                }
                if atom.kind == "exec" {
                    let (out, _) = runtime
                        .block_on(runtime::run_exec(
                            &atom,
                            &compiled,
                            &input,
                            &runtime::EffectIdentity {
                                run_id: "examples".into(),
                                step_id: atom.name.clone(),
                            },
                            Arc::new(atomic_exec::LocalBackend::default()),
                            None,
                            &runtime::Artifacts::none(),
                            &atomic_core::usage::Metering::none(),
                        ))
                        .unwrap_or_else(|e| panic!("{what}: {e}"));
                    assert_eq!(out, golden, "{what}: the command's output");
                } else {
                    let prompt = render::render(
                        &compiled,
                        &RunInput {
                            run_id: "example",
                            input: &input,
                            frontmatter: &atom.frontmatter,
                            origin: "external",
                            origins: None,
                            step: None,
                        },
                    )
                    .unwrap_or_else(|e| panic!("{what}: {e}"));
                    assert!(!prompt.developer.is_empty(), "{what}: instructions");
                }
                fixtures += 1;
            }
        }
        assert!(
            fixtures > 0 || *example == "04-embedded-review",
            "{example}: no fixtures checked"
        );
        assert!(
            eval_cases > 0 || *example == "04-embedded-review",
            "{example}: no eval cases checked"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// The workspace example (`examples/06-workspace`): a member checked from an
// unrelated directory resolves the shared schema its output references,
// offline; two members sharing a schema ID fail.
#[test]
fn a_workspace_member_resolves_shared_schemas_from_anywhere() {
    let dir = copy("06-workspace");
    let check = |cwd: &Path| {
        std::process::Command::new(env!("CARGO_BIN_EXE_atomic"))
            .arg("check")
            .arg(dir.join("services/billing"))
            .current_dir(cwd)
            .env("ATOMIC_HOME", dir.join(".state"))
            .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
            .env_remove("OPENAI_API_KEY")
            .env("XDG_CONFIG_HOME", dir.join(".state/config"))
            .output()
            .expect("atomic check")
    };
    let elsewhere = std::env::temp_dir();
    let output = check(&elsewhere);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The billing atom's contract is the shared schema.
    let project = project::discover(&dir.join("services/billing")).expect("member");
    let mut registry = Registry::new();
    collections::load_schemas(&project, &mut registry).expect("schemas");
    let atom = atom::load(
        &dir.join("services/billing/atoms/assess-change.atom.astro"),
        &mut registry,
    )
    .expect("atom");
    compile::compile(&atom, &registry).expect("compiles");
    assert!(
        registry
            .get("https://atomic.example/schemas/assessment-v1")
            .is_some()
    );

    // Without the workspace, the reference has nothing to resolve to.
    std::fs::rename(dir.join("atomic.yaml"), dir.join("workspace.yaml.off")).expect("hide");
    assert!(!check(&elsewhere).status.success());
    std::fs::rename(dir.join("workspace.yaml.off"), dir.join("atomic.yaml")).expect("restore");

    // A second member declaring the same ID is a duplicate in the shared scope.
    let other = dir.join("services/payments");
    std::fs::create_dir_all(other.join("schemas")).expect("member");
    std::fs::write(other.join("atomic.yaml"), "atomic: 1\nname: payments\n").expect("manifest");
    std::fs::copy(
        dir.join("shared/schemas/assessment.schema.yaml"),
        other.join("schemas/assessment.schema.yaml"),
    )
    .expect("duplicate");
    let output = check(&elsewhere);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("assessment-v1"), "{error}");
}

// Example 09's contracts written in the compact form check, and `atomic
// schemas expand` prints exactly the long forms the example writes by hand.
#[test]
fn compact_sidecars_expand_to_the_example_long_forms() {
    let dir = copy("09-compact-and-variants");
    let atoms = dir.join("atoms");
    std::fs::write(
        atoms.join("label-compact.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/label-compact';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'Label one ticket.', model: 'basic' };\n\
         ---\nLabel the ticket.\nSubject: {input.subject}\nBody: {input.body}\n",
    )
    .expect("atom");
    std::fs::write(
        atoms.join("label-compact.input.schema.yaml"),
        "subject: string, the ticket's subject line\nbody: string, the customer's message\nplan?: string, the customer's plan when known\n",
    )
    .expect("input");
    std::fs::write(
        atoms.join("label-compact.output.schema.yaml"),
        "queue(enum, the team that should handle it): [billing, bugs, account, other]\nurgent: boolean, true when the customer is blocked from working\nreason: string, one sentence that explains the labels\n",
    )
    .expect("output");
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_atomic"))
            .args(args)
            .current_dir(&dir)
            .env("ATOMIC_HOME", dir.join(".state"))
            .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
            .env_remove("OPENAI_API_KEY")
            .env("XDG_CONFIG_HOME", dir.join(".state/config"))
            .output()
            .expect("atomic")
    };
    let checked = run(&["check"]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    // The compact atom and the example's full-schema atom print the same.
    for atom in ["label-compact", "label-ticket"] {
        let printed = run(&["schemas", "expand", atom]);
        assert!(
            printed.status.success(),
            "{}",
            String::from_utf8_lossy(&printed.stderr)
        );
        let printed: Value = serde_json::from_slice(&printed.stdout).expect("JSON");
        for role in ["input", "output"] {
            let file = dir.join(format!("expanded/label-ticket.{role}.schema.yaml"));
            let text = std::fs::read_to_string(&file).expect("expanded");
            let expected = atomic_schema::yaml::load(&file, &text).expect("yaml").value;
            assert_eq!(printed[role], expected, "{atom} {role}");
        }
    }
    // A compact schema that needs a constraint is refused, with its file.
    std::fs::write(
        atoms.join("label-compact.input.schema.yaml"),
        "subject: string\nwhen: date-time\n",
    )
    .expect("input");
    let refused = run(&["check"]);
    assert!(!refused.status.success());
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(
        error.contains("label-compact.input.schema.yaml") && error.contains("full JSON Schema"),
        "{error}"
    );
}

// OpenAPI toolsets under toolsets/ are checked with the project: each
// operation's tool schemas must compile, and a spec atomic cannot read fails
// the check with its file.
#[test]
fn atomic_check_reads_openapi_toolsets() {
    let dir = copy("01-classify");
    std::fs::create_dir_all(dir.join("toolsets")).expect("toolsets");
    std::fs::write(
        dir.join("toolsets/forecast.openapi.yaml"),
        "openapi: 3.0.3\ninfo: { title: Forecast, version: '1' }\npaths:\n  /v1/forecast:\n    get:\n      operationId: getForecast\n      parameters:\n        - { name: latitude, in: query, required: true, schema: { type: number, nullable: true } }\n      responses:\n        '200':\n          description: ok\n          content:\n            application/json:\n              schema: { type: object, properties: { temperature_2m: { type: number } } }\n",
    )
    .expect("spec");
    let check = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_atomic"))
            .arg("check")
            .current_dir(&dir)
            .env("ATOMIC_HOME", dir.join(".state"))
            .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
            .env_remove("OPENAI_API_KEY")
            .env("XDG_CONFIG_HOME", dir.join(".state/config"))
            .output()
            .expect("atomic check")
    };
    let output = check();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 toolset operations"));
    // A response the executor checks against, though not the first, must
    // compile too, or every call answered with its status would fail after
    // its request was made.
    std::fs::write(
        dir.join("toolsets/jobs.openapi.yaml"),
        "openapi: 3.1.0\ninfo: { title: Jobs, version: '1' }\npaths:\n  /jobs:\n    post:\n      operationId: submit\n      responses:\n        '200': { description: done, content: { application/json: { schema: { type: object } } } }\n        '202': { description: queued, content: { application/json: { schema: { type: object, properties: { ticket: { type: 12 } } } } } }\n",
    )
    .expect("spec");
    let output = check();
    assert!(
        !output.status.success(),
        "a response schema that does not compile passed"
    );
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("jobs.openapi.yaml") && error.contains("operation `submit`"),
        "{error}"
    );
    std::fs::remove_file(dir.join("toolsets/jobs.openapi.yaml")).expect("remove");
    // The same for a response that declares its body under more than one media
    // type: the reply's `Content-Type` picks which schema governs it, so the
    // second one is checked too, and must compile.
    std::fs::write(
        dir.join("toolsets/media.openapi.yaml"),
        "openapi: 3.1.0\ninfo: { title: Media, version: '1' }\npaths:\n  /thing:\n    get:\n      operationId: thing\n      responses:\n        '200':\n          description: two media types, the second one's schema broken\n          content:\n            application/json: { schema: { type: object } }\n            application/problem+json: { schema: { type: object, properties: { title: { type: 12 } } } }\n",
    )
    .expect("spec");
    let output = check();
    assert!(
        !output.status.success(),
        "a second media type's schema that does not compile passed"
    );
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("media.openapi.yaml") && error.contains("operation `thing`"),
        "{error}"
    );
    std::fs::remove_file(dir.join("toolsets/media.openapi.yaml")).expect("remove");
    // A failure response is not one a reply is checked against, so nothing of
    // it is compiled: a `404` whose schema reaches for a component that is not
    // in the spec does not fail the check, and the operation beside it is
    // imported with its `200`.
    std::fs::write(
        dir.join("toolsets/failed.openapi.yaml"),
        "openapi: 3.1.0\ninfo: { title: Failed, version: '1' }\npaths:\n  /thing:\n    get:\n      operationId: thing\n      responses:\n        '200': { description: ok, content: { application/json: { schema: { type: object } } } }\n        '404': { description: no such thing, content: { application/json: { schema: { $ref: '#/components/schemas/Nowhere' } } } }\n        '500': { description: broken, content: { application/json: { schema: { type: 12 } } } }\n",
    )
    .expect("spec");
    let output = check();
    assert!(
        output.status.success(),
        "a failure response's broken schema failed the check: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("2 toolset operations"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    std::fs::remove_file(dir.join("toolsets/failed.openapi.yaml")).expect("remove");
    std::fs::write(
        dir.join("toolsets/broken.openapi.yaml"),
        "swagger: '2.0'\npaths: {}\n",
    )
    .expect("spec");
    let output = check();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("broken.openapi.yaml") && error.contains("Swagger 2.0"),
        "{error}"
    );
}

/// Where an atom's eval cases and the fixtures in `fixtures` beside them
/// have drifted apart: a YAML case whose input is not its fixture's input, a
/// case whose `prepared` values are not its fixture's (or that has none
/// while its fixture does), or a JSONL file with not one golden output per
/// case.
fn drift(fixtures: &Path, cases: &[eval::Case]) -> Vec<String> {
    let mut drifted = Vec::new();
    let mut rows: BTreeMap<String, usize> = BTreeMap::new();
    for case in cases {
        if let Some((file, _)) = case.id.split_once(".jsonl:") {
            *rows.entry(file.to_owned()).or_default() += 1;
            continue;
        }
        let name = case.id.trim_end_matches(".yaml");
        let fixture = |suffix: &str| fixtures.join(format!("{name}.{suffix}.json"));
        match std::fs::read_to_string(fixture("input")) {
            Ok(text) if serde_json::from_str::<Value>(&text).ok() == Some(case.input.clone()) => {}
            _ => drifted.push(format!(
                "{}: the case's input is not its fixture's",
                case.id
            )),
        }
        let prepared = std::fs::read_to_string(fixture("prepared"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        if prepared != case.prepared {
            drifted.push(format!(
                "{}: the case's prepared values are not its fixture's",
                case.id
            ));
        }
    }
    for (file, count) in rows {
        let golden = std::fs::read_to_string(fixtures.join(format!("{file}.outputs.jsonl")))
            .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
        if golden != count {
            drifted.push(format!(
                "{file}.jsonl: {count} cases, and {golden} golden outputs"
            ));
        }
    }
    drifted
}

#[test]
fn the_drift_check_sees_inputs_prepared_values_and_jsonl_rows() {
    let fixtures = std::env::temp_dir().join(format!("atomic-drift-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&fixtures);
    std::fs::create_dir_all(&fixtures).expect("fixtures");
    let case = |id: &str, input: Value, prepared: Option<Value>| eval::Case {
        id: id.into(),
        input,
        expect: vec!["true".into()],
        case: Value::Null,
        prepared,
        origin: "external".into(),
    };
    let write = |name: &str, text: &str| std::fs::write(fixtures.join(name), text).expect("write");
    write("one.input.json", r#"{"text": "a"}"#);
    write("two.input.json", r#"{"text": "b"}"#);
    write("two.prepared.json", r#"{"total": 3}"#);
    write(
        "cases.outputs.jsonl",
        "{\"ticket\": \"A\"}\n{\"ticket\": \"B\"}\n",
    );
    let cases = vec![
        case("one.yaml", json!({"text": "a"}), None),
        case("two.yaml", json!({"text": "b"}), Some(json!({"total": 3}))),
        case("cases.jsonl:1", json!({"text": "c"}), None),
        case("cases.jsonl:2", json!({"text": "d"}), None),
    ];
    assert!(
        drift(&fixtures, &cases).is_empty(),
        "{:?}",
        drift(&fixtures, &cases)
    );
    // Each drift, one at a time.
    let mut input = cases.clone();
    input[0].input = json!({"text": "changed"});
    assert_eq!(drift(&fixtures, &input).len(), 1);
    let mut prepared = cases.clone();
    prepared[1].prepared = Some(json!({"total": 4}));
    assert_eq!(drift(&fixtures, &prepared).len(), 1);
    let mut unprepared = cases.clone();
    unprepared[1].prepared = None;
    assert_eq!(drift(&fixtures, &unprepared).len(), 1);
    let mut extra = cases.clone();
    extra.push(case("cases.jsonl:3", json!({"text": "e"}), None));
    assert!(drift(&fixtures, &extra)[0].contains("3 cases, and 2 golden outputs"));
    let _ = std::fs::remove_dir_all(&fixtures);
}
