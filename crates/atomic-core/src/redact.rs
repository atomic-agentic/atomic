// SPDX-License-Identifier: Apache-2.0

//! Secrets out of what atomic writes about itself: its log lines, its spans'
//! attributes, and error messages that may carry a provider's response.
//!
//! A secret is found by its shape (the formats of well-known API keys and
//! tokens, JSON Web Tokens, private key blocks), even right after an escape
//! or a terminal colour, or by a name beside it: a value assigned to a
//! secret's name (`password`, `api_key`, `AWS_SECRET_KEY`, `AccountKey`,
//! `DB_PASS`), or a credential-looking value assigned to a name that hints
//! at one (`token`, `pass`); an authorization or cookie header; a password
//! or token in a URL; a value after a secret-named command-line flag; a
//! secret-named field in JSON; and a header given as a name and value pair.
//! Every rule is one pass over the text, and callers bound what they redact
//! ([`excerpt`], [`bounded`], [`tail`]).
//!
//! The known-format finder (its table of prefixes, the JSON Web Token scan,
//! the AWS access key ID check, the private key block finder and the rule
//! for where a token may start) is adapted from rig-core's redaction module
//! (MIT); see THIRD-PARTY-NOTICES.md.
//!
//! This is best effort, and it over-matches on purpose: a password in prose, a
//! bare random string or anything encoded is not found, and a wrong match only
//! loses a little text from a log. So it is applied to what atomic reports
//! about a run, never to what a model reads or writes, where it would damage
//! code.

use regex::{Captures, Regex};
use serde_json::Value;
use std::borrow::Cow;
use std::io::Write as _;
use std::sync::LazyLock;

#[cfg(test)]
mod tests;

/// What a secret is replaced with.
pub const REDACTED: &str = "[REDACTED]";

/// Parts of a name that make it a secret's name on their own: `password`,
/// `DB_PASSWORD`, `client_secret`.
const STRONG_PARTS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "passcode",
    "pwd",
    "secret",
    "secrets",
    "apikey",
];
/// Endings of a name's parts run together that make it a secret's name:
/// `apikey`, `MyPassWord`, `clientsecret`.
const STRONG_ENDINGS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "apikey",
    "accesskey",
    "secretkey",
    "privatekey",
    "accountkey",
    "subscriptionkey",
    "authkey",
    "accesstoken",
    "authtoken",
    "refreshtoken",
];
/// Parts that name a secret when another part qualifies them (`DB_PASS`,
/// `GH_PAT`, `accessToken`, `X-Amz-Signature`), and only hint at one alone
/// (`pass`, `token`).
const WEAK_PARTS: &[&str] = &[
    "credential",
    "credentials",
    "token",
    "pass",
    "pat",
    "signature",
    "sig",
    "auth",
    "cookie",
    "authorization",
];
/// Words that make the `token` right after them a count of tokens or a kind
/// of token rather than a credential: `max_token`, `prompt_token`,
/// `error_token`, `next_token`.
const TOKEN_KINDS: &[&str] = &[
    "max",
    "min",
    "count",
    "total",
    "limit",
    "num",
    "used",
    "prompt",
    "completion",
    "reasoning",
    "error",
    "next",
    "eos",
    "bos",
    "special",
];
/// Words that make the `tokens` right after them a count, though `token`
/// alone after them may be a credential: `input_tokens`, but
/// `--input-token`.
const COUNTED_KINDS: &[&str] = &["input", "output", "cached"];
/// What makes `key` a secret's name: `api_key`, `AccountKey`,
/// `Ocp-Apim-Subscription-Key`. `key` alone, `primary_key` or `KeyError`
/// are not.
const KEY_QUALIFIERS: &[&str] = &[
    "api",
    "access",
    "secret",
    "private",
    "auth",
    "account",
    "subscription",
    "client",
    "master",
    "encryption",
    "signing",
    "sas",
    "storage",
    "app",
    "service",
    "license",
    "ssh",
    "deploy",
    "webhook",
    "session",
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
    "error",
    "method",
    "mode",
    "scheme",
    "provider",
    "source",
    "policy",
    "strategy",
    "format",
    "version",
    "status",
    "state",
];
/// Values that are words, not secrets, even beside a secret's name:
/// `token: expired`, `password: required`, `ExpiredToken: The token…`.
const STOPWORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "not",
    "no",
    "yes",
    "ok",
    "on",
    "off",
    "to",
    "of",
    "for",
    "in",
    "and",
    "or",
    "it",
    "this",
    "that",
    "set",
    "unset",
    "none",
    "null",
    "nil",
    "true",
    "false",
    "undefined",
    "required",
    "optional",
    "missing",
    "invalid",
    "expired",
    "revoked",
    "denied",
    "refused",
    "rejected",
    "failed",
    "empty",
    "unknown",
    "error",
    "provided",
    "present",
    "absent",
    "hidden",
    "redacted",
    "masked",
    "string",
    "str",
    "boolean",
    "bool",
    "number",
    "int",
    "integer",
    "object",
    "array",
    "any",
    "value",
    "default",
];
/// The most of any input atomic redacts in one piece: longer text is cut
/// first (see [`excerpt`]), so a huge provider body or stderr costs little.
pub const MAX_TEXT: usize = 64 * 1024;
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

/// A command-line flag and its value: `--password x`, at the start of the
/// text or after a space, a quote, a bracket or a comma, and as two items of
/// an argument list, `"--token", "abc"`.
static FLAG: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?:^|[\s"'\[,])(--?([A-Za-z0-9][A-Za-z0-9-]*)(?:\s+|["']\s*,\s*["']))("[^"\n]*"|'[^'\n]*'|\[REDACTED\]|[^\s"',\]-][^\s"',\]]*)"#,
    )
});
/// An authorization, cookie or API key header and its value, to the end of
/// the line or the quote around it.
static HEADER: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?i)\b((?:proxy-)?authorization|(?:set-)?cookie|x-api-key|x-auth-token)(\\*["']?[ \t]*:[ \t]*\\*["']?)([^\n"'\\]+)"#,
    )
});
static SCHEME: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)\b(bearer|basic)(\s+)([a-z0-9._~+/=-]{8,})"));
/// A URL's password, to the last `@` before its host.
static URL_PASSWORD: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)\b([a-z][a-z0-9+.-]*://[^\s/:@'"]*:)([^\s/'"]+)(@)"#));
/// A URL whose user is a token, as `https://TOKEN@host` gives one to git.
static URL_USERINFO: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?i)\b([a-z][a-z0-9+.-]*://)([^\s/:@'"]+)(@)"#));
/// References to a secret rather than the secret: `$NAME`, `${NAME}`,
/// `${{ expression }}`, `%NAME%`, `{{ template }}`, `<placeholder>` and
/// masks such as `***`.
static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^(?:\$\{?[A-Z_][A-Z0-9_]*\}?|\$\{\{[^{}]*\}\}|%[A-Za-z_][A-Za-z0-9_]*%|\{\{[^{}]*\}\}|<[^<>\n]{1,64}>|\*{3,}|x{3,}|X{3,}|\.{3,}|…)$",
    )
});
/// Webhook URLs whose path is their secret: Slack's, Discord's and
/// Microsoft Teams', and Telegram's bot URLs, whose token is a number, a
/// colon and the secret (`/botinfo` is not one). The host is kept, the path
/// redacted.
static WEBHOOK: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)\b(https?://(?:hooks\.slack\.com/(?:services|workflows|triggers)/|(?:canary\.|ptb\.)?discord(?:app)?\.com/api/(?:v[0-9]+/)?webhooks/|[a-z0-9-]+\.webhook\.office\.com/webhookb2/|outlook\.office\.com/webhook/))[A-Za-z0-9@/_.%:-]+|\b(https?://api\.telegram\.org/(?:file/)?bot)[0-9]+:[A-Za-z0-9@/_.%:-]+",
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

/// How surely a name says its value is a secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Strength {
    /// Not a secret's name.
    None,
    /// A hint, such as `token` or `pass` alone: the value is a secret only
    /// if it looks like a credential.
    Weak,
    /// A secret's name, such as `password` or `api_key`: any value that is
    /// not a reference, a placeholder or a word is a secret.
    Strong,
}

