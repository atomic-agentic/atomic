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
/// generated declarations there.
fn copy(example: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("atomic-example-{example}-{}", std::process::id()));
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
                            Arc::new(atomic_exec::LocalBackend),
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
