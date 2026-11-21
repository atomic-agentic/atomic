// SPDX-License-Identifier: Apache-2.0

//! The fixtures are synthetic, and each is split in the source so no
//! scanner takes it for a real credential. Failures name a case by its
//! index and never print it.

use super::*;
use serde_json::json;

const SAMPLE: &str = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789");

/// The indices of the cases whose input does not redact to what they expect.
fn mismatches(cases: &[(&str, &str)]) -> Vec<usize> {
    cases
        .iter()
        .enumerate()
        .filter(|(_, (input, expected))| text(input) != *expected)
        .map(|(index, _)| index)
        .collect()
}

/// The indices of the inputs that redaction changes.
fn changed(kept: &[&str]) -> Vec<usize> {
    kept.iter()
        .enumerate()
        .filter(|(_, input)| text(input) != **input)
        .map(|(index, _)| index)
        .collect()
}

fn assert_cases(cases: &[(&str, &str)]) {
    let wrong = mismatches(cases);
    assert!(
        wrong.is_empty(),
        "cases {wrong:?} were not redacted as expected"
    );
}

fn assert_kept(kept: &[&str]) {
    let wrong = changed(kept);
    assert!(wrong.is_empty(), "cases {wrong:?} were changed");
}

const SHAPES: &[&str] = &[
    SAMPLE,
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
    concat!(
        "eyJ",
        "hbGciOiJSUzI1NiJ9.eyJ",
        "zdWIiOiIxMjM0In0.c2lnbmF0dXJlLWJ5dGVz"
    ),
    concat!("gsk_", "abcdefghijklmnopqrstuvwxyz0123456789"),
    concat!("xai-", "abcdefghijklmnopqrstuvwxyz0123456789"),
    concat!("nvapi-", "abcdefghijklmnopqrstuvwxyz0123456789"),
    concat!("npm_", "abcdefghijklmnopqrstuvwxyz0123456789"),
    concat!("ya29.", "a0AfH6SMBabcdefghijklmnopqrstuvwxyz"),
    concat!(
        "SG.",
        "abcdefghijklmnopqrstuv.abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG"
    ),
    concat!("whsec_", "abcdefghijklmnopqrstuvwxyz0123"),
    concat!("xoxe-", "1-abcdefghij0123456789"),
    concat!("xapp-", "1-A0123456789-abcdefghij"),
];

#[test]
fn known_shapes_are_found() {
    let wrong: Vec<usize> = SHAPES
        .iter()
        .enumerate()
        .filter(|(_, shape)| {
            text(&format!("before {shape} after")) != format!("before {REDACTED} after")
        })
        .map(|(index, _)| index)
        .collect();
    assert!(wrong.is_empty(), "shapes {wrong:?} were not found");
}

/// A key with no known prefix, under a name that has `key` in it.
#[test]
fn a_key_under_a_key_name_is_found() {
    const HEX: &str = "0123456789abcdef0123456789abcdef";
    let names = [
        "OPENAI_KEY=",
        "ANTHROPIC_KEY=",
        "AZURE_OPENAI_KEY=",
        "SPEECH_KEY=",
        "+ export OPENAI_KEY=",
        "x-functions-key: ",
        "\"primaryKey\": \"",
        "key=",
        "https://h/x?key=",
        "\"key\": \"",
        "Your API key: ",
        "api key: ",
    ];
    let wrong: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, name)| {
            let input = format!("{name}{HEX}\" x");
            let out = text(&input);
            out.contains(HEX) || !out.contains(REDACTED)
        })
        .map(|(index, _)| index)
        .collect();
    assert!(wrong.is_empty(), "cases {wrong:?} kept the key");
    assert_cases(&[("api key: short", "api key: [REDACTED]")]);
    assert_kept(&["duplicate key: name", "cache-key: v1-deps"]);
}

/// Webhook URLs carry their secret in the path.
#[test]
fn a_webhook_url_is_found() {
    assert_cases(&[
        (
            concat!(
                "post to https://hooks.slack.com/services/",
                "T0001/B0002/abcdefghijABCDEFGHIJ1234 now"
            ),
            "post to https://hooks.slack.com/services/[REDACTED] now",
        ),
        (
            concat!(
                "https://discord.com/api/webhooks/",
                "123456789012345678/abcDEF-ghi_JKL0123"
            ),
            "https://discord.com/api/webhooks/[REDACTED]",
        ),
        (
            concat!(
                "https://contoso.webhook.office.com/webhookb2/",
                "a1b2c3@d4e5/IncomingWebhook/f6/g7"
            ),
            "https://contoso.webhook.office.com/webhookb2/[REDACTED]",
        ),
    ]);
}

