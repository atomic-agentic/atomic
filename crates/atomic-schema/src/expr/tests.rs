// SPDX-License-Identifier: Apache-2.0

use super::*;
use serde_json::json;

fn input() -> Value {
    json!({
        "type": "object",
        "properties": {
            "risk": {"enum": ["low", "high"]},
            "files": {"type": "array", "items": {"$ref": "#/$defs/file"}},
            "meta": {"type": "object"}
        },
        "$defs": {
            "file": {"type": "object", "properties": {"path": {"type": "string"}, "lines": {"type": "integer"}}}
        }
    })
}

fn check(source: &str) -> Result<(), ExprError> {
    let schema = input();
    let scope = Scope::new().with("input", &schema).with_any("env");
    parse(source)?.check(&scope)
}

#[test]
fn a_field_absent_from_the_schema_fails_with_its_location() {
    let e = check("input.risc == \"high\"").expect_err("misspelt");
    assert_eq!(e.column, 7);
    assert_eq!(
        e.message,
        "`input` has no field `risc` (fields: files, meta, risk)"
    );
}

#[test]
fn declared_paths_pass() {
    for ok in [
        "input.risk == \"high\"",
        "size(input.files) > 0 && input.risk != \"low\"",
        "input.files.all(f, f.path.endsWith(\".rs\") && f.lines < 500)",
        "has(input.risk)",
        "input.meta.anything.below == 1",
        "env.ANY.thing",
        "[1, 2].exists(x, x > 1)",
    ] {
        check(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
}

#[test]
fn loop_variables_are_typed_from_their_list() {
    let e = check("input.files.exists(f, f.pth == \"x\")").expect_err("misspelt in a loop");
    assert!(
        e.message
            .contains("`f` has no field `pth` (fields: lines, path)"),
        "{e}"
    );
    assert_eq!(e.column, 25);
}

#[test]
fn unknown_names_fail() {
    let e = check("inptu.risk == 1").expect_err("unknown root");
    assert_eq!(e.column, 1);
    assert!(e.message.contains("unknown name `inptu`"), "{e}");
}

#[test]
fn a_syntax_error_fails_at_parse() {
    assert!(parse("input.risk ==").is_err());
    assert!(parse("input.risk == \"high\"").is_ok());
}

#[test]
fn an_expression_evaluates_to_json() {
    let input = json!({"order_id": "A-1", "lines": [{"cents": 125, "units": 2}]});
    let steps = json!({"total": {"output": {"total_cents": 250}}});
    let values = [("input", &input), ("steps", &steps)];
    assert_eq!(evaluate("input.order_id", &values), Ok(json!("A-1")));
    assert_eq!(
        evaluate("steps.total.output", &values),
        Ok(json!({"total_cents": 250}))
    );
    assert_eq!(
        evaluate("'Accept ' + input.order_id", &values),
        Ok(json!("Accept A-1"))
    );
    assert_eq!(
        evaluate("input.lines.map(l, l.cents * l.units)", &values),
        Ok(json!([250]))
    );
    assert!(evaluate("input.missing", &values).is_err());
}
