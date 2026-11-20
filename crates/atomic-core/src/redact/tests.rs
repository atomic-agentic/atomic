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
/// bound a quadratic scan could not meet.
#[test]
fn worst_cases_finish_quickly() {
    let slow: Vec<(&str, std::time::Duration)> = worst_cases(1024 * 1024)
        .into_iter()
        .filter_map(|(name, input)| {
            let start = std::time::Instant::now();
            let _ = text(&input);
            let taken = start.elapsed();
            (taken > std::time::Duration::from_secs(3)).then_some((name, taken))
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
