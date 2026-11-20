// SPDX-License-Identifier: Apache-2.0

//! Secrets out of what atomic writes about itself: its log lines, its spans'
//! attributes, and error messages that may carry a provider's response.
//!
//! A secret is found by its shape (the formats of well-known API keys and
//! tokens, JSON Web Tokens, private key blocks) or by a name beside it: a
//! value assigned to a name with a secret word in it (`key`, `secret`,
//! `token`, `pass`, `pat`, `signature` and the like, as a part of the name
//! such as `AWS_SECRET_KEY` or `AccountKey`), an authorization or cookie
//! header, a password or token in a URL, a value after a secret-named
//! command-line flag, a secret-named field in JSON whatever its value, and a
//! header given as a name and value pair.
//!
//! The known-format finder and its table of prefixes are adapted from
//! rig-core's redaction module (MIT); see THIRD-PARTY-NOTICES.md.
//!
//! This is best effort, and it over-matches on purpose: a password in prose, a
//! bare random string or anything encoded is not found, and a wrong match only
//! loses a little text from a log. So it is applied to what atomic reports
//! about a run, never to what a model reads or writes, where it would damage
//! code.

use regex::{Captures, Regex};
use serde_json::Value;
use std::borrow::Cow;
use std::sync::LazyLock;

#[cfg(test)]
mod tests;

/// What a secret is replaced with.
pub const REDACTED: &str = "[REDACTED]";

/// Parts of a name that say its value is secret. A name is split into parts
/// at punctuation and at a change from lower to upper case.
const SECRET_PARTS: &[&str] = &[
    "key",
    "secret",
    "secrets",
    "token",
    "pass",
    "passwd",
    "password",
    "passphrase",
    "pwd",
    "pat",
    "signature",
    "sig",
    "credential",
    "credentials",
    "authorization",
    "cookie",
];
/// Endings of a part that run secret words together, such as `apikey` or
/// `accesstoken`.
const SECRET_ENDINGS: &[&str] = &[
    "apikey",
    "accesskey",
    "secretkey",
    "privatekey",
    "authkey",
    "accountkey",
    "subscriptionkey",
    "token",
    "secret",
    "password",
    "passwd",
];
/// Parts that say a name is about a secret rather than one: its size, its
/// kind, where it is kept. `max_tokens`, `token_count`, `api_key_env` and
/// `token_file` hold no secret.
const NOT_SECRET_PARTS: &[&str] = &[
    "count",
    "max",
    "min",
    "limit",
    "usage",
    "total",
    "len",
    "length",
    "size",
    "budget",
    "input",
    "output",
    "prompt",
    "completion",
    "cached",
    "reasoning",
    "type",
    "kind",
    "name",
    "id",
    "url",
    "uri",
    "endpoint",
    "env",
    "file",
    "path",
    "expires",
    "expiry",
    "ttl",
    "timeout",
    "header",
];
/// How deep JSON text inside JSON strings is opened. Each level doubles its
/// escaping, and opening without a bound could overflow the stack.
const MAX_NESTED_JSON: usize = 4;

/// Token formats of well-known providers: a prefix, and the least number of
/// characters after it.
const KEY_PREFIXES: &[(&str, usize)] = &[
    ("sk-ant-", 16),
    ("gsk_", 20),
    ("xai-", 20),
    ("nvapi-", 20),
    ("npm_", 30),
    ("whsec_", 20),
    ("xoxe-", 10),
    ("xapp-", 10),
    ("sk-proj-", 16),
    ("sk-svcacct-", 16),
    ("sk-", 32),
    ("AIza", 30),
    ("github_pat_", 22),
    ("ghp_", 30),
    ("gho_", 30),
    ("ghu_", 30),
    ("ghs_", 30),
    ("ghr_", 30),
    ("glpat-", 20),
    ("xoxa-", 10),
    ("xoxb-", 10),
    ("xoxp-", 10),
    ("xoxr-", 10),
    ("xoxs-", 10),
    ("sk_live_", 16),
    ("sk_test_", 16),
    ("rk_live_", 16),
    ("rk_test_", 16),
    ("hf_", 30),
];

/// Formats whose prefix ends in a dot: Google OAuth access tokens and
/// SendGrid keys (which have a second dotted part).
const DOTTED_PREFIXES: &[(&str, usize)] = &[("ya29.", 20), ("SG.", 16)];

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("these patterns are fixed, and a test compiles them all")
}

