// SPDX-License-Identifier: Apache-2.0

//! The v0.1 examples (01 to 05) under atomic's own loader: each one passes
//! `atomic check`, every golden fixture fits its atom's full contracts and
//! passes the atom's validators, every model atom renders its prompt for its
//! fixture input, and every exec atom, run for real, turns its fixture input
//! into exactly its fixture output.

use atomic_core::render::{self, RunInput};
use atomic_core::{atom, collections, compile, project, runtime};
use atomic_schema::registry::Registry;
use serde_json::{Value, json};
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
        for source in &files["atoms"] {
            let atom = atom::load(&source.path, &mut registry).expect("atom");
            let compiled = compile::compile(&atom, &registry).expect("compiles");
            let stem = source
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".atom.astro"))
                .expect("atom file name");
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
                            Arc::new(atomic_exec::LocalBackend),
                            None,
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
