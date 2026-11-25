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
    // A float's leading zero is no integer's leading zero, and an exponent
    // needs no dot to be a float: JSON and the core schema read both as
    // numbers, where PyYAML reads `1e3` and `1.5e3` as strings.
    assert_eq!(
        ok("a: 00.5\nb: 01.5\nc: -01.5\nd: 0e0\ne: 1.5e3\n").value,
        json!({"a": 0.5, "b": 1.5, "c": -1.5, "d": 0.0, "e": 1500.0})
    );
    // A zero on its own is canonical, and a signed zero reads as zero; a zero
    // written with a leading zero is refused with every other such spelling,
    // though both versions read it as 0.
    assert_eq!(
        ok("a: 0\nb: -0\nc: +0\n").value,
        json!({"a": 0, "b": 0, "c": 0})
    );
    // The fix a leading zero gets keeps the sign the author wrote, and a plus
    // is no part of a number to write back.
    for (spelling, fix) in [
        ("00", "write 0, or 0o0 for octal"),
        ("-00", "write -0, or -0o0 for octal"),
        ("+00", "write 0, or 0o0 for octal"),
    ] {
        let e = err(&format!("a: {spelling}\n"));
        assert!(e.message.contains("has a leading zero"), "{spelling}: {e}");
        assert!(
            e.message
                .contains(&format!("{fix}, or quote it (\"{spelling}\")")),
            "{spelling}: {e}"
        );
    }
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
    // A mapping or a sequence is no key here, and "quote it" would be odd
    // advice: it is no key whatever its own text, so the rule is the advice.
    for text in ["? [a]\n: d\n", "? {a: b}\n: d\n"] {
        let e = err(text);
        assert_eq!(
            e.message,
            "keys must be a string or a plain decimal integer, not a mapping or a sequence"
        );
        assert_eq!(e.at.column, 3, "{text:?}: {e}");
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
        ("0x1F: a\n", "reads as 31"),
        ("0o17: a\n", "reads as 15"),
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
    // A key spelling that is refused as a number is refused as a key too, for
    // the same reason and with the same advice, and the key rule still leads.
    for text in ["007: a\n", "0200: a\n", "0x-1F: a\n"] {
        let e = err(text);
        assert!(
            e.message
                .starts_with("keys must be a string or a plain decimal integer, and unquoted"),
            "{text:?}: {e}"
        );
        assert!(e.message.contains("quote it"), "{text:?}: {e}");
        assert_eq!(e.at, at(1, 1), "{text:?}: {e}");
    }
    // A refused spelling is refused where it would have been renamed into a
    // clash, not reported as a duplicate of the canonical key.
    let e = err("007: a\n7: b\n");
    assert_eq!(e.at, at(1, 1), "{e}");
    assert!(
        e.message.contains(
            "keys must be a string or a plain decimal \
                            integer, and unquoted \"007\" has a leading zero"
        ),
        "{e}"
    );
}

/// The key rule, spelled out: which plain keys the loader reads as their own
/// text and which it refuses. `tools/check_examples.py` classifies the same
/// spellings the same way, and its `check_yaml_loader_mirror` says so against
/// PyYAML; this is the loader's own half of that table, so the two cannot drift
/// apart without one of them failing.
#[test]
fn a_plain_key_is_read_or_refused_by_its_own_text() {
    // Read as their own text: `0` and `-?[1-9][0-9]*`, while a 64-bit integer
    // can hold it. Digits past that are a number the key would be renamed to,
    // which is the row below that reads as one; 400 of them are a number here
    // too, as [`an_integer_too_large_for_64_bits_is_a_number`] shows.
    for spelling in [
        "200",
        "0",
        "-5",
        "9223372036854775807",
        "9223372036854775808",
        "18446744073709551615",
    ] {
        assert_eq!(
            ok(&format!("{spelling}: a\n")).value,
            json!({spelling: "a"}),
            "{spelling}"
        );
    }
    // Refused, each for the reason its own text gives.
    for (spelling, why) in [
        ("08", "has a leading zero"),
        ("09", "has a leading zero"),
        ("0089", "has a leading zero"),
        ("0200", "has a leading zero"),
        ("007", "has a leading zero"),
        ("00", "has a leading zero"),
        ("-00", "has a leading zero"),
        ("0o17", "reads as 15"),
        ("0x1F", "reads as 31"),
        ("+2", "reads as 2"),
        ("-0", "reads as 0"),
        ("1.5", "reads as 1.5"),
        ("1e3", "reads as 1000"),
        ("99999999999999999999", "reads as"),
        ("18446744073709551616", "reads as"),
        ("0x-1F", "puts a sign after its 0x"),
        ("-0x1F", "puts a sign before its hexadecimal prefix"),
        ("1_000", "is a number in YAML 1.1"),
        ("0b11", "is a number in YAML 1.1"),
        ("12:30", "is a number in YAML 1.1"),
        ("yes", "is a boolean in YAML 1.1"),
        (".inf", "cannot be written in JSON"),
    ] {
        let e = err(&format!("{spelling}: a\n"));
        assert!(
            e.message
                .starts_with("keys must be a string or a plain decimal integer"),
            "{spelling}: {e}"
        );
        assert!(e.message.contains(why), "{spelling}: {e}");
        assert!(e.message.ends_with("; quote it"), "{spelling}: {e}");
        assert_eq!(e.at, at(1, 1), "{spelling}: {e}");
        // Quoted, the same text is an ordinary key.
        assert_eq!(
            ok(&format!("\"{spelling}\": a\n")).value,
            json!({spelling: "a"}),
            "{spelling}"
        );
    }
}

