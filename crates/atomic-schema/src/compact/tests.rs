// SPDX-License-Identifier: Apache-2.0
use super::*;
use std::path::{Path, PathBuf};

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn yaml(text: &str) -> Value {
    crate::yaml::load(Path::new("compact.yaml"), text)
        .expect("yaml")
        .value
}

fn expanded(file: &str) -> Value {
    let path = repository()
        .join("examples/09-compact-and-variants/expanded")
        .join(file);
    yaml(&std::fs::read_to_string(path).expect("expanded"))
}

/// The compact forms of example 09's contracts compile to exactly the long
/// forms the example writes out by hand.
#[test]
fn a_compact_schema_compiles_to_its_long_form() {
    let input = yaml(
        "subject: string, the ticket's subject line\n\
         body: string, the customer's message\n\
         plan?: string, the customer's plan when known\n",
    );
    assert!(is_compact(&input));
    assert_eq!(
        expand(&input),
        Ok(expanded("label-ticket.input.schema.yaml"))
    );
    let output = yaml(
        "queue(enum, the team that should handle it): [billing, bugs, account, other]\n\
         urgent: boolean, true when the customer is blocked from working\n\
         reason: string, one sentence that explains the labels\n",
    );
    assert_eq!(
        expand(&output),
        Ok(expanded("label-ticket.output.schema.yaml"))
    );
}

#[test]
fn lists_nesting_wildcards_and_any() {
    let schema = yaml(
        "verdict(enum): [approve, request_changes, reject]\n\
         summary: string, at most 600 characters\n\
         findings(array, what was found):\n  file: string\n  line?: integer\n  severity(enum): [blocker, major, minor, nit]\n\
         tags(array): string\n\
         owner(object):\n  team: string\n  (*): string\n\
         extra?: any\n",
    );
    let out = expand(&schema).expect("expands");
    assert_eq!(
        out["required"],
        json!(["verdict", "summary", "findings", "tags", "owner"])
    );
    assert_eq!(
        out["properties"]["findings"]["description"],
        "what was found"
    );
    assert_eq!(
        out["properties"]["findings"]["items"]["required"],
        json!(["file", "severity"])
    );
    assert_eq!(
        out["properties"]["findings"]["items"]["additionalProperties"],
        false
    );
    assert_eq!(
        out["properties"]["tags"],
        json!({ "type": "array", "items": { "type": "string" } })
    );
    assert_eq!(
        out["properties"]["owner"]["additionalProperties"],
        json!({ "type": "string" })
    );
    assert_eq!(out["properties"]["extra"], json!({}));
    assert_eq!(
        out["properties"]["summary"]["description"],
        "at most 600 characters"
    );
    // An optional field may be omitted; it is not also nullable.
    assert_eq!(
        out["properties"]["findings"]["items"]["properties"]["line"],
        json!({ "type": "integer" })
    );
}

#[test]
fn a_constraint_is_refused_with_a_pointer_to_full_json_schema() {
    for (text, at, says) in [
        (
            "name: 'string(minLength 3)'\n",
            "/name",
            "need full JSON Schema",
        ),
        ("when: date-time\n", "/when", "unknown type `date-time`"),
        (
            "items(array):\n  sku: text\n",
            "/items/sku",
            "unknown type `text`",
        ),
        ("status(enum): open\n", "/status", "non-empty list"),
        ("owner:\n  team: string\n", "/owner", "name(object):"),
        ("lines(table): string\n", "/lines", "unknown kind"),
        ("9lives: string\n", "/", "cannot read the field name"),
    ] {
        let error = expand(&yaml(text)).expect_err(text);
        assert_eq!(error.path, at, "{text}");
        assert!(error.message.contains(says), "{text}: {error}");
    }
}

#[test]
fn full_json_schema_is_never_read_as_compact() {
    for text in [
        "type: object\nproperties: {}\n",
        "enum: [a, b]\n",
        "$ref: other.schema.yaml\n",
        "anyOf:\n  - type: string\n  - type: integer\n",
        "const: x\n",
        "items:\n  type: string\n",
    ] {
        assert!(!is_compact(&yaml(text)), "{text}");
    }
    assert!(!is_compact(&json!({})));
    assert!(!is_compact(&json!(true)));
    assert!(is_compact(&yaml("name: string\n")));
}

// A full schema need not say `type`: constraints and annotations alone are
// valid JSON Schema, and load unchanged. Where such a keyword is also a
// natural field name, a compact field value keeps it a field.
#[test]
fn constraint_and_annotation_schemas_without_a_type_load_unchanged() {
    for text in [
        "minLength: 3\n",
        "minimum: 0\n",
        "pattern: '^x'\n",
        "format: date-time\n",
        "description: A city name\n",
        "title: Forecast\ndescription: string\n",
        "examples: [Lisbon]\n",
        "deprecated: true\n",
        "default: 3\n",
        "multipleOf: 5\nexclusiveMaximum: 100\n",
    ] {
        let schema = yaml(text);
        assert!(!is_compact(&schema), "{text}");
        assert_eq!(to_json_schema(&schema).expect(text), schema, "{text}");
    }
    for text in [
        "title: string\ndescription: string, what happened\n",
        "format(enum): [json, text]\n",
        "default?: integer\n",
    ] {
        assert!(is_compact(&yaml(text)), "{text}");
    }
    let out = to_json_schema(&yaml("title: string\ndescription: string, what happened\n"))
        .expect("compact");
    assert_eq!(
        out["properties"]["description"]["description"],
        "what happened"
    );
    assert_eq!(out["required"], json!(["title", "description"]));
}

// A compact field beside a structural keyword would be an ignored unknown
// keyword in JSON Schema, so the mixture is refused rather than misread.
#[test]
fn a_root_mixing_compact_fields_and_structural_keywords_is_refused() {
    for (text, at) in [
        ("type: string\nmessage: string\n", "/message"),
        ("id: string\nconst: string\n", "/id"),
        ("enum: [a, b]\nnote?: string\n", "/note?"),
        (
            "items: { type: string }\nlines(array): string\n",
            "/lines(array)",
        ),
    ] {
        let error = to_json_schema(&yaml(text)).expect_err(text);
        assert_eq!(error.path, at, "{text}");
        assert!(
            error.message.contains("reads as a compact field"),
            "{text}: {error}"
        );
    }
    // Full schemas keep unknown keywords that are not field-shaped, and
    // extension keywords.
    for text in [
        "type: object\nproperties: {}\nx-order: string\n",
        "type: string\nnullable: true\n",
        "type: string\ntitle: string\n",
    ] {
        let schema = yaml(text);
        assert_eq!(to_json_schema(&schema).expect(text), schema, "{text}");
    }
}
