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
    normalize(&mut generated, &l);
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

#[test]
fn an_artifact_reference_is_inlined_for_generation() {
    let contract = json!({
        "type": "object",
        "required": ["patch"],
        "properties": {"patch": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}}
    });
    let l = lower(&contract, &OPENAI_STRICT).expect("lowers");
    let patch = &l.schema["properties"]["patch"];
    assert_eq!(patch["type"], json!("string"));
    assert!(
        patch["description"]
            .as_str()
            .is_some_and(|d| d.contains("text/x-diff"))
    );
    assert_eq!(check(&l.schema, &OPENAI_STRICT), Vec::<String>::new());
}

#[test]
fn an_optional_anyof_with_a_null_branch_is_ambiguous() {
    let contract = json!({
        "type": "object",
        "properties": {"owner": {"anyOf": [{"type": "string"}, {"type": "null"}]}}
    });
    let e = lower(&contract, &OPENAI_STRICT).expect_err("ambiguous");
    assert_eq!(e.path, "/properties/owner");
    assert!(e.message.contains("ambiguous"), "{e}");
}

/// Normalize `generated` under `contract`, and check the result against the
/// full contract.
fn normalized(contract: &Value, mut generated: Value) -> Value {
    let l = lower(contract, &OPENAI_STRICT).expect("lowers");
    normalize(&mut generated, &l);
    let validator = jsonschema::validator_for(contract).expect("valid contract");
    let errors: Vec<String> = validator
        .iter_errors(&generated)
        .map(|e| format!("{}: {e}", e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{generated} breaks the contract: {errors:?}"
    );
    generated
}

#[test]
fn optional_fields_in_definitions_normalize_where_they_are_used() {
    let contract = json!({
        "type": "object",
        "required": ["person", "team"],
        "properties": {
            "person": {"$ref": "#/$defs/Person"},
            "team": {"type": "array", "items": {"$ref": "#/$defs/Person"}},
            "lead": {"$ref": "#/$defs/Person"}
        },
        "$defs": {
            "Person": {
                "type": "object", "additionalProperties": false, "required": ["name"],
                "properties": {"name": {"type": "string"}, "age": {"type": "integer"}}
            }
        }
    });
    let out = normalized(
        &contract,
        json!({
            "person": {"name": "Ada", "age": null},
            "team": [{"name": "Bo", "age": null}, {"name": "Cy", "age": 3}],
            "lead": null
        }),
    );
    assert_eq!(
        out,
        json!({
            "person": {"name": "Ada"},
            "team": [{"name": "Bo"}, {"name": "Cy", "age": 3}]
        })
    );
}

/// `payload` is one of two shapes; in the second, `line` may be null as a
/// value, whether it is optional or required there.
fn alternatives(second_line_required: bool, discriminated: bool) -> Value {
    let mut a = json!({
        "type": "object", "additionalProperties": false, "required": ["kind"],
        "properties": {"kind": {"const": "a"}, "line": {"type": "integer"}}
    });
    let mut b = json!({
        "type": "object", "additionalProperties": false, "required": ["kind"],
        "properties": {"kind": {"enum": ["b", "c"]}, "line": {"type": ["integer", "null"]}}
    });
    if second_line_required {
        b["required"] = json!(["kind", "line"]);
    }
    if !discriminated {
        // Both may say either kind: the tag no longer tells them apart.
        a["properties"]["kind"] = json!({"enum": ["a", "b"]});
        b["properties"]["kind"] = json!({"enum": ["a", "b"]});
    }
    json!({
        "type": "object", "required": ["payload"],
        "properties": {"payload": {"anyOf": [a, b]}}
    })
}

#[test]
fn a_null_one_alternative_permits_survives_where_another_lowered_an_optional() {
    for required in [false, true] {
        let contract = alternatives(required, true);
        // The second alternative's null is a value: kept.
        let out = normalized(&contract, json!({"payload": {"kind": "b", "line": null}}));
        assert_eq!(
            out,
            json!({"payload": {"kind": "b", "line": null}}),
            "required: {required}"
        );
        // The first alternative's null means omitted: removed.
        let out = normalized(&contract, json!({"payload": {"kind": "a", "line": null}}));
        assert_eq!(
            out,
            json!({"payload": {"kind": "a"}}),
            "required: {required}"
        );
    }
}

#[test]
fn alternatives_that_disagree_about_a_null_with_nothing_to_tell_them_apart_are_refused() {
    for required in [false, true] {
        let e = lower(&alternatives(required, false), &OPENAI_STRICT).expect_err("refused");
        assert_eq!(e.path, "/properties/payload", "required: {required}");
        assert!(
            e.message.contains("/line") && e.message.contains("tells the alternatives apart"),
            "{e}"
        );
    }
}

#[test]
fn alternatives_that_agree_about_nulls_need_no_tag() {
    let contract = json!({
        "type": "object", "required": ["payload"],
        "properties": {"payload": {"anyOf": [
            {"type": "object", "additionalProperties": false, "required": ["file"],
             "properties": {"file": {"type": "string"}, "line": {"type": "integer"}}},
            {"type": "object", "additionalProperties": false, "required": ["url"],
             "properties": {"url": {"type": "string"}}}
        ]}}
    });
    let out = normalized(
        &contract,
        json!({"payload": {"file": "a.rs", "line": null}}),
    );
    assert_eq!(out, json!({"payload": {"file": "a.rs"}}));
}

#[test]
fn alternatives_given_as_definitions_are_told_apart_by_their_tags() {
    let contract = json!({
        "type": "object", "required": ["events"],
        "properties": {"events": {"type": "array", "items": {"anyOf": [
            {"$ref": "#/$defs/Edit"}, {"$ref": "#/$defs/Note"}
        ]}}},
        "$defs": {
            "Edit": {"type": "object", "additionalProperties": false, "required": ["kind", "file"],
                     "properties": {"kind": {"const": "edit"}, "file": {"type": "string"},
                                    "line": {"type": "integer"}}},
            "Note": {"type": "object", "additionalProperties": false, "required": ["kind", "line"],
                     "properties": {"kind": {"const": "note"}, "line": {"type": ["integer", "null"]}}}
        }
    });
    let out = normalized(
        &contract,
        json!({"events": [
            {"kind": "edit", "file": "a.rs", "line": null},
            {"kind": "note", "line": null}
        ]}),
    );
    assert_eq!(
        out,
        json!({"events": [{"kind": "edit", "file": "a.rs"}, {"kind": "note", "line": null}]})
    );
}

#[test]
fn an_optional_reference_to_a_nullable_definition_keeps_its_null() {
    for note in [
        json!({"type": ["string", "null"]}),
        json!({"anyOf": [{"type": "string"}, {"type": "null"}]}),
        json!({"$ref": "#/$defs/Inner"}),
    ] {
        let contract = json!({
            "type": "object",
            "properties": {"note": {"$ref": "#/$defs/Note"}},
            "$defs": {"Note": note.clone(), "Inner": {"type": ["string", "null"]}}
        });
        match lower(&contract, &OPENAI_STRICT) {
            Ok(l) => {
                assert_eq!(l.warnings, ["/note"], "{note}: null is a value here");
                assert!(l.nulls_added.is_empty(), "{note}: {:?}", l.nulls_added);
                assert_eq!(
                    normalized(&contract, json!({"note": null})),
                    json!({"note": null})
                );
                assert_eq!(
                    normalized(&contract, json!({"note": "x"})),
                    json!({"note": "x"})
                );
            }
            // A null-admitting anyOf behind an optional reference is the
            // ambiguous case: refused, never silently normalized.
            Err(e) => assert!(
                note.get("anyOf").is_some() && e.message.contains("ambiguous"),
                "{note}: {e}"
            ),
        }
    }
}

#[test]
fn nullable_objects_and_optional_constants_survive_generation() {
    let contract = json!({"type":"object","required":["error"],"properties":{
        "error":{"type":["object","null"],"properties":{"message":{"type":"string"}}},
        "flag":{"const":true}
    }});
    let lowered = lower(&contract, &OPENAI_STRICT).expect("lowers");
    let validator = jsonschema::validator_for(&lowered.schema).expect("valid generation schema");
    assert!(validator.is_valid(&json!({"error":null,"flag":null})));
    assert!(validator.is_valid(&json!({"error":{"message":"failed"},"flag":true})));
    assert!(!validator.is_valid(&json!({"error":null,"flag":false})));
    assert_eq!(
        normalized(&contract, json!({"error":null,"flag":null})),
        json!({"error":null})
    );
}

#[test]
fn nullable_objects_are_allowed_only_below_the_generation_root() {
    let contract = json!({"type":["object","null"],"properties":{}});
    assert!(
        lower(&contract, &OPENAI_STRICT)
            .expect_err("nullable generation root")
            .message
            .contains("root")
    );
    assert!(!check(&contract, &OPENAI_STRICT).is_empty());
}

#[test]
fn a_nullable_type_restricted_by_an_enum_can_be_omitted() {
    let contract = json!({"type":"object","properties":{"color":{
        "type":["string","null"],"enum":["red"]
    }}});
    let lowered = lower(&contract, &OPENAI_STRICT).expect("lower optional enum");
    let validator = jsonschema::validator_for(&lowered.schema).expect("valid nullable enum");
    assert!(validator.is_valid(&json!({"color":null})));
    assert!(validator.is_valid(&json!({"color":"red"})));
    assert!(!validator.is_valid(&json!({"color":"blue"})));
    assert_eq!(normalized(&contract, json!({"color":null})), json!({}));
}
