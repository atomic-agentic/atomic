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

#[test]
fn numbers_compare_as_numbers() {
    let output = json!({"n": 2});
    assert_eq!(
        evaluate("output.n >= 1", &[("output", &output)]),
        Ok(json!(true))
    );
}

#[test]
fn json_converts_to_cel_values_by_kind() {
    assert_eq!(cel_value(&json!(2)), cel::Value::Int(2));
    assert_eq!(cel_value(&json!(-7)), cel::Value::Int(-7));
    assert_eq!(cel_value(&json!(i64::MAX)), cel::Value::Int(i64::MAX));
    assert_eq!(cel_value(&json!(u64::MAX)), cel::Value::UInt(u64::MAX));
    assert_eq!(cel_value(&json!(0.5)), cel::Value::Float(0.5));
    assert_eq!(cel_value(&json!(2.0)), cel::Value::Float(2.0));
    assert_eq!(cel_value(&Value::Null), cel::Value::Null);
    let beyond_u64: Value = serde_json::from_str("100000000000000000000").expect("parses");
    assert_eq!(cel_value(&beyond_u64), cel::Value::Float(1e20));
}

#[test]
fn every_kind_of_json_value_evaluates() {
    let output = json!({
        "count": 3,
        "negative": -4,
        "big": u64::MAX,
        "ratio": 0.25,
        "huge": serde_json::from_str::<Value>("100000000000000000000").expect("parses"),
        "none": null,
        "items": [{"tags": ["a", "b"], "meta": {"level": 2}}, {"tags": [], "meta": {"level": 5}}],
    });
    let values = [("output", &output)];
    for source in [
        "output.count + 1 == 4",
        "output.count > 2 && output.count <= 3",
        "output.negative < 0",
        "output.big == 18446744073709551615u",
        "output.big > 0u",
        "output.ratio > 0.2 && output.ratio < 0.3",
        "output.ratio * 4.0 == 1.0",
        "output.huge > 1.0e19",
        "output.none == null",
        "output.items.size() == 2",
        "output.items[0].tags[1] == 'b'",
        "output.items[1].tags.size() == 0",
        "output.items.all(i, i.meta.level >= 2)",
        "output.items.map(i, i.meta.level) == [2, 5]",
    ] {
        assert_eq!(evaluate(source, &values), Ok(json!(true)), "{source}");
    }
    // Values come back out as the same JSON.
    assert_eq!(
        evaluate("output.items", &values),
        Ok(output["items"].clone())
    );
    assert_eq!(evaluate("output.big", &values), Ok(json!(u64::MAX)));
}

/// Why the converter exists: serde_json's `arbitrary_precision` is on in this
/// workspace, and through serde a number reaches CEL as a map. If this starts
/// passing, the feature has gone from the dependency graph; the hand-built
/// converter is still correct and this test can go.
#[test]
fn serde_conversion_turns_numbers_into_maps() {
    let program = cel::Program::compile("output.n >= 1").expect("compiles");
    let mut context = cel::Context::default();
    context
        .add_variable("output", json!({"n": 2}))
        .expect("serializes");
    let error = program
        .execute(&context)
        .expect_err("a map is not a number");
    assert!(error.to_string().contains("map"), "{error}");
}

/// Every CEL evaluation over JSON goes through [`evaluate`]: no other source
/// file in the workspace builds its own CEL context.
#[test]
fn only_this_module_builds_a_cel_context() {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crates directory");
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let allowed = [here.join("expr.rs"), here.join("expr").join("tests.rs")];
    let needles = [
        concat!("cel::", "Context"),
        concat!("add_", "variable"),
        concat!("Program::", "compile"),
    ];
    let mut offenders = Vec::new();
    let mut stack = vec![crates.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("reads") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != "target") {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "rs") && !allowed.contains(&path) {
                let text = std::fs::read_to_string(&path).expect("reads");
                if needles.iter().any(|n| text.contains(n)) {
                    offenders.push(path.display().to_string());
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "evaluate CEL through atomic_schema::expr::evaluate: {offenders:?}"
    );
}
