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

/// A schema file that holds only definitions, one per role.
const COMMON: &str = "$defs:\n  Request:\n    type: object\n    additionalProperties: false\n    required: [text]\n    properties:\n      text: {type: string, minLength: 1}\n  Answer:\n    type: object\n    additionalProperties: false\n    required: [category]\n    properties:\n      category: {enum: [bug, feature]}\n";

fn with_common(input: &str, output: &str) -> (PathBuf, Registry, Result<Atom, AtomError>) {
    // Explicit references override the sidecars `project` writes.
    let root = project(
        "{input.text}",
        &format!(
            "{{input:{{$ref:'./common.schema.yaml#/$defs/{input}'}},output:{{$ref:'./common.schema.yaml#/$defs/{output}'}}}}"
        ),
        None,
    );
    std::fs::write(root.join("common.schema.yaml"), COMMON).expect("schema");
    let file = std::fs::canonicalize(root.join("test.atom.astro")).expect("atom");
    let mut registry = Registry::new();
    let result = load(&file, &mut registry);
    (file, registry, result)
}

#[test]
fn a_reference_to_a_place_in_a_file_keeps_that_exact_target() {
    let (_, mut registry, result) = with_common("Request", "Answer");
    let a = result.expect("loads");

    // Validation uses Request itself: its required field and its minLength.
    let input = registry.validator(&a.input).expect("input validator");
    assert!(input.is_valid(&serde_json::json!({"text": "x"})));
    assert!(
        !input.is_valid(&serde_json::json!({"text": ""})),
        "minLength kept"
    );
    assert!(!input.is_valid(&serde_json::json!({})), "required kept");
    let output = registry.validator(&a.output).expect("output validator");
    assert!(output.is_valid(&serde_json::json!({"category": "bug"})));
    assert!(!output.is_valid(&serde_json::json!({"category": "spam"})));

    // Lowering and workflow mapping inline the schema from where it was
    // registered, and reach Answer, an object root, not the file's bare $defs.
    let base = |registry: &Registry, id: &str| {
        let entry = registry.get(id).expect("registered");
        (
            entry.schema.clone(),
            entry.file.parent().expect("dir").to_owned(),
        )
    };
    for id in [&a.input, &a.output] {
        let (schema, dir) = base(&registry, id);
        let inlined = crate::workflow::inline(&schema, &dir, &mut registry, 0).expect("inlines");
        assert_eq!(inlined["type"], "object", "{id}: {inlined}");
        assert!(inlined.get("$defs").is_none(), "{id}: {inlined}");
    }
    let (answer, dir) = base(&registry, &a.output);
    let answer = crate::workflow::inline(&answer, &dir, &mut registry, 0).expect("inlines");
    let lowered =
        atomic_schema::lower::lower(&answer, &atomic_schema::lower::OPENAI_STRICT).expect("lowers");
    assert_eq!(lowered.schema["required"], serde_json::json!(["category"]));
}

#[test]
fn a_reference_to_a_missing_place_in_a_file_fails_the_load() {
    for (role, other) in [("input", "output"), ("output", "input")] {
        let other_def = if other == "input" {
            "Request"
        } else {
            "Answer"
        };
        let (input, output) = if role == "input" {
            ("Nope", other_def)
        } else {
            (other_def, "Nope")
        };
        let (file, _, result) = with_common(input, output);
        let e = result.expect_err("missing fragment");
        assert_eq!((e.file, e.path), (file, format!("/{role}")));
        assert!(e.message.contains("/$defs/Nope"), "{}", e.message);
    }
}

/// An atom and a component that import the shared `Change` type from
/// `schemas/change.schema.json`, which references `schemas/tier.schema.json`.
/// The input schema is inline, so the shared schemas supply types only.
pub(crate) fn shared_types_project(component_body: &str) -> PathBuf {
    let dir = project("", "{}", None);
    std::fs::write(
        dir.join("test.atom.astro"),
        "---\nimport type { Props, Input, Output } from './.atomic/types/test';\nimport type { AtomConfig } from './.atomic/types/authoring';\nimport type { Change } from './.atomic/types/schemas/change';\nimport Summary from './Summary.astro';\ntype Shown = Pick<Change, 'id'>;\nconst { input }: Props = Astro.props;\nexport const config: AtomConfig<Input, Output> = {};\n---\n<Summary change={input.change} />\n",
    )
    .expect("atom");
    std::fs::write(
        dir.join("Summary.astro"),
        format!("---\nimport type {{ Change }} from './.atomic/types/schemas/change';\ninterface Props {{ change: Change }}\nconst {{ change }}: Props = Astro.props;\n---\n{component_body}\n"),
    )
    .expect("component");
    let change = json!({"type":"object","additionalProperties":false,"required":["id","tier"],"properties":{"id":{"type":"string"},"tier":{"enum":["tier-1","tier-2"]}}});
    std::fs::write(
        dir.join("test.input.schema.json"),
        json!({"type":"object","additionalProperties":false,"required":["change"],"properties":{"change":change}}).to_string(),
    )
    .expect("input");
    std::fs::create_dir_all(dir.join("schemas")).expect("schemas");
    std::fs::write(
        dir.join("schemas/change.schema.json"),
        json!({"type":"object","additionalProperties":false,"required":["id","tier"],"properties":{"id":{"type":"string","description":"The change ID."},"tier":{"$ref":"tier.schema.json"}}}).to_string(),
    )
    .expect("change");
    std::fs::write(
        dir.join("schemas/tier.schema.json"),
        json!({"enum":["tier-1","tier-2"]}).to_string(),
    )
    .expect("tier");
    std::fs::write(
        dir.join("schemas/unrelated.schema.json"),
        json!({"type":"string"}).to_string(),
    )
    .expect("unrelated");
    dir
}

