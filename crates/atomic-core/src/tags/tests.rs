// SPDX-License-Identifier: Apache-2.0

use super::*;

fn tags(body: &str) -> Vec<Tag> {
    recognize(body)
        .expect("recognizes")
        .into_iter()
        .filter_map(|p| match p {
            Piece::Tag(t, _) => Some(t),
            Piece::Text(_) => None,
        })
        .collect()
}

fn err(body: &str) -> TagError {
    recognize(body).expect_err("should fail")
}

#[test]
fn an_unknown_tag_fails_with_its_line_and_column() {
    let e = err("# Review\nCheck {% widget kind=\"x\" %} first.\n");
    assert_eq!(e.at, At { line: 2, column: 7 });
    assert!(e.message.contains("unknown tag {% widget %}"), "{e}");
    // Columns count characters, not bytes.
    assert_eq!(err("é {% nope %}").at, At { line: 1, column: 3 });
}

#[test]
fn an_underscore_inside_a_tag_path_stays_literal() {
    let body = "Read _carefully_: {% $input.file_path %} and {% $input.base_branch_name %}.\n";
    assert_eq!(
        tags(body),
        [
            Tag::Value {
                root: "input".into(),
                path: vec!["file_path".into()]
            },
            Tag::Value {
                root: "input".into(),
                path: vec!["base_branch_name".into()]
            },
        ]
    );
    // The text around the tags is passed through untouched, emphasis and all.
    let pieces = recognize(body).expect("recognizes");
    assert_eq!(pieces[0], Piece::Text("Read _carefully_: ".into()));
}

#[test]
fn values_take_fields_and_indexes() {
    assert_eq!(
        tags("{% $input.findings[0].line %}"),
        [Tag::Value {
            root: "input".into(),
            path: vec!["findings".into(), "0".into(), "line".into()]
        }]
    );
    assert!(err("{% $input. %}").message.contains("not a value path"));
    assert!(err("{% $input[x] %}").message.contains("not a value path"));
}

#[test]
fn if_and_else_balance() {
    let t = tags("{% if $input.risky %}Be careful.{% else /%}Go ahead.{% /if %}");
    assert_eq!(
        t,
        [
            Tag::If {
                condition: "$input.risky".into()
            },
            Tag::Else,
            Tag::EndIf
        ]
    );
    assert!(err("{% if x %}open").message.contains("never closed"));
    assert!(err("{% /if %}").message.contains("no matching"));
    assert!(err("{% else /%}").message.contains("outside an {% if %}"));
    let e = err("{% example name=\"a\" %}\n{% if x %}\n{% /example %}\n");
    assert_eq!(e.at.line, 3);
    assert!(e.message.contains("opened at 2:1"), "{e}");
}

#[test]
fn a_condition_may_contain_a_closing_delimiter_in_a_string() {
    assert_eq!(
        tags("{% if $input.note == \"50%} off\" %}x{% /if %}")[0],
        Tag::If {
            condition: "$input.note == \"50%} off\"".into()
        }
    );
}

#[test]
fn partials_and_examples() {
    assert_eq!(
        tags("{% partial file=\"_house_style.md\" /%}"),
        [Tag::Partial {
            file: "_house_style.md".into()
        }]
    );
    assert_eq!(
        tags("{% example name=\"missing tests\" %}\n```json input\n{}\n```\n{% /example %}"),
        [
            Tag::Example {
                name: "missing tests".into()
            },
            Tag::EndExample
        ]
    );
    assert!(err("{% partial /%}").message.contains("needs a file"));
}

#[test]
fn tags_in_fenced_code_are_text() {
    let body = "Before.\n```text\n{% widget %} is literal here\n```\n~~~\n{% also %}\n~~~\nAfter {% $input.x %}.\n";
    assert_eq!(
        tags(body),
        [Tag::Value {
            root: "input".into(),
            path: vec!["x".into()]
        }]
    );
}

#[test]
fn later_tags_are_known_but_not_supported_yet() {
    for body in [
        "{% for c in $input.items %}x{% /for %}",
        "{% schema \"output\" /%}",
        "{% truncate($input.x, 10) %}",
    ] {
        assert!(
            err(body).message.contains("not supported by this version"),
            "{body}"
        );
    }
}

#[test]
fn an_unclosed_delimiter_is_an_error() {
    let e = err("text {% $input.x\nmore");
    assert_eq!(e.at, At { line: 1, column: 6 });
}
