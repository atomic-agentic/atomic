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
        ("review.atom.md", ""),
        ("review.input.schema.yaml", TASK_YAML),
        ("review.input.schema.json", TASK_JSON),
        ("review.output.schema.json", TASK_JSON),
    ]);
    assert!(matches!(
        sidecar(&atom.join("review.atom.md"), "input"),
        Err(SchemaError::BothForms { .. })
    ));
    assert_eq!(
        sidecar(&atom.join("review.atom.md"), "output").expect("output"),
        Some(atom.join("review.output.schema.json"))
    );
    assert_eq!(
        sidecar(&atom.join("review.atom.md"), "config").expect("none"),
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
