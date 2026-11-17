// SPDX-License-Identifier: Apache-2.0

//! A failure's fingerprint: the same failure gives the same one in another run, on another day,
//! in another checkout, so that "has this happened before?" is one lookup, and a step that keeps
//! failing the same way can be recognized as stuck.
//!
//! It is the most specific failure line in a tool's output, with what changes between runs taken
//! out: directories, line and column numbers, other numbers, long hex and UUIDs, and colour codes.
//! Then it is hashed. It matches on the same patterns as the summary ([`super::summary`]), so a tool with
//! its own wording can still be missed, and two different failures can share a line such as
//! `error: could not compile`, which is why lines like that are chosen only when nothing better is
//! there.

use super::summary::{collect_text, is_failure};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

/// A version in each fingerprint, so that a change to the normalising starts a new series instead
/// of quietly mixing two.
const VERSION: &str = "f1";
const MAX_LINE_CHARS: usize = 200;
/// How much of an output is read for its failure lines.
const MAX_SCAN_BYTES: usize = 256 * 1024;

/// Lines that say something failed without saying what, and that many different failures share.
static GENERIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)could not compile|test result:|error: test failed|aborting due to|",
        r"previous errors?|\d+ (failed|errors?)\b|npm ERR! (code|errno|A complete log)|",
        r"\*\*\* .*Error \d+|exit (code|status) \d|^\s*FAILED\s*$|^FAIL\s*$|",
        r"^Traceback \(most recent call last\)|^goroutine \d+ \[running\]",
    ))
    .expect("this pattern is fixed, and a test compiles it")
});

static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").expect("fixed pattern"));
/// A path with at least one slash, and an optional :line:column after it. Only its last part stays.
static PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:[A-Za-z]:)?(?:[\w.~-]*[/\\])+([\w.-]+)(?::\d+)*").expect("fixed pattern")
});
static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .expect("fixed pattern")
});
static HEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:0x)?[0-9a-fA-F]{8,}\b").expect("fixed pattern"));
static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d+\b").expect("fixed pattern"));
static SPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("fixed pattern"));

/// A failure line with what varies between runs taken out.
pub fn normalize(line: &str) -> String {
    let line = ANSI.replace_all(line, "");
    let line = PATH.replace_all(&line, "$1");
    let line = UUID.replace_all(&line, "<id>");
    let line = HEX.replace_all(&line, "<hex>");
    let line = NUMBER.replace_all(&line, "<n>");
    let line = SPACE.replace_all(line.trim(), " ");
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((end, _)) => line[..end].to_string(),
        None => line.into_owned(),
    }
}

/// The failure an output reports, as its fingerprint and the normalised line it came from.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Failure {
    /// The fingerprint: a version, then 16 hex digits of the line's hash.
    pub fingerprint: String,
    /// The normalised failure line it was made from.
    pub line: String,
}

/// The failure in some text, if a line in it looks like one.
pub fn of_text(text: &str) -> Option<Failure> {
    let mut end = text.len().min(MAX_SCAN_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    // Colour codes would hide a line's start from the failure patterns.
    let text = ANSI.replace_all(&text[..end], "");
    let failures: Vec<&str> = text.lines().filter(|line| is_failure(line)).collect();
    let chosen = failures
        .iter()
        .find(|line| !GENERIC.is_match(line))
        .or(failures.first())?;
    let line = normalize(chosen);
    if line.is_empty() {
        return None;
    }
    let digest = Sha256::digest(line.as_bytes());
    let hex: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Some(Failure {
        fingerprint: format!("{VERSION}:{hex}"),
        line,
    })
}

/// The failure in a tool's result, wherever in the payload its text is.
pub fn of_value(value: &Value) -> Option<Failure> {
    let mut parts = Vec::new();
    collect_text(value, &mut parts);
    of_text(&parts.join("\n"))
}

/// Whether some text is a fingerprint, as a client might send one back.
pub fn is_fingerprint(text: &str) -> bool {
    text.strip_prefix(VERSION)
        .and_then(|rest| rest.strip_prefix(':'))
        .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_same_failure_in_another_checkout_and_run_is_the_same() {
        let one = "   Compiling synthetic v0.1.0 (/home/a/work/synthetic)\n\
                   error[E0308]: mismatched types\n --> src/lib.rs:12:5\n\
                   error: could not compile `synthetic` (lib) due to 1 previous error";
        let two = "\x1b[1m\x1b[31merror[E0308]\x1b[0m: mismatched types\n --> src/lib.rs:40:9\n\
                   error: could not compile `synthetic` (lib) due to 3 previous errors";
        let (a, b) = (
            of_text(one).expect("a failure"),
            of_text(two).expect("a failure"),
        );
        assert_eq!(a, b);
        assert_eq!(a.line, "error[E0308]: mismatched types");
        assert!(is_fingerprint(&a.fingerprint), "{}", a.fingerprint);
    }

    #[test]
    fn paths_numbers_and_ids_are_taken_out_and_the_rest_kept() {
        assert_eq!(
            normalize("thread 'main' panicked at /home/a/src/x/lib.rs:12:5: index 3 out of range"),
            "thread 'main' panicked at lib.rs: index <n> out of range"
        );
        assert_eq!(
            normalize("Error: session 550e8400-e29b-41d4-a716-446655440000 at 0x7ffd5e8a1c20"),
            "Error: session <id> at <hex>"
        );
        assert_eq!(normalize("  a \t  b  "), "a b");
        // A variable's name is what tells two failures apart.
        assert_ne!(
            of_text("NameError: name 'alpha' is not defined"),
            of_text("NameError: name 'beta' is not defined")
        );
    }

    #[test]
    fn a_specific_line_is_chosen_over_a_generic_one() {
        let text = "test result: FAILED. 3 passed; 1 failed\n\
                    thread 'parses' panicked at src/parse.rs:9:1:\nassertion failed: ok";
        assert_eq!(
            of_text(text).expect("a failure").line,
            "thread 'parses' panicked at parse.rs:"
        );
        // With only a generic one, that one serves.
        let generic = of_text("error: could not compile `synthetic`").expect("a failure");
        assert_eq!(generic.line, "error: could not compile `synthetic`");
    }

    #[test]
    fn no_failure_line_means_no_fingerprint() {
        assert_eq!(of_text("all 12 tests passed\nok"), None);
        assert_eq!(of_text(""), None);
        assert_eq!(of_value(&json!({"stdout": "fine", "exit_code": 0})), None);
    }

    #[test]
    fn it_reads_every_string_in_a_payload() {
        let payload =
            json!({"tool_response": {"stdout": "ok", "stderr": "FAILED synthetic::case_one"}});
        assert_eq!(
            of_value(&payload).expect("a failure").line,
            "FAILED synthetic::case_one"
        );
    }

    #[test]
    fn a_long_output_is_cut_on_a_character_boundary() {
        let long = format!("{}é\nERROR: synthetic", "x".repeat(MAX_SCAN_BYTES - 1));
        assert_eq!(of_text(&long), None, "the line past the limit is not read");
    }

    #[test]
    fn only_its_own_shape_is_a_fingerprint() {
        assert!(is_fingerprint("f1:0123456789abcdef"));
        for other in [
            "f1:0123",
            "f2:0123456789abcdef",
            "0123456789abcdef",
            "f1:0123456789abcdeg",
        ] {
            assert!(!is_fingerprint(other), "{other}");
        }
    }
}