/// `name = value` or `name: value`, with the value quoted or not, including
/// JSON whose quotes are escaped because it sits inside another string, and
/// Rust's `Some(value)`. Whether the name is a secret one is decided in code.
static ASSIGNED: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"([A-Za-z0-9_.-]+)(\\*["']?[ \t]*[=:][ \t]*(?:Some\()?)(?:(\\*")([^"\n]*?)(\\*")|(\\*')([^'\n]*?)(\\*')|([^\s"'`\\]+))"#,
    )
});
static FLAG: LazyLock<Regex> = LazyLock::new(|| {
    regex(r#"(\s--?([A-Za-z0-9][A-Za-z0-9-]*)\s+)("[^"\n]*"|'[^'\n]*'|[^\s"'-][^\s"']*)"#)
});
static HEADER: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?i)((?:proxy-)?authorization|(?:set-)?cookie|x-api-key|x-auth-token)(\\*["']?\s*:\s*\\*["']?)([^\n"'\\]+)"#,
    )
});
static SCHEME: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)\b(bearer|basic)(\s+)([a-z0-9._~+/=-]{8,})"));
static URL_PASSWORD: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)\b([a-z][a-z0-9+.-]*://[^\s/:@'"]*:)([^\s/@'"]+)(@)"#));
/// A URL whose user is a token, as `https://TOKEN@host` gives one to git.
static URL_USERINFO: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)\b([a-z][a-z0-9+.-]*://)([^\s/:@'"]+)(@)"#));
/// The next assignment in a run of them, such as `&page=2` in a query.
static NEXT_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| regex(r"^[A-Za-z_][A-Za-z0-9_.-]*[=:]"));
/// References to a secret rather than the secret: `$NAME`, `${NAME}`,
/// `${{ expression }}`, `%NAME%`, `{{ template }}`, `<placeholder>` and
/// masks such as `***`.
static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^(?:\$\{?[A-Z_][A-Z0-9_]*\}?|\$\{\{[^{}]*\}\}|%[A-Za-z_][A-Za-z0-9_]*%|\{\{[^{}]*\}\}|<[A-Za-z _-]+>|\*{3,})$",
    )
});
static PRIVATE_KEY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^-----BEGIN ([A-Z ]*PRIVATE KEY)-----"));

/// The parts of a name, lower-cased: split at punctuation and where lower
/// case turns to upper, so `AWS_SECRET_KEY`, `AccountKey` and
/// `X-Amz-Signature` all have their secret word as a part.
fn name_parts(name: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut previous_lower = false;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            parts.extend((!part.is_empty()).then(|| std::mem::take(&mut part)));
            previous_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && previous_lower {
            parts.push(std::mem::take(&mut part));
        }
        previous_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        part.push(c.to_ascii_lowercase());
    }
    parts.extend((!part.is_empty()).then_some(part));
    parts
}

/// Whether a value under `name` is a secret, whatever it looks like.
pub fn is_secret_name(name: &str) -> bool {
    let parts = name_parts(name);
    parts.iter().any(|part| {
        SECRET_PARTS.contains(&part.as_str())
            || SECRET_ENDINGS.iter().any(|end| part.ends_with(end))
    }) && !parts
        .iter()
        .any(|part| NOT_SECRET_PARTS.contains(&part.as_str()))
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// The length of the token at the start of `s`.
fn token_len(s: &str) -> usize {
    s.find(|c: char| !is_token_char(c)).unwrap_or(s.len())
}

/// How long the known-format secret starting `rest` is, if one does.
fn known_shape(rest: &str) -> Option<usize> {
    for (prefix, least) in DOTTED_PREFIXES {
        if let Some(tail) = rest.strip_prefix(prefix) {
            let mut len = token_len(tail);
            if len < *least {
                continue;
            }
            // A second dotted part, as SendGrid's keys have.
            if tail[len..].starts_with('.') && token_len(&tail[len + 1..]) >= *least {
                len += 1 + token_len(&tail[len + 1..]);
            }
            return Some(prefix.len() + len);
        }
    }
    let token = &rest[..token_len(rest)];
    for (prefix, least) in KEY_PREFIXES {
        if let Some(tail) = token.strip_prefix(prefix)
            && tail.len() >= *least
            && tail.chars().any(|c| c.is_ascii_alphabetic())
        {
            return Some(token.len());
        }
    }
    // An AWS access key ID: four letters, then sixteen upper-case letters or
    // digits.
    if token.len() == 20
        && (token.starts_with("AKIA") || token.starts_with("ASIA"))
        && token[4..]
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return Some(20);
    }
    // A JSON Web Token: two base64url segments that open with `{"`, and a
    // signature, joined by dots.
    let mut at = 0;
    for _ in 0..2 {
        let part = &rest[at..];
        let len = token_len(part);
        if !part.starts_with("eyJ") || len < 10 || !part[len..].starts_with('.') {
            return None;
        }
        at += len + 1;
    }
    Some(at + token_len(&rest[at..]))
}

