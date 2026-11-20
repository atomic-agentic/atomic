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
    let input_schema = json!({
        "type": "object",
        "properties": {
            "order_id": {"type": "string"},
            "lines": {"type": "array", "items": {"type": "object", "properties": {
                "cents": {"type": "integer"}, "units": {"type": "integer"}}}}
        }
    });
    let output_schema =
        json!({"type": "object", "properties": {"total_cents": {"type": "integer"}}});
    let steps = json!({"total": {"output": {"total_cents": 250}}});
    let steps_shape = Shape::fields([(
        "total",
        Shape::fields([("output", Shape::Schema(&output_schema))]),
    )]);
    let input_shape = Shape::Schema(&input_schema);
    let values = [
        ("input", &input, &input_shape),
        ("steps", &steps, &steps_shape),
    ];
    assert_eq!(evaluate("input.order_id", &values), Ok(json!("A-1")));
    assert_eq!(
        evaluate("steps.total.output", &values),
        Ok(json!({"total_cents": 250}))
    );
    assert_eq!(
        evaluate("steps.total.output.total_cents + 1", &values),
        Ok(json!(251))
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

/// `assert_eq!` on the values' debug forms: CEL's `==` holds between an
/// `int` and an equal `double`, and these tests are about which one it is.
macro_rules! assert_same {
    ($a:expr, $b:expr $(, $($message:tt)*)?) => {{
        let (a, b) = ($a, $b);
        // Ties the two types together, as `assert_eq!` would.
        let _ = || a == b;
        assert_eq!(format!("{a:?}"), format!("{b:?}") $(, $($message)*)?)
    }};
}

/// Evaluate `source` with `output` bound to `value` under `schema`.
fn with_schema(source: &str, schema: &Value, value: &Value) -> Result<Value, String> {
    evaluate(source, &[("output", value, &Shape::Schema(schema))])
}

fn number(text: &str) -> Value {
    serde_json::from_str(text).expect("a JSON number")
}

#[test]
fn numbers_compare_as_numbers() {
    let schema = json!({"type": "object", "properties": {"n": {"type": "integer"}}});
    assert_eq!(
        with_schema("output.n >= 1", &schema, &json!({"n": 2})),
        Ok(json!(true))
    );
    // Untyped, a number is a double, which still compares with an int.
    assert_eq!(
        evaluate(
            "output.n >= 1",
            &[("output", &json!({"n": 2}), &Shape::Untyped)]
        ),
        Ok(json!(true))
    );
}

#[test]
fn an_integer_field_is_an_int_however_it_is_spelled() {
    let schema = json!({"type": "object", "properties": {"n": {"type": "integer"}}});
    let shape = Shape::Schema(&schema);
    for spelling in ["1", "1.0", "1e0", "10e-1"] {
        let value = json!({"n": number(spelling)});
        assert_same!(
            cel_value(&value["n"], &Shape::Schema(&schema["properties"]["n"])),
            Ok(cel::Value::Int(1)),
            "{spelling}"
        );
        assert_same!(
            evaluate("output.n + 1 == 2", &[("output", &value, &shape)]),
            Ok(json!(true)),
            "{spelling}"
        );
    }
    let field = json!({"type": "integer"});
    let field = Shape::Schema(&field);
    assert_same!(cel_value(&json!(-7), &field), Ok(cel::Value::Int(-7)));
    assert_same!(
        cel_value(&json!(i64::MAX), &field),
        Ok(cel::Value::Int(i64::MAX))
    );
    // Above i64::MAX, a uint.
    assert_same!(
        cel_value(&json!(u64::MAX), &field),
        Ok(cel::Value::UInt(u64::MAX))
    );
    assert_same!(
        cel_value(&number("9223372036854775808"), &field),
        Ok(cel::Value::UInt(1 << 63))
    );
    // What an int or a uint cannot hold is refused.
    for text in [
        "1.5",
        "18446744073709551616",
        "-9223372036854775809",
        "1e400",
    ] {
        assert!(cel_value(&number(text), &field).is_err(), "{text}");
    }
}

#[test]
fn number_fields_are_doubles_and_untyped_integers_stay_integers() {
    let schema = json!({"type": "object", "properties": {"x": {"type": "number"}}});
    let shape = Shape::Schema(&schema);
    for spelling in ["1", "1.0", "1e0"] {
        let value = json!({"x": number(spelling), "y": number(spelling)});
        assert_same!(
            evaluate("output.x * 2.0 == 2.0", &[("output", &value, &shape)]),
            Ok(json!(true)),
            "{spelling}"
        );
        // `y` is not in the schema: an untyped integer stays an int.
        assert_same!(
            evaluate("output.y + 1 == 2", &[("output", &value, &shape)]),
            Ok(json!(true)),
            "{spelling}"
        );
        assert_same!(
            cel_value(&number(spelling), &Shape::Untyped),
            Ok(cel::Value::Int(1))
        );
    }
    // Passed through untyped, an integer comes back as one.
    let open = json!({"type": "object"});
    assert_eq!(
        serde_json::to_string(
            &with_schema("output", &open, &json!({"count": number("1.0"), "n": 3}))
                .expect("evaluates")
        )
        .expect("serializes"),
        r#"{"count":1,"n":3}"#
    );
    assert_same!(
        cel_value(&number("0.5"), &Shape::Untyped),
        Ok(cel::Value::Float(0.5))
    );
    assert_same!(
        cel_value(&json!(u64::MAX), &Shape::Untyped),
        Ok(cel::Value::UInt(u64::MAX))
    );
    // Past uint, an exact double, or refused.
    assert_same!(
        cel_value(&number("100000000000000000000"), &Shape::Untyped),
        Ok(cel::Value::Float(1e20))
    );
    assert!(cel_value(&number("100000000000000000001"), &Shape::Untyped).is_err());
    // A number field never rounds an integer: one a double cannot hold is
    // refused, with advice.
    let field = json!({"type": "number"});
    let field = Shape::Schema(&field);
    assert_same!(
        cel_value(&number("9007199254740992"), &field),
        Ok(cel::Value::Float(9007199254740992.0))
    );
    let error = cel_value(&number("9007199254740993"), &field).expect_err("not exact");
    assert!(error.contains("type the field `integer`"), "{error}");
    assert_same!(
        cel_value(&number("9007199254740993"), &Shape::Untyped),
        Ok(cel::Value::Int(9007199254740993))
    );
    // A number f64 cannot hold is refused, not turned into infinity or NaN.
    assert!(cel_value(&number("1e400"), &Shape::Untyped).is_err());
    assert!(cel_value(&number("-1e400"), &field).is_err());
}

#[test]
fn integers_through_nullable_alternatives_stay_integers() {
    let schema = json!({"type": "object", "properties": {
        "limits": {"anyOf": [{"type": "object", "properties": {"retries": {"type": "integer"}}}, {"type": "null"}]},
        "listed": {"anyOf": [{"type": "array", "items": {"type": "integer"}}, {"type": "null"}]},
        "maybe": {"anyOf": [{"type": "integer"}, {"type": "null"}]},
        "one": {"oneOf": [{"type": "integer"}, {"type": "null"}]},
        "array_type": {"type": ["integer", "null"]},
        "either": {"type": ["integer", "number"]},
        "branches": {"anyOf": [
            {"type": "object", "properties": {"n": {"type": "integer"}}},
            {"type": "object", "properties": {"n": {"type": "number"}}}
        ]},
        "one_of": {"oneOf": [
            {"type": "object", "properties": {"n": {"type": "number"}}},
            {"type": "string"}
        ]},
        "by_ref": {"$ref": "#/$defs/measured"},
        "by_all": {"allOf": [{"$ref": "#/$defs/measured"}]}
    }, "$defs": {"measured": {"type": "object", "properties": {"n": {"type": "number"}}}}});
    let value = json!({
        "limits": {"retries": 3}, "listed": [1, 2], "maybe": 4, "one": 5, "array_type": 6,
        "either": 7, "branches": {"n": 8}, "one_of": {"n": 9}, "by_ref": {"n": 10}, "by_all": {"n": 11}
    });
    let shape = Shape::Schema(&schema);
    for (source, want) in [
        ("output.limits.retries", "Int(3)"),
        ("output.listed[1]", "Int(2)"),
        ("output.maybe", "Int(4)"),
        ("output.one", "Int(5)"),
        ("output.array_type", "Int(6)"),
        ("output.either", "Float(7.0)"),
        ("output.branches.n", "Float(8.0)"),
        ("output.one_of.n", "Float(9.0)"),
        ("output.by_ref.n", "Float(10.0)"),
        ("output.by_all.n", "Float(11.0)"),
    ] {
        let program = cel::Program::compile(source).expect("compiles");
        let mut context = cel::Context::default();
        context.add_variable_from_value("output", cel_value(&value, &shape).expect("converts"));
        assert_eq!(
            format!("{:?}", program.execute(&context).expect("runs")),
            want,
            "{source}"
        );
    }
    assert_eq!(
        evaluate("output.limits.retries + 1", &[("output", &value, &shape)]),
        Ok(json!(4))
    );
    // They are integer fields, not untyped ones: a fraction there is refused
    // rather than read as a double.
    for (field, fraction) in [
        ("limits", json!({"retries": 1.5})),
        ("listed", json!([1.5])),
        ("maybe", json!(1.5)),
        ("one", json!(1.5)),
        ("array_type", json!(1.5)),
    ] {
        let mut value = value.clone();
        value[field] = fraction;
        assert!(cel_value(&value, &shape).is_err(), "{field}");
    }
}

#[test]
fn deep_and_recursive_schemas_type_their_leaves() {
    // A leaf forty objects down is still typed by its schema.
    let schema = json!({"type": "object", "properties": {
        "x": {"type": "number"},
        "child": {"anyOf": [{"type": "null"}, {"$ref": "#"}]}
    }});
    let mut value = json!({"x": 1, "child": null});
    for _ in 0..40 {
        value = json!({"x": 1, "child": value});
    }
    let mut source = "output".to_owned();
    for _ in 0..40 {
        source.push_str(".child");
    }
    source.push_str(".x");
    assert_same!(
        evaluate(
            &format!("{source} * 2.0"),
            &[("output", &value, &Shape::Schema(&schema))]
        ),
        Ok(json!(2.0))
    );
    // Unions of references that would multiply finish, untyped where they
    // run out of budget.
    let mut defs = serde_json::Map::new();
    for i in 0..40 {
        let next = if i == 39 {
            json!({"type": "integer"})
        } else {
            json!({"anyOf": [{"$ref": format!("#/$defs/d{}", i + 1)}, {"$ref": format!("#/$defs/d{}", i + 1)}]})
        };
        defs.insert(format!("d{i}"), next);
    }
    let schema = json!({"$ref": "#/$defs/d0", "$defs": defs});
    let started = std::time::Instant::now();
    assert!(cel_value(&json!(1), &Shape::Schema(&schema)).is_ok());
    let looping = json!({"anyOf": [{"$ref": "#"}, {"$ref": "#"}, {"type": "integer"}]});
    assert!(cel_value(&json!(1), &Shape::Schema(&looping)).is_ok());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn integer_text_is_read_exactly() {
    let field = json!({"type": "integer"});
    let field = Shape::Schema(&field);
    for (text, want) in [
        ("0e40", "Ok(Int(0))"),
        ("0e-5", "Ok(Int(0))"),
        ("0.000", "Ok(Int(0))"),
        ("-0.0e-400", "Ok(Int(0))"),
        ("12e-1", "Err"),
        ("120e-1", "Ok(Int(12))"),
        ("1e38", "Err"),
        ("1e999999999", "Err"),
        ("1e99999999999", "Err"),
        ("1e-9223372036854775808", "Err"),
        ("1.0000000000000000000001", "Err"),
        ("1e-400", "Err"),
    ] {
        let started = std::time::Instant::now();
        let got = format!("{:?}", cel_value(&number(text), &field));
        assert!(got.starts_with(want), "{text}: {got}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "{text}"
        );
    }
}

#[test]
fn additional_properties_beside_pattern_properties_do_not_type_a_field() {
    // `n1` matches the pattern, so `additionalProperties` does not apply to it.
    let schema = json!({"type": "object",
        "patternProperties": {"^n": {"type": "number"}},
        "additionalProperties": {"type": "integer"}});
    let value = json!({"n1": 1.5});
    assert_same!(
        evaluate("output.n1", &[("output", &value, &Shape::Schema(&schema))]),
        Ok(json!(1.5))
    );
}

#[test]
fn map_keys_in_a_result_are_strings() {
    let none = Value::Null;
    let bind = [("output", &none, &Shape::Untyped)];
    for source in ["{1: 'a'}", "{true: 1}", "{1u: 2}"] {
        assert!(evaluate(source, &bind).is_err(), "{source}");
    }
    assert_eq!(evaluate("{'1': 'a'}", &bind), Ok(json!({"1": "a"})));
}

#[test]
fn a_value_is_checked_for_conversion_where_it_is_accepted() {
    let schema = json!({"type": "object", "properties": {"count": {"type": "integer"}}});
    assert_eq!(convertible(&json!({"count": 3}), &schema), Ok(()));
    for text in ["1e20", "1.0000000000000000000001", "1e-400"] {
        let value = json!({"count": number(text)});
        assert!(convertible(&value, &schema).is_err(), "{text}");
    }
}

#[test]
fn types_follow_references_combinators_and_containers() {
    let schema = json!({
        "type": "object",
        "properties": {
            "by_ref": {"$ref": "#/$defs/count"},
            "all": {"allOf": [{"type": "number"}, {"type": "integer"}]},
            "either_int": {"anyOf": [{"type": "integer"}, {"$ref": "#/$defs/count"}]},
            "mixed": {"anyOf": [{"type": "integer"}, {"type": "number"}]},
            "list": {"type": "array", "items": {"type": "integer"}},
            "tuple": {"type": "array", "prefixItems": [{"type": "integer"}, {"type": "number"}],
                      "items": {"type": "integer"}},
            "map": {"type": "object", "additionalProperties": {"type": "integer"}},
            "nullable": {"type": ["integer", "null"]}
        },
        "anyOf": [
            {"properties": {"tagged": {"type": "integer"}}},
            {"properties": {"tagged": {"type": "integer"}}}
        ],
        "$defs": {"count": {"type": "integer"}}
    });
    let value = json!({
        "by_ref": 1.0, "all": 1.0, "either_int": 1.0, "mixed": 1, "list": [1.0, 2],
        "tuple": [1.0, 1, 3.0], "map": {"a": 1.0}, "nullable": 2.0, "tagged": 4.0
    });
    let shape = Shape::Schema(&schema);
    let cel = cel_value(&value, &shape).expect("converts");
    let cel::Value::Map(map) = cel else {
        panic!("a map: {cel:?}")
    };
    let get = |key: &str| {
        map.map
            .get(&cel::objects::Key::from(key.to_owned()))
            .cloned()
            .expect(key)
    };
    assert_same!(get("by_ref"), cel::Value::Int(1));
    assert_same!(get("all"), cel::Value::Int(1));
    assert_same!(get("either_int"), cel::Value::Int(1));
    assert_same!(get("mixed"), cel::Value::Float(1.0));
    assert_same!(
        get("list"),
        cel::Value::List(std::sync::Arc::new(vec![
            cel::Value::Int(1),
            cel::Value::Int(2)
        ]))
    );
    assert_same!(
        get("tuple"),
        cel::Value::List(std::sync::Arc::new(vec![
            cel::Value::Int(1),
            cel::Value::Float(1.0),
            cel::Value::Int(3)
        ]))
    );
    assert_same!(get("nullable"), cel::Value::Int(2));
    assert_same!(get("tagged"), cel::Value::Int(4));
    let cel::Value::Map(inner) = get("map") else {
        panic!("a map")
    };
    assert_same!(
        inner.map.get(&cel::objects::Key::from("a".to_owned())),
        Some(&cel::Value::Int(1))
    );
    // A field shape types the fields it names and leaves the rest untyped.
    let field = json!({"type": "integer"});
    let shape = Shape::fields([("n", Shape::Schema(&field))]);
    let value = json!({"n": 1.0, "m": 1});
    assert_same!(
        evaluate(
            "output.n + 1 == 2 && output.m + 1 == 2",
            &[("output", &value, &shape)]
        ),
        Ok(json!(true))
    );
}

#[test]
fn results_are_finite_json_with_sorted_keys() {
    let none = Value::Null;
    let bind = [("output", &none, &Shape::Untyped)];
    for source in [
        "1.0 / 0.0",
        "-1.0 / 0.0",
        "0.0 / 0.0",
        "[1.0 / 0.0]",
        "{'a': 0.0 / 0.0}",
    ] {
        assert!(evaluate(source, &bind).is_err(), "{source}");
    }
    let result = evaluate("{'b': 1, 'a': 2, 'c': {'z': 1, 'y': 2}}", &bind).expect("evaluates");
    assert_eq!(
        serde_json::to_string(&result).expect("serializes"),
        r#"{"a":2,"b":1,"c":{"y":2,"z":1}}"#
    );
    // An object bound in comes back with its keys sorted, every time.
    let value: Value =
        serde_json::from_str(r#"{"zeta": 1, "alpha": 2, "mid": [{"q": 1, "p": 2}]}"#)
            .expect("parses");
    for _ in 0..5 {
        let result = evaluate("output", &[("output", &value, &Shape::Untyped)]).expect("evaluates");
        assert_eq!(
            serde_json::to_string(&result).expect("serializes"),
            r#"{"alpha":2,"mid":[{"p":2,"q":1}],"zeta":1}"#
        );
    }
    // Integers and uints come back as integers.
    let schema = json!({"type": "object", "properties": {"big": {"type": "integer"}}});
    assert_eq!(
        with_schema("output.big", &schema, &json!({"big": u64::MAX})),
        Ok(json!(u64::MAX))
    );
}

#[test]
fn every_kind_of_json_value_evaluates() {
    let schema = json!({
        "type": "object",
        "properties": {
            "count": {"type": "integer"}, "negative": {"type": "integer"},
            "big": {"type": "integer"}, "ratio": {"type": "number"},
            "huge": {"type": "number"},
            "items": {"type": "array", "items": {"type": "object", "properties": {
                "meta": {"type": "object", "properties": {"level": {"type": "integer"}}}}}}
        }
    });
    let output = json!({
        "count": 3,
        "negative": -4,
        "big": u64::MAX,
        "ratio": 0.25,
        "huge": number("100000000000000000000"),
        "none": null,
        "items": [{"tags": ["a", "b"], "meta": {"level": 2}}, {"tags": [], "meta": {"level": 5}}],
    });
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
        assert_eq!(
            with_schema(source, &schema, &output),
            Ok(json!(true)),
            "{source}"
        );
    }
    assert_eq!(
        with_schema("output.items", &schema, &output),
        Ok(output["items"].clone())
    );
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
