// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::atom::tests::project;

#[test]
fn source_graph_is_immutable_and_component_edits_change_new_compilations() {
    let dir = project("<Style />{input.text}", "{}", Some("First version\n"));
    let mut registry = Registry::new();
    let atom = crate::atom::load(&dir.join("test.atom.astro"), &mut registry).expect("load");
    let first = compile(&atom, &registry).expect("compile");
    assert_eq!(first.hash, compile(&atom, &registry).expect("again").hash);
    std::fs::write(dir.join("Style.astro"), "Second version\n").expect("edit");
    let second =
        crate::atom::load(&dir.join("test.atom.astro"), &mut Registry::new()).expect("reload");
    assert_ne!(first.hash, second.program.hash());
    assert_eq!(first.hash, atom.program.hash());
}

#[test]
fn examples_are_checked_against_complete_contracts() {
    let dir = project(
        "{input.text}",
        "{examples:[{name:'bad',input:{text:'note'},output:{ticket:'TOO-LONG'}}]}",
        None,
    );
    let mut registry = Registry::new();
    let atom = crate::atom::load(&dir.join("test.atom.astro"), &mut registry).expect("types fit");
    let entry = registry.get(&atom.output).expect("entry").clone();
    let mut changed = entry.schema;
    changed["properties"]["ticket"]["maxLength"] = serde_json::json!(100);
    registry.register(entry.file, changed).expect("reload");
    let error = compile(&atom, &registry).expect_err("full maxLength");
    assert!(error.message.contains("example bad output"));
}

#[test]
fn imported_components_cannot_cycle() {
    let dir = project(
        "<Style />",
        "{}",
        Some("---\nimport Again from './Style.astro';\n---\n<Again />"),
    );
    assert!(
        crate::atom::load(&dir.join("test.atom.astro"), &mut Registry::new())
            .expect_err("cycle")
            .message
            .contains("cycle")
    );
}

#[test]
fn schema_constraint_changes_invalidate_the_compiled_version_even_when_types_stay_equal() {
    let dir = project("{input.text}", "{}", None);
    let first =
        crate::atom::load(&dir.join("test.atom.astro"), &mut Registry::new()).expect("first");
    let file = dir.join("test.output.schema.json");
    let mut schema: Value =
        serde_json::from_slice(&std::fs::read(&file).expect("read")).expect("schema");
    schema["properties"]["ticket"]["maxLength"] = serde_json::json!(2);
    std::fs::write(&file, schema.to_string()).expect("edit constraint");
    let second =
        crate::atom::load(&dir.join("test.atom.astro"), &mut Registry::new()).expect("second");
    assert_eq!(
        first.declarations, second.declarations,
        "TypeScript erases maxLength"
    );
    assert_ne!(
        first.program.hash(),
        second.program.hash(),
        "full contract belongs in the fingerprint"
    );
}

#[test]
fn an_output_contract_whose_artifact_references_cannot_be_checked_is_refused() {
    let dir = project("{input.text}", "{}", None);
    let registry = Registry::new();
    let mut atom =
        crate::atom::load(&dir.join("test.atom.astro"), &mut Registry::new()).expect("loads");
    compile(&atom, &registry).expect("the plain contract compiles");
    atom.program.contracts["Output"]["properties"]["files"] = serde_json::json!({
        "type": "array", "contains": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}
    });
    let error = compile(&atom, &registry).expect_err("refused");
    assert_eq!(error.path, "/output");
    assert!(error.message.contains("contains"), "{}", error.message);
    // Followed the way the check follows it, the same reference compiles.
    atom.program.contracts["Output"]["properties"]["files"] = serde_json::json!({
        "type": "array", "items": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}
    });
    compile(&atom, &registry).expect("compiles");
}