/// How surely `name` names a secret. A dotted path is judged by its last
/// segment: `input.password` names a password, `process.env.API_KEY` a key.
fn strength(name: &str) -> Strength {
    let last = name.rsplit('.').next().unwrap_or(name);
    let parts = name_parts(last);
    // `auth_method`, `secret_version`, `token_count`: about a secret, said
    // by the name's last word. `PROVIDER_API_KEY` is a key.
    if parts
        .last()
        .is_none_or(|part| NOT_SECRET_PARTS.contains(&part.as_str()))
    {
        return Strength::None;
    }
    let joined = parts.concat();
    let qualified_key = parts
        .windows(2)
        .any(|pair| pair[1] == "key" && KEY_QUALIFIERS.contains(&pair[0].as_str()));
    if qualified_key
        || parts
            .iter()
            .any(|part| STRONG_PARTS.contains(&part.as_str()))
        || STRONG_ENDINGS.iter().any(|end| joined.ends_with(end))
    {
        return Strength::Strong;
    }
    // `key` unqualified, as in `OPENAI_KEY` or `primaryKey`, only hints:
    // `cache-key: v1-deps` holds no secret.
    if parts.iter().any(|part| part == "key") || joined.ends_with("key") {
        return Strength::Weak;
    }
    let weak =
        parts.iter().any(|part| WEAK_PARTS.contains(&part.as_str())) || joined.ends_with("token");
    // `max_token`, `prompt_token`, `input_tokens`, `nextToken`: a count or a
    // kind of token, which only hints, said by the word right before
    // `token`. Never in an all-upper-case name, an environment variable's,
    // where `INPUT_TOKEN` is how GitHub Actions passes a token, and never
    // for `pass` or `auth`: `INPUT_PASS` and `PROVIDER_TOKEN` are secrets'
    // names.
    let upper = !last.bytes().any(|b| b.is_ascii_lowercase());
    let about = !upper
        && parts.windows(2).any(|pair| match pair[1].as_str() {
            "token" => TOKEN_KINDS.contains(&pair[0].as_str()),
            "tokens" => {
                TOKEN_KINDS.contains(&pair[0].as_str()) || COUNTED_KINDS.contains(&pair[0].as_str())
            }
            _ => false,
        });
    match (weak, parts.len()) {
        (false, _) => Strength::None,
        (true, 2..) if about => Strength::Weak,
        // `DB_PASS`, `GH_PAT`, `accessToken`: qualified.
        (true, 2..) => Strength::Strong,
        (true, _) => Strength::Weak,
    }
}

/// Whether a value under `name` is a secret, whatever it looks like.
pub fn is_secret_name(name: &str) -> bool {
    strength(name) == Strength::Strong
}

/// The largest character boundary of `s` at or before byte `at`.
fn floor_boundary(s: &str, at: usize) -> usize {
    let mut at = at.min(s.len());
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
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

/// How many bytes after a backslash make one escape: `\n`, `\t`, `\"`,
/// `\uXXXX`, `\x1b`. What an escape stands for is never part of a token,
/// so a key right after `\n` still starts one, as it does after `\\n`,
/// where JSON inside a JSON string escapes the backslash: the second
/// backslash starts the escape.
fn escape_len(after: &str) -> usize {
    let b = after.as_bytes();
    let hex = |range: std::ops::Range<usize>| {
        b.len() >= range.end && b[range].iter().all(u8::is_ascii_hexdigit)
    };
    match b.first() {
        Some(b'u') if hex(1..5) => 5,
        Some(b'x') if hex(1..3) => 3,
        Some(b'n' | b't' | b'r' | b'b' | b'f' | b'a' | b'v' | b'0' | b'"' | b'\'' | b'/') => 1,
        _ => 0,
    }
}

std::thread_local! {
    /// How many steps this thread's scans have taken, for the tests that
    /// check a scan stays linear.
    #[cfg(test)]
    static STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Count `n` steps of a scan (see [`STEPS`]); nothing outside tests.
#[inline]
fn step(n: usize) {
    #[cfg(test)]
    STEPS.with(|steps| steps.set(steps.get() + n as u64));
    #[cfg(not(test))]
    let _ = n;
}

/// ESC as atomic's terminal escaping and Rust's `{:?}` write it, so text
/// escaped before it is redacted hides no token either.
const BRACED_ESC: &[&str] = &["\\u{1b}", "\\u{001b}", "\\u{1B}", "\\u{001B}"];

/// How long the terminal escape sequence starting `rest` is, if one does: a
/// control sequence (`ESC[1;33m`, `ESC[38:5:196m`, `ESC[1 q`), the
/// introducer of a title, link or clipboard write (`ESC]`), or a character
/// set (`ESC(B`). ESC is the byte, or written out as `\u001b`, `\x1b`, `\e`
/// or `\033`. A sequence whose final letter starts a key ends before it:
/// in `ESC[ghp_…` the `g` is the key's.
fn escape_sequence_len(rest: &str) -> Option<usize> {
    let introducer = [
        "\x1b", "\\u001b", "\\u001B", "\\x1b", "\\x1B", "\\e", "\\033",
    ]
    .iter()
    .chain(BRACED_ESC)
    .find(|i| rest.starts_with(**i))?;
    let body = &rest[introducer.len()..];
    let bytes = body.as_bytes();
    match bytes.first()? {
        b'[' => {
            // Parameters, then intermediates, then one final byte.
            let params = bytes[1..]
                .iter()
                .take(256)
                .take_while(|b| matches!(b, b'0'..=b'9' | b';' | b':' | b'?' | b'<' | b'=' | b'>'))
                .count();
            let inter = bytes[1 + params..]
                .iter()
                .take(4)
                .take_while(|b| (0x20..=0x2f).contains(*b))
                .count();
            let fin = *bytes.get(1 + params + inter)?;
            // A backslash starts an escape, `\n` say, not the sequence's
            // end: the sequence stops before it, so what follows the escape
            // starts a token.
            if fin == b'\\' {
                return Some(introducer.len() + 1 + params + inter);
            }
            (0x40..=0x7e)
                .contains(&fin)
                .then_some(introducer.len() + 2 + params + inter)
        }
        // A title, a link or the clipboard: only the introducer is passed
        // over. The body is scanned like any text, so a key in it is found,
        // and its terminator, BEL or ST, is a boundary.
        b']' => Some(introducer.len() + 1),
        b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => {
            let set = *bytes.get(1)?;
            // A backslash starts an escape, as after `ESC[`.
            if set == b'\\' {
                return Some(introducer.len() + 1);
            }
            (0x30..=0x7e).contains(&set).then_some(introducer.len() + 2)
        }
        _ => None,
    }
}

/// Replace each private key block, and each token of a known format, with
/// [`REDACTED`]. A key block with no end runs to the end of the text. Each
/// character is looked at a bounded number of times.
fn shapes(input: &str) -> Option<String> {
    let mut out = String::new();
    let mut copied = 0;
    let mut at = 0;
    // Whether a token may start at `at`: not inside a longer word, a path
    // or base64 text, such as after `/` or `+`.
    let mut boundary = true;
    while at < input.len() {
        step(1);
        let rest = &input[at..];
        // Checked only where a block starts, so the scan stays linear.
        if let Some(found) = rest
            .starts_with("-----BEGIN ")
            .then(|| PRIVATE_KEY.captures(rest))
            .flatten()
        {
            let end = format!("-----END {}-----", &found[1]);
            let len = rest.find(&end).map_or(rest.len(), |i| i + end.len());
            step(len);
            out.push_str(&input[copied..at]);
            out.push_str(REDACTED);
            at += len;
            copied = at;
            boundary = true;
            continue;
        }
        // A terminal escape sequence, kept as it is: a boundary.
        if let Some(mut len) = escape_sequence_len(rest) {
            if rest[len - 1..].starts_with(|c: char| c.is_ascii_alphanumeric())
                && known_shape(&rest[len - 1..]).is_some()
            {
                len -= 1;
            }
            at += len;
            boundary = true;
            continue;
        }
        let c = rest.chars().next().unwrap_or_default();
        if c == '\\' {
            at += 1 + escape_len(&rest[1..]);
            boundary = true;
            continue;
        }
        if boundary && is_token_char(c) {
            if let Some(len) = known_shape(rest) {
                out.push_str(&input[copied..at]);
                out.push_str(REDACTED);
                at += len;
                copied = at;
            } else {
                // Not a secret: move past the whole token.
                at += token_len(rest).max(c.len_utf8());
            }
            boundary = false;
            continue;
        }
        boundary = !is_token_char(c) && c != '/' && c != '+';
        at += c.len_utf8();
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&input[copied..]);
    Some(out)
}

/// A value next to a secret name that is plainly not the secret: a reference
/// to one, a placeholder, a type, or a word.
fn is_not_a_secret(value: &str) -> bool {
    value.is_empty()
        || value == REDACTED
        || value.contains(REDACTED) && value.len() <= REDACTED.len() + 2
        || REFERENCE.is_match(value)
        || STOPWORDS
            .iter()
            .any(|word| value.eq_ignore_ascii_case(word))
}

/// Text that is code rather than a value, which is common beside a word like
/// `token`: a call, a path, a generic type or a reference.
fn looks_like_code(value: &str) -> bool {
    value.ends_with("()")
        || value.ends_with("();")
        || value.contains("::")
        || value.contains("->")
        || value.starts_with('&')
        || value.starts_with("self.")
        || value.starts_with(['=', '>', ':'])
        || (value.contains('<') && value.ends_with('>'))
}

/// Whether `segment` reads as words or a small number rather than random
/// characters: `operator`, `InvalidToken`, `deps`, `v1`, `2024`.
fn is_wordlike(segment: &str) -> bool {
    let b = segment.as_bytes();
    if b.len() <= 3 || (b.len() <= 4 && b.iter().all(u8::is_ascii_digit)) {
        return true;
    }
    // Lower-case words, each but the first may start with a capital.
    let mut previous_upper = false;
    for (i, c) in b.iter().enumerate() {
        match c {
            b'a'..=b'z' => previous_upper = false,
            b'A'..=b'Z' if i == 0 || !previous_upper => previous_upper = true,
            _ => return false,
        }
    }
    !previous_upper
}

/// Whether a value looks like a credential rather than words or a number:
/// a known format, or eight or more characters, no spaces, of at least two
/// kinds (lower case, upper case, digits, symbols), that are not just words
/// and small numbers joined by `-`, `_`, `.`, `/` or `:` (`operator-1`,
/// `InvalidToken`, `v1-deps`).
fn looks_like_credential(value: &str) -> bool {
    if known_shape(value).is_some() {
        return true;
    }
    if value.chars().count() < 8 || value.contains(char::is_whitespace) {
        return false;
    }
    if value.split(['-', '_', '.', '/', ':']).all(is_wordlike) {
        return false;
    }
    let mut kinds = [false; 4];
    for c in value.chars() {
        let kind = if c.is_ascii_lowercase() {
            0
        } else if c.is_ascii_uppercase() {
            1
        } else if c.is_ascii_digit() {
            2
        } else {
            3
        };
        kinds[kind] = true;
    }
    kinds.iter().filter(|k| **k).count() >= 2
}

/// Whether a value beside a name of this strength is the secret.
fn is_secret_value(strength: Strength, value: &str) -> bool {
    match strength {
        Strength::None => false,
        _ if is_not_a_secret(value) => false,
        Strength::Strong => true,
        Strength::Weak => looks_like_credential(value),
    }
}

/// Basic credentials: base64 of `user:password`.
fn is_basic_credential(value: &str) -> bool {
    let value = value.trim_end_matches('=');
    let mut bits = 0u32;
    let mut count = 0;
    let mut decoded = Vec::new();
    for c in value.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return false,
        };
        bits = (bits << 6) | u32::from(v);
        count += 6;
        if count >= 8 {
            count -= 8;
            decoded.push((bits >> count) as u8);
        }
    }
    decoded.contains(&b':') && decoded.iter().all(|b| b.is_ascii_graphic() || *b == b' ')
}