/// Replace each private key block, and each token of a known format, with
/// [`REDACTED`]. A key block with no end runs to the end of the text.
fn shapes(input: &str) -> Cow<'_, str> {
    let mut out = String::new();
    let mut copied = 0;
    let mut at = 0;
    let mut previous: Option<char> = None;
    while at < input.len() {
        let rest = &input[at..];
        // Checked only where a block starts, so the scan stays linear.
        if let Some(found) = rest
            .starts_with("-----BEGIN ")
            .then(|| PRIVATE_KEY.captures(rest))
            .flatten()
        {
            let end = format!("-----END {}-----", &found[1]);
            let len = rest.find(&end).map_or(rest.len(), |i| i + end.len());
            out.push_str(&input[copied..at]);
            out.push_str(REDACTED);
            at += len;
            copied = at;
            previous = None;
            continue;
        }
        let c = rest.chars().next().unwrap_or_default();
        // A token starts where the character before it cannot be part of one
        // (nor of a path or base64 text, such as `/` or `+`).
        let starts = previous.is_none_or(|p| !is_token_char(p) && p != '/' && p != '+');
        if starts && is_token_char(c) {
            if let Some(len) = known_shape(rest) {
                out.push_str(&input[copied..at]);
                out.push_str(REDACTED);
                at += len;
                copied = at;
                previous = input[..at].chars().next_back();
                continue;
            }
            // Not a secret: move past the whole token.
            let len = token_len(rest).max(c.len_utf8());
            previous = rest[..len].chars().next_back();
            at += len;
            continue;
        }
        previous = Some(c);
        at += c.len_utf8();
    }
    if copied == 0 {
        return Cow::Borrowed(input);
    }
    out.push_str(&input[copied..]);
    Cow::Owned(out)
}

/// A value next to a secret name that is plainly not the secret: a reference
/// to one, a type, or a placeholder.
fn is_not_a_secret(value: &str) -> bool {
    const WORDS: [&str; 13] = [
        "string",
        "str",
        "boolean",
        "bool",
        "number",
        "int",
        "null",
        "none",
        "true",
        "false",
        "undefined",
        "required",
        "optional",
    ];
    value.is_empty()
        || value == REDACTED
        || REFERENCE.is_match(value)
        || WORDS.iter().any(|word| value.eq_ignore_ascii_case(word))
}

/// Text that is code rather than a value, which is common beside a word like
/// `token`: a call, a path, a generic type or a reference.
fn looks_like_code(value: &str) -> bool {
    value.ends_with("()")
        || value.contains("::")
        || value.contains("->")
        || value.starts_with('&')
        || value.starts_with("self.")
        || (value.contains('<') && value.ends_with('>'))
}

/// Where a bare value ends: at a `,`, `;` or `&` that ends the text or
/// starts the next assignment (so a query's next parameter is kept), and
/// before closing brackets it did not open.
fn bare_value_len(value: &str) -> usize {
    let mut end = value.len();
    for (i, c) in value.char_indices() {
        if matches!(c, ',' | ';' | '&') {
            let after = &value[i + 1..];
            if after.is_empty() || NEXT_ASSIGNMENT.is_match(after) {
                end = i;
                break;
            }
        }
    }
    let mut value = &value[..end];
    loop {
        let unopened = |close: char, open: char| {
            value.ends_with(close) && value.matches(close).count() > value.matches(open).count()
        };
        if unopened(')', '(') || unopened(']', '[') || unopened('}', '{') {
            value = &value[..value.len() - 1];
        } else {
            return value.len();
        }
    }
}

