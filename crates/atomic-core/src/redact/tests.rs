// SPDX-License-Identifier: Apache-2.0

use super::*;
use serde_json::json;

const KEY: &str = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789");

#[test]
fn known_shapes_are_found() {
    for secret in [
        KEY,
        concat!("github_pat_", "11ABCDEFG0123456789_abcdefghijklmnop"),
        concat!("sk-ant-", "api03-abcdefghijklmnopqrstuvwxyz1234567890"),
        concat!("sk-proj-", "abcdefghijklmnopqrstuvwxyz123456"),
        concat!("sk-", "abcdefghijklmnopqrstuvwxyz0123456789ABCD"),
        concat!("AIza", "SyA-abcdefghijklmnopqrstuvwxyz01234"),
        concat!("AKIA", "1234567890ABCDEF"),
        concat!("glpat-", "abcdefghij0123456789"),
        concat!("xoxb-", "1234567890-abcdefghij"),
        concat!("sk_live_", "abcdefghijklmnop1234"),
        concat!("hf_", "abcdefghijklmnopqrstuvwxyz0123456"),
        concat!("eyJ", "hbGciOiJSUzI1NiJ9.eyJ", "zdWIiOiIxMjM0In0.c2lnbmF0dXJlLWJ5dGVz"),
    ] {
        let redacted = text(&format!("before {secret} after")).into_owned();
        assert_eq!(redacted, format!("before {REDACTED} after"), "{secret}");
    }
}

#[test]
fn a_known_shape_inside_a_longer_word_is_not_one() {
    for kept in [
        concat!("task-", "abcdefghijklmnopqrstuvwxyz0123456789ABCD"),
        concat!("/usr/lib/sk-", "abcdefghijklmnopqrstuvwxyz0123456789ABCD"),
        concat!("xAKIA", "1234567890ABCDEF"),
        "sk-short",
    ] {
        assert_eq!(text(kept), kept, "{kept}");
    }
}

#[test]
fn a_private_key_block_is_found_even_when_cut_off() {
    let whole = concat!("x -----BEGIN RSA PRIVATE", " KEY-----\nMIIEsecret\n-----END RSA PRIVATE", " KEY----- y");
    assert_eq!(text(whole), format!("x {REDACTED} y"));
    let cut = concat!("x -----BEGIN OPENSSH PRIVATE", " KEY-----\nMIIEsecret");
    assert_eq!(text(cut), format!("x {REDACTED}"));
    let public = "-----BEGIN PUBLIC KEY-----\nMIIBIjAN\n-----END PUBLIC KEY-----";
    assert_eq!(text(public), public);
}

