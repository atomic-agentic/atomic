// SPDX-License-Identifier: Apache-2.0

use super::*;
use serde_json::json;

fn ok(text: &str) -> Document {
    load(Path::new("t.yaml"), text).unwrap_or_else(|e| panic!("expected to load: {e}"))
}

fn err(text: &str) -> YamlError {
    match load(Path::new("t.yaml"), text) {
        Ok(d) => panic!("expected an error, loaded {}", d.value),
        Err(e) => e,
    }
}

fn at(line: usize, column: usize) -> Position {
    Position { line, column }
}

#[test]
fn a_workflow_loads_as_json_with_positions() {
    let d = ok("name: review\nsteps:\n  - atom: plan\n  - atom: implement\n    retries: 2\n");
    assert_eq!(
        d.value,
        json!({"name": "review", "steps": [{"atom": "plan"}, {"atom": "implement", "retries": 2}]})
    );
    assert_eq!(d.position("/steps/1/atom"), Some(at(4, 11)));
    assert_eq!(d.position("/steps/1/retries"), Some(at(5, 14)));
}

#[test]
fn core_schema_scalars() {
    let d = ok(
        "a: true\nb: False\nc: ~\nd:\ne: 12\nf: -3\ng: 0x1F\nh: 0o17\ni: 1.5\nj: 1e3\nk: .5\nl: '12'\nm: 1.2.3\n",
    );
    assert_eq!(
        d.value,
        json!({"a": true, "b": false, "c": null, "d": null, "e": 12, "f": -3, "g": 31, "h": 15,
               "i": 1.5, "j": 1000.0, "k": 0.5, "l": "12", "m": "1.2.3"})
    );
}

#[test]
fn an_integer_too_large_for_64_bits_is_a_number() {
    let d = ok("a: 123456789012345678901234567890\nb: -98765432109876543210\n");
    assert!(d.value["a"].is_number(), "{}", d.value);
    assert_eq!(
        d.value["a"],
        serde_json::from_str::<Value>("123456789012345678901234567890").expect("json")
    );
    assert!(d.value["b"].is_number(), "{}", d.value);
}

#[test]
fn an_empty_file_is_null() {
    assert_eq!(ok("").value, Value::Null);
}

#[test]
fn a_duplicate_key_is_refused_with_its_line() {
    let e = err("name: a\nsteps: []\nname: b\n");
    assert_eq!(e.at, at(3, 1));
    assert!(
        e.message
            .contains("duplicate key \"name\" (first at line 1)"),
        "{e}"
    );
    assert_eq!(e.to_string().split(": ").next(), Some("t.yaml:3:1"));
}

#[test]
fn an_unquoted_yes_in_an_enum_is_refused_with_its_line() {
    let e = err("type: string\nenum: [maybe, yes, no]\n");
    assert_eq!(e.at, at(2, 15));
    assert!(e.message.contains("quote it"), "{e}");
    for word in ["no", "On", "OFF", "YES"] {
        let e = err(&format!("flag: {word}\n"));
        assert_eq!(e.at, at(1, 7), "{word}");
    }
    // Quoted, they are just strings.
    assert_eq!(
        ok("enum: ['yes', \"no\"]\n").value,
        json!({"enum": ["yes", "no"]})
    );
}

#[test]
fn keys_must_be_strings() {
    for (text, column, tail) in [
        ("x: 1\ntrue: b\n", 1, "reads as true"),
        ("null: c\n", 1, "reads as null"),
        ("1.5: d\n", 1, "reads as 1.5"),
        ("1e3: e\n", 1, "reads as 1000"),
        ("? [a]\n: d\n", 3, "not a mapping or a sequence"),
    ] {
        let e = err(text);
        assert!(
            e.message
                .starts_with("keys must be a string or a plain decimal integer"),
            "{text:?}: {e}"
        );
        assert!(e.message.contains(tail), "{text:?}: {e}");
        assert!(e.message.contains("quote it"), "{text:?}: {e}");
        assert_eq!(e.at.column, column, "{text:?}: {e}");
    }
    assert_eq!(
        ok("'1': a\n\"true\": b\n").value,
        json!({"1": "a", "true": "b"})
    );
}