/// A plain integer with a leading zero is a decimal in YAML 1.2 and an octal
/// in YAML 1.1, or a string there when a digit is not an octal digit, so the
/// loader refuses it as it refuses every other spelling versions differ on.
#[test]
fn a_leading_zero_integer_is_refused() {
    // Octal digits, but too many for a number to name.
    let too_many = format!("a: 0{}\n", "7".repeat(40));
    for (text, why) in [
        ("a: 0200\n", "YAML 1.1 reads as 128"),
        ("a: 010\n", "YAML 1.1 reads as 8"),
        ("a: 007\n", "YAML 1.1 reads as 7"),
        ("a: -0200\n", "YAML 1.1 reads as -128"),
        ("a: +0200\n", "YAML 1.1 reads as 128"),
        ("a: 08\n", "YAML 1.1 reads as a string"),
        ("a: 06008\n", "YAML 1.1 reads as a string"),
        (too_many.as_str(), "YAML 1.1 reads as an octal number"),
    ] {
        let e = err(text);
        let spelling = text.trim_start_matches("a: ").trim_end();
        assert!(e.message.contains("has a leading zero"), "{text:?}: {e}");
        assert!(e.message.contains(why), "{text:?}: {e}");
        assert!(
            e.message.contains("and YAML 1.2 as decimal"),
            "{text:?}: {e}"
        );
        assert!(
            e.message.contains(&format!("quote it (\"{spelling}\")")),
            "{text:?}: {e}"
        );
        assert_eq!(e.at, at(1, 4), "{text:?}: {e}");
        // Quoted, the very same text is the string it is read as elsewhere.
        assert_eq!(
            ok(&format!("a: \"{spelling}\"\n")).value,
            json!({"a": spelling}),
            "{text:?}"
        );
    }
    // A value names both fixes, and the octal one only when the digits say an
    // octal: `0o8` is no octal, and no number can hold 40 octal digits.
    for (spelling, fix) in [
        ("0200", "write 200, or 0o200 for octal, or quote it"),
        ("007", "write 7, or 0o7 for octal, or quote it"),
        ("-0200", "write -200, or -0o200 for octal, or quote it"),
        ("+0200", "write 200, or 0o200 for octal, or quote it"),
        ("08", "write 8, or quote it"),
        ("06008", "write 6008, or quote it"),
    ] {
        let e = err(&format!("a: {spelling}\n"));
        assert!(e.message.contains(fix), "{spelling}: {e}");
    }
    let e = err(&too_many);
    assert!(!e.message.contains(" for octal"), "{e}");
    // A refused spelling is refused in a sequence too, where there is no key to
    // name and no position to blame but its own.
    let e = err("a: [0200]\n");
    assert!(e.message.contains("has a leading zero"), "{e}");
    assert_eq!(e.at, at(1, 5), "{e}");
    // A zero on its own is canonical, and a float's leading zero is no such
    // thing: both still read as they did.
    assert_eq!(
        ok("a: 0\nb: -0\nc: 0.5\nd: 0e0\n").value,
        json!({"a": 0, "b": 0, "c": 0.5, "d": 0.0})
    );
}