/// The schemes of an `Authorization` header's value.
const SCHEMES: &[&str] = &[
    "bearer",
    "basic",
    "token",
    "digest",
    "negotiate",
    "ntlm",
    "apikey",
    "sharedkey",
    "signature",
    "aws4-hmac-sha256",
];

/// Whether a header's value is a credential: a scheme and what follows it,
/// a cookie's `name=value`, or a value that looks like a key. Not
/// `authorization: denied for user bob`.
fn is_header_secret(header: &str, value: &str) -> bool {
    let value = value.trim();
    let first = value.split_whitespace().next().unwrap_or_default();
    let header = header.to_ascii_lowercase();
    if header.ends_with("cookie") {
        return value.contains('=');
    }
    // A key header holds nothing but its key.
    if header.starts_with("x-") {
        return is_secret_value(Strength::Strong, first);
    }
    let scheme = SCHEMES.iter().any(|s| first.eq_ignore_ascii_case(s));
    (scheme && value.len() > first.len()) || looks_like_credential(first)
}

/// A bracket in `value` closed but never opened in it, at its end.
fn trim_unopened(value: &str) -> &str {
    let mut depth = [0i64; 3];
    for c in value.chars() {
        match c {
            '(' => depth[0] += 1,
            ')' => depth[0] -= 1,
            '[' => depth[1] += 1,
            ']' => depth[1] -= 1,
            '{' => depth[2] += 1,
            '}' => depth[2] -= 1,
            _ => {}
        }
    }
    let mut value = value;
    loop {
        let slot = match value.chars().next_back() {
            Some(')') => 0,
            Some(']') => 1,
            Some('}') => 2,
            _ => return value,
        };
        if depth[slot] >= 0 {
            return value;
        }
        depth[slot] += 1;
        value = &value[..value.len() - 1];
    }
}

/// Whether `rest` starts another assignment, as `page=2` does after `&`.
/// Looks at most a name's length ahead.
fn starts_assignment(rest: &str) -> bool {
    let name = rest
        .bytes()
        .take(128)
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        .count();
    name > 0
        && rest.as_bytes()[0].is_ascii_alphabetic()
        && matches!(rest.as_bytes().get(name), Some(b'=' | b':'))
}

/// The end of a bare value starting `rest`: at white space, a quote, a
/// backtick or an escaped quote; at a `,`, `;` or `&` that ends the text or
/// starts the next assignment (so a query's next parameter is kept); and
/// before closing brackets it did not open.
fn bare_value_len(rest: &str) -> usize {
    let bytes = rest.as_bytes();
    let mut end = rest.len();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'`' => {
                end = i;
                break;
            }
            b'\\' if matches!(bytes.get(i + 1), Some(b'"' | b'n' | b'r' | b't')) => {
                end = i;
                break;
            }
            b',' | b';' | b'&' if i + 1 == bytes.len() || starts_assignment(&rest[i + 1..]) => {
                end = i;
                break;
            }
            b',' | b';' if matches!(bytes.get(i + 1), Some(b' ' | b'\t' | b'\n')) => {
                end = i;
                break;
            }
            _ => {}
        }
        i += 1;
    }
    trim_unopened(&rest[..end]).len()
}