/// Redact the values assigned to secret names in `input`. The search goes on
/// after each value, or after each name that is not a secret one, so a value
/// that holds another assignment is searched too.
fn assigned(input: &str) -> Option<String> {
    let mut out = String::new();
    let mut copied = 0;
    let mut at = 0;
    while let Some(captures) = ASSIGNED.captures_at(input, at) {
        let (Some(name), Some(prefix)) = (captures.get(1), captures.get(2)) else {
            break;
        };
        let mut resume = prefix.end();
        if is_secret_name(name.as_str()) {
            let quoted = [(3, 4, 5), (6, 7, 8)]
                .into_iter()
                .find_map(|(open, value, close)| {
                    captures.get(open)?;
                    captures.get(close)?;
                    captures.get(value)
                });
            let value = match (quoted, captures.get(9)) {
                (Some(value), _) => Some((value.start(), value.as_str())),
                (None, Some(bare)) => {
                    // A template, `{{ x }}` or `${{ x }}`, may hold spaces.
                    let rest = &input[bare.start()..];
                    let template = (rest.starts_with("{{") || rest.starts_with("${{"))
                        .then(|| rest.find("}}").map(|end| &rest[..end + 2]))
                        .flatten()
                        .filter(|t| !t.contains('\n'));
                    let value =
                        template.unwrap_or_else(|| &bare.as_str()[..bare_value_len(bare.as_str())]);
                    Some((bare.start(), value))
                }
                (None, None) => None,
            };
            if let Some((start, value)) = value {
                let plain = quoted.is_some()
                    || !(value.starts_with([':', '=', '>']) || looks_like_code(value));
                if plain && !is_not_a_secret(value) {
                    out.push_str(&input[copied..start]);
                    out.push_str(REDACTED);
                    copied = start + value.len();
                }
                resume = resume.max(start + value.len());
            }
        }
        at = resume;
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&input[copied..]);
    Some(out)
}

fn redact_flag(captures: &Captures<'_>) -> String {
    let value = captures[3].trim_matches(['"', '\'']);
    if !is_secret_name(&captures[2]) || is_not_a_secret(value) {
        return captures[0].to_string();
    }
    format!("{}{REDACTED}", &captures[1])
}

/// "bearer authentication" is prose: a bearer token has a digit or is long,
/// and Basic credentials are base64, padded to a multiple of four.
fn redact_scheme(captures: &Captures<'_>) -> String {
    let value = &captures[3];
    let tokenish = if captures[1].eq_ignore_ascii_case("basic") {
        (value.len().is_multiple_of(4) || value.ends_with('='))
            && !value.bytes().all(|byte| byte.is_ascii_lowercase())
    } else {
        value.bytes().any(|byte| byte.is_ascii_digit()) || value.len() >= 20
    };
    if !tokenish || is_not_a_secret(value) {
        return captures[0].to_string();
    }
    format!("{}{}{REDACTED}", &captures[1], &captures[2])
}

/// A URL's user that is a token rather than a name: long, with letters and
/// digits.
fn redact_userinfo(captures: &Captures<'_>) -> String {
    let user = &captures[2];
    let tokenish = user.len() >= 12
        && user.bytes().any(|b| b.is_ascii_digit())
        && user.bytes().any(|b| b.is_ascii_alphabetic());
    if !tokenish || is_not_a_secret(user) {
        return captures[0].to_string();
    }
    format!("{}{REDACTED}{}", &captures[1], &captures[3])
}

/// Replaces secrets in text.
pub fn text(input: &str) -> Cow<'_, str> {
    let mut output = shapes(input);
    let mut apply = |rule: &Regex, replace: &dyn Fn(&Captures<'_>) -> String| {
        // Most text has nothing, so this avoids building a new string for
        // each rule.
        if rule.is_match(&output) {
            let replaced = rule.replace_all(&output, replace).into_owned();
            output = Cow::Owned(replaced);
        }
    };
    apply(&URL_PASSWORD, &|c| {
        if is_not_a_secret(&c[2]) {
            c[0].to_string()
        } else {
            format!("{}{REDACTED}{}", &c[1], &c[3])
        }
    });
    apply(&URL_USERINFO, &redact_userinfo);
    // The whole value, even when part of it was redacted already: the rest
    // of a cookie is as secret as its first part.
    apply(&HEADER, &|c| format!("{}{}{REDACTED}", &c[1], &c[2]));
    apply(&SCHEME, &redact_scheme);
    apply(&FLAG, &redact_flag);
    if let Some(replaced) = assigned(&output) {
        output = Cow::Owned(replaced);
    }
    output
}

fn redact_string(string: &mut String, nested: usize) {
    if let Cow::Owned(redacted) = text(string) {
        *string = redacted;
    }
    // JSON inside a string, such as a model's raw tool arguments: an escape
    // can hide where a secret starts, and a secret-named field is only clear
    // once parsed.
    if nested < MAX_NESTED_JSON
        && string.trim_start().starts_with(['{', '['])
        && let Ok(mut inner) = serde_json::from_str::<Value>(string)
    {
        let before = inner.clone();
        redact_nested(&mut inner, nested + 1);
        if inner != before {
            *string = inner.to_string();
        }
    }
}

