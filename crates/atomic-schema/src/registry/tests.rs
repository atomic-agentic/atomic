// SPDX-License-Identifier: Apache-2.0

use super::*;

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
use serde_json::json;

fn dir(files: &[(&str, &str)]) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "atomic-schemas-{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).expect("mkdir");
    for (name, text) in files {
        std::fs::write(d.join(name), text).expect("write");
    }
    std::fs::canonicalize(d).expect("canonical")
}

const TASK_YAML: &str = "\
type: object
additionalProperties: false
required: [title, priority]
properties:
  title: {type: string, minLength: 1}
  priority: {enum: [low, normal, high]}
  done: {type: boolean}
";

const TASK_JSON: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["title", "priority"],
  "properties": {
    "title": {"type": "string", "minLength": 1},
    "priority": {"enum": ["low", "normal", "high"]},
    "done": {"type": "boolean"}
  }
}"#;

#[test]
fn a_yaml_schema_loads_to_the_same_contract_as_its_json_equivalent() {
    let (y, j) = (
        dir(&[("task.schema.yaml", TASK_YAML)]),
        dir(&[("task.schema.json", TASK_JSON)]),
    );
    let mut r = Registry::new();
    let yid = r.load_file(&y.join("task.schema.yaml")).expect("yaml");
    let jid = r.load_file(&j.join("task.schema.json")).expect("json");
    assert_eq!(
        r.get(&yid).expect("y").schema,
        r.get(&jid).expect("j").schema
    );
    let (vy, vj) = (
        r.validator(&yid).expect("vy"),
        r.validator(&jid).expect("vj"),
    );
    for instance in [
        json!({"title": "x", "priority": "high"}),
        json!({"title": "", "priority": "high"}),
        json!({"title": "x", "priority": "urgent"}),
        json!({"title": "x", "priority": "low", "extra": 1}),
        json!({"title": "x", "priority": "low", "done": "yes"}),
    ] {
        assert_eq!(vy.is_valid(&instance), vj.is_valid(&instance), "{instance}");
    }
    assert!(vy.is_valid(&json!({"title": "x", "priority": "normal", "done": true})));
}

#[test]
fn two_schemas_with_one_id_fail() {
    let d = dir(&[
        (
            "a.schema.yaml",
            "$id: https://example.test/task\ntype: object\n",
        ),
        (
            "b.schema.yaml",
            "$id: https://example.test/task\ntype: string\n",
        ),
    ]);
    let mut r = Registry::new();
    r.load_file(&d.join("a.schema.yaml")).expect("first");
    match r.load_file(&d.join("b.schema.yaml")) {
        Err(SchemaError::DuplicateId { id, .. }) => assert_eq!(id, "https://example.test/task"),
        other => panic!("expected DuplicateId, got {other:?}"),
    }
}

#[test]
fn one_schema_written_as_both_yaml_and_json_fails() {
    let d = dir(&[
        ("task.schema.yaml", TASK_YAML),
        ("task.schema.json", TASK_JSON),
    ]);
    for f in ["task.schema.yaml", "task.schema.json"] {
        assert!(
            matches!(
                Registry::new().load_file(&d.join(f)),
                Err(SchemaError::BothForms { .. })
            ),
            "{f}"
        );
    }
    let atom = dir(&[
        ("review.atom.astro", ""),
        ("review.input.schema.yaml", TASK_YAML),
        ("review.input.schema.json", TASK_JSON),
        ("review.output.schema.json", TASK_JSON),
    ]);
    assert!(matches!(
        sidecar(&atom.join("review.atom.astro"), "input"),
        Err(SchemaError::BothForms { .. })
    ));
    assert_eq!(
        sidecar(&atom.join("review.atom.astro"), "output").expect("output"),
        Some(atom.join("review.output.schema.json"))
    );
    assert_eq!(
        sidecar(&atom.join("review.atom.astro"), "config").expect("none"),
        None
    );
}