/// The end of a quoted value whose opening quote, `quote`, follows `level`
/// backslashes (JSON inside a string escapes its quotes): the first such
/// quote after it at the same level, so `\"` inside `"…"` does not end it.
fn quoted_len(rest: &str, quote: u8, level: usize) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut backslashes = 0;
    for (i, b) in bytes.iter().enumerate() {
        step(1);
        match *b {
            b'\n' => return None,
            b'\\' => backslashes += 1,
            q if q == quote => {
                let closes = if level == 0 {
                    backslashes % 2 == 0
                } else {
                    backslashes == level
                };
                if closes {
                    return Some(i - backslashes);
                }
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
    }
    None
}

/// Where the value after a separator at `at` starts, past white space and
/// Rust's Debug wrappers (`Some(`, `Secret(`), whose pretty form, `{:#?}`,
/// puts the value on the next line.
fn value_start(input: &str, mut at: usize) -> usize {
    let skip_blanks = |at: usize, newlines: bool| {
        at + input[at..]
            .bytes()
            .take_while(|b| matches!(b, b' ' | b'\t') || newlines && matches!(b, b'\n' | b'\r'))
            .count()
    };
    at = skip_blanks(at, false);
    for _ in 0..3 {
        let rest = &input[at..];
        let ident = rest
            .bytes()
            .take(64)
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        if ident > 0
            && rest.as_bytes()[0].is_ascii_uppercase()
            && rest.as_bytes().get(ident) == Some(&b'(')
        {
            at = skip_blanks(at + ident + 1, true);
        } else {
            break;
        }
    }
    at
}

/// The name before a separator ending at `sep`: back past blanks and a
/// closing quote (escaped or not), then name characters, at most 128.
fn name_before(input: &str, sep: usize) -> Option<(usize, usize)> {
    let bytes = input.as_bytes();
    let mut end = sep;
    while end > 0 && matches!(bytes[end - 1], b' ' | b'\t') {
        end -= 1;
    }
    if end > 0 && matches!(bytes[end - 1], b'"' | b'\'') {
        end -= 1;
        while end > 0 && bytes[end - 1] == b'\\' {
            end -= 1;
        }
    }
    let mut start = end;
    while start > 0
        && end - start < 128
        && (bytes[start - 1].is_ascii_alphanumeric()
            || matches!(bytes[start - 1], b'_' | b'.' | b'-'))
    {
        start -= 1;
    }
    // Not the letter of an escape, as the `n` of `\npassword`.
    if start > 0 && bytes[start - 1] == b'\\' && start < end {
        start += 1;
    }
    (start < end).then_some((start, end))
}

/// The value after a separator: its span and whether it was quoted.
fn value_after(input: &str, at: usize) -> Option<(usize, usize)> {
    let rest = &input[at..];
    let bytes = rest.as_bytes();
    let level = bytes.iter().take_while(|b| **b == b'\\').count();
    match bytes.get(level) {
        // Quotes escaped at most eight deep: each depth's quotes are scanned
        // once, so the scan stays linear.
        Some(q @ (b'"' | b'\'')) if level <= 8 => {
            let start = at + level + 1;
            let len = quoted_len(&input[start..], *q, level)?;
            Some((start, start + len))
        }
        None => None,
        // A placeholder, `<your key>`, or a template, `{{ x }}`, may hold
        // spaces.
        Some(b'<') => {
            let len = rest
                .bytes()
                .take(66)
                .position(|b| b == b'>' || b == b'\n')?;
            (bytes[len] == b'>').then_some((at, at + len + 1))
        }
        // Looked for within a bounded window, so the scan stays linear.
        Some(_) if rest.starts_with("{{") || rest.starts_with("${{") => {
            let window = &rest.as_bytes()[..rest.len().min(256)];
            let close = window.windows(2).position(|w| w == b"}}");
            match close {
                Some(end) if !window[..end].contains(&b'\n') => Some((at, at + end + 2)),
                _ => {
                    let len = bare_value_len(rest);
                    (len > 0).then_some((at, at + len))
                }
            }
        }
        Some(_) => {
            let len = bare_value_len(rest);
            (len > 0).then_some((at, at + len))
        }
    }
}

/// Redact the values assigned to secret names in `input`: `name=value`,
/// `name: value`, `name := value`, `'name' => 'value'`. One pass: each
/// separator is looked at once, a name at most 128 characters back, and a
/// value once, after which the search goes on past it.
fn assigned(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = String::new();
    let mut copied = 0;
    let mut at = 0;
    while let Some(found) = input[at..].find(['=', ':']) {
        let sep = at + found;
        let next = bytes.get(sep + 1).copied();
        let sep_len = match (bytes[sep], next) {
            // A path, a comparison, a URL: no assignment.
            (b':', Some(b':')) | (b'=', Some(b'=')) | (b'=', Some(b'~')) => {
                at = sep + 2;
                continue;
            }
            (b':', Some(b'/')) => {
                at = sep + 1;
                continue;
            }
            (b':', Some(b'=')) | (b'=', Some(b'>')) => 2,
            _ => 1,
        };
        at = sep + sep_len;
        let Some((name_start, name_end)) = name_before(input, sep) else {
            continue;
        };
        let mut strength = strength(&input[name_start..name_end]);
        // `api key: …`, `Your API key: …`: qualified by the word before.
        if input[name_start..name_end].eq_ignore_ascii_case("key")
            && name_start > 0
            && bytes[name_start - 1] == b' '
        {
            let from = floor_boundary(input, name_start.saturating_sub(16));
            let before = &input[from..name_start - 1];
            let word = before
                .rsplit(|c: char| !c.is_ascii_alphanumeric())
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if KEY_QUALIFIERS.contains(&word.as_str()) {
                strength = Strength::Strong;
            }
        }
        if strength == Strength::None {
            continue;
        }
        let start = value_start(input, sep + sep_len);
        let Some((value_from, mut value_to)) = value_after(input, start) else {
            continue;
        };
        // `authorization=Bearer abc`: the scheme and its credential.
        let value = &input[value_from..value_to];
        if SCHEMES.iter().any(|s| value.eq_ignore_ascii_case(s))
            && input[value_to..].starts_with(' ')
            && let Some((credential_from, credential_to)) = value_after(input, value_to + 1)
            && !is_not_a_secret(&input[credential_from..credential_to])
        {
            value_to = credential_to;
            strength = Strength::Strong;
        }
        let value = &input[value_from..value_to];
        let quoted = value_from > start;
        if (quoted || !looks_like_code(value)) && is_secret_value(strength, value) {
            out.push_str(&input[copied..value_from]);
            out.push_str(REDACTED);
            copied = value_to;
        }
        at = at.max(value_to);
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&input[copied..]);
    Some(out)
}

fn redact_flag(captures: &Captures<'_>) -> String {
    let value = captures[3].trim_matches(['"', '\'']);
    if !is_secret_value(strength(&captures[2]), value) {
        return captures[0].to_string();
    }
    let whole = captures.get(0).map_or(0, |m| m.start());
    let value_at = captures.get(3).map_or(0, |m| m.start());
    format!("{}{REDACTED}", &captures[0][..value_at - whole])
}

/// "bearer authentication" is prose: a bearer token has a digit or is long,
/// and Basic credentials are base64 of `user:password`.
fn redact_scheme(captures: &Captures<'_>) -> String {
    let value = &captures[3];
    let tokenish = if captures[1].eq_ignore_ascii_case("basic") {
        is_basic_credential(value)
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

/// How many readings [`text`] takes at most: a reading can leave what
/// another rule reads as a secret only once a first rule has redacted
/// around it, as a header after an assignment's marker. Most text settles
/// in one or two; text still changing at the last, such as a header before
/// a long run of backslashed chunks that a reading takes one at a time, is
/// hidden whole.
const TEXT_PASSES: usize = 4;

/// Replaces secrets in text, read again until a reading changes nothing,
/// so redacting the result again changes nothing; text still changing
/// after [`TEXT_PASSES`] readings is hidden whole, since what a further
/// reading would hide cannot be told. Linear in the length of `input`;
/// callers with text of unknown length bound it first (see [`excerpt`]).
pub fn text(input: &str) -> Cow<'_, str> {
    let mut output = match text_once(input) {
        Cow::Owned(once) if once != input => once,
        _ => return Cow::Borrowed(input),
    };
    for _ in 1..text_passes() {
        let again = text_once(&output).into_owned();
        if again == output {
            return Cow::Owned(output);
        }
        output = again;
    }
    if text_once(&output) == output.as_str() {
        Cow::Owned(output)
    } else {
        Cow::Owned(REDACTED.to_owned())
    }
}

#[cfg(test)]
std::thread_local! {
    /// How many readings [`text`] takes at most on this thread, for tests
    /// that try a lower bound.
    static PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(TEXT_PASSES) };
}

#[cfg(test)]
fn text_passes() -> usize {
    PASSES.with(std::cell::Cell::get)
}

#[cfg(not(test))]
const fn text_passes() -> usize {
    TEXT_PASSES
}

/// How surely `name` names a secret in every reading [`text`] makes of it,
/// and as written too when `written`, the surest taken: a later reading
/// can hide what made an earlier one name a secret.
fn read_strength(name: &str, written: bool) -> Strength {
    let mut surest = if written {
        name_strength(name)
    } else {
        Strength::None
    };
    let mut reading = name.to_owned();
    for _ in 0..=text_passes() {
        let next = text_once(&reading).into_owned();
        if next == reading {
            break;
        }
        surest = surer(surest, name_strength(&next));
        reading = next;
    }
    surest
}

/// One reading of `input` by every text rule (see [`text`]).
fn text_once(input: &str) -> Cow<'_, str> {
    let mut output = match shapes(input) {
        Some(shaped) => Cow::Owned(shaped),
        None => Cow::Borrowed(input),
    };
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
    apply(&WEBHOOK, &|c| {
        let host = c.get(1).or_else(|| c.get(2)).map_or("", |m| m.as_str());
        format!("{host}{REDACTED}")
    });
    // The whole value, even when part of it was redacted already: the rest
    // of a cookie is as secret as its first part.
    apply(&HEADER, &|c| {
        if is_header_secret(&c[1], &c[3]) {
            format!("{}{}{REDACTED}", &c[1], &c[2])
        } else {
            c[0].to_string()
        }
    });
    apply(&SCHEME, &redact_scheme);
    apply(&FLAG, &redact_flag);
    if let Some(replaced) = assigned(&output) {
        output = Cow::Owned(replaced);
    }
    output
}

