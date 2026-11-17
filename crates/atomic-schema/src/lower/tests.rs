// SPDX-License-Identifier: Apache-2.0

use super::*;
use serde_json::json;

/// A code review's output: a verdict, a summary and findings, where a finding
/// may omit its line.
fn review() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "required": ["verdict", "summary", "findings"],
        "properties": {
            "verdict": {"type": "string", "enum": ["approve", "request_changes", "reject"]},
            "summary": {"type": "string", "minLength": 1, "maxLength": 600},
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["file", "severity", "message"],
                    "properties": {
                        "file": {"type": "string", "minLength": 1},
                        "severity": {"type": "string", "enum": ["blocker", "major", "minor", "nit"]},
                        "message": {"type": "string", "minLength": 1},
                        "line": {"type": "integer", "minimum": 1}
                    }
                }
            }
        }
    })
}

#[test]
fn the_review_schema_lowers_to_a_strict_valid_schema_and_reports_line() {
    let contract = review();
    // The contract itself is not strict-valid: line is optional, and
    // minLength is not supported.
    assert!(!check(&contract, &OPENAI_STRICT).is_empty());

    let l = lower(&contract, &OPENAI_STRICT).expect("lowers");
    assert_eq!(check(&l.schema, &OPENAI_STRICT), Vec::<String>::new());
    assert_eq!(l.nulls_added, ["/findings/*/line"]);
    assert!(l.warnings.is_empty());

    let line = &l.schema["properties"]["findings"]["items"]["properties"]["line"];
    assert_eq!(line["type"], json!(["integer", "null"]));
    assert_eq!(
        line["minimum"],
        json!(1),
        "minimum is supported, so it stays"
    );
    let summary = &l.schema["properties"]["summary"];
    assert!(summary.get("minLength").is_none());
    assert_eq!(
        summary["description"],
        json!("Constraints: maxLength: 600; minLength: 1.")
    );
    assert!(
        l.hints
            .contains(&("/properties/summary".into(), "minLength: 1".into()))
    );
    assert_eq!(
        l.schema["properties"]["findings"]["items"]["required"],
        json!(["file", "line", "message", "severity"])
    );
}

#[test]
fn a_null_for_a_lowered_optional_is_removed_and_a_permitted_null_is_kept() {
    let mut contract = review();
    // `note` may be omitted and may be null; nulls there are values.
    contract["properties"]["findings"]["items"]["properties"]["note"] =
        json!({"type": ["string", "null"]});
    let l = lower(&contract, &OPENAI_STRICT).expect("lowers");
    assert_eq!(l.warnings, ["/findings/*/note"]);

    let mut generated = json!({
        "verdict": "request_changes",
        "summary": "Two problems.",
        "findings": [
            {"file": "a.rs", "severity": "major", "message": "m", "line": null, "note": null},
            {"file": "b.rs", "severity": "nit", "message": "n", "line": 7, "note": "x"}
        ]
    });
    normalize(&mut generated, &l.nulls_added);
    assert_eq!(
        generated["findings"][0].get("line"),
        None,
        "lowered optional: omitted"
    );
    assert_eq!(
        generated["findings"][0]["note"],
        Value::Null,
        "permitted null: kept"
    );
    assert_eq!(generated["findings"][1]["line"], json!(7));

    // The normalized value is valid against the full contract.
    let validator = jsonschema::validator_for(&contract).expect("contract compiles");
    assert!(validator.is_valid(&generated));
}

#[test]
fn schemas_the_profile_cannot_express_are_refused_with_their_path() {
    let cases = [
        (json!({"anyOf": [{"type": "object"}]}), "/", "root"),
        (json!({"type": "array"}), "/", "root"),
        (
            json!({"type": "object", "properties": {"x": {"allOf": [{"type": "string"}]}}}),
            "/properties/x",
            "allOf",
        ),
        (
            json!({"type": "object", "properties": {"m": {"type": "object", "additionalProperties": {"type": "string"}}}}),
            "/properties/m",
            "extra properties",
        ),
        (
            json!({"type": "object", "properties": {"any": true}}),
            "/properties/any",
            "unconstrained",
        ),
    ];
    for (schema, path, needle) in cases {
        let e = lower(&schema, &OPENAI_STRICT).expect_err("refused");
        assert_eq!(e.path, path, "{schema}");
        assert!(e.message.contains(needle), "{schema}: {e}");
    }
}

#[test]
fn an_optional_reference_becomes_a_nullable_anyof() {
    let contract = json!({
        "type": "object",
        "properties": {"owner": {"$ref": "#/$defs/person"}},
        "$defs": {"person": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}
    });
    let l = lower(&contract, &OPENAI_STRICT).expect("lowers");
    assert_eq!(
        l.schema["properties"]["owner"],
        json!({"anyOf": [{"$ref": "#/$defs/person"}, {"type": "null"}]})
    );
    assert_eq!(check(&l.schema, &OPENAI_STRICT), Vec::<String>::new());
    assert_eq!(l.nulls_added, ["/owner"]);
}
