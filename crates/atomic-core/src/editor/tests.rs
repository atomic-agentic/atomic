// SPDX-License-Identifier: Apache-2.0
use super::*;
use atomic_schema::registry::Registry;
use std::path::{Path, PathBuf};

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn yaml(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("read");
    atomic_schema::yaml::load(path, &text).expect("yaml").value
}

fn errors(schema: &Value, value: &Value) -> Vec<String> {
    let validator = Registry::new()
        .validator_for(schema)
        .expect("a valid schema");
    validator
        .iter_errors(value)
        .map(|e| format!("{}: {e}", e.instance_path()))
        .collect()
}

/// The editor schemas agree with the loader: every example manifest the
/// loader accepts fits the manifest schema and every one it refuses does not,
/// and every workflow of the examples the loader runs today fits the
/// workflow schema.
#[test]
fn the_editor_schemas_agree_with_the_loader() {
    let manifest = manifest_schema();
    let mut checked = 0;
    for file in walk(&repository().join("examples")) {
        let name = file.to_string_lossy().into_owned();
        if name.ends_with("atomic.yaml") {
            let fits = errors(&manifest, &yaml(&file)).is_empty();
            assert_eq!(fits, crate::project::load(&file).is_ok(), "{name}");
            checked += 1;
        } else if name.ends_with(".workflow.yaml")
            && ["01-", "02-", "03-", "04-", "05-"]
                .iter()
                .any(|v| name.contains(&format!("examples/{v}")))
        {
            // Names are open here: the atom enum is per project.
            let schema = workflow_schema(&BTreeMap::new(), &[]);
            assert_eq!(
                errors(&schema, &yaml(&file)),
                Vec::<String>::new(),
                "{name}"
            );
            checked += 1;
        }
    }
    assert!(checked >= 12, "{checked}");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn a_project_schema_lists_its_atoms_and_their_fields() {
    let dir = crate::atom::tests::project("{input.text}", "{}", None);
    let atom = crate::atom::load(&dir.join("test.atom.astro"), &mut Registry::new()).expect("atom");
    let atoms: BTreeMap<String, Atom> = [(atom.name.clone(), atom)].into_iter().collect();
    let schema = workflow_schema(&atoms, &["https://example.test/note".to_owned()]);
    let workflow = |step: Value| json!({ "atomic": 1, "steps": { "one": step }, "returns": "steps.one.output" });
    assert!(
        errors(
            &schema,
            &workflow(json!({ "atom": "test", "with": { "text": "input.text" } }))
        )
        .is_empty()
    );
    // A misspelled atom, an unknown input field and an unknown origin fail.
    assert!(!errors(&schema, &workflow(json!({ "atom": "tset" }))).is_empty());
    assert!(
        !errors(
            &schema,
            &workflow(json!({ "atom": "test", "with": { "txt": "input.text" } }))
        )
        .is_empty()
    );
    assert!(
        !errors(
            &schema,
            &workflow(
                json!({ "atom": "test", "with": { "text": "x" }, "origins": { "text": "trusted" } })
            )
        )
        .is_empty()
    );
    assert!(
        !errors(
            &schema,
            &workflow(json!({ "gate": { "title": "'ok?'" }, "atom": "test" }))
        )
        .is_empty()
    );
    assert_eq!(
        schema["$defs"]["step"]["properties"]["atom"]["enum"],
        json!(["test"])
    );
    assert_eq!(
        schema["$defs"]["contract"]["properties"]["$ref"]["anyOf"][0]["enum"],
        json!(["https://example.test/note"])
    );
}

#[test]
fn the_manifest_schema_flags_unknown_keys() {
    let schema = manifest_schema();
    assert!(errors(&schema, &json!({ "atomic": 1, "name": "x" })).is_empty());
    assert!(!errors(&schema, &json!({ "atomic": 1, "colections": {} })).is_empty());
    assert!(!errors(&schema, &json!({ "atomic": 2 })).is_empty());
    assert!(
        !errors(
            &schema,
            &json!({ "atomic": 1, "workspace": { "members": [] } })
        )
        .is_empty()
    );
}