/// `input` cut to `chars` characters, ending `…` if it was longer.
fn cut(input: &str, chars: usize) -> Cow<'_, str> {
    match input.char_indices().nth(chars) {
        Some((at, _)) => Cow::Owned(format!("{}…", &input[..at])),
        None => Cow::Borrowed(input),
    }
}

/// How much of a text to redact so that `chars` of it survive: enough to
/// spare that the cut stays out of view unless redaction shrinks the window
/// a great deal, when the line at the cut is dropped (see
/// [`cut_came_into_view`]).
fn margin(chars: usize) -> usize {
    chars.saturating_mul(2).saturating_add(16 * 1024)
}

/// Whether `c` can be part of a key, so a cut there may split one: token
/// characters, and the `.`, `/`, `+` and `=` of dotted and base64 keys.
fn may_be_in_key(c: char) -> bool {
    is_token_char(c) || matches!(c, '.' | '/' | '+' | '=')
}

/// The start of `input` up to character `chars`, less the token that cut
/// splits, however long.
fn window_start_at(input: &str, chars: usize) -> &str {
    let Some((at, next)) = input.char_indices().nth(chars) else {
        return input;
    };
    let head = &input[..at];
    if !may_be_in_key(next) {
        return head;
    }
    let keep = head
        .char_indices()
        .rev()
        .find(|(_, c)| !may_be_in_key(*c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    &head[..keep]
}

/// The end of `input` from its last `chars` characters, less the token that
/// cut splits, however long.
fn window_end_at(input: &str, chars: usize) -> &str {
    let count = input.chars().count();
    if count <= chars {
        return input;
    }
    let Some((at, _)) = input.char_indices().nth(count - chars) else {
        return input;
    };
    let before = input[..at].chars().next_back();
    let tail = &input[at..];
    if !before.is_some_and(may_be_in_key) {
        return tail;
    }
    let skip = tail
        .char_indices()
        .find(|(_, c)| !may_be_in_key(*c))
        .map_or(tail.len(), |(i, _)| i);
    &tail[skip..]
}

/// The start of `input` to the redaction margin for `chars`, less a token
/// the margin cuts in half.
fn window_start(input: &str, chars: usize) -> &str {
    window_start_at(input, margin(chars))
}

/// The end of `input` to the redaction margin for `chars`, less a token
/// the margin cuts in half.
fn window_end(input: &str, chars: usize) -> &str {
    window_end_at(input, margin(chars))
}

/// How far before a window's cut, in characters, a secret whose rule needs
/// what lies across the cut may start: as long as a realistic secret, or a
/// header line at a common cap.
const REACH: usize = 8 * 1024;

/// Whether redacting a window of `window` characters into `redacted`
/// brought its cut into view: the window was longer than `chars` and the
/// reach of a secret, and redaction shrank it to within them. A window
/// that was short already, as one whose cut dropped a long token, did not.
fn cut_came_into_view(window: &str, redacted: &str, chars: usize) -> bool {
    let room = chars.saturating_add(REACH);
    window.chars().count() > room && redacted.chars().count() <= room
}

/// `redacted`, a window that cut its input at its end, less its last line
/// if redaction brought the cut into view: a secret whose rule needs what
/// the cut took (the `@` after a URL's password, the quote that ends a JSON
/// value) is kept whole there.
fn less_cut_line_at_end<'a>(window: &str, redacted: &'a str, chars: usize) -> &'a str {
    if !cut_came_into_view(window, redacted, chars) {
        return redacted;
    }
    redacted.rfind('\n').map_or("", |i| &redacted[..=i])
}

/// `redacted`, a window that cut its input at its start, less its first
/// line if redaction brought the cut into view (see
/// [`less_cut_line_at_end`]): a token whose `Authorization: Bearer ` the
/// cut took, say.
fn less_cut_line_at_start<'a>(window: &str, redacted: &'a str, chars: usize) -> &'a str {
    if !cut_came_into_view(window, redacted, chars) {
        return redacted;
    }
    redacted.find('\n').map_or("", |i| &redacted[i + 1..])
}

/// `shown`, ending `…` if anything of `input` was left out, as a window's
/// cut or a dropped token, so a cut never reads as the whole.
fn marked_end(shown: Cow<'_, str>, left_out: bool) -> String {
    let mut shown = shown.into_owned();
    if left_out && !shown.ends_with('…') {
        shown.push('…');
    }
    shown
}

/// The start of `input`, at most `chars` characters, redacted. Only a
/// window with room to spare is redacted, so a huge input costs little,
/// less any token the window cuts in half, and less the line at the cut
/// when redaction shrank the window enough to show it; then the result is
/// cut, and ends `…` when anything was left out. JSON is redacted as JSON
/// (see [`value`]), so its escapes hide no secret, and then as text.
pub fn excerpt(input: &str, chars: usize) -> String {
    let window = window_start(input, chars);
    let left_out = window.len() < input.len();
    let trimmed = window.trim_start();
    let json = match serde_json::from_str::<Value>(trimmed) {
        Ok(mut doc) if trimmed.starts_with(['{', '[']) => {
            let before = doc.clone();
            value(&mut doc);
            (doc != before).then(|| doc.to_string())
        }
        _ => None,
    };
    let redacted = text(json.as_deref().unwrap_or(window));
    let shown = if left_out {
        less_cut_line_at_end(window, &redacted, chars)
    } else {
        &redacted
    };
    marked_end(cut(shown, chars), left_out)
}

/// The start of `input`, at most `chars` characters, redacted as text: as
/// [`excerpt`], for a log line or a span's attribute.
pub fn bounded(input: &str, chars: usize) -> String {
    let window = window_start(input, chars);
    let left_out = window.len() < input.len();
    let redacted = text(window);
    let shown = if left_out {
        less_cut_line_at_end(window, &redacted, chars)
    } else {
        &redacted
    };
    marked_end(cut(shown, chars), left_out)
}

/// The end of `input`, at most `chars` characters, redacted: as
/// [`excerpt`], for the tail of a command's stderr, starting `…` when
/// anything was left out.
pub fn tail(input: &str, chars: usize) -> String {
    let window = window_end(input, chars);
    let cut_off = window.len() < input.len();
    let redacted = text(window);
    let shown = if cut_off {
        less_cut_line_at_start(window, &redacted, chars)
    } else {
        &redacted
    };
    let count = shown.chars().count();
    let mut out: String = shown.chars().skip(count.saturating_sub(chars)).collect();
    if (cut_off || count > chars) && !out.starts_with('…') {
        out.insert(0, '…');
    }
    out
}

fn redact_string(string: &mut String, nested: usize) {
    // Whether the string read as JSON before text redaction touched it.
    let was_json = |s: &str| {
        nested < MAX_NESTED_JSON
            && s.trim_start().starts_with(['{', '['])
            && serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok()
    };
    let mut opened = false;
    if string.len() > MAX_TEXT {
        *string = bounded(string, MAX_TEXT);
    } else if let Cow::Owned(redacted) = text(string) {
        opened = redacted != *string && was_json(string);
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
    } else if opened {
        // JSON that text redaction broke: the rules that read its names
        // and pairs cannot read it, and text redaction alone may leave
        // their secrets, so it is hidden whole.
        *string = REDACTED.to_owned();
    }
}