/// Right after a JSON or C escape, or a terminal colour, as a provider's raw
/// body or a coloured log line has them.
#[test]
fn a_shape_right_after_an_escape_or_a_colour_is_found() {
    let shape = SHAPES[2];
    let inputs = [
        format!(r#"{{"error":{{"message":"Incorrect API key provided:\n{shape}"}}}}"#),
        format!(r#"key rejected:\t{shape}"#),
        format!(r#"key rejected:\r\n{shape}"#),
        format!(r#"key rejected:\u000a{shape}"#),
        format!("\x1b[1;33m{shape}\x1b[0m"),
        format!(r#"colour:\u001b[1;33m{shape}\u001b[0m"#),
        format!(r#"colour:\x1b[1;33m{shape}\x1b[0m"#),
        format!("\x1b]0;title\x07{shape}"),
        format!("label\x1b[36m{shape}"),
        format!("label\x1b(B{shape}"),
        format!("label\x1b[38:5:196m{shape}"),
        format!("label\x1b[1 q{shape}"),
        format!(r#"nested:\\n{shape}"#),
    ];
    let wrong: Vec<usize> = inputs
        .iter()
        .enumerate()
        .filter(|(_, input)| {
            let out = text(input);
            out.contains(shape) || !out.contains(REDACTED)
        })
        .map(|(index, _)| index)
        .collect();
    assert!(wrong.is_empty(), "inputs {wrong:?} kept the shape");
    // The sequences themselves are kept.
    let coloured = format!("label\x1b[36m{shape}\x1b[0m");
    assert!(
        text(&coloured) == format!("label\x1b[36m{REDACTED}\x1b[0m"),
        "the colours were not kept"
    );
}

#[test]
fn a_known_shape_inside_a_longer_word_is_not_one() {
    assert_kept(&[
        concat!("task-", "abcdefghijklmnopqrstuvwxyz0123456789ABCD"),
        concat!("/usr/lib/sk-", "abcdefghijklmnopqrstuvwxyz0123456789ABCD"),
        concat!("xAKIA", "1234567890ABCDEF"),
        "sk-short",
    ]);
}

#[test]
fn a_private_key_block_is_found_even_when_cut_off() {
    let whole = concat!(
        "x -----BEGIN RSA PRIVATE",
        " KEY-----\nMIIEsample\n-----END RSA PRIVATE",
        " KEY----- y"
    );
    assert!(text(whole) == format!("x {REDACTED} y"), "a whole block");
    let open = concat!("x -----BEGIN OPENSSH PRIVATE", " KEY-----\nMIIEsample");
    assert!(text(open) == format!("x {REDACTED}"), "a block cut off");
    assert_kept(&["-----BEGIN PUBLIC KEY-----\nMIIBIjAN\n-----END PUBLIC KEY-----"]);
}

#[test]
fn a_value_next_to_a_secret_name_is_found_in_any_case() {
    assert_cases(&[
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
        ("token='abcdEFGH1234'", "token='[REDACTED]'"),
        (
            r#"{\"password\":\"hunter2\",\"x\":1}"#,
            r#"{\"password\":\"[REDACTED]\",\"x\":1}"#,
        ),
        ("a?token=abcdefgh1234&page=2", "a?token=[REDACTED]&page=2"),
        // Mixed case, and other separators and quotes.
        ("MyPassWord=hunter2", "MyPassWord=[REDACTED]"),
        ("PASSWORD := hunter2", "PASSWORD := [REDACTED]"),
        ("'password' => 'hunter2'", "'password' => '[REDACTED]'"),
    ]);
}

#[test]
fn headers_urls_and_flags_are_found() {
    assert_cases(&[
        (
            "curl -H 'Authorization: Bearer abcd1234efgh' x",
            "curl -H 'Authorization: [REDACTED]' x",
        ),
        ("Authorization: abcd1234efgh", "Authorization: [REDACTED]"),
        ("Cookie: session=abcd1234; a=b", "Cookie: [REDACTED]"),
        ("x-api-key: abcd1234efgh", "x-api-key: [REDACTED]"),
        ("got Bearer abcd1234efgh back", "got Bearer [REDACTED] back"),
        (
            "postgres://atomic:hunter2@127.0.0.1:5433/atomic",
            "postgres://atomic:[REDACTED]@127.0.0.1:5433/atomic",
        ),
        (
            "https://u:p4ss@example.invalid/x",
            "https://u:[REDACTED]@example.invalid/x",
        ),
        // A password with an `@`: to the last one.
        (
            "https://u:p@ss@example.invalid/x",
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
        // At the start, and as items of an argument list.
        ("--password hunter2 now", "--password [REDACTED] now"),
        (
            r#"["deploy","--token","abcdEFGH1234"]"#,
            r#"["deploy","--token","[REDACTED]"]"#,
        ),
        ("authorization=Bearer abc", "authorization=[REDACTED]"),
    ]);
}

#[test]
fn a_secret_word_in_a_name_is_found() {
    assert_cases(&[
        ("SECRET_KEY=abc123xyz", "SECRET_KEY=[REDACTED]"),
        (
            "AWS_SECRET_KEY=wJalrXUtnFEMIK7MDENG x",
            "AWS_SECRET_KEY=[REDACTED] x",
        ),
        (
            "DefaultEndpointsProtocol=https;AccountName=acct;AccountKey=abcdEFGH0123==;EndpointSuffix=core.windows.net",
            "DefaultEndpointsProtocol=https;AccountName=acct;AccountKey=[REDACTED];EndpointSuffix=core.windows.net",
        ),
        ("DB_PASS=pw", "DB_PASS=[REDACTED]"),
        ("GH_PAT=abc", "GH_PAT=[REDACTED]"),
        (
            "s3?X-Amz-Credential=AKIDEXAMPLE%2F2013&X-Amz-Signature=fe5f80f77d5f&X-Amz-Date=2013",
            "s3?X-Amz-Credential=[REDACTED]&X-Amz-Signature=[REDACTED]&X-Amz-Date=2013",
        ),
        (
            "Ocp-Apim-Subscription-Key: 0123abcd",
            "Ocp-Apim-Subscription-Key: [REDACTED]",
        ),
        ("accessToken=abc", "accessToken=[REDACTED]"),
        ("run --db-pass hunter2 now", "run --db-pass [REDACTED] now"),
        ("secret_id=abc123", "secret_id=[REDACTED]"),
        // A dotted path is judged by its last segment.
        ("input.password: hunter2", "input.password: [REDACTED]"),
        (
            "steps.login.output.token = abcdEFGH1234",
            "steps.login.output.token = [REDACTED]",
        ),
        (
            "process.env.API_KEY=abc123",
            "process.env.API_KEY=[REDACTED]",
        ),
    ]);
}

#[test]
fn any_value_beside_a_secrets_name_is_found() {
    assert_cases(&[
        ("API_KEY=short", "API_KEY=[REDACTED]"),
        ("pin_password=1234", "pin_password=[REDACTED]"),
        // Leading characters that only look like references.
        ("password=$ecret123", "password=[REDACTED]"),
        ("password=%p4ss", "password=[REDACTED]"),
        ("password={{p4ss", "password=[REDACTED]"),
        // Punctuation, brackets, escaped quotes and backslashes inside it.
        ("password=ab,cd;ef&gh x", "password=[REDACTED] x"),
        ("password=p(a)ss[1]{2} x", "password=[REDACTED] x"),
        ("(password=hunter2) x", "(password=[REDACTED]) x"),
        (
            "password=hunter2, user=bob",
            "password=[REDACTED], user=bob",
        ),
        (r#"password="ab\"cd" x"#, r#"password="[REDACTED]" x"#),
        (r"password=ab\cd x", "password=[REDACTED] x"),
        // Rust's Debug output, plain and pretty.
        (
            r#"Endpoint { api_key: Some("hunter2"), model: "m" }"#,
            r#"Endpoint { api_key: Some("[REDACTED]"), model: "m" }"#,
        ),
        ("api_key: Some(hunter2)", "api_key: Some([REDACTED])"),
        (
            r#"Endpoint { api_key: Secret("hunter2") }"#,
            r#"Endpoint { api_key: Secret("[REDACTED]") }"#,
        ),
        (
            "Endpoint {\n    api_key: Some(\n        \"hunter2\",\n    ),\n}",
            "Endpoint {\n    api_key: Some(\n        \"[REDACTED]\",\n    ),\n}",
        ),
        // Basic credentials, and a URL whose user is a token.
        ("got Basic dXNlcjpwYXNz back", "got Basic [REDACTED] back"),
        (
            "https://abcdef0123456789abcd@github.com/o/r",
            "https://[REDACTED]@github.com/o/r",
        ),
        // The rest of a cookie after a part already redacted by its shape.
        (
            concat!(
                "Cookie: a=ghp_",
                "abcdefghijklmnopqrstuvwxyz0123456789; b=othersample"
            ),
            "Cookie: [REDACTED]",
        ),
    ]);
}

/// What ordinary stderr and code say beside words like `key` and `token`.
#[test]
fn ordinary_text_is_left_alone() {
    assert_kept(&[
        "max_tokens: 4096",
        "max_tokens=4096",
        "token_count: 12345678",
        "token: 12345678",
        "TOKEN_BUDGET=generous-enough",
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
        "api_key_env: OPENAI_API_KEY",
        "token_file=/run/secrets/token",
        "monkey: banana",
        "token::Kind",
        "atomic_core::redact::text",
        "password=***",
        "password=<your-password>",
        "token=%TOKEN%",
        "token={{ secrets.token }}",
        "token=${{ secrets.TOKEN }}",
        "Basic settings",
        "https://x-access-token@github.com/o/r",
        "ssh://git@github.com/o/r",
        // A false-positive corpus: ordinary stderr.
        "KeyError: 'title'",
        "ExpiredToken: The security token included in the request is expired",
        "duplicate key: name",
        "token: expired",
        "tests pass: 12",
        "authorization: denied for user bob",
        "Basic Settings",
        "OPENAI_API_KEY=<your key>",
        r#"{"key":"value"}"#,
        r#""required": ["password","email"]"#,
        "primary_key: id",
        "password: required",
        "Error: invalid token: missing",
        // An escaped backslash is not an escape: what follows is a word.
        r"C:\\new\\task-abc",
        // Written-out escapes that are not whole sequences, or are, kept.
        r"invalid regex [\s\e]+ at col 3; expected ]",
        r"char class [^\e] then more",
        r"C:\some\e[1]\file.txt",
        r"printf '\033[1mbold\033[0m'",
        "osc \\e]0;title then more text\nnext line",
        // More ordinary stderr.
        "no randomness for a new credential: Resource temporarily unavailable",
        "credential: operator-1",
        "auth_method: password",
        "token: InvalidToken",
        "export OPENAI_API_KEY=...",
    ]);
}

#[test]
fn redacting_twice_changes_nothing() {
    let inputs = [
        SAMPLE,
        "password=hunter2hunter2",
        "Authorization: Bearer abcd1234efgh",
        "Cookie: a=b",
        concat!(
            "Cookie: a=ghp_",
            "abcdefghijklmnopqrstuvwxyz0123456789; b=c"
        ),
        "api_key: Some(\"hunter2\")",
        "https://abcdef0123456789abcd@github.com/o/r",
        "Basic dXNlcjpwYXNz",
        "postgres://u:hunter2@h/d",
        "--password hunter2",
        "Bearer abcd1234efgh",
        "authorization=Bearer abc",
    ];
    let wrong: Vec<usize> = inputs
        .iter()
        .enumerate()
        .filter(|(_, input)| {
            let once = text(input).into_owned();
            text(&once) != once
        })
        .map(|(index, _)| index)
        .collect();
    assert!(wrong.is_empty(), "inputs {wrong:?} changed again");
}

#[test]
fn every_string_and_key_of_a_value_is_covered() {
    let mut payload = json!({
        SAMPLE: 1,
        "command": format!("export X={SAMPLE}"),
        "list": ["plain", {"deep": [format!("Bearer {SAMPLE}")]}],
        "password": "abc",
        "Authorization": "anything at all",
        "apiKey": "x",
        "tokens": 12,
        "secret": true,
        "token": "",
        "token_count": "12",
        "note": "fine",
        "pin_password": 1234,
        "credentials": {"user": "bob", "password": "x", "port": 5, "on": true},
        "headers": [["Authorization", "Bearer abcd1234efgh"], ["Accept", "json"]],
        "header_list": [{"name": "X-Api-Key", "value": "abcd1234efgh"}, {"name": "Accept", "value": "json"}],
        "keys": [1, 2],
        "key": "value",
        "required": ["password", "email"],
        "argv": ["deploy", "--password", "hunter2", "--name", "web"],
        "env": [{"name": "DB_PASSWORD", "value": "hunter2"}, {"name": "HOME", "value": "/root"}],
        "pair": ["X-Api-Key", "abc"],
    });
    value(&mut payload);
    let expected = json!({
        REDACTED: 1,
        "command": format!("export X={REDACTED}"),
        "list": ["plain", {"deep": [format!("Bearer {REDACTED}")]}],
        "password": REDACTED,
        "Authorization": "anything at all",
        "apiKey": REDACTED,
        "tokens": 12,
        "secret": true,
        "token": "",
        "token_count": "12",
        "note": "fine",
        "pin_password": REDACTED,
        "credentials": {"user": "bob", "password": REDACTED, "port": 5, "on": true},
        "headers": [["Authorization", REDACTED], ["Accept", "json"]],
        "header_list": [{"name": "X-Api-Key", "value": REDACTED}, {"name": "Accept", "value": "json"}],
        "keys": [1, 2],
        "key": "value",
        "required": ["password", "email"],
        "argv": ["deploy", "--password", REDACTED, "--name", "web"],
        "env": [{"name": "DB_PASSWORD", "value": REDACTED}, {"name": "HOME", "value": "/root"}],
        "pair": ["X-Api-Key", REDACTED],
    });
    let wrong: Vec<&String> = expected
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(field, want)| payload.get(field.as_str()) != Some(*want))
        .map(|(field, _)| field)
        .collect();
    assert!(
        wrong.is_empty(),
        "fields {wrong:?} were not redacted as expected"
    );
}

/// As a model's raw tool arguments arrive: an escape sits just before the
/// key.
#[test]
fn json_inside_a_string_is_covered() {
    let raw = format!(r#"{{"content":"\n{SAMPLE}","password":"abc"}}"#);
    let mut payload = json!({"arguments": raw});
    value(&mut payload);
    let inner: serde_json::Value =
        serde_json::from_str(payload["arguments"].as_str().expect("a string")).expect("JSON");
    assert!(
        inner == json!({"content": format!("\n{REDACTED}"), "password": REDACTED}),
        "the arguments were not redacted as expected"
    );
}

#[test]
fn json_inside_a_string_without_secrets_keeps_its_exact_text() {
    let raw = r#"{ "b": 1,   "a": 2 }"#;
    let mut payload = json!(raw);
    value(&mut payload);
    assert_eq!(payload, json!(raw));
}

/// A provider's raw body, as `excerpt` takes it: redacted as JSON, then cut.
#[test]
fn an_excerpt_of_a_json_body_is_redacted_as_json() {
    let shape = SHAPES[2];
    let body = format!(
        r#"{{"error":{{"message":"Incorrect API key provided:\n{shape}","type":"invalid_request_error"}}}}"#
    );
    let out = excerpt(&body, 500);
    assert!(
        !out.contains(shape) && out.contains(REDACTED),
        "the body kept the key"
    );
    // An escape inside the key itself: only JSON sees the key.
    let escaped = format!(r#"{{"message":"\u0073{}"}}"#, &shape[1..]);
    let out = excerpt(&escaped, 500);
    assert!(!out.contains(&shape[1..]), "the escaped key was kept");
    // Cut off, so no longer JSON: redacted as text.
    let cut_off = format!("{}{}", &body[..body.len() - 10], "x".repeat(100));
    let out = excerpt(&cut_off, 500);
    assert!(
        !out.contains(shape) && out.contains(REDACTED),
        "the cut body kept the key"
    );
    // A key cut in half by the bound is dropped, not kept in part: after a
    // long line that redaction shrinks, at the start and at the end.
    let margin = 4096 * 2 + 16 * 1024;
    let key = SAMPLE;
    let lead = format!("Cookie: a={}\n", "b".repeat(margin - 20 - 11));
    let head = format!("{lead}{key} after");
    let out = bounded(&head, 4096);
    assert!(!out.contains(&key[..20]), "bounded kept the key's head");
    let out = excerpt(&head, 4096);
    assert!(!out.contains(&key[..20]), "excerpt kept the key's head");
    let trail = format!("\nCookie: a={}", "b".repeat(margin - 20 - 11));
    let ending = format!("before {key}{trail}");
    let out = tail(&ending, 4096);
    assert!(!out.contains(&key[20..]), "tail kept the key's end");
    // Bounded before it is redacted: ten megabytes cost no more than the
    // excerpt's margin.
    let huge = "a=".repeat(5 * 1024 * 1024);
    for (name, redact) in [
        ("excerpt", excerpt as fn(&str, usize) -> String),
        ("bounded", bounded),
        ("tail", tail),
    ] {
        let start = std::time::Instant::now();
        let _ = redact(&huge, 500);
        let taken = start.elapsed();
        assert!(
            taken < std::time::Duration::from_secs(1),
            "{name} took {taken:?}"
        );
    }
    // A JSON report's strings, and so a panic's message, are bounded too.
    let mut report = json!({"description": huge});
    let start = std::time::Instant::now();
    value(&mut report);
    let taken = start.elapsed();
    assert!(
        taken < std::time::Duration::from_secs(1),
        "a JSON string took {taken:?}"
    );
    // The excerpt is bounded, and so is the tail.
    let long = format!("{shape} {}", "y".repeat(10_000_000));
    assert!(excerpt(&long, 500).chars().count() <= 501);
    let ending = format!("{}{shape}", "y ".repeat(1_000_000));
    assert!(
        tail(&ending, 50).contains(REDACTED),
        "the tail kept the key"
    );
}

/// The runner writes its errors line by line: the body of a private key
/// block one line opens is never written.
#[test]
fn the_lines_writer_holds_back_a_private_key_body() {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let sink = Shared(Arc::new(Mutex::new(Vec::new())));
    let mut lines = lines_to(Box::new(sink.clone()));
    let block = concat!(
        "before\n-----BEGIN RSA PRIVATE",
        " KEY-----\nMIIEbodyLINEone\nMIIEbodyLINEtwo\n-----END RSA PRIVATE",
        " KEY-----\nafter\n"
    );
    lines.write_all(block.as_bytes()).expect("write");
    drop(lines);
    let written = String::from_utf8(sink.0.lock().expect("lock").clone()).expect("UTF-8");
    assert!(!written.contains("MIIEbody"), "the key's body was written");
    assert!(written.starts_with("before\n") && written.ends_with("after\n"));
}

/// Shapes that could make a scanner rescan its input: text with no white
/// space above all.
fn worst_cases(size: usize) -> Vec<(&'static str, String)> {
    let repeated = |unit: &str| unit.repeat(size / unit.len());
    vec![
        (
            "ordinary text",
            repeated("fn main() { println!(\"synthetic\"); }\n"),
        ),
        ("one long word", repeated("a")),
        ("assignments with no space", repeated("a=")),
        ("secret assignments with no space", repeated("token=")),
        ("query strings", repeated("a=b&c=d;")),
        ("secret query strings", repeated("token=b&password=d;")),
        ("separators and brackets", repeated("password=)(][}{,")),
        ("names with no values", repeated("password password ")),
        ("names with open quotes", repeated("password=\"")),
        ("escaped open quotes", repeated("password=\\\"")),
        ("flags with no values", repeated(" --password")),
        (
            "block starts with no end",
            repeated(concat!("-----BEGIN RSA PRIVATE", " KEY-----\n")),
        ),
        ("block starts that are not keys", repeated("-----BEGIN ")),
        ("upper-case names", repeated("API_KEY API_KEY=")),
        ("URLs with no password", repeated("https://a:b ")),
        ("URLs with no host", repeated("https://a:b@")),
        ("dense secrets", repeated("password=hunter2hunter2 ")),
        ("key prefixes", repeated("ghp_ ")),
        ("token starts", repeated("eyJ")),
        ("escapes", repeated("\\n\\u001b[1m")),
        ("colours", repeated("\x1b[1;33m")),
        ("wrappers", repeated("api_key: Some(")),
        ("headers", repeated("Authorization: ")),
        ("open templates", repeated("api_key={{ ")),
        ("written-out colours", repeated("\\u001b[1;33m")),
        ("open titles", repeated("\x1b]0;")),
        (
            "quotes escaped ever deeper",
            (1..1400)
                .map(|depth| format!("password={}\"x ", "\\".repeat(depth)))
                .collect(),
        ),
    ]
}

/// A megabyte of each, in a debug build on a shared machine, within a
/// bound a quadratic scan could not meet. A linear pass over a megabyte
/// takes about a second a case on a loaded CI runner running other tests
/// alongside, and has taken past three; a quadratic one takes about 25 s
/// for 64 KB and so hours for a megabyte. Thirty seconds keeps the two far
/// apart. Two cases whose slower growth this bound could hide have their
/// own check (see `escaped_quotes_and_open_key_blocks_grow_linearly`).
#[test]
fn worst_cases_finish_quickly() {
    let slow: Vec<(&str, std::time::Duration)> = worst_cases(1024 * 1024)
        .into_iter()
        .filter_map(|(name, input)| {
            let start = std::time::Instant::now();
            let _ = text(&input);
            let taken = start.elapsed();
            (taken > std::time::Duration::from_secs(30)).then_some((name, taken))
        })
        .collect();
    assert!(slow.is_empty(), "too slow: {slow:?}");
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
    let mut nested = json!(format!("{SAMPLE} "));
    for _ in 0..12 {
        nested = json!([nested.to_string()]);
    }
    value(&mut nested);
    assert!(!nested.to_string().contains(SAMPLE));
}

/// The hook `main` installs writes what this returns.
#[test]
fn a_panic_message_is_redacted() {
    use std::sync::{Arc, Mutex};
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(mut seen) = sink.lock() {
            seen.push(panic_message(info));
        }
    }));
    let sample = concat!("glpat-", "PANICKEDwithAsample0123");
    let payload = format!("the provider said {sample}");
    let _ = std::panic::catch_unwind(move || std::panic::panic_any(payload));
    std::panic::set_hook(previous);
    let seen = seen.lock().expect("lock").join("\n");
    assert!(
        seen.contains("the provider said [REDACTED]"),
        "not redacted"
    );
    assert!(!seen.contains(sample), "the sample was written");
}

/// Text that once made redaction panic: slices at fixed byte offsets.
#[test]
fn multibyte_text_near_a_bound_does_not_panic() {
    let osc = format!("\x1b]0;build\x07{}", "日本語のテキスト".repeat(60));
    for input in [
        "la clé est invalide: key: abc",
        "ééééééééééééé api key: x",
        osc.as_str(),
    ] {
        let _ = text(input);
        let _ = bounded(input, 10);
        let _ = excerpt(input, 10);
        let _ = tail(input, 10);
    }
}

/// A key in a terminal title, a clipboard write or a link, raw or written
/// out: invisible on a terminal, present in what is stored.
#[test]
fn a_key_inside_an_osc_is_found() {
    let shape = SHAPES[0];
    const HEX: &str = "0123456789abcdef0123456789abcdef";
    let inputs = [
        format!("\x1b]0;{shape}\x07"),
        format!("\x1b]52;c;{shape}\x07"),
        format!("\x1b]8;;https://h/x?token={HEX}\x1b\\link\x1b]8;;\x1b\\"),
        format!(r"\u001b]0;{shape}\u0007"),
        format!(r"\e]2;{shape}\a"),
    ];
    let wrong: Vec<usize> = inputs
        .iter()
        .enumerate()
        .filter(|(_, input)| {
            let out = text(input);
            out.contains(shape) || out.contains(HEX)
        })
        .map(|(index, _)| index)
        .collect();
    assert!(wrong.is_empty(), "inputs {wrong:?} kept the key");
}

/// A sequence that does not end before a key, or whose final letter is the
/// key's first: the key is found whole.
#[test]
fn a_key_after_an_incomplete_or_long_sequence_is_found() {
    let shape = SHAPES[0];
    let jwt = SHAPES[11];
    let inputs = [
        format!("\x1b[{shape}"),
        format!("\x1b[1;33{shape}"),
        format!("\x1b({shape}"),
        format!("\x1b[38;2;255;255;255;48;2;255;255;255;1;3;4m{shape}"),
        format!(r"C:\{jwt}"),
    ];
    let wrong: Vec<usize> = inputs
        .iter()
        .enumerate()
        .filter(|(_, input)| {
            let out = text(input);
            out.contains(&shape[4..]) || out.contains(&jwt[3..]) || !out.contains(REDACTED)
        })
        .map(|(index, _)| index)
        .collect();
    assert!(wrong.is_empty(), "inputs {wrong:?} kept part of the key");
}

/// A word about a secret counts only as a name's last word.
#[test]
fn a_word_about_a_secret_counts_only_at_the_end_of_a_name() {
    assert_cases(&[
        ("PROVIDER_API_KEY=abc123", "PROVIDER_API_KEY=[REDACTED]"),
        (
            "DATA_SOURCE_PASSWORD=hunter2",
            "DATA_SOURCE_PASSWORD=[REDACTED]",
        ),
        ("STATE_SECRET=hunter2", "STATE_SECRET=[REDACTED]"),
    ]);
    assert_kept(&[
        "auth_method: password",
        "secret_version: 3",
        "token_count: 12",
    ]);
}

#[test]
fn versioned_discord_and_telegram_webhooks_are_found() {
    assert_cases(&[
        (
            concat!(
                "https://discord.com/api/v10/webhooks/",
                "123456789012345678/abcDEF-ghi_JKL0123"
            ),
            "https://discord.com/api/v10/webhooks/[REDACTED]",
        ),
        (
            concat!(
                "https://api.telegram.org/bot",
                "123456789:AAbcDEF-ghi_JKL0123/sendMessage"
            ),
            "https://api.telegram.org/bot[REDACTED]",
        ),
    ]);
}

/// The token a window's cut splits is dropped whole, however long.
#[test]
fn a_long_token_split_by_a_window_is_dropped_whole() {
    let long = "k".repeat(2000);
    let head = format!("kept {long} after");
    assert_eq!(window_start_at(&head, 1500), "kept ");
    let end = format!("before {long} kept");
    assert_eq!(window_end_at(&end, 1500), " kept");
    // A cut at a boundary drops nothing.
    assert_eq!(window_start_at("abc def", 3), "abc");
    assert_eq!(window_end_at("abc def", 4), " def");
}

/// If redacting a panic's message panics, the hook writes a fixed message
/// rather than panic inside itself.
#[test]
fn a_panic_in_redaction_writes_a_fixed_message() {
    fn panics(_: &str) -> String {
        std::panic::panic_any("redaction failed")
    }
    assert_eq!(redact_panic("anything".into(), panics), UNREDACTED_PANIC);
    assert_eq!(
        redact_panic("the provider said nothing".into(), |t| bounded(t, MAX_TEXT)),
        "the provider said nothing"
    );
}

/// Random text mixing ASCII, characters of every width, escapes, colours
/// and secret-shaped tokens: redaction never panics, at any bound.
#[test]
fn redaction_never_panics_on_random_text() {
    let fragments: Vec<String> = [
        "a",
        "Z",
        "0",
        " ",
        "\n",
        "\t",
        "é",
        "—",
        "✓",
        "日",
        "😀",
        "’",
        "…",
        "\\",
        "\\n",
        "\\\\n",
        "\\u001b[1m",
        "\\e]0;",
        "\\a",
        "\\u0007",
        "\x1b[",
        "\x1b[1;33m",
        "\x1b]0;",
        "\x1b]52;c;",
        "\x07",
        "\x1b\\",
        "\x1b(",
        "key: ",
        "api key: ",
        "password=",
        "token=",
        "Cookie: ",
        "Authorization: Bearer ",
        "https://u:p@h/",
        "{\"password\":\"",
        "\"",
        "=",
        ":",
        ";",
        ",",
        "&",
        "{",
        "}",
        "[",
        "]",
        "(",
        ")",
        "<",
        ">",
        "$",
        "%",
        "{{",
        "}}",
        concat!("-----BEGIN RSA PRIVATE", " KEY-----\n"),
        concat!("-----END RSA PRIVATE", " KEY-----"),
        "ghp_",
        "sk-",
        "eyJ",
        "SG.",
        "ya29.",
        "AKIA",
    ]
    .iter()
    .map(|f| (*f).to_owned())
    .chain(SHAPES.iter().map(|s| (*s).to_owned()))
    .collect();
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for case in 0..4000 {
        let len = (next() % 120) as usize;
        let input: String = (0..len)
            .map(|_| fragments[(next() % fragments.len() as u64) as usize].as_str())
            .collect();
        let bound = (next() % 64) as usize;
        let survived = std::panic::catch_unwind(|| {
            let _ = text(&input);
            let _ = bounded(&input, bound);
            let _ = excerpt(&input, bound);
            let _ = tail(&input, bound);
            let _ = window_start_at(&input, bound);
            let _ = window_end_at(&input, bound);
            let mut doc =
                json!({"k": input, "list": [input, {"password": input}], input.clone(): 1});
            value(&mut doc);
        });
        assert!(survived.is_ok(), "case {case} panicked");
    }
}

/// How many steps redacting `input` takes (see `STEPS`).
fn steps_of(input: &str) -> u64 {
    STEPS.with(|steps| steps.set(0));
    let _ = text(input);
    STEPS.with(std::cell::Cell::get)
}

/// Two worst cases whose slower growth the megabyte bound could hide: quotes
/// escaped ever deeper, whose cap keeps each depth's scan to one pass, and
/// key blocks with no end, each consumed whole. At sixteen times the size,
/// a linear scan takes about sixteen times the steps, one that grows as the
/// 1.5th power 64 times, a quadratic one 256 times. Steps are counted, not
/// timed, so no machine's load changes the answer.
#[test]
fn escaped_quotes_and_open_key_blocks_grow_linearly() {
    let quotes = |depths: usize| -> String {
        (1..depths)
            .map(|depth| format!("password={}\"x ", "\\".repeat(depth)))
            .collect()
    };
    let blocks = |size: usize| concat!("-----BEGIN RSA PRIVATE", " KEY-----\n").repeat(size / 32);
    for (name, small, large) in [
        ("escaped quotes", quotes(350), quotes(1400)),
        ("open key blocks", blocks(64 * 1024), blocks(1024 * 1024)),
    ] {
        let (small, large) = (steps_of(&small), steps_of(&large));
        assert!(
            large < 32 * small,
            "{name}: {large} steps at sixteen times the size of {small}"
        );
    }
}

/// A private key block of `chars` characters in all, with its end.
fn key_block(chars: usize) -> String {
    let begin = concat!("-----BEGIN RSA PRIVATE", " KEY-----\n");
    let end = concat!("\n-----END RSA PRIVATE", " KEY-----\n");
    format!(
        "{begin}{}{end}",
        "A".repeat(chars - begin.len() - end.len())
    )
}

/// A secret whose rule needs what a window's cut took, the `@` after a
/// URL's password, the quote that ends a JSON value or the `Bearer ` before
/// a token, is not shown when redacting what comes before it shrinks the
/// window enough to bring the cut into view.
#[test]
fn a_secret_the_cut_parted_from_its_neighbour_is_not_shown() {
    const SEEDED: &str = "hunter2-seeded-passphrase";
    let chars = 100;
    let window = margin(chars);
    let at_end = |line: String, after: &str| {
        let block = key_block(window - line.len());
        format!("{block}{line}{after}")
    };
    let url = at_end(
        format!("connect postgres://app:{SEEDED}"),
        "@db.internal/x\n",
    );
    let json = at_end(format!("{{\"password\": \"{SEEDED}"), "\"}\n");
    let shown = [
        bounded(&url, chars),
        excerpt(&url, chars),
        bounded(&json, chars),
    ];
    for (i, out) in shown.iter().enumerate() {
        assert!(!out.contains(SEEDED), "case {i} showed the secret");
        assert!(out.ends_with('…'), "case {i} does not say it was cut");
    }
    let token = concat!("abcdef0123456789", "abcdef0123456789");
    let block = key_block(window - token.len() - 1);
    let header = format!("Authorization: Bearer {token}\n{block}");
    let out = tail(&header, chars);
    assert!(!out.contains(token), "the tail showed the token");
    assert!(out.starts_with('…'), "the tail does not say it was cut");
    // With nothing shrunk, the cut is far out of view and nothing more is
    // dropped; so with a little shrunk, as one-line text with a key or two.
    let plain = format!("{}\n{}", "word ".repeat(window), "more");
    assert_eq!(bounded(&plain, chars), format!("{}…", &plain[..chars]));
}

/// A window that left everything out says so, rather than show nothing:
/// one unbroken token longer than the window, say.
#[test]
fn a_window_that_leaves_everything_out_says_so() {
    let long = "k".repeat(50_000);
    assert_eq!(bounded(&long, 100), "…");
    assert_eq!(excerpt(&long, 100), "…");
    assert_eq!(tail(&long, 100), "…");
    // What fits is shown whole, unmarked.
    assert_eq!(bounded("short", 100), "short");
    assert_eq!(tail("short", 100), "short");
}

/// A redaction that never ends, as one waiting on a rule its panicking
/// thread was building would, does not hold the hook: it writes the fixed
/// message.
#[test]
fn a_panic_redaction_that_never_ends_writes_the_fixed_message() {
    fn hangs(_: &str) -> String {
        loop {
            std::thread::park();
        }
    }
    let (done, written) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(redact_panic_within(
            "anything".into(),
            hangs,
            std::time::Duration::from_millis(200),
        ));
    });
    let written = written
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the hook waited on redaction for good");
    assert_eq!(written, UNREDACTED_PANIC);
}

/// A panic in redaction writes nothing of its own: the hook that waited on
/// it writes the fixed message, once.
#[test]
fn a_panic_in_redaction_is_written_once() {
    assert_eq!(hook_line(true, || UNREDACTED_PANIC.to_owned()), None);
    assert_eq!(
        hook_line(false, || "message".into()),
        Some("message".into())
    );
}

/// A sequence cut short by an escape, `ESC[` then `\n`, does not take the
/// escape's letter: the key after it is found whole.
#[test]
fn a_key_after_an_open_sequence_and_an_escape_is_found() {
    let shape = SHAPES[0];
    let inputs = [
        format!("\x1b[\\n{shape}"),
        format!("\x1b[1;33\\n{shape}"),
        format!("\x1b[\\t{shape}"),
        format!("\\u001b[\\n{shape}"),
        format!("\\x1b[\\u0007{shape}"),
        format!("\x1b(\\n{shape}"),
        format!("\\x1b(\\n{shape}"),
    ];
    let wrong: Vec<usize> = inputs
        .iter()
        .enumerate()
        .filter(|(_, input)| {
            let out = text(input);
            out.contains(&shape[4..]) || !out.contains(REDACTED)
        })
        .map(|(index, _)| index)
        .collect();
    assert!(wrong.is_empty(), "inputs {wrong:?} kept part of the key");
}

/// Names of a count or a kind of token, and Telegram URLs that hold no
/// bot token, are not secrets.
#[test]
fn counts_and_kinds_of_tokens_are_not_secrets() {
    assert_kept(&[
        "max_token=4096",
        "prompt_token: 12",
        "error_token: abc",
        "next_token: page2",
        "https://api.telegram.org/botinfo",
    ]);
    assert_cases(&[
        ("GH_TOKEN=abc123", "GH_TOKEN=[REDACTED]"),
        ("access_token: Zx81kQ0pL2mN4bV6", "access_token: [REDACTED]"),
    ]);
}

/// One-line text that redaction shrinks a little, or whose cut dropped a
/// long token, keeps its leading text: the cut is still out of view.
#[test]
fn one_line_text_keeps_its_start_when_the_cut_stays_out_of_view() {
    // A provider's JSON body, on one line, echoing keys.
    let keys = vec![SAMPLE; 30].join(" ");
    let body = format!(
        "{{\"error\":{{\"message\":\"Incorrect API key provided: {keys}\",\"detail\":\"{}\"}}}}",
        "lorem ipsum ".repeat(19 * 1024 / 12)
    );
    let out = excerpt(&body, 500);
    assert!(
        out.starts_with("{\"error\":{\"message\":\"Incorrect API key provided: [REDACTED]"),
        "the body's start was dropped"
    );
    assert!(!out.contains(SAMPLE), "the body showed a key");
    // A command's stderr on one line, with keys redacted along it.
    let keys = vec![SAMPLE; 90].join(" ");
    let stderr = format!(
        "{} {keys} {} final words",
        "warning ".repeat(15 * 1024 / 8),
        "detail ".repeat(10 * 1024 / 7)
    );
    let out = tail(&stderr, 2000);
    assert!(out.ends_with("final words"), "the tail's end was dropped");
    assert!(out.starts_with('…'), "the tail does not say it was cut");
    // A long base64 payload, dropped whole where the cut splits it, with
    // its `payload=`, since `=` may be part of a key.
    let upload = format!(
        "error: upload failed for payload={}",
        "QUJD".repeat(47 * 1024)
    );
    let out = bounded(&upload, 4096);
    assert_eq!(out, "error: upload failed for …");
}

/// Hint words qualified by a word that is not a count or a kind of token
/// still name secrets.
#[test]
fn only_a_count_or_kind_of_token_is_a_hint() {
    assert_cases(&[
        ("INPUT_PASS=hunter2", "INPUT_PASS=[REDACTED]"),
        ("PROVIDER_TOKEN=abc123", "PROVIDER_TOKEN=[REDACTED]"),
        ("ERROR_AUTH=abc", "ERROR_AUTH=[REDACTED]"),
        ("INPUT_TOKEN=hunter2", "INPUT_TOKEN=[REDACTED]"),
        ("CACHED_TOKEN=12345678", "CACHED_TOKEN=[REDACTED]"),
        (
            "NEXT_PUBLIC_API_TOKEN=x",
            "NEXT_PUBLIC_API_TOKEN=[REDACTED]",
        ),
        ("input_token=hunter2", "input_token=[REDACTED]"),
        // A kind word right before `TOKEN`, in an environment variable.
        ("NEXT_TOKEN=hunter2", "NEXT_TOKEN=[REDACTED]"),
        ("ERROR_TOKEN=hunter2", "ERROR_TOKEN=[REDACTED]"),
    ]);
    // Counts and pagination fields, in snake or camel case, still only hint.
    assert_kept(&[
        "input_tokens: 12",
        "nextToken: page2",
        "max_tokens_used=4096",
        "cachedTokens: 120",
    ]);
}

/// A header line as long as a common cap, cut at a tail's start: once the
/// window shrinks to bring the cut into view, the rest of the line, a
/// cookie's values here, is not shown.
#[test]
fn a_cookie_line_cut_at_a_tails_start_is_not_shown() {
    let chars = 2000;
    let pair = "sid=SEEDEDabc123; ";
    let rest_of_line = pair.repeat(7000 / pair.len());
    let end = "\nend\n";
    let block = key_block(margin(chars) - rest_of_line.len() - 1 - end.len());
    let input = format!("Cookie: {}{rest_of_line}\n{block}{end}", pair.repeat(30));
    let out = tail(&input, chars);
    assert!(!out.contains("SEEDED"), "the cookie's values were shown");
    assert!(out.starts_with('…'), "the tail does not say it was cut");
}

/// A secret used as a key under a secret's name is redacted, as a key of a
/// known format is anywhere in it; a key that reads as a name stays.
#[test]
fn a_secret_used_as_a_key_under_a_secrets_name_is_redacted() {
    let (shaped, other) = (SHAPES[2], SHAPES[0]);
    let looks = concat!("Zx81kQ0p", "L2mN4bV6");
    let mut held = json!({
        "token": { shaped: 1, "openai": 2 },
        "credentials": { "deep": { other: "x" }, looks: true },
    });
    value(&mut held);
    let text = held.to_string();
    assert!(
        !text.contains(&shaped[7..]),
        "a key of a known format showed"
    );
    assert!(
        !text.contains(&other[4..]),
        "a nested key of a known format showed"
    );
    assert!(
        !text.contains(looks),
        "a key that looks like a credential showed"
    );
    assert!(
        text.contains("\"openai\""),
        "a key that reads as a name was redacted: {text}"
    );
    assert_eq!(
        held["credentials"].as_object().map(serde_json::Map::len),
        Some(2),
        "{text}"
    );
}

/// Two keys that redact to the same text each keep their entry: neither
/// value is lost, nor shown under the other's key.
#[test]
fn keys_that_redact_alike_keep_both_entries() {
    let block = concat!("-----BEGIN PRIVATE", " KEY-----");
    let mut keyed = json!({
        format!("{block}1"): "harmless",
        format!("{block}2"): "rm -rf /",
    });
    value(&mut keyed);
    let fields = keyed.as_object().expect("an object");
    assert_eq!(fields.len(), 2, "an entry was lost: {keyed}");
    let mut values: Vec<&str> = fields.values().filter_map(Value::as_str).collect();
    values.sort_unstable();
    assert_eq!(values, ["harmless", "rm -rf /"]);
    assert!(fields.contains_key("[REDACTED]"), "{keyed}");
    assert!(fields.contains_key("[REDACTED] (2)"), "{keyed}");
    // Assignments in keys, the same way.
    let mut assigned = json!({"password=1": "a", "password=2": "b"});
    value(&mut assigned);
    assert_eq!(
        assigned.as_object().expect("an object").len(),
        2,
        "{assigned}"
    );
    // Redacting again changes nothing.
    let again = keyed.clone();
    value(&mut keyed);
    assert_eq!(keyed, again);
}

/// A line is written as it was put together, and bounded: atomic's words
/// and the names in it are never redacted, so a run named for a secret
/// reads as it is.
#[test]
fn a_line_is_written_as_it_was_put_together() {
    let line = Line::new()
        .plain("run ")
        .shown("rotate-secret")
        .plain(": waits at gate ")
        .shown("password-reset");
    assert_eq!(
        line.written(),
        "run rotate-secret: waits at gate password-reset"
    );
    let long = Line::new().shown(&"x".repeat(MAX_TEXT + 10));
    assert_eq!(long.written().chars().count(), MAX_TEXT + 1);
    assert!(long.written().ends_with('…'));
}

/// Many keys that redact alike are numbered in linear time: each name keeps
/// its next number rather than counting up from two again.
#[test]
fn numbering_keys_that_redact_alike_stays_linear() {
    let keyed = |n: usize| {
        let fields: serde_json::Map<String, Value> =
            (0..n).map(|i| (format!("secret=a{i}"), json!(i))).collect();
        Value::Object(fields)
    };
    let steps = |mut v: Value| {
        STEPS.with(|s| s.set(0));
        value(&mut v);
        STEPS.with(std::cell::Cell::get)
    };
    let (small, large) = (steps(keyed(500)), steps(keyed(8_000)));
    assert!(
        large < 32 * small,
        "{large} steps at sixteen times the keys of {small}"
    );
    // The same under a secret's name, where keys are held.
    let held = |n: usize| json!({ "token": keyed(n) });
    let (small, large) = (steps(held(500)), steps(held(8_000)));
    assert!(
        large < 32 * small,
        "held: {large} steps at sixteen times the keys of {small}"
    );
}

/// A numbered key's value is judged by its name both with and without its
/// number, the surer reading taken: a secret's name stays one, a count's
/// name alone keeps its count, and a count that redaction numbered hides
/// it. Redacting again then changes nothing.
#[test]
fn a_numbered_keys_value_is_judged_by_its_name() {
    let (a, b) = (SHAPES[0], SHAPES[1]);
    let mut held = json!({
        format!("{a} dbpassword"): "hunter2",
        format!("{b} dbpassword"): "hunter3",
    });
    value(&mut held);
    let fields = held.as_object().expect("an object");
    assert_eq!(fields.len(), 2, "{held}");
    assert!(
        fields.values().all(|v| v == REDACTED),
        "a numbered secret's value showed"
    );
    let mut counted = json!({
        format!("{a} token_count"): 12,
        format!("{b} token_count"): 13,
    });
    value(&mut counted);
    let fields = counted.as_object().expect("an object");
    assert_eq!(fields.len(), 2, "{counted}");
    let name = format!("{REDACTED} token_count");
    assert!(
        matches!(fields.get(&name).and_then(Value::as_i64), Some(12 | 13)),
        "the count under its name alone was hidden: {counted}"
    );
    // Numbered, it reads as a token's name, as a second pass reads it.
    assert_eq!(
        fields.get(&format!("{name} (2)")),
        Some(&json!(REDACTED)),
        "{counted}"
    );
    let mut again = counted.clone();
    value(&mut again);
    assert_eq!(again, counted, "a second pass changed it");
}

/// A key numbered already, fed back in, is judged by its whole name and its
/// name alone, and a second pass is the last that changes anything.
#[test]
fn a_numbered_key_fed_back_in_settles_after_one_more_pass() {
    let mut once = json!({
        format!("{REDACTED} token_count"): 12,
        format!("{REDACTED} token_count (2)"): 13,
        format!("{REDACTED}.token (2)"): "hunter2",
        format!("{REDACTED} password_count (2)"): "hunter3",
        "token": {"pass (2)": "abc", "pass (2) (3)": "abd"},
    });
    value(&mut once);
    let shown = once.to_string();
    for secret in ["hunter2", "hunter3", "\"abc\"", "\"abd\""] {
        assert!(!shown.contains(secret), "a secret showed");
    }
    let mut twice = once.clone();
    value(&mut twice);
    assert_eq!(twice, once, "a second pass changed it");
}

/// Under a secret's name, a key that redacts as text and is then hidden
/// whole still holds its value as what it said redacted: the value is not
/// left to the weaker name it is under.
#[test]
fn a_held_key_hidden_whole_holds_its_value_as_its_text_form_says() {
    let mut out = json!({"token": {"password=> secret.txt": "wordy"}});
    value(&mut out);
    assert!(!out.to_string().contains("wordy"), "the value showed");
    assert!(!out.to_string().contains("secret.txt"), "the key showed");
}

/// Under a secret's name, a credential with white space or a separator
/// around it is hidden as one.
#[test]
fn a_padded_held_credential_key_is_hidden() {
    let credential = concat!("Zx81", "kQ0pL2mN4bV6");
    for (case, key) in [
        format!(" {credential}"),
        format!("{credential}\n"),
        format!(": {credential}"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut out = json!({"token": {key: 1}});
        value(&mut out);
        assert!(
            !out.to_string().contains(&credential[4..]),
            "case {case}: a credential showed"
        );
    }
}

/// Two held keys that redact alike and end in a long number settle in one
/// pass: the padded token in each is hidden whole before they are
/// numbered, so no number is left to read as a credential on a second.
#[test]
fn held_keys_numbered_after_a_long_number_settle_in_one_pass() {
    let other = concat!("ghp_", "zyxwvutsrqponmlkjihgfedcba9876543210");
    let mut once = json!({"token": {
        format!(" {SAMPLE} (123456)"): 1,
        format!(" {other} (123456)"): 2,
    }});
    value(&mut once);
    assert!(!once.to_string().contains(&SAMPLE[4..]), "a token showed");
    let mut twice = once.clone();
    value(&mut twice);
    assert_eq!(twice, once, "a second pass changed it");
}

/// The same object numbers its entries the same, whatever order its keys
/// were put in.
#[test]
fn keys_that_redact_alike_are_numbered_in_their_own_order() {
    let block = concat!("-----BEGIN PRIVATE", " KEY-----");
    let one = format!("{block}1");
    let two = format!("{block}2");
    let mut forwards = serde_json::Map::new();
    forwards.insert(one.clone(), json!("first"));
    forwards.insert(two.clone(), json!("second"));
    let mut backwards = serde_json::Map::new();
    backwards.insert(two, json!("second"));
    backwards.insert(one, json!("first"));
    let (mut f, mut b) = (Value::Object(forwards), Value::Object(backwards));
    value(&mut f);
    value(&mut b);
    assert_eq!(f, b);
    assert_eq!(f["[REDACTED]"], "first", "{f}");
    assert_eq!(f["[REDACTED] (2)"], "second", "{f}");
}

/// Under a secret's name, a key that names a secret stays, to say what its
/// value is, and its value is held as surely as its name says; a key of a
/// known format redacted as a whole still holds its value as its name did.
#[test]
fn a_held_key_that_names_a_secret_stays_and_holds_its_value() {
    let mut named =
        json!({"credentials": {"DB_PASSWORD": "hunter2", "oauth2ClientSecret": "hunter3"}});
    value(&mut named);
    assert_eq!(
        named,
        json!({"credentials": {"DB_PASSWORD": "[REDACTED]", "oauth2ClientSecret": "[REDACTED]"}})
    );
    // A known format whose token runs on into a secret's name: the key goes
    // whole, and the value is still held as a secret's.
    let mut shaped = json!({"token": {format!("{}_secret", SHAPES[0]): "hunter2"}});
    value(&mut shaped);
    assert_eq!(shaped, json!({"token": {"[REDACTED]": "[REDACTED]"}}));
}

/// Under a secret's name, keys that are names stay: environment variables,
/// model ids, emails and references, so a person sees which secrets a step
/// is given.
#[test]
fn held_keys_that_are_names_stay() {
    let names = [
        "OPENAI_API_KEY",
        "GITHUB_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "DATABASE_URL",
        "NODE_ENV",
        "S3_BUCKET",
        "OAUTH2_SCOPE",
        "claude-3-5-sonnet-20241022",
        "dev@example.com",
        "${DATABASE_URL}",
    ];
    let fields: serde_json::Map<String, Value> = names
        .iter()
        .map(|n| ((*n).to_owned(), json!("value")))
        .collect();
    let mut held = json!({ "secrets": Value::Object(fields) });
    value(&mut held);
    let kept = held["secrets"].as_object().expect("an object");
    let lost: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| !kept.contains_key(*n))
        .collect();
    assert!(lost.is_empty(), "{lost:?}");
}

/// Under a secret's name, a key that redacts as text does keeps the rest of
/// itself; two that come out alike are numbered, and each judged by its
/// name before its number.
#[test]
fn held_keys_redact_as_text_and_are_numbered_alike() {
    let (a, b) = (SHAPES[0], SHAPES[1]);
    let mut held = json!({"token": {
        format!("Bearer {a}"): 1,
        format!("{a} dbpassword"): "hunter2",
        format!("{b} dbpassword"): "hunter3",
    }});
    value(&mut held);
    // A key that starts with a known format is hidden whole, the others
    // numbered after it, and each value held as its original key's name
    // says: `dbpassword` names a secret.
    assert_eq!(
        held,
        json!({"token": {
            "Bearer [REDACTED]": 1,
            "[REDACTED]": "[REDACTED]",
            "[REDACTED] (2)": "[REDACTED]",
        }}),
    );
    // A numbered key that comes in so, as from an output redacted before,
    // is judged by its name before its number too.
    let mut again = json!({"token": {"[REDACTED] dbpassword (2)": "hunter3"}});
    value(&mut again);
    assert_eq!(
        again,
        json!({"token": {"[REDACTED] dbpassword (2)": "[REDACTED]"}})
    );
}

/// Redacting a value redacted already changes nothing, held keys included.
#[test]
fn held_keys_redacted_again_change_nothing() {
    // A key redacted in part already, which would otherwise look like a
    // credential itself, is left as it is.
    let partly = json!({"token": {"x9Q2[REDACTED]": 1}});
    let mut kept = partly.clone();
    value(&mut kept);
    assert_eq!(kept, partly);
    let mut once = json!({"secrets": {"pwd=abc": 1, "k": {"pwd=def": 2}}, "pwd=ghi": 3});
    value(&mut once);
    let mut twice = once.clone();
    value(&mut twice);
    assert_eq!(once, twice);
}

/// Under a secret's name, a key that looks like a credential is redacted
/// whatever word it spells, as vendors' keys do, and so is a UUID, a TOTP
/// seed or an upper-case hex key: a key hidden is better than a secret
/// shown. A secret's name still holds its value.
#[test]
fn held_keys_shaped_like_credentials_are_redacted_whatever_they_spell() {
    let keys = [
        concat!("secret_", "Zx81kQ0pL2mN4bV6Zx81kQ0pL2mN4bV6Zx81kQ0pL2m"),
        concat!("key-", "3f9a1c7e5b2d46803f9a1c7e5b2d4680"),
        concat!("pat-na1-", "3f9a1c7e-5b2d-4680-9a1c-7e5b2d46803f"),
        concat!("API_KEY_", "Zx81kQ0pL2mN4bV6"),
        "123e4567-e89b-12d3-a456-426614174000",
        concat!("JBSWY3DP", "EHPK3PXP"),
        concat!("DEADBEEF", "0123456789ABCDEF"),
    ];
    let fields: serde_json::Map<String, Value> = keys
        .iter()
        .map(|k| ((*k).to_owned(), json!("hunter2")))
        .collect();
    let mut held = json!({ "token": Value::Object(fields) });
    value(&mut held);
    let shown = held.to_string();
    let wrong: Vec<usize> = keys
        .iter()
        .enumerate()
        .filter(|(_, k)| shown.contains(*k))
        .map(|(i, _)| i)
        .collect();
    assert!(wrong.is_empty(), "keys {wrong:?} showed");
    // A secret's name stays readable, and holds its value.
    let mut named =
        json!({"credentials": {"DB_PASSWORD": "hunter2", "oauth2ClientSecret": "hunter3"}});
    value(&mut named);
    assert_eq!(
        named,
        json!({"credentials": {"DB_PASSWORD": "[REDACTED]", "oauth2ClientSecret": "[REDACTED]"}})
    );
}

/// A held key redacted in part already still redacts as text: what is left
/// of it, a key of a known format, is redacted too.
#[test]
fn a_held_key_redacted_in_part_still_redacts_what_is_left() {
    let (a, b) = (SHAPES[2], SHAPES[0]);
    let mut held = json!({"token": {
        format!("[REDACTED] {a}"): 1,
        format!("sid=[REDACTED]; tok={b}"): 2,
    }});
    value(&mut held);
    let shown = held.to_string();
    assert!(!shown.contains(&a[7..]), "a key's remainder showed");
    assert!(!shown.contains(&b[4..]), "a key's remainder showed");
}

/// JSON in a string held under a secret's name has a name and value pair's
/// secret redacted, as anywhere else: the pair rules hold under a secret's
/// name too.
#[test]
fn a_pair_in_json_held_under_a_secrets_name_is_redacted() {
    let mut held =
        json!({"credentials": "[ { \"name\": \"DB_PASSWORD\", \"value\": \"hunter2\" } ]"});
    value(&mut held);
    assert!(!held.to_string().contains("hunter2"), "{held}");
    let mut real = json!({"credentials": [{"name": "DB_PASSWORD", "value": "hunter2"}],
                          "token": {"Cookie": "sid=q9z"}});
    value(&mut real);
    assert!(!real.to_string().contains("hunter2"), "{real}");
    // A header's value, which only the header's rule takes for a secret.
    assert!(!real.to_string().contains("q9z"), "{real}");
}

/// A JSON string under a secret's name is hidden whole, as any string
/// there is, however its JSON reads: what text redaction rewrites, a key
/// given twice, and JSON as deep as can be opened.
#[test]
fn json_under_a_secrets_name_is_hidden_whole() {
    let mut deep = json!("hunter2");
    for _ in 0..MAX_NESTED_JSON + 1 {
        deep = json!({ "k": deep.to_string() });
    }
    for held in [
        json!({"password": "{\"password\": 1, \"x\": \"hunter2\"}"}),
        json!({"password": "{\"a\": \"hunter2\", \"a\": true}"}),
        json!({"secret": "{\"k\": \"v\"}"}),
        json!({ "password": deep.to_string() }),
    ] {
        let mut out = held.clone();
        value(&mut out);
        let field = out.as_object().and_then(|o| o.values().next()).cloned();
        assert_eq!(field, Some(json!(REDACTED)), "{out}");
    }
}

/// Under a secret's name, the value after a secret-named flag is redacted.
#[test]
fn a_flag_value_under_a_secrets_name_is_redacted() {
    let mut held = json!({"token": ["--password", "hunter2"]});
    value(&mut held);
    assert_eq!(held, json!({"token": ["--password", REDACTED]}));
}

/// A key that only looks numbered is read whole too, so a secret's name
/// with a number after it holds its value as surely as the name alone
/// would, at the top level and under a secret's name.
#[test]
fn a_key_that_looks_numbered_holds_its_value_as_its_whole_name_says() {
    for (case, held) in [
        json!({"token (2)": "hunter2", "pass (2)": "abc"}),
        json!({"secrets": {"token (2)": "hunter2", "pass (2)": "abc"}}),
        json!({"token": {"pass (2)": "abc"}}),
        json!({format!("{REDACTED}.token (2)"): "hunter2"}),
        json!({format!("{REDACTED} password_count (2)"): "hunter2"}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut out = held.clone();
        value(&mut out);
        let shown = out.to_string();
        assert!(!shown.contains("hunter2"), "case {case}: a secret showed");
        assert!(!shown.contains("\"abc\""), "case {case}: a secret showed");
    }
}

/// Under a secret's name, a key that names a secret and had its value
/// redacted stays so when redacted again.
#[test]
fn a_held_name_and_value_key_redacted_again_stays() {
    for (case, key) in ["password= hunter2x", "password=\"hunter2 x\""]
        .into_iter()
        .enumerate()
    {
        let mut once = json!({"token": {key: 1}});
        value(&mut once);
        assert!(
            !once.to_string().contains("hunter2"),
            "case {case}: a secret showed"
        );
        let mut twice = once.clone();
        value(&mut twice);
        assert_eq!(once, twice, "case {case}: a second pass changed it");
    }
}

/// Under a secret's name, a key with no marker in it is judged whole: one
/// that looks like a credential only with its padding is hidden.
#[test]
fn a_held_key_with_no_marker_is_judged_whole() {
    let mut out = json!({"token": {"MFRGGZDFMZTWQ===": 1}});
    value(&mut out);
    let keys: Vec<&String> = out["token"]
        .as_object()
        .expect("an object")
        .keys()
        .collect();
    assert_eq!(keys, [REDACTED], "{out}");
}