#[test]
fn references_resolve_by_id_and_by_file_and_never_by_network() {
    let d = dir(&[
        (
            "task.schema.yaml",
            "$id: https://example.test/task\ntype: object\nrequired: [title]\nproperties:\n  title: {type: string}\n",
        ),
        ("priority.schema.json", r#"{"enum": ["low", "high"]}"#),
        (
            "plan.schema.yaml",
            "type: object\nproperties:\n  task: {$ref: \"https://example.test/task\"}\n  priority: {$ref: \"priority.schema.json\"}\n",
        ),
        (
            "remote.schema.yaml",
            "properties:\n  x: {$ref: \"https://schemas.invalid/never.json\"}\n",
        ),
    ]);
    let mut r = Registry::new();
    for f in [
        "task.schema.yaml",
        "priority.schema.json",
        "plan.schema.yaml",
    ] {
        r.load_file(&d.join(f)).expect(f);
    }
    r.check().expect("every reference resolves");
    let plan = r
        .validator(&format!("file://{}", d.join("plan.schema.yaml").display()))
        .expect("plan");
    assert!(plan.is_valid(&json!({"task": {"title": "t"}, "priority": "low"})));
    assert!(
        !plan.is_valid(&json!({"task": {}, "priority": "low"})),
        "the $id reference applies"
    );
    assert!(
        !plan.is_valid(&json!({"task": {"title": "t"}, "priority": "mid"})),
        "the file reference applies"
    );

    r.load_file(&d.join("remote.schema.yaml"))
        .expect("registers; the reference is checked later");
    let e = r.check().expect_err("remote reference");
    assert!(e.to_string().contains("never fetched"), "{e}");
}

#[test]
fn a_local_retrieval_uri_resolves_a_schema_with_a_different_declared_id() {
    let root = dir(&[
        (
            "child.schema.json",
            r#"{"$id":"https://atomic.example/child","type":"string","maxLength":3}"#,
        ),
        (
            "parent.schema.json",
            r#"{"type":"object","properties":{"name":{"$ref":"child.schema.json"}}}"#,
        ),
    ]);
    let mut registry = Registry::new();
    let parent = registry
        .load_file(&root.join("parent.schema.json"))
        .expect("parent");
    assert!(
        registry.get("https://atomic.example/child").is_some(),
        "dependency loaded offline"
    );
    let validator = registry.validator(&parent).expect("retrieval alias");
    assert!(validator.is_valid(&json!({"name":"abc"})));
    assert!(!validator.is_valid(&json!({"name":"too long"})));
}

#[test]
fn nested_identifiers_and_anchors_resolve_using_schema_scopes() {
    let root = dir(&[(
        "root.schema.json",
        r##"{
      "$id":"https://atomic.example/root",
      "$defs":{"child":{"$id":"child","type":"object","properties":{"value":{"$ref":"#text"}},"$defs":{"text":{"$anchor":"text","type":"string","maxLength":3}}}},
      "$ref":"child"
    }"##,
    )]);
    let mut registry = Registry::new();
    let id = registry
        .load_file(&root.join("root.schema.json"))
        .expect("schema");
    let expanded = registry.inline(&id).expect("scopes and anchors");
    let validator = jsonschema::validator_for(&expanded).expect("validator");
    assert!(validator.is_valid(&json!({"value":"abc"})));
    assert!(!validator.is_valid(&json!({"value":"too long"})));
}

#[test]
fn local_schema_uris_preserve_spaces_without_fetching_network_resources() {
    let root = dir(&[
        (
            "child schema.schema.json",
            r#"{"type":"integer","minimum":1}"#,
        ),
        (
            "parent.schema.json",
            r#"{"$ref":"child%20schema.schema.json"}"#,
        ),
    ]);
    let mut registry = Registry::new();
    let id = registry
        .load_file(&root.join("parent.schema.json"))
        .expect("local reference");
    assert!(
        !registry
            .validator(&id)
            .expect("validator")
            .is_valid(&json!(0))
    );
    assert!(
        registry
            .validator(&id)
            .expect("validator")
            .is_valid(&json!(1))
    );
}

#[test]
fn reference_annotations_preserve_strict_output_shapes_and_full_constraints() {
    let root = dir(&[]);
    let mut registry = Registry::new();
    let schema = json!({
        "$defs":{"answer":{"type":"object","additionalProperties":false,"required":["text"],"properties":{"text":{"$ref":"#/$defs/text","description":"Nested answer","maxLength":3}}},"text":{"type":"string","maxLength":6}},
        "$ref":"#/$defs/answer", "description":"A short answer"
    });
    let inline = registry
        .inline_value(&schema, &root.join("answer.schema.json"))
        .expect("inline");
    assert_eq!(inline["description"], "A short answer");
    assert!(inline.get("allOf").is_none());
    assert!(inline["properties"]["text"].get("allOf").is_none());
    crate::lower::lower(&inline, &crate::lower::OPENAI_STRICT).expect("strict output");
    let validator = jsonschema::validator_for(&inline).expect("full schema");
    assert!(validator.is_valid(&json!({"text":"yes"})));
    assert!(!validator.is_valid(&json!({"text":"too long"})));
}