/// Redact what a value held under a name of this strength holds, keeping
/// its shape: under `password`, every string and number; under `token`,
/// strings that look like credentials.
fn redact_held(strength: Strength, value: &mut Value, nested: usize) {
    // The rules any value has: a name and value pair's secret.
    if let Some(secret) = header_pair(value) {
        *secret = Value::String(REDACTED.into());
    }
    match value {
        Value::String(string) if is_secret_value(strength, string) => {
            *string = REDACTED.into();
        }
        Value::String(string) => redact_held_string(strength, string, nested, true),
        Value::Number(_) if strength == Strength::Strong => *value = Value::String(REDACTED.into()),
        Value::Array(items) => {
            redact_flag_values(items);
            items
                .iter_mut()
                .for_each(|item| redact_held(strength, item, nested));
        }
        Value::Object(fields) => {
            // Its keys are redacted as any key is, and one that looks like a
            // credential too: under a secret's name, a key can be the
            // secret, as in `{"token": {"<key>": 1}}`.
            let renamed = rename_keys(fields, held_key);
            // A field inside is held at least as surely as its own name says.
            for (key, field) in fields.iter_mut() {
                // A header's value, such as `"Authorization": "Bearer …"`.
                let name = renamed
                    .get(key)
                    .map_or_else(|| base_name(key), |r| base_name(&r.original));
                if let Some(header) = header_named(name)
                    && field.as_str().is_some_and(|v| is_header_secret(header, v))
                {
                    *field = Value::String(REDACTED.into());
                    continue;
                }
                let own = judged(key, &renamed, true);
                let held = if own == Strength::Strong {
                    own
                } else {
                    strength
                };
                redact_held(held, field, nested);
                hide_header_secret(name, field);
            }
        }
        Value::Number(_) | Value::Null | Value::Bool(_) => {}
    }
}

/// Hide `field`, under `name`, if it is a header's secret as redacted: what
/// redaction made of it, rewritten JSON say, can read as one where it did
/// not before, and a second pass would hide it.
fn hide_header_secret(name: &str, field: &mut Value) {
    if !as_main()
        && let Some(header) = header_named(name)
        && field.as_str().is_some_and(|v| is_header_secret(header, v))
    {
        *field = Value::String(REDACTED.into());
    }
}

/// A string held under a name of this strength, which does not read as its
/// secret whole: redacted as any string is, and JSON in it, under a hint,
/// opened and redacted as held (see [`open_held_json`]). One that is
/// rewritten is read again as a second pass would read it (`settle`), and
/// hidden whole if that would change it, so redacting again changes
/// nothing.
fn redact_held_string(strength: Strength, string: &mut String, nested: usize, settle: bool) {
    if is_secret_value(strength, string) {
        *string = REDACTED.into();
        return;
    }
    let rewritten = strength == Strength::Weak && open_held_json(string, nested);
    redact_string(string, nested);
    if rewritten && settle {
        let mut again = string.clone();
        redact_held_string(strength, &mut again, nested, false);
        if again != *string {
            *string = REDACTED.into();
        }
    }
}

/// JSON held in a string under a hint, such as `{"key": "{\"note\": …}"}`:
/// redacted as text, then opened and redacted as any JSON in a string is
/// and then as held, so a value that looks like a credential inside is
/// hidden as it would be in a real object under that name. It is written
/// back only when the opened JSON hid something, pretty if it came so, so a
/// string with nothing more to hide is left to be redacted as any string
/// is; one that text redaction grows past [`MAX_TEXT`] is hidden whole.
/// Whether it was rewritten.
fn open_held_json(string: &mut String, nested: usize) -> bool {
    if as_main()
        || nested >= MAX_NESTED_JSON
        || string.len() > MAX_TEXT
        || !string.trim_start().starts_with(['{', '['])
    {
        return false;
    }
    // Redacted as text first, as it is unopened: what that finds in the
    // string as written, an escaped `\/` before a token say, stays hidden
    // once the JSON is written back without the escape. Text that no
    // longer reads as JSON is left to [`redact_string`], which hides JSON
    // that text redaction broke whole.
    let texted = text(string);
    let Ok(mut inner) = serde_json::from_str::<Value>(&texted) else {
        return false;
    };
    // Grown past the bound, a second pass would cut it: hidden whole now.
    if texted.len() > MAX_TEXT {
        *string = REDACTED.to_owned();
        return true;
    }
    let before = inner.clone();
    // As any JSON in a string is, which reads names and pairs as they are
    // written, and then as held, so it is never read less closely than it
    // would be unopened.
    redact_nested(&mut inner, nested + 1);
    redact_held(Strength::Weak, &mut inner, nested + 1);
    if inner == before {
        return false;
    }
    // Written as it came, pretty or compact. What would read as a
    // credential whole, as compact JSON can, or grow past the bound, a
    // second pass would hide: [`redact_held_string`] hides it now.
    *string = if string.contains('\n') {
        serde_json::to_string_pretty(&inner).unwrap_or_default()
    } else {
        inner.to_string()
    };
    true
}