#[test]
fn a_value_next_to_a_secret_name_is_found_in_any_case() {
    let cases = [
        ("password=hunter2hunter2", "password=[REDACTED]"),
        (
            "PGPASSWORD=hunter2hunter2 psql",
            "PGPASSWORD=[REDACTED] psql",
        ),
        (
            "OPENAI_API_KEY=abcdefgh1234 x",
            "OPENAI_API_KEY=[REDACTED] x",
        ),
        ("db_password: hunter2hunter2", "db_password: [REDACTED]"),
        ("my.api-key = abcdefgh1234", "my.api-key = [REDACTED]"),
        ("clientSecret=abcdefgh1234", "clientSecret=[REDACTED]"),
        (r#""password": "abc""#, r#""password": "[REDACTED]""#),
        ("token='abc'", "token='[REDACTED]'"),
        (
            r#"{\"password\":\"hunter2\",\"x\":1}"#,
            r#"{\"password\":\"[REDACTED]\",\"x\":1}"#,
        ),
        ("a?token=abcdefgh1234&page=2", "a?token=[REDACTED]&page=2"),
    ];
    for (input, expected) in cases {
        assert_eq!(text(input), expected, "{input}");
    }
}

#[test]
fn headers_urls_and_flags_are_found() {
    let cases = [
        (
            "curl -H 'Authorization: Bearer abcd1234efgh' x",
            "curl -H 'Authorization: [REDACTED]' x",
        ),
        ("Authorization: abcd", "Authorization: [REDACTED]"),
        ("Cookie: session=abcd1234; a=b", "Cookie: [REDACTED]"),
        ("x-api-key: abcd1234", "x-api-key: [REDACTED]"),
        ("got Bearer abcd1234efgh back", "got Bearer [REDACTED] back"),
        (
            "postgres://atomic:hunter2@127.0.0.1:5433/atomic",
            "postgres://atomic:[REDACTED]@127.0.0.1:5433/atomic",
        ),
        (
            "https://u:p4ss@example.invalid/x",
            "https://u:[REDACTED]@example.invalid/x",
        ),
        (
            "run --password hunter2 now",
            "run --password [REDACTED] now",
        ),
        (
            "run --api-key 'abc def' now",
            "run --api-key [REDACTED] now",
        ),
        ("run --token=abcdefgh1234", "run --token=[REDACTED]"),
    ];
    for (input, expected) in cases {
        assert_eq!(text(input), expected, "{input}");
    }
}

#[test]
fn ordinary_text_is_left_alone() {
    for kept in [
        "max_tokens: 4096",
        "max_tokens=4096",
        "token_count: 12345678",
        "token: 12345678",
        "TOKEN_BUDGET=generous-enough",
        "API_KEY=short",
        "let token = next_token();",
        "token: Option<String>,",
        "password: string",
        "secret: boolean",
        r#""password": "string""#,
        "GH_TOKEN=$GITHUB_TOKEN",
        "password=${DB_PASSWORD}",
        "the bearer token is required",
        "Bearer authentication",
        "--token-file path",
        "commit 0123456789abcdef0123456789abcdef01234567",
        "aaaaaaaa-0000-4000-8000-000000000001",
        "https://example.invalid:8080/a@b",
        "git@github.com:org/repo.git",
        "plain prose with é and 日本語",
    ] {
        assert_eq!(text(kept), kept, "{kept}");
    }
}

#[test]
fn redacting_twice_changes_nothing() {
    for input in [
        KEY,
        "password=hunter2hunter2",
        "Authorization: Bearer abcd1234efgh",
        "Cookie: a=b",
        "postgres://u:hunter2@h/d",
        "--password hunter2",
        "Bearer abcd1234efgh",
    ] {
        let once = text(input).into_owned();
        assert_eq!(text(&once), once, "{input}");
    }
}

#[test]
fn every_string_and_key_of_a_value_is_covered() {
    let mut payload = json!({
        KEY: 1,
        "command": format!("export X={KEY}"),
        "list": ["plain", {"deep": [format!("Bearer {KEY}")]}],
        "password": "abc",
        "Authorization": "anything at all",
        "apiKey": "x",
        "tokens": 12,
        "secret": true,
        "token": "",
        "token_count": "12",
        "note": "fine",
    });
    value(&mut payload);
    assert_eq!(
        payload,
        json!({
            REDACTED: 1,
            "command": format!("export X={REDACTED}"),
            "list": ["plain", {"deep": [format!("Bearer {REDACTED}")]}],
            "password": REDACTED,
            "Authorization": REDACTED,
            "apiKey": REDACTED,
            "tokens": 12,
            "secret": true,
            "token": "",
            "token_count": "12",
            "note": "fine",
        })
    );
}

/// As a model's raw tool arguments arrive: an escape sits just before the
/// secret.
#[test]
fn json_inside_a_string_is_covered() {
    let raw = format!(r#"{{"content":"\n{KEY}","password":"abc"}}"#);
    let mut payload = json!({"arguments": raw});
    value(&mut payload);
    let inner: serde_json::Value =
        serde_json::from_str(payload["arguments"].as_str().expect("a string")).expect("JSON");
    assert_eq!(
        inner,
        json!({"content": format!("\n{REDACTED}"), "password": REDACTED})
    );
}

#[test]
fn json_inside_a_string_without_secrets_keeps_its_exact_text() {
    let raw = r#"{ "b": 1,   "a": 2 }"#;
    let mut payload = json!(raw);
    value(&mut payload);
    assert_eq!(payload, json!(raw));
}

/// Shapes that could make a scanner rescan its input.
fn worst_cases(size: usize) -> Vec<(&'static str, String)> {
    let repeated = |unit: &str| unit.repeat(size / unit.len());
    vec![
        (
            "ordinary text",
            repeated("fn main() { println!(\"synthetic\"); }\n"),
        ),
        ("one long word", repeated("a")),
        ("names with no values", repeated("password password ")),
        ("names with open quotes", repeated("password=\"")),
        ("flags with no values", repeated(" --password")),
        (
            "block starts with no end",
            repeated(concat!("-----BEGIN RSA PRIVATE", " KEY-----\n")),
        ),
        ("block starts that are not keys", repeated("-----BEGIN ")),
        ("upper-case names", repeated("API_KEY API_KEY=")),
        ("URLs with no password", repeated("https://a:b ")),
        ("dense secrets", repeated("password=hunter2hunter2 ")),
        ("key prefixes", repeated("ghp_ ")),
        ("token starts", repeated("eyJ")),
    ]
}

/// Generous, because this is a debug build on a shared machine. It catches
/// quadratic behaviour.
#[test]
fn worst_cases_finish_quickly() {
    for (name, input) in worst_cases(1024 * 1024) {
        let start = std::time::Instant::now();
        let _ = text(&input);
        let taken = start.elapsed();
        assert!(taken.as_secs() < 20, "{name} took {taken:?}");
    }
}

#[test]
fn nesting_as_deep_as_a_large_payload_allows_does_not_overflow() {
    let mut nested = json!("synthetic");
    for _ in 0..19 {
        let mut level = nested;
        for _ in 0..120 {
            level = json!([level]);
        }
        nested = json!(level.to_string());
    }
    assert!(nested.to_string().len() < 8 * 1024 * 1024);
    // A small stack, because a debug build's frames differ from a release
    // build's.
    std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || value(&mut nested))
        .expect("thread")
        .join()
        .expect("no overflow");
}

/// Past the nesting limit a shape is still found, because the text rules see
/// through the escaping.
#[test]
fn deeply_nested_json_is_bounded() {
    let mut nested = json!(format!("{KEY} "));
    for _ in 0..12 {
        nested = json!([nested.to_string()]);
    }
    value(&mut nested);
    assert!(!nested.to_string().contains(KEY));
}
