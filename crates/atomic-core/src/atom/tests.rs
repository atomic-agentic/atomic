// SPDX-License-Identifier: Apache-2.0
use super::*;
use serde_json::json;

pub(crate) fn project(body: &str, config: &str, component: Option<&str>) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "atomic-astro-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let imports = if component.is_some() {
        "import Style from './Style.astro';"
    } else {
        ""
    };
    let source = format!(
        "---\nimport type {{ Props, Input, Output }} from './.atomic/types/test';\nimport type {{ AtomConfig }} from './.atomic/types/authoring';\n{imports}\nconst {{ input }}: Props = Astro.props;\nexport const config: AtomConfig<Input, Output> = {config};\n---\n{body}"
    );
    std::fs::write(dir.join("test.atom.astro"), source).expect("source");
    for (role, schema) in [
        (
            "input",
            json!({"type":"object","required":["text"],"additionalProperties":false,"properties":{"text":{"type":"string","minLength":1}}}),
        ),
        (
            "output",
            json!({"type":"object","required":["ticket"],"additionalProperties":false,"properties":{"ticket":{"type":"string","maxLength":6}}}),
        ),
    ] {
        std::fs::write(
            dir.join(format!("test.{role}.schema.json")),
            schema.to_string(),
        )
        .expect("schema");
    }
    if let Some(source) = component {
        std::fs::write(dir.join("Style.astro"), source).expect("component");
    }
    dir
}

pub(crate) fn checked(body: &str) -> (Atom, Registry) {
    let dir = project(body, "{}", None);
    let mut registry = Registry::new();
    let atom = load(&dir.join("test.atom.astro"), &mut registry).expect("loads Astro");
    (atom, registry)
}

#[test]
fn conventions_and_full_schema_sidecars_load() {
    let (atom, _) = checked("Note: {input.text}");
    assert_eq!(
        (atom.name.as_str(), atom.kind.as_str(), atom.version),
        ("test", "lite", 1)
    );
    assert!(atom.input.ends_with("test.input.schema.json"));
    assert!(atom.declarations.to_string().contains("Props"));
}

#[test]
fn markdown_and_mdx_are_explicitly_rejected() {
    for file in ["test.atom.md", "test.atom.mdx"] {
        let error = load(Path::new(file), &mut Registry::new()).expect_err("legacy");
        assert!(error.message.contains("replaced"));
    }
}

#[test]
fn misspelled_fields_in_templates_and_contracts_fail_before_dispatch() {
    for (body, config) in [
        ("{input.txt}", "{}"),
        (
            "{input.text}",
            "{validate: [({output}) => output.tickett.length > 0]}",
        ),
    ] {
        let root = project(body, config, None);
        let error =
            load(&root.join("test.atom.astro"), &mut Registry::new()).expect_err("type error");
        assert!(
            (error.message.contains("TS2339") || error.message.contains("TS2551")),
            "{error}"
        );
    }
}

#[test]
fn source_cannot_execute_and_cel_rules_are_rejected() {
    let root = project("{input.text}", "{validate: ['output.ticket != x']}", None);
    assert!(
        load(&root.join("test.atom.astro"), &mut Registry::new())
            .expect_err("CEL")
            .message
            .contains("callbacks")
    );
    let file = root.join("test.atom.astro");
    std::fs::write(&file, "---\nDeno.exit(0);\n---\nNever run").expect("source");
    let error = load(&file, &mut Registry::new()).expect_err("execution");
    assert!(error.message.contains("never executed"), "{error}");
    // The diagnostic points at the statement, not at line 1.
    assert_eq!(error.line, 2, "{error}");
    for (reach, denied) in [
        ("{input.constructor}", "constructor"),
        ("{input.text.__proto__}", "__proto__"),
    ] {
        let root = project(reach, "{}", None);
        let error =
            load(&root.join("test.atom.astro"), &mut Registry::new()).expect_err("denied property");
        assert!(error.message.contains(denied), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn unknown_fields_and_unavailable_permissions_fail_closed() {
    for config in [
        "{colour:'blue'}",
        "{sandbox:'anything'}",
        "{network:{allow:['example.com']}}",
        "{tools:['shell']}",
        "{skills:['future']}",
    ] {
        let root = project("{input.text}", config, None);
        load(&root.join("test.atom.astro"), &mut Registry::new()).expect_err(config);
    }
}

#[test]
fn dangling_and_ambiguous_schemas_fail() {
    let root = project("{input.text}", "{input:{$ref:'missing.schema.json'}}", None);
    assert!(
        load(&root.join("test.atom.astro"), &mut Registry::new())
            .expect_err("dangling")
            .message
            .contains("dangling")
    );
    let root = project("{input.text}", "{}", None);
    std::fs::write(root.join("test.input.schema.yaml"), "type: object").expect("ambiguous");
    load(&root.join("test.atom.astro"), &mut Registry::new()).expect_err("both forms");
}

#[test]
fn unlowerable_full_contracts_fail_before_dispatch() {
    let root = project("{input.text}", "{}", None);
    std::fs::write(root.join("test.output.schema.json"), json!({"type":"object","properties":{"tags":{"type":"object","additionalProperties":{"type":"string"}}}}).to_string()).expect("schema");
    let error =
        load(&root.join("test.atom.astro"), &mut Registry::new()).expect_err("cannot lower");
    assert!(error.message.contains("cannot be lowered"), "{error}");
}

#[test]
fn source_cannot_suppress_or_replace_the_mandatory_type_gate() {
    for source in [
        "---\n// @ts-nocheck\n---\ntext",
        "---\ntype Bypass = any;\n---\ntext",
    ] {
        let root = project("text", "{}", None);
        std::fs::write(root.join("test.atom.astro"), source).expect("source");
        let error =
            load(&root.join("test.atom.astro"), &mut Registry::new()).expect_err("type gate");
        assert!(
            error.message.contains("suppression") || error.message.contains("explicit any"),
            "{error}"
        );
    }
}

#[test]
fn an_explicit_file_reference_preserves_its_schema_fragment() {
    let root = project(
        "{input.text}",
        "{output:{$ref:'./contracts.schema.json#/$defs/Ticket'}}",
        None,
    );
    std::fs::write(root.join("contracts.schema.json"),json!({"$defs":{"Ticket":{"type":"object","additionalProperties":false,"required":["ticket"],"properties":{"ticket":{"type":"string","maxLength":3}}}}}).to_string()).expect("schema");
    let atom =
        load(&root.join("test.atom.astro"), &mut Registry::new()).expect("fragment contract");
    let contract = &atom.program.contracts["Output"];
    assert_eq!(contract["properties"]["ticket"]["maxLength"], 3);
    assert!(
        !jsonschema::validator_for(contract)
            .expect("validator")
            .is_valid(&json!({"ticket":"too long"}))
    );
}