#[test]
fn a_sign_after_the_base_prefix_is_refused() {
    for (text, marker, number) in [
        ("a: 0x-1F\n", "0x", "-31"),
        ("a: 0x+1F\n", "0x", "31"),
        ("a: 0x-1\n", "0x", "-1"),
        ("a: 0o-17\n", "0o", "-15"),
    ] {
        let e = err(text);
        assert!(
            e.message.contains(&format!(
                "puts a sign after its {marker}, which is no number in either YAML version"
            )),
            "{text:?}: {e}"
        );
        assert!(
            e.message.contains(&format!(
                "write {number} for the number, or quote it ({:?}) for the string",
                text.trim_start_matches("a: ").trim_end()
            )),
            "{text:?}: {e}"
        );
        assert_eq!(e.at, at(1, 4), "{text:?}: {e}");
    }
    // Quoted, they are the strings the core schema reads them as.
    assert_eq!(
        ok("a: '0x-1F'\nb: \"0o-17\"\n").value,
        json!({"a": "0x-1F", "b": "0o-17"})
    );
    // A digit the base has no such thing as is no integer at all, and a sign
    // with nothing after it is no integer either: those stay strings, as the
    // core schema reads them.
    assert_eq!(
        ok("a: 0xZZ\nb: 0x-\nc: 0x\nd: 0x-ZZZ\n").value,
        json!({"a": "0xZZ", "b": "0x-", "c": "0x", "d": "0x-ZZZ"})
    );
}

/// A sign before the base prefix is the other mistake in the same place, and
/// this one is a real difference between the versions: YAML 1.1's integer rules
/// take the sign before the digits of any base, so PyYAML reads `-0x1F` as -31
/// where the core schema reads the text as a string. Refusing it is the whole
/// point of this loader, so it is refused rather than read as a string.
#[test]
fn a_sign_before_the_base_prefix_is_refused() {
    for (text, name, number) in [
        ("a: -0x1F\n", "hexadecimal", "-31"),
        ("a: +0x1F\n", "hexadecimal", "31"),
        ("a: -0o17\n", "octal", "-15"),
        ("a: +0o17\n", "octal", "15"),
    ] {
        let e = err(text);
        assert!(
            e.message
                .contains(&format!("puts a sign before its {name} prefix")),
            "{text:?}: {e}"
        );
        assert!(
            e.message.contains(&format!(
                "which YAML 1.1 reads as {number} and YAML 1.2 as a string; write {number} for \
                 the number, or quote it ({:?}) for the string",
                text.trim_start_matches("a: ").trim_end()
            )),
            "{text:?}: {e}"
        );
        assert_eq!(e.at, at(1, 4), "{text:?}: {e}");
        assert_eq!(
            ok(&format!(
                "a: \"{}\"\n",
                text.trim_start_matches("a: ").trim_end()
            ))
            .value,
            json!({"a": text.trim_start_matches("a: ").trim_end()}),
            "{text:?}"
        );
    }
    // A sign before a prefix with no digits of that base after it is no number
    // in either version and stays the string it is.
    assert_eq!(
        ok("a: -0xZZ\nb: -0x\nc: -0b2\nd: -0o8\n").value,
        json!({"a": "-0xZZ", "b": "-0x", "c": "-0b2", "d": "-0o8"})
    );
}