#[cfg(test)]
std::thread_local! {
    /// Whether this thread redacts as atomic did before held JSON was
    /// opened and dotted header names read, for tests that compare the two.
    static AS_MAIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether this thread redacts as before (see `AS_MAIN`); never outside
/// tests.
#[cfg(test)]
fn as_main() -> bool {
    AS_MAIN.with(std::cell::Cell::get)
}

#[cfg(not(test))]
const fn as_main() -> bool {
    false
}

/// The header `name` names, if any: the name itself, or its last dotted
/// segment, as in `http.request.header.authorization`.
fn header_named(name: &str) -> Option<&str> {
    let last = if as_main() {
        name
    } else {
        name.rsplit('.').next().unwrap_or(name)
    };
    [name, last]
        .into_iter()
        .find(|n| HEADERS.iter().any(|h| n.eq_ignore_ascii_case(h)))
}

/// A key [`rename_keys`] renamed: what it was, and its new name before any
/// number.
struct Renamed {
    original: String,
    name: String,
}

/// Rename each key of `fields` that `redact` changes, in the keys' own order
/// so the same object comes out the same however it was built. Two that
/// come out alike, as two that start private key blocks do, each keep
/// their entry, the later ones numbered: each name keeps its next number,
/// so numbering stays linear. Returns each renamed key, by its new key.
fn rename_keys(
    fields: &mut serde_json::Map<String, Value>,
    redact: impl Fn(&str) -> Option<String>,
) -> std::collections::HashMap<String, Renamed> {
    let mut renamed: Vec<(String, String)> = fields
        .keys()
        .filter_map(|key| redact(key).map(|new_key| (key.clone(), new_key)))
        .collect();
    renamed.sort();
    let mut done = std::collections::HashMap::new();
    let mut next: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (old_key, new_key) in renamed {
        if let Some(field) = fields.remove(&old_key) {
            let mut key = new_key.clone();
            if fields.contains_key(&key) {
                let n = next.entry(new_key.clone()).or_insert(2);
                loop {
                    step(1);
                    key = format!("{new_key} ({n})");
                    *n += 1;
                    if !fields.contains_key(&key) {
                        break;
                    }
                }
            }
            done.insert(
                key.clone(),
                Renamed {
                    original: old_key,
                    name: new_key,
                },
            );
            fields.insert(key, field);
        }
    }
    done
}

/// `key` without the ` (n)` [`rename_keys`] numbers it with.
fn base_name(key: &str) -> &str {
    key.strip_suffix(')')
        .and_then(|k| k.rsplit_once(" ("))
        .filter(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .map_or(key, |(base, _)| base)
}

/// How surely the field at `key` holds a secret: what its name says (see
/// [`name_strength`]), number and all, so a second pass reads it as this
/// one does; for a key renamed, the surest of that, of what its new name
/// said before it was numbered, and of every reading text redaction makes
/// of its original key (see [`read_strength`]), so a later reading cannot
/// weaken what an earlier one said; and, under a secret's name (`held`),
/// of the original key as written too, so a secret's name redacted, or
/// hidden whole, is still one there.
fn judged(key: &str, renamed: &std::collections::HashMap<String, Renamed>, held: bool) -> Strength {
    let whole = name_strength(key);
    match renamed.get(key) {
        Some(r) => surer(
            surer(whole, read_strength(&r.original, held)),
            name_strength(&r.name),
        ),
        None => whole,
    }
}

/// How surely a key, which may carry a number, names a secret: read both
/// whole and without what looks like a number, and the surer reading
/// taken, so `token (2)` is never weaker than its whole name or its name
/// alone says. A key that redaction numbered can read surer than its name
/// did, and so hide a count: a value hidden is better than a secret shown.
fn name_strength(key: &str) -> Strength {
    surer(strength(key), strength(base_name(key)))
}

/// The surer of two strengths.
fn surer(a: Strength, b: Strength) -> Strength {
    let rank = |s: Strength| match s {
        Strength::None => 0,
        Strength::Weak => 1,
        Strength::Strong => 2,
    };
    if rank(a) >= rank(b) { a } else { b }
}

/// What a key under a secret's name becomes, if anything: wholly
/// [`REDACTED`] when it looks like a credential itself, as in
/// `{"token": {"<key>": 1}}`, whatever word it spells, or when what is
/// left of it once it redacts as text does; else as it redacts as text.
/// A key redacted in part is judged by what is left of it, as a second pass
/// would judge it. Keys that collide once redacted are numbered after this,
/// and a second pass can then hide one of them whole, when what precedes
/// its number is itself a long number; that only ever hides more.
fn held_key(key: &str) -> Option<String> {
    if held_credential(key) {
        return Some(REDACTED.to_owned());
    }
    match text(key) {
        Cow::Owned(new_key) if held_credential(&new_key) => Some(REDACTED.to_owned()),
        Cow::Owned(new_key) => Some(new_key),
        Cow::Borrowed(_) => None,
    }
}

/// Whether `key`, under a secret's name, looks like a credential: before
/// its number, read without any marker it holds, both as it is and with
/// the white space and separators around it trimmed, and taken for one if
/// either reading is. So of `password= [REDACTED]` both `password= ` and
/// `password` are read, and a padded credential such as ` <credential>`
/// is one.
fn held_credential(key: &str) -> bool {
    let base = base_name(key);
    if base == REDACTED {
        return false;
    }
    let whole = base.replace(REDACTED, "");
    let trimmed =
        whole.trim_matches(|c: char| c.is_whitespace() || matches!(c, '=' | ':' | ';' | ','));
    [whole.as_str(), trimmed]
        .iter()
        .any(|k| !k.trim().is_empty() && key_looks_like_credential(k))
}

/// Whether a key looks like a credential rather than a name: not a
/// reference, a placeholder, an email, or words and numbers joined by `-`,
/// `_`, `.`, `/` or `:`, where a word may be all capitals, as an
/// environment variable's (`DATABASE_URL`), and either kind may end in a
/// digit or two (`OAUTH2`, `S3`, `oauth2ClientSecret`), and a number may be
/// as long as a date (`claude-3-5-sonnet-20241022`). When it is not plain
/// which a key is, it is taken for a credential: a key hidden is better
/// than a secret shown.
fn key_looks_like_credential(key: &str) -> bool {
    // Letters, then at most two digits, as in `OAUTH2` or `S3`.
    let caps_word = |s: &str| {
        let letters = s.bytes().take_while(u8::is_ascii_uppercase).count();
        let digits = s.len() - letters;
        letters > 0 && digits <= 2 && s.bytes().skip(letters).all(|b| b.is_ascii_digit())
    };
    // A word, a short number or a date, with a word's digit or two dropped
    // after a letter: `oauth2ClientSecret` reads as `oauthClientSecret`.
    let word = |s: &str| {
        if !s.is_empty() && s.len() <= 8 && s.bytes().all(|b| b.is_ascii_digit()) {
            return true;
        }
        let mut plain = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        let mut after_letter = false;
        while let Some(c) = chars.next() {
            if c.is_ascii_digit() && after_letter {
                let mut run = String::from(c);
                while let Some(d) = chars.next_if(char::is_ascii_digit) {
                    run.push(d);
                }
                // Dropped only at the word's end, or before the capital of
                // the next: within a word, digits are not a word's.
                let ends = chars.peek().is_none_or(char::is_ascii_uppercase);
                if run.len() <= 2 && ends {
                    after_letter = false;
                    continue;
                }
                plain.push_str(&run);
                after_letter = false;
                continue;
            }
            after_letter = c.is_ascii_alphabetic();
            plain.push(c);
        }
        is_wordlike(&plain) || caps_word(s)
    };
    let email = key
        .split_once('@')
        .is_some_and(|(user, host)| !user.is_empty() && host.contains('.'));
    !(is_not_a_secret(key) || email || key.split(['-', '_', '.', '/', ':']).all(word))
        && looks_like_credential(key)
}

/// Header names whose values are credentials.
const HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
];

/// Whether `secret` is the credential of a name and value pair named `name`:
/// a header's credential, or a credential under a secret's name. A pair
/// that says it is one, `{"name": …, "value": …}`, needs only a secret's
/// name; a bare pair, `[…, …]`, needs a value that looks like a
/// credential too, so `["password", "email"]` is a list.
fn pair_holds_secret(name: &str, secret: &Value, explicit: bool) -> bool {
    let Some(secret) = secret.as_str() else {
        return false;
    };
    if let Some(header) = header_named(name) {
        if is_header_secret(header, secret) {
            return true;
        }
        // A header by its own name is judged as a header alone; one only
        // by its last dotted segment is read by its name too.
        if header.len() == name.len() {
            return false;
        }
    }
    match strength(name) {
        Strength::Strong if explicit => is_secret_value(Strength::Strong, secret),
        Strength::None => false,
        _ => looks_like_credential(secret),
    }
}

/// A header as a pair, `["Authorization", "..."]`, or as an object,
/// `{"name": "Authorization", "value": "..."}`: the value, when it is the
/// pair's secret.
fn header_pair(value: &mut Value) -> Option<&mut Value> {
    match value {
        Value::Array(items) if items.len() == 2 => {
            let named = items[0]
                .as_str()
                .is_some_and(|name| pair_holds_secret(name, &items[1], false));
            named.then(|| &mut items[1])
        }
        Value::Object(fields) => {
            let secret = fields.get("value")?;
            let named = ["name", "key", "header"].iter().any(|field| {
                fields
                    .get(*field)
                    .and_then(Value::as_str)
                    .is_some_and(|name| pair_holds_secret(name, secret, true))
            });
            if named { fields.get_mut("value") } else { None }
        }
        _ => None,
    }
}

/// In an argument list, the value after a secret-named flag.
fn redact_flag_values(items: &mut [Value]) {
    for i in 1..items.len() {
        let flag = items[i - 1]
            .as_str()
            .and_then(|f| f.strip_prefix("--").or_else(|| f.strip_prefix('-')))
            .filter(|f| !f.contains('='))
            .map(strength)
            .unwrap_or(Strength::None);
        if flag != Strength::None && items[i].as_str().is_some_and(|v| is_secret_value(flag, v)) {
            items[i] = Value::String(REDACTED.into());
        }
    }
}

fn redact_nested(value: &mut Value, nested: usize) {
    if let Some(secret) = header_pair(value) {
        *secret = Value::String(REDACTED.into());
    }
    match value {
        Value::String(string) => redact_string(string, nested),
        Value::Array(items) => {
            redact_flag_values(items);
            items
                .iter_mut()
                .for_each(|item| redact_nested(item, nested));
        }
        Value::Object(fields) => {
            let renamed = rename_keys(fields, |key| match text(key) {
                Cow::Owned(new_key) => Some(new_key),
                Cow::Borrowed(_) => None,
            });
            for (key, field) in fields.iter_mut() {
                let name = renamed
                    .get(key)
                    .map_or_else(|| base_name(key), |r| r.original.as_str());
                // A header's value, such as `"Authorization": "Bearer …"`.
                if let Some(header) = header_named(name)
                    && field.as_str().is_some_and(|v| is_header_secret(header, v))
                {
                    *field = Value::String(REDACTED.into());
                    continue;
                }
                match judged(key, &renamed, false) {
                    Strength::None => redact_nested(field, nested),
                    held => redact_held(held, field, nested),
                }
                hide_header_secret(name, field);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Replaces secrets in every key and string of a JSON value.
pub fn value(value: &mut Value) {
    redact_nested(value, 0);
}

/// Write one log line to stderr, redacted and bounded to [`MAX_TEXT`]
/// characters, and then made safe for a terminal, which can lengthen it. atomic's own log lines go
/// through this, never straight to `eprintln!`: what they name, a run's id
/// or a principal, and what they quote can come from a caller or a model.
pub fn log(line: impl AsRef<str>) {
    eprintln!("{}", logged(line.as_ref()));
}

/// What [`log`] writes for `line`: redacted first, since redaction reads a
/// terminal escape sequence around a token as it is written, then escaped.
fn logged(line: &str) -> String {
    crate::terminal::block(&bounded(line, MAX_TEXT))
}

/// One log line put together from atomic's own words, names and text made
/// safe already, written as they are: none of it is redacted again, so
/// atomic's words and the names in them are never mistaken for a secret,
/// and nothing one part holds, such as the start of a private key, reaches
/// the others. Each line is written by itself, so nothing one line holds
/// changes what later lines show.
#[derive(Debug, Default)]
pub struct Line {
    text: String,
}

impl Line {
    /// An empty line.
    pub fn new() -> Self {
        Self::default()
    }

    /// atomic's own words. Only text written into atomic itself is
    /// `'static`, so no value read at run time comes in here.
    pub fn plain(mut self, words: &'static str) -> Self {
        self.text.push_str(words);
        self
    }

    /// Text that is safe to show as it is: a name a person or a workflow
    /// chose (a run's id, a gate's), escaped for the terminal, or a value
    /// redacted already, such as by [`value`] or [`bounded`].
    pub fn shown(mut self, text: &str) -> Self {
        self.text.push_str(text);
        self
    }

    /// What it writes: its text, bounded to [`MAX_TEXT`] characters, then
    /// made safe for a terminal, whatever a part of it missed, which can
    /// lengthen it.
    fn written(&self) -> String {
        crate::terminal::block(&cut(&self.text, MAX_TEXT))
    }

    /// Write it to stderr, as [`Line::written`] gives it.
    pub fn log(self) {
        eprintln!("{}", self.written());
    }

    /// Write it to stderr with no line end, flushed, as a prompt for a
    /// person to type after.
    pub fn prompt(self) {
        use std::io::Write;
        let mut e = std::io::stderr().lock();
        let _ = write!(e, "{}", self.written());
        let _ = e.flush();
    }
}

/// A writer that redacts each line before it reaches stderr, for code that
/// writes its log lines to a `Write` it is given. A line is written once
/// it ends, or when the writer is flushed or dropped. The lines inside a
/// private key block that one line opens are not written at all.
pub struct Lines {
    pending: Vec<u8>,
    in_key: bool,
    out: Box<dyn std::io::Write + Send>,
}

/// Whether this process's stderr is a terminal, for code that writes to it
/// through [`stderr_lines`] or [`log`] and must know whether a person sees
/// it.
pub fn stderr_is_terminal() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}

/// A [`Lines`] writer to this process's stderr.
pub fn stderr_lines() -> Lines {
    lines_to(Box::new(std::io::stderr()))
}

/// A [`Lines`] writer to `out`.
pub fn lines_to(out: Box<dyn std::io::Write + Send>) -> Lines {
    Lines {
        pending: Vec::new(),
        in_key: false,
        out,
    }
}

impl Lines {
    fn emit(&mut self, upto: usize) -> std::io::Result<()> {
        let line: Vec<u8> = self.pending.drain(..upto).collect();
        let line = String::from_utf8_lossy(&line);
        let line = line.strip_suffix('\n').unwrap_or(&line);
        if self.in_key {
            self.in_key = !line.contains("-----END ");
            return Ok(());
        }
        let opens = line
            .find("-----BEGIN ")
            .is_some_and(|at| PRIVATE_KEY.is_match(&line[at..]));
        self.in_key = opens && !line.contains("-----END ");
        writeln!(self.out, "{}", logged(line))
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
        self.out.flush()
    }
}

impl Drop for Lines {
    fn drop(&mut self) {
        let _ = std::io::Write::flush(self);
    }
}

/// Format and write one redacted log line to stderr, as `eprintln!` would.
#[macro_export]
macro_rules! log_line {
    ($($arg:tt)*) => {
        $crate::redact::log(format!($($arg)*))
    };
}

/// What a panic writes, before it is redacted: its thread, where it
/// happened, its message and, when `RUST_BACKTRACE` asks for one, its
/// backtrace.
fn panic_text(info: &std::panic::PanicHookInfo<'_>) -> String {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("<unnamed>");
    let mut message = format!("thread '{name}' {info}");
    let backtrace = std::backtrace::Backtrace::capture();
    if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
        message.push_str(&format!("\nstack backtrace:\n{backtrace}"));
    }
    message
}

/// What a panic writes, redacted (see [`panic_text`]).
pub fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    bounded(&panic_text(info), MAX_TEXT)
}

/// What a panic writes when its message could not be redacted.
pub const UNREDACTED_PANIC: &str =
    "atomic panicked, and its message could not be redacted, so it is not shown";

std::thread_local! {
    /// Whether this thread redacts a panic's message, whose own panic must
    /// not be redacted again.
    static REDACTING_A_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Where this thread's last panic happened, as the hook saw it.
    static PANICKED_AT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// How long a panic's hook waits for its message to be redacted. Redacting
/// a bounded message takes far less; a redaction that waits on what the
/// panicking thread holds, such as a rule it was building when it panicked,
/// never ends.
const PANIC_REDACTION_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// `text` redacted by `redact` on a thread of its own, so a panic in
/// redaction is that thread's and not one inside this panic's hook; if it
/// panics, or has not finished within [`PANIC_REDACTION_WAIT`],
/// [`UNREDACTED_PANIC`].
fn redact_panic(text: String, redact: fn(&str) -> String) -> String {
    redact_panic_within(text, redact, PANIC_REDACTION_WAIT)
}

/// [`redact_panic`], waiting at most `wait`.
fn redact_panic_within(
    text: String,
    redact: fn(&str) -> String,
    wait: std::time::Duration,
) -> String {
    let (done, redacted) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("atomic-panic-redaction".into())
        .spawn(move || {
            REDACTING_A_PANIC.with(|r| r.set(true));
            let _ = done.send(redact(&text));
        });
    match spawned {
        // A thread that panics drops its sender, so this ends at once.
        Ok(_) => redacted
            .recv_timeout(wait)
            .unwrap_or_else(|_| UNREDACTED_PANIC.to_owned()),
        Err(_) => UNREDACTED_PANIC.to_owned(),
    }
}

/// What the hook writes for a panic: nothing for one in redaction itself,
/// whose hook writes the fixed message for it, and `message` otherwise.
fn hook_line(redacting: bool, message: impl FnOnce() -> String) -> Option<String> {
    (!redacting).then(message)
}

/// Where this thread's last panic happened (`file:line:column`), once, if
/// the hook [`install_panic_hook`] installs saw it: for code that catches
/// the panic and reports it.
pub fn take_panic_location() -> Option<String> {
    PANICKED_AT
        .try_with(|at| at.borrow_mut().take())
        .ok()
        .flatten()
}

/// Make a panic write its message redacted, in place of the standard
/// hook's unredacted text. Redaction runs on a thread of its own: if it
/// panics, or does not finish, a fixed message is written once instead,
/// and that panic is not redacted again. It also notes where the panic
/// happened, for [`take_panic_location`]. Call it first thing in `main`.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        if let Some(at) = info.location() {
            let _ = PANICKED_AT.try_with(|p| *p.borrow_mut() = Some(at.to_string()));
        }
        if let Some(line) = hook_line(REDACTING_A_PANIC.with(std::cell::Cell::get), || {
            redact_panic(panic_text(info), logged)
        }) {
            eprintln!("{line}");
        }
    }));
}
