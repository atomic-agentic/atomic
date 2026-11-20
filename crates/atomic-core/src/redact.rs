// SPDX-License-Identifier: Apache-2.0

//! Secrets out of what atomic writes about itself: its log lines, its spans'
//! attributes, and error messages that may carry a provider's response.
//!
//! A secret is found by its shape (the formats of well-known API keys and
//! tokens, JSON Web Tokens, private key blocks) or by a name beside it: a
//! value assigned to a name that ends in a secret word, an authorization or
//! cookie header, a password in a URL, a value after a secret-named
//! command-line flag, and a secret-named field in JSON.
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

/// Names that say a value is secret, as the end of a longer name.
const SECRET_NAME: &str = r"password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|auth[_-]?key|credentials?";
/// Fields that hold a secret in JSON, whatever the value looks like. Headers
/// are included.
const SECRET_FIELD: &str = r"authorization|cookie|set-cookie";
/// How deep JSON text inside JSON strings is opened. Each level doubles its
/// escaping, and opening without a bound could overflow the stack.
const MAX_NESTED_JSON: usize = 4;

/// Token formats of well-known providers: a prefix, and the least number of
/// characters after it.
const KEY_PREFIXES: &[(&str, usize)] = &[
    ("sk-ant-", 16),
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

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("these patterns are fixed, and a test compiles them all")
}

/// `name = value`, in any case, with the value quoted or not, including JSON
/// whose quotes are escaped because it sits inside another string.
static ASSIGNED: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r#"(?i)([a-z0-9_.-]*(?:{SECRET_NAME})\\*["']?\s*[=:]\s*)(?:(\\*")([^"\n]*?)(\\*")|(\\*')([^'\n]*?)(\\*')|([^\s"'`,;&\\]+))"#
    ))
});
static FLAG: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?i)(\s--?[a-z0-9-]*(?:password|passwd|token|secret|api-?key)\s+)("[^"\n]*"|'[^'\n]*'|[^\s"'-][^\s"']*)"#,
    )
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
static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    regex(&format!(
        r"(?i)^[a-z0-9_.-]*(?:{SECRET_NAME}|{SECRET_FIELD}|x-api-key|x-auth-token)$"
    ))
});
static PRIVATE_KEY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^-----BEGIN ([A-Z ]*PRIVATE KEY)-----"));

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// The length of the token at the start of `s`.
fn token_len(s: &str) -> usize {
    s.find(|c: char| !is_token_char(c)).unwrap_or(s.len())
}

/// How long the known-format secret starting `rest` is, if one does.
fn known_shape(rest: &str) -> Option<usize> {
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
        || value.starts_with('$')
        || value.starts_with('%')
        || value.starts_with("{{")
        || value == REDACTED
        || WORDS.iter().any(|word| value.eq_ignore_ascii_case(word))
}

/// Text that looks like code rather than a value, which is common beside a
/// word like `token`.
fn looks_like_code(value: &str) -> bool {
    value.contains(['(', ')', '<', '>', '[', ']', '{', '}'])
        || value.contains("->")
        || value.contains("::")
}

fn redact_assigned(captures: &Captures<'_>) -> String {
    let name = &captures[1];
    let quoted = [(2, 3, 4), (5, 6, 7)]
        .into_iter()
        .find_map(|(open, value, close)| {
            Some((
                captures.get(open)?,
                captures.get(value)?,
                captures.get(close)?,
            ))
        });
    if let Some((open, value, close)) = quoted {
        return if is_not_a_secret(value.as_str()) {
            captures[0].to_string()
        } else {
            format!("{name}{}{REDACTED}{}", open.as_str(), close.as_str())
        };
    }
    let value = &captures[8];
    // Short bare values next to a name are mostly prose and counts, such as
    // `max_token: 4096`.
    if value.len() < 6
        || value.bytes().all(|byte| byte.is_ascii_digit())
        || is_not_a_secret(value)
        || looks_like_code(value)
    {
        return captures[0].to_string();
    }
    format!("{name}{REDACTED}")
}

fn redact_flag(captures: &Captures<'_>) -> String {
    let value = captures[2].trim_matches(['"', '\'']);
    if is_not_a_secret(value) {
        return captures[0].to_string();
    }
    format!("{}{REDACTED}", &captures[1])
}

/// "bearer authentication" is prose, and a real token has a digit or is long.
fn redact_scheme(captures: &Captures<'_>) -> String {
    let value = &captures[3];
    let tokenish = value.bytes().any(|byte| byte.is_ascii_digit()) || value.len() >= 20;
    if !tokenish || is_not_a_secret(value) {
        return captures[0].to_string();
    }
    format!("{}{}{REDACTED}", &captures[1], &captures[2])
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
    apply(&HEADER, &|c| {
        if c[3].contains(REDACTED) {
            c[0].to_string()
        } else {
            format!("{}{}{REDACTED}", &c[1], &c[2])
        }
    });
    apply(&SCHEME, &redact_scheme);
    apply(&FLAG, &redact_flag);
    apply(&ASSIGNED, &redact_assigned);
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

fn redact_nested(value: &mut Value, nested: usize) {
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
                match field {
                    Value::String(string)
                        if SECRET_KEY.is_match(key) && !is_not_a_secret(string) =>
                    {
                        *string = REDACTED.to_string();
                    }
                    _ => redact_nested(field, nested),
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

/// Format and write one redacted log line to stderr, as `eprintln!` would.
#[macro_export]
macro_rules! log_line {
    ($($arg:tt)*) => {
        $crate::redact::log(format!($($arg)*))
    };
}