#[test]
fn an_atom_and_a_component_check_against_a_shared_schema_type() {
    let dir = shared_types_project("Change {change.id} ({change.tier})");
    let atom = load(&dir.join("test.atom.astro"), &mut Registry::new()).expect("loads");
    let types = std::fs::canonicalize(&dir)
        .expect("root")
        .join(".atomic/types");
    let declarations = atom.declarations.as_object().expect("declarations");
    let change = declarations[&types.join("schemas/change.d.ts").display().to_string()]
        .as_str()
        .expect("change module");
    assert!(
        change.contains("import type { Tier } from './tier';"),
        "{change}"
    );
    assert!(
        change.contains("/** The change ID. */ \"id\": string;"),
        "{change}"
    );
    // Referenced schemas come along; unreferenced ones do not.
    assert!(declarations.contains_key(&types.join("schemas/tier.d.ts").display().to_string()));
    assert!(
        !declarations.contains_key(&types.join("schemas/unrelated.d.ts").display().to_string())
    );
}

#[test]
fn a_misspelled_shared_type_field_fails_in_the_component() {
    let dir = shared_types_project("Change {change.idd}");
    let error = load(&dir.join("test.atom.astro"), &mut Registry::new()).expect_err("type error");
    assert!(
        error.message.contains("TS2339") || error.message.contains("TS2551"),
        "{error}"
    );
    assert!(error.file.ends_with("Summary.astro"), "{error:?}");
}

#[test]
fn without_its_schema_file_a_shared_type_import_fails() {
    // Negative control: the generated declaration is what makes the import resolve.
    let dir = shared_types_project("Change {change.id}");
    std::fs::remove_file(dir.join("schemas/change.schema.json")).expect("remove");
    let error = load(&dir.join("test.atom.astro"), &mut Registry::new()).expect_err("unresolved");
    assert!(error.message.contains("TS2307"), "{error}");
}

#[test]
fn editing_an_unrelated_schema_keeps_the_compiled_atom() {
    let dir = shared_types_project("Change {change.id}");
    let hash = || {
        load(&dir.join("test.atom.astro"), &mut Registry::new())
            .expect("loads")
            .program
            .hash()
    };
    let first = hash();
    std::fs::write(
        dir.join("schemas/unrelated.schema.json"),
        json!({"type":"integer"}).to_string(),
    )
    .expect("edit");
    assert_eq!(hash(), first);
    std::fs::write(
        dir.join("schemas/tier.schema.json"),
        json!({"enum":["tier-1","tier-2","tier-3"]}).to_string(),
    )
    .expect("edit");
    assert_ne!(
        hash(),
        first,
        "an imported schema's types are part of the atom"
    );
}

#[test]
fn a_schema_cannot_replace_the_atom_declarations() {
    // `test.schema.json` would generate `.atomic/types/test.d.ts`, the atom's
    // own declarations, which the atom imports its Props from.
    let dir = shared_types_project("Change {change.id}");
    std::fs::write(
        dir.join("test.schema.json"),
        json!({"type":"string"}).to_string(),
    )
    .expect("schema");
    let error = load(&dir.join("test.atom.astro"), &mut Registry::new()).expect_err("collision");
    assert!(error.message.contains("rename the schema file"), "{error}");
}

// A mermaid fence's braces and tags are diagram text: they pass the type
// check and reach the prompt unchanged.
#[test]
fn a_mermaid_fence_checks_and_renders_as_text() {
    let (atom, registry) = checked(
        "Route it as drawn.\n```mermaid\nflowchart TD\n  A[Ticket] --> B{Urgent?}\n  B --> C<page>\n```\nNote: {input.text}",
    );
    let compiled = crate::compile::compile(&atom, &registry).expect("compiles");
    let out = crate::render::render(
        &compiled,
        &crate::render::RunInput {
            run_id: "run-1",
            input: &json!({"text": "hello"}),
            frontmatter: &atom.frontmatter,
            origin: "external",
            origins: None,
            step: None,
        },
    )
    .expect("renders");
    assert!(
        out.developer
            .contains("A[Ticket] --> B{Urgent?}\n  B --> C<page>"),
        "{}",
        out.developer
    );
}

// Loops are `map` over a list and partials are components with declared
// props: a loop over a value that is not a list, and a component given a
// prop it does not declare, both fail the check before any model call.
#[test]
fn a_loop_over_a_non_list_and_an_undeclared_component_prop_fail_the_check() {
    let root = project(
        "{input.text.map((x) => <Fragment>{x}</Fragment>)}",
        "{}",
        None,
    );
    let error = load(&root.join("test.atom.astro"), &mut Registry::new()).expect_err("not a list");
    assert!(error.message.contains("TS2339"), "{error}");
    let component = "---\ninterface Props { strict: boolean }\nconst { strict }: Props = Astro.props;\n---\n{strict ? <Fragment>Follow every rule.</Fragment> : null}\n<slot />\n";
    let root = project(
        "<Style strict={true} tone=\"warm\">Be brief.</Style>",
        "{}",
        Some(component),
    );
    let error =
        load(&root.join("test.atom.astro"), &mut Registry::new()).expect_err("undeclared prop");
    assert!(
        error.message.contains("TS2322") && error.message.contains("tone"),
        "{error}"
    );
    // Declared props and a list loop check, and the loop renders one block
    // per element.
    let root = project(
        "<Style strict={true}>Be brief.</Style>\n{input.text}",
        "{}",
        Some(component),
    );
    load(&root.join("test.atom.astro"), &mut Registry::new()).expect("declared props");
}