/// Three shapes YAML 1.1's integer type reads and the YAML 1.2 core schema and
/// JSON read as strings: binary, an underscore in a whole number, and the
/// sexagesimal. The loader refuses all three and names the number 1.1 reads, so
/// that an author who meant a number has it written the other way.
#[test]
fn a_number_only_one_yaml_version_reads_is_refused() {
    for (spelling, number) in [
        ("0b11", "3"),
        ("-0b11", "-3"),
        ("0b1_1", "3"),
        ("1_000", "1000"),
        ("-1_0", "-10"),
        ("1000_", "1000"),
        ("1__000", "1000"),
        ("0x_1F", "31"),
        ("0x1_", "1"),
        ("07_7", "63"),
        ("010_7", "71"),
        ("0_", "0"),
        ("12:30", "750"),
        ("1:30:00", "5400"),
        ("1:2:3:4:5", "13403045"),
        ("-12:30", "-750"),
        // An underscore sits in the head of a sexagesimal and not in a group,
        // and a group may keep its leading zero.
        ("1_2:30", "750"),
        ("1:07", "67"),
    ] {
        let e = err(&format!("a: {spelling}\n"));
        assert!(
            e.message.contains(&format!(
                "unquoted {spelling:?} is a number in YAML 1.1 and a string in YAML 1.2 and JSON"
            )),
            "{spelling}: {e}"
        );
        assert!(
            e.message.contains(&format!(
                "write {number} for the number, or quote it (\"{spelling}\") for the string"
            )),
            "{spelling}: {e}"
        );
        assert_eq!(e.at, at(1, 4), "{spelling}: {e}");
        assert_eq!(
            ok(&format!("a: \"{spelling}\"\n")).value,
            json!({"a": spelling}),
            "{spelling}"
        );
        // As a key the same reason is given, and the only fix a key has.
        let e = err(&format!("{spelling}: a\n"));
        assert!(
            e.message
                .starts_with("keys must be a string or a plain decimal integer, and unquoted"),
            "{spelling}: {e}"
        );
        assert!(e.message.contains("quote it"), "{spelling}: {e}");
        assert!(!e.message.contains("for the number"), "{spelling}: {e}");
    }
    // A digit the base has no such thing as is no number to either version, and
    // a colon group of 60 or more, an underscore in a group, a leading zero or
    // an underscore where the first digit is, is no sexagesimal, so those stay
    // strings. A colon the parser reads as a mapping indicator, as in `1:`, is
    // no scalar to ask about at all, so none is written here.
    for spelling in [
        "0b2", "0B11", "0b", "0x", "08_9", "_1000", "_1:30", "x_1", "1_x", "1:70", "1:77", "0:00",
        "01:30", "_1", "12:30:99", "1:0:70", "12:3a", "1:2_0", "1:2:30_0",
    ] {
        assert_eq!(
            ok(&format!("a: {spelling}\n")).value,
            json!({"a": spelling}),
            "{spelling}"
        );
        assert_eq!(
            ok(&format!("{spelling}: a\n")).value,
            json!({spelling: "a"}),
            "{spelling}"
        );
    }
    // A marker with nothing but an underscore after it is in the class YAML 1.1
    // gives its binary and hexadecimal integers, so it is refused even though no
    // number reads out of it. The message then offers only the string.
    for spelling in ["0b_", "0x_", "0x__"] {
        let e = err(&format!("a: {spelling}\n"));
        assert!(
            e.message
                .contains("is a number in YAML 1.1 and a string in YAML 1.2 and JSON"),
            "{spelling}: {e}"
        );
        assert!(
            e.message
                .contains(&format!("quote it (\"{spelling}\") for the string")),
            "{spelling}: {e}"
        );
        assert!(!e.message.contains("write "), "{spelling}: {e}");
    }
    // An underscore in a float is no integer spelling, so no rule here reads it:
    // the core schema and JSON read this text as a string, and the loader reads
    // it as one. YAML 1.1's float type takes the underscore and PyYAML reads
    // 1000.5, so the same difference exists on the float side; the loader does
    // not refuse it yet, and the refusal is filed as a follow-up.
    assert_eq!(ok("a: 1_000.5\n").value, json!({"a": "1_000.5"}));
    assert_eq!(ok("a: -1_000.5\n").value, json!({"a": "-1_000.5"}));
    // `0o` is YAML 1.2's octal marker and YAML 1.1 has no such thing: `0o17` is
    // 15 to the core schema and a string to 1.1, so the loader reads the number
    // and says nothing about a difference. An underscore in it is a string to
    // both, since no class 1.1 gives its integers holds an `o`.
    assert_eq!(ok("a: 0o17\n").value, json!({"a": 15}));
    assert_eq!(ok("a: 0o7_7\n").value, json!({"a": "0o7_7"}));
    // A number with underscores in it is refused as a value too, and a flow
    // sequence has no key to name.
    let e = err("a: [1_000]\n");
    assert!(
        e.message
            .contains("is a number in YAML 1.1 and a string in YAML 1.2 and JSON"),
        "{e}"
    );
    assert_eq!(e.at, at(1, 5), "{e}");
}