#[test]
fn an_unquoted_integer_key_reads_as_a_string() {
    let unquoted = ok("responses:\n  200: ok\n  404: gone\n");
    // The same document with the keys quoted is the same document.
    let quoted = ok("responses:\n  \"200\": ok\n  \"404\": gone\n");
    assert_eq!(unquoted.value, quoted.value);
    assert_eq!(
        unquoted.value,
        json!({"responses": {"200": "ok", "404": "gone"}})
    );
    // Same JSON Pointer, so positions and the errors that cite them agree too.
    assert_eq!(unquoted.position("/responses/200"), Some(at(2, 8)));
    assert_eq!(quoted.position("/responses/200"), Some(at(2, 10)));
    // Only a canonical decimal integer is a key: 0, and -?[1-9][0-9]*.
    assert_eq!(
        ok("0: a\n-3: b\n9223372036854775807: c\n9223372036854775808: d\n").value,
        json!({"0": "a", "-3": "b",
               "9223372036854775807": "c", "9223372036854775808": "d"})
    );
    // An integer JSON cannot hold exactly is not an integer key; quoted, it
    // is the exact digits.
    let e = err("123456789012345678901234567890: a\n");
    assert!(
        e.message
            .contains("keys must be a string or a plain decimal integer"),
        "{e}"
    );
    assert_eq!(
        ok("\"123456789012345678901234567890\": a\n").value,
        json!({"123456789012345678901234567890": "a"})
    );
}

/// An integer key is accepted only while its text is the number it reads as.
/// Every other spelling would be renamed to that number, so `007:` would load
/// as `"7"` and no longer match `"007":`; YAML 1.1 and 1.2 also do not read
/// them alike, so the loader refuses them and says to quote them.
#[test]
fn a_non_canonical_integer_key_is_refused() {
    let cases = [
        ("+2: a\n", "reads as 2"),
        ("-0: a\n", "reads as 0"),
        ("007: a\n", "reads as 7"),
        ("0200: a\n", "reads as 200"),
        ("0x1F: a\n", "reads as 31"),
        ("0o17: a\n", "reads as 15"),
        ("0x-1F: a\n", "reads as -31"),
    ];
    for (text, reads_as) in cases {
        let e = err(text);
        assert!(
            e.message
                .contains("keys must be a string or a plain decimal integer"),
            "{text:?}: {e}"
        );
        assert!(e.message.contains(reads_as), "{text:?}: {e}");
        assert!(e.message.contains("quote it"), "{text:?}: {e}");
        assert_eq!(e.at, at(1, 1), "{text:?}: {e}");
        // Quoted, the very same text is an ordinary string key.
        let spelling = text.trim_end_matches(": a\n");
        let mut wanted = Map::new();
        wanted.insert(spelling.to_owned(), json!("a"));
        let quoted = format!("\"{spelling}\": a\n");
        assert_eq!(ok(&quoted).value, Value::Object(wanted), "{text:?}");
    }
    // A refused spelling is refused where it would have been renamed into a
    // clash, not reported as a duplicate of the canonical key.
    let e = err("007: a\n7: b\n");
    assert_eq!(e.at, at(1, 1), "{e}");
    assert!(
        e.message
            .contains("keys must be a string or a plain decimal integer"),
        "{e}"
    );
}

#[test]
fn an_integer_key_and_its_quoted_form_are_one_key() {
    let e = err("responses:\n  200: ok\n  \"200\": fine\n");
    assert_eq!(e.at, at(3, 3));
    assert!(
        e.message
            .contains("duplicate key \"200\" (first at line 2)"),
        "{e}"
    );
    // Either order gives the duplicate.
    let e = err("200: ok\n\"200\": fine\n");
    assert_eq!(e.at, at(2, 1));
    assert!(
        e.message
            .contains("duplicate key \"200\" (first at line 1)"),
        "{e}"
    );
    let e = err("\"200\": fine\n200: ok\n");
    assert_eq!(e.at, at(2, 1));
    assert!(
        e.message
            .contains("duplicate key \"200\" (first at line 1)"),
        "{e}"
    );
}

#[test]
fn tags_anchors_and_aliases_are_refused() {
    let e = err("a: !!str 5\n");
    assert!(e.message.contains("tags are not allowed"), "{e}");
    assert_eq!(e.at.line, 1);
    let e = err("a: !custom x\n");
    assert!(e.message.contains("tags are not allowed"), "{e}");
    let e = err("base: &b {x: 1}\n");
    assert!(e.message.contains("anchors"), "{e}");
    let e = err("x: 1\ny: *b\n");
    // An undefined alias is a parse error in itself; a defined one never
    // gets that far, because its anchor is refused first.
    assert_eq!(e.at.line, 2, "{e}");
}

#[test]
fn a_second_document_is_refused() {
    let e = err("a: 1\n---\nb: 2\n");
    assert_eq!(e.at.line, 2);
    assert!(e.message.contains("second YAML document"), "{e}");
}

#[test]
fn values_json_cannot_hold_are_refused() {
    for text in ["x: .inf\n", "x: -.Inf\n", "x: .nan\n"] {
        assert!(
            err(text).message.contains("cannot be written in JSON"),
            "{text:?}"
        );
    }
}

#[test]
fn a_syntax_error_carries_its_position() {
    let e = err("a: [1, 2\nb: 3\n");
    assert!(e.at.line >= 1);
    assert!(!e.message.is_empty());
}