#[test]
fn known_resource_index_is_reused_across_many_references() {
    let root = dir(&[]);
    let mut registry = Registry::new();
    for n in 0..64 {
        registry
            .register(
                root.join(format!("s{n}.schema.json")),
                json!({"$id":format!("urn:atomic:test:{n}"),"type":"string"}),
            )
            .expect("register");
    }
    let known = registry.known();
    let indexed = registry.indexed_documents;
    let references: Vec<_> = (0..256)
        .map(|n| json!({"$ref":file_uri(&root.join(format!("s{}.schema.json", n % 64)))}))
        .collect();
    registry
        .inline_value(&json!({"anyOf":references}), &root.join("many.schema.json"))
        .expect("inline");
    assert_eq!(
        registry.indexed_documents, indexed,
        "references must not rebuild document indexes"
    );
    assert!(
        Arc::ptr_eq(&known, &registry.known()),
        "same immutable resource map"
    );
}

#[test]
fn a_reference_extended_with_properties_lowers_without_all_of() {
    let root = dir(&[]);
    let mut registry = Registry::new();
    let schema = json!({
        "$defs":{"base":{"type":"object","required":["id"],"properties":{"id":{"type":"string"}}}},
        "$ref":"#/$defs/base",
        "properties":{"note":{"type":"string","maxLength":4},"id":{"description":"The ticket id","maxLength":8}},
        "required":["note"],
        "additionalProperties":false
    });
    let inline = registry
        .inline_value(&schema, &root.join("extended.schema.json"))
        .expect("inline");
    assert!(inline.get("allOf").is_none(), "{inline}");
    assert_eq!(inline["required"], json!(["id", "note"]));
    crate::lower::lower(&inline, &crate::lower::OPENAI_STRICT).expect("strict output");
    let validator = jsonschema::validator_for(&inline).expect("full schema");
    assert!(validator.is_valid(&json!({"id":"A-1","note":"ok"})));
    assert!(
        !validator.is_valid(&json!({"id":"A-1"})),
        "the extension's required holds"
    );
    assert!(!validator.is_valid(&json!({"id":"A-1","note":"too long"})));
    assert!(!validator.is_valid(&json!({"id":"A-123456789","note":"ok"})));
    assert!(!validator.is_valid(&json!({"id":"A-1","note":"ok","other":1})));
}

#[test]
fn a_property_a_closed_reference_forbids_is_an_error() {
    let root = dir(&[]);
    let mut registry = Registry::new();
    let schema = json!({
        "$defs":{"base":{"type":"object","additionalProperties":false,"properties":{"id":{"type":"string"}}}},
        "$ref":"#/$defs/base",
        "properties":{"note":{"type":"string"}}
    });
    let error = registry
        .inline_value(&schema, &root.join("closed.schema.json"))
        .expect_err("refused extension")
        .to_string();
    assert!(error.contains("additionalProperties: false"), "{error}");
}

#[test]
fn references_that_double_at_each_level_stop_at_the_expansion_budget() {
    let root = dir(&[]);
    let mut registry = Registry::new();
    let mut defs = serde_json::Map::new();
    defs.insert("d0".into(), json!({"type":"string"}));
    // 25 levels stay inside the depth limit; 2^25 subschemas do not fit the budget.
    for level in 1..25 {
        let child = format!("#/$defs/d{}", level - 1);
        defs.insert(
            format!("d{level}"),
            json!({"type":"object","properties":{"a":{"$ref":child},"b":{"$ref":child}}}),
        );
    }
    let schema = json!({"$defs": defs, "$ref": "#/$defs/d24"});
    let error = registry
        .inline_value(&schema, &root.join("doubling.schema.json"))
        .expect_err("budget")
        .to_string();
    assert!(error.contains("subschemas"), "{error}");
}