/// The core schema reads a base-8 or base-16 integer of any size; this loader
/// has no number to read one past signed 64 bits as, so it refuses it rather
/// than read it as a string. A decimal that large is still a number, as JSON
/// reads it.
#[test]
fn a_base_integer_past_signed_64_bits_is_refused() {
    // One past i64's last value in each base.
    let hex = format!("a: 0x1{}\n", "0".repeat(16));
    let octal = format!("a: 0o1{}\n", "0".repeat(21));
    for (text, name) in [
        ("a: 0xFFFFFFFFFFFFFFFF\n", "hexadecimal"),
        (hex.as_str(), "hexadecimal"),
        (octal.as_str(), "octal"),
    ] {
        let e = err(text);
        assert!(
            e.message
                .contains(&format!("is a {name} integer past signed 64 bits")),
            "{text:?}: {e}"
        );
        let spelling = text.trim_start_matches("a: ").trim_end();
        assert!(
            e.message.contains(&format!(
                "write it in decimal, or quote it (\"{spelling}\")"
            )),
            "{text:?}: {e}"
        );
        assert_eq!(e.at, at(1, 4), "{text:?}: {e}");
        assert_eq!(
            ok(&format!("a: \"{spelling}\"\n")).value,
            json!({"a": spelling}),
            "{text:?}"
        );
        // As a key the same reason, and only the fix a key has.
        let e = err(&format!("{spelling}: a\n"));
        assert!(
            e.message
                .starts_with("keys must be a string or a plain decimal integer, and 0x")
                || e.message
                    .starts_with("keys must be a string or a plain decimal integer, and 0o"),
            "{text:?}: {e}"
        );
        assert!(
            e.message.contains(&format!(
                "{spelling} is a {name} integer past signed 64 bits"
            )),
            "{text:?}: {e}"
        );
        assert!(e.message.ends_with("; quote it"), "{text:?}: {e}");
    }
    // What fits, fits, and a decimal that large is still read as a number.
    let fits = format!(
        "a: 0x7FFFFFFFFFFFFFFF\nb: 0o{}\nc: 99999999999999999999\n",
        "7".repeat(21)
    );
    let d = ok(&fits);
    assert_eq!(d.value["a"], json!(i64::MAX), "{}", d.value);
    assert_eq!(d.value["b"], json!(i64::MAX), "{}", d.value);
    assert!(d.value["c"].is_number(), "{}", d.value);
    // A decimal past even a float's hold is still a number here, and keeps the
    // digits it was written with: numbers are read with a precision that holds
    // any decimal, so digits alone are never read as a string. Past 64 bits they
    // are no key, which is what the row above that reads as one says.
    let huge = "9".repeat(400);
    assert_eq!(
        ok(&format!("a: {huge}\n")).value,
        serde_json::from_str::<Value>(&format!("{{\"a\": {huge}}}")).expect("json"),
        "a decimal no float can hold"
    );
    let e = err(&format!("{huge}: a\n"));
    assert!(
        e.message
            .starts_with("keys must be a string or a plain decimal integer, and this one reads as"),
        "a decimal past 64 bits is no key"
    );
}

/// A flow mapping's keys are read by the same rule as a block mapping's, so an
/// unquoted integer key is its own text and is one key with its quoted form.
#[test]
fn a_flow_mapping_key_is_read_as_a_block_key_is() {
    assert_eq!(
        ok("{200: ok, 404: gone}\n").value,
        ok("{\"200\": ok, \"404\": gone}\n").value
    );
    assert_eq!(
        ok("{200: ok}\n").value,
        json!({"200": "ok"}),
        "the key is its own text"
    );
    assert_eq!(ok("{200: ok}\n").position("/200"), Some(at(1, 7)));
    let e = err("{200: a, \"200\": b}\n");
    assert_eq!(e.at, at(1, 10), "{e}");
    assert!(
        e.message
            .contains("duplicate key \"200\" (first at line 1)"),
        "{e}"
    );
    // And a refused spelling is refused in a flow mapping as it is in a block.
    let e = err("{0200: a}\n");
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

#[test]
fn nested_collections_stop_at_the_loader_limit() {
    for (open, close) in [("[", "]"), ("{a: ", "}")] {
        let nested = |n| format!("{}0{}", open.repeat(n), close.repeat(n));
        ok(&nested(64));
        assert!(err(&nested(65)).message.contains("nesting"));
    }
}