/// Redact every string and number in a value held under a secret name,
/// keeping its shape.
fn redact_all(value: &mut Value) {
    match value {
        Value::String(string) if is_not_a_secret(string) => {}
        Value::String(_) | Value::Number(_) => *value = Value::String(REDACTED.into()),
        Value::Array(items) => items.iter_mut().for_each(redact_all),
        Value::Object(fields) => fields.values_mut().for_each(redact_all),
        Value::Null | Value::Bool(_) => {}
    }
}

/// A header as a pair, `["Authorization", "..."]`, or as an object,
/// `{"name": "Authorization", "value": "..."}`: the value whose name is a
/// secret one.
fn header_pair(value: &mut Value) -> Option<&mut Value> {
    match value {
        Value::Array(items) if items.len() == 2 => {
            let named = items[0].as_str().is_some_and(is_secret_name);
            named.then(|| &mut items[1])
        }
        Value::Object(fields) => {
            let named = ["name", "key", "header"].iter().any(|field| {
                fields
                    .get(*field)
                    .and_then(Value::as_str)
                    .is_some_and(is_secret_name)
            });
            if named { fields.get_mut("value") } else { None }
        }
        _ => None,
    }
}

fn redact_nested(value: &mut Value, nested: usize) {
    if let Some(secret) = header_pair(value) {
        redact_all(secret);
    }
    match value {
        Value::String(string) => redact_string(string, nested),
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_nested(item, nested)),
        Value::Object(fields) => {
            let renamed: Vec<(String, String)> = fields
                .keys()
                .filter_map(|key| match text(key) {
                    Cow::Owned(new_key) => Some((key.clone(), new_key)),
                    Cow::Borrowed(_) => None,
                })
                .collect();
            for (old_key, new_key) in renamed {
                if let Some(field) = fields.remove(&old_key) {
                    fields.insert(new_key, field);
                }
            }
            for (key, field) in fields.iter_mut() {
                if is_secret_name(key) {
                    redact_all(field);
                } else {
                    redact_nested(field, nested);
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Replaces secrets in every key and string of a JSON value.
pub fn value(value: &mut Value) {
    redact_nested(value, 0);
}

/// Write one log line to stderr, redacted. atomic's own log lines go
/// through this, never straight to `eprintln!`.
pub fn log(line: impl AsRef<str>) {
    eprintln!("{}", text(line.as_ref()));
}

/// A writer that redacts each line before it reaches stderr, for code that
/// writes its log lines to a `Write` it is given. A line is written once
/// it ends, or when the writer is flushed or dropped.
pub struct Lines {
    pending: Vec<u8>,
}

/// A [`Lines`] writer to this process's stderr.
pub fn stderr_lines() -> Lines {
    Lines {
        pending: Vec::new(),
    }
}

impl Lines {
    fn emit(&mut self, upto: usize) -> std::io::Result<()> {
        let line: Vec<u8> = self.pending.drain(..upto).collect();
        let line = String::from_utf8_lossy(&line);
        let line = line.strip_suffix('\n').unwrap_or(&line);
        use std::io::Write;
        writeln!(std::io::stderr().lock(), "{}", text(line))
    }
}

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            self.emit(end + 1)?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            self.emit(self.pending.len())?;
        }
        Ok(())
    }
}

impl Drop for Lines {
    fn drop(&mut self) {
        use std::io::Write;
        let _ = self.flush();
    }
}

/// Format and write one redacted log line to stderr, as `eprintln!` would.
#[macro_export]
macro_rules! log_line {
    ($($arg:tt)*) => {
        $crate::redact::log(format!($($arg)*))
    };
}

/// What a panic writes, redacted: its thread, where it happened, its message
/// and, when `RUST_BACKTRACE` asks for one, its backtrace.
pub fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("<unnamed>");
    let mut message = format!("thread '{name}' {info}");
    let backtrace = std::backtrace::Backtrace::capture();
    if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
        message.push_str(&format!("\nstack backtrace:\n{backtrace}"));
    }
    text(&message).into_owned()
}

/// Make a panic write [`panic_message`] in place of the standard hook's
/// unredacted text. Call it first thing in `main`.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| log(panic_message(info))));
}
