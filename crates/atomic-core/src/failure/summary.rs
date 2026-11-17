// SPDX-License-Identifier: Apache-2.0

//! A short account of a large output, such as a failed step's log, so that its failures stay
//! visible when the whole of it is stored elsewhere.
//!
//! A summary that hides the one line that matters costs more than it saves, and truncation must never
//! look like completeness. So it matches generously, it always says it is a summary, it says how many
//! failure lines it left out, and when it finds none it says that this proves nothing.

use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::sync::LazyLock;

/// Small enough to keep in a step's own record.
const MAX_SUMMARY_BYTES: usize = 8 * 1024;
const MAX_LINE_CHARS: usize = 200;
const HEAD_LINES: usize = 3;
const TAIL_LINES: usize = 5;

/// Lines that report a failure in common test runners, compilers and build tools. A wrong match costs
/// one line of the summary, and a missed one can hide the point of the log, so this leans towards
/// matching. Lower-case "failed" is too common in ordinary output to match by itself.
static FAILURE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\bFAIL(ED|URE|URES)?\b",
        r"|\bERROR\b",
        // At the start of a line or after a location, so that `fn f(error: String)` does not match.
        r"|(?i:(^\s*|:\s+|\]\s+)error(\[[a-z0-9]+\])?:)",
        r"|(?i:\bfatal:)",
        r"|\bpanicked at\b|^panic:|^goroutine \d+ \[running\]",
        r"|^Traceback \(most recent call last\)",
        r"|^E\s{3}",
        r"|\b[1-9]\d* (failed|errors?)\b",
        r"|\b[A-Z][A-Za-z]*(Error|Exception):",
        r"|^\s*●",
        r"|[✗✕✖]",
        r"|\*\*\* .*Error \d+",
        r"|npm ERR!",
        r"|Segmentation fault|core dumped",
        r"|(?i:\bexit (code|status) [1-9])",
    ))
    .expect("this pattern is fixed, and a test compiles it")
});

pub(crate) fn is_failure(line: &str) -> bool {
    FAILURE.is_match(line)
}

fn shortened(line: &str) -> String {
    let line = line.trim_end();
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((end, _)) => format!("{}... [line cut]", &line[..end]),
        None => line.to_string(),
    }
}

/// Every string in a JSON value, in order, because a tool's output can sit anywhere in a payload.
pub(crate) fn collect_text<'a>(value: &'a Value, text: &mut Vec<&'a str>) {
    match value {
        Value::String(string) => text.push(string),
        Value::Array(items) => items.iter().for_each(|item| collect_text(item, text)),
        Value::Object(fields) => fields.values().for_each(|field| collect_text(field, text)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// The summary of a payload.
pub fn value(value: &Value) -> String {
    let mut parts = Vec::new();
    collect_text(value, &mut parts);
    text(&parts.join("\n"))
}

/// The summary of some text.
pub fn text(input: &str) -> String {
    let lines: Vec<&str> = input.lines().collect();
    let failures: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_failure(line))
        .map(|(number, line)| (number + 1, *line))
        .collect();

    let mut summary = format!(
        "This is a summary of {} bytes and {} lines. The rest is stored.\n",
        input.len(),
        lines.len()
    );
    if failures.is_empty() {
        summary.push_str(
            "No line looked like a failure. That does not mean nothing failed, only that this \
             found no known pattern.\n",
        );
    }
    let mut shown = 0;
    for (number, line) in &failures {
        let entry = format!("line {number}: {}\n", shortened(line));
        // Room is kept for the closing notes, so that those can never be what gets cut.
        if summary.len() + entry.len() > MAX_SUMMARY_BYTES - 1024 {
            break;
        }
        summary.push_str(&entry);
        shown += 1;
    }
    let left_out = failures.len() - shown;
    if left_out > 0 {
        let _ = writeln!(
            summary,
            "{left_out} more failure lines are NOT shown here. Read the stored output for them."
        );
    } else if !failures.is_empty() {
        let _ = writeln!(
            summary,
            "That is all {} lines that looked like failures.",
            failures.len()
        );
    }
    // Often where a tool says how it went, and worth it when no pattern matched.
    if lines.len() > HEAD_LINES + TAIL_LINES {
        summary.push_str("It starts:\n");
        for line in &lines[..HEAD_LINES] {
            let _ = writeln!(summary, "{}", shortened(line));
        }
        summary.push_str("It ends:\n");
        for line in &lines[lines.len() - TAIL_LINES..] {
            let _ = writeln!(summary, "{}", shortened(line));
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Synthetic logs of each tool's real shape: the lines that must appear, then the whole log.
    fn logs() -> Vec<(&'static str, Vec<&'static str>, String)> {
        let noise = |n: usize| "   Compiling something v1.2.3\n".repeat(n);
        vec![
            (
                "cargo test",
                vec![
                    "test tests::parses_a_header ... FAILED",
                    "test tests::rejects_a_bad_key ... FAILED",
                    "thread 'tests::parses_a_header' panicked at src/lib.rs:42:9:",
                    "test result: FAILED. 10 passed; 2 failed; 0 ignored; 0 measured",
                ],
                format!(
                    "{}running 12 tests\ntest tests::ok_one ... ok\ntest tests::parses_a_header ... FAILED\n\
                     test tests::rejects_a_bad_key ... FAILED\n\nfailures:\n\n\
                     thread 'tests::parses_a_header' panicked at src/lib.rs:42:9:\nassertion failed\n\
                     test result: FAILED. 10 passed; 2 failed; 0 ignored; 0 measured\n",
                    noise(400)
                ),
            ),
            (
                "rustc",
                vec![
                    "error[E0308]: mismatched types",
                    "error: could not compile `synthetic` (lib) due to 1 previous error",
                ],
                format!(
                    "{}error[E0308]: mismatched types\n --> src/main.rs:2:18\n\
                     error: could not compile `synthetic` (lib) due to 1 previous error\n",
                    noise(400)
                ),
            ),
            (
                "pytest",
                vec![
                    "E       assert 1 == 2",
                    "FAILED tests/test_a.py::test_one - assert 1 == 2",
                    "=== 1 failed, 40 passed in 1.20s ===",
                ],
                format!(
                    "{}tests/test_a.py F...\n    def test_one():\n>       assert 1 == 2\nE       assert 1 == 2\n\
                     FAILED tests/test_a.py::test_one - assert 1 == 2\n=== 1 failed, 40 passed in 1.20s ===\n",
                    "tests/test_b.py ....\n".repeat(400)
                ),
            ),
            (
                "go test",
                vec![
                    "--- FAIL: TestOne (0.00s)",
                    "FAIL\texample.com/synthetic\t0.012s",
                    "panic: boom",
                ],
                format!(
                    "{}=== RUN   TestOne\n--- FAIL: TestOne (0.00s)\n    a_test.go:9: wrong\npanic: boom\n\
                     FAIL\texample.com/synthetic\t0.012s\n",
                    "=== RUN   TestOther\n--- PASS: TestOther (0.00s)\n".repeat(300)
                ),
            ),
            (
                "jest",
                vec![
                    "  ● adds two numbers",
                    "FAIL src/a.test.js",
                    "Tests:       1 failed, 3 passed, 4 total",
                ],
                format!(
                    "{}FAIL src/a.test.js\n  ● adds two numbers\n\n    expect(received).toBe(expected)\n\
                     Tests:       1 failed, 3 passed, 4 total\n",
                    "PASS src/other.test.js\n".repeat(400)
                ),
            ),
            (
                "make and gcc",
                vec![
                    "a.c:3:5: error: unknown type name 'foo'",
                    "make: *** [Makefile:4: a.o] Error 1",
                ],
                format!(
                    "{}a.c:3:5: error: unknown type name 'foo'\nmake: *** [Makefile:4: a.o] Error 1\n",
                    "cc -c other.c -o other.o\n".repeat(400)
                ),
            ),
            (
                "a Python crash",
                vec![
                    "Traceback (most recent call last):",
                    "ValueError: synthetic",
                ],
                format!(
                    "{}Traceback (most recent call last):\n  File \"a.py\", line 1, in <module>\nValueError: synthetic\n",
                    "processing item\n".repeat(400)
                ),
            ),
            (
                "maven and npm",
                vec![
                    "[ERROR] Failed to execute goal",
                    "BUILD FAILURE",
                    "npm ERR! code ELIFECYCLE",
                ],
                format!(
                    "{}[ERROR] Failed to execute goal\nBUILD FAILURE\nnpm ERR! code ELIFECYCLE\n",
                    "[INFO] Building\n".repeat(400)
                ),
            ),
            (
                "a shell",
                vec![
                    "Segmentation fault (core dumped)",
                    "fatal: not a git repository",
                    "Exit code 2",
                ],
                format!(
                    "{}Segmentation fault (core dumped)\nfatal: not a git repository\nExit code 2\n",
                    "output line\n".repeat(400)
                ),
            ),
        ]
    }

    /// The acceptance test: every known failure appears in the summary.
    #[test]
    fn every_known_failure_appears() {
        for (tool, failures, log) in logs() {
            // Followed by more output, so that the start and end of the log, which are also shown,
            // cannot be what makes this pass.
            let log = format!("{log}{}", "output after the failures\n".repeat(20));
            let summary = text(&log);
            for failure in failures {
                let listed = summary.lines().any(|line| {
                    line.starts_with("line ") && line.ends_with(&format!(": {failure}"))
                });
                assert!(listed, "{tool}: missing {failure:?}\n{summary}");
            }
            assert!(
                summary.len() < log.len() / 2,
                "{tool}: not much of a summary"
            );
        }
    }

    #[test]
    fn a_failure_is_given_with_its_line_number() {
        let summary = text("one\ntwo\nerror: three\nfour\n");
        assert!(summary.contains("line 3: error: three"), "{summary}");
    }

    /// Passing runs must not look like failures, or the real ones get lost.
    #[test]
    fn a_passing_log_reports_no_failures() {
        for line in [
            "test result: ok. 12 passed; 0 failed; 0 ignored",
            "=== 40 passed in 1.20s ===",
            "--- PASS: TestOther (0.00s)",
            "PASS src/other.test.js",
            "Tests:       0 failed, 4 passed, 4 total",
            "Finished `dev` profile",
            "fn handle_error(error: String) {}",
            "0 errors, 0 warnings",
            "the terror of it",
            "use std::error::Error;",
        ] {
            assert!(!is_failure(line), "{line}");
        }
    }

    #[test]
    fn finding_nothing_is_not_presented_as_success() {
        let summary = text(&"ordinary line\n".repeat(50));
        assert!(!summary.contains("That is all"), "{summary}");
        assert!(
            summary.contains("does not mean nothing failed"),
            "{summary}"
        );
        assert!(summary.contains("summary"), "{summary}");
    }

    #[test]
    fn it_always_says_it_is_a_summary_of_something_bigger() {
        let log = "output\n".repeat(1000);
        let summary = text(&log);
        assert!(summary.starts_with(&format!(
            "This is a summary of {} bytes and 1000 lines",
            log.len()
        )));
        assert!(summary.contains("The rest is stored"));
    }

    /// Truncation must never look like completeness.
    #[test]
    fn too_many_failures_are_counted_and_it_says_so() {
        let log: String = (0..5000)
            .map(|n| format!("test case_{n} ... FAILED\n"))
            .collect();
        let summary = text(&log);
        assert!(summary.len() <= MAX_SUMMARY_BYTES, "{}", summary.len());
        let shown = summary.matches("... FAILED").count();
        // The last lines of the log are repeated at the end.
        let listed = summary
            .lines()
            .filter(|line| line.starts_with("line "))
            .count();
        assert!(listed > 50 && shown >= listed, "{listed} {shown}");
        assert!(
            summary.contains(&format!(
                "{} more failure lines are NOT shown",
                5000 - listed
            )),
            "{summary}"
        );
        assert!(!summary.contains("That is all"), "{summary}");
    }

    #[test]
    fn when_all_are_shown_it_says_that_too() {
        let summary = text("a\nerror: b\nc\nerror: d\n");
        assert!(summary.contains("That is all 2 lines"), "{summary}");
        assert!(!summary.contains("NOT shown"), "{summary}");
    }

    #[test]
    fn a_long_line_is_cut_and_says_so_without_splitting_a_character() {
        let long = format!("error: {}", "é".repeat(5000));
        let summary = text(&long);
        assert!(summary.contains("[line cut]"), "{summary}");
        assert!(summary.len() < 1000, "{}", summary.len());
    }

    #[test]
    fn it_shows_how_a_log_starts_and_ends() {
        let log: String = (0..100).map(|n| format!("output {n}\n")).collect();
        let summary = text(&log);
        assert!(summary.contains("It starts:\noutput 0\n"), "{summary}");
        assert!(summary.contains("output 99"), "{summary}");
        assert!(!summary.contains("output 50\n"), "{summary}");
    }

    #[test]
    fn a_failure_anywhere_in_a_payload_is_found() {
        let payload = json!({
            "hook_event_name": "PostToolUse",
            "tool_input": {"command": "cargo test"},
            "tool_response": {"stdout": "ok\ntest a ... FAILED\n", "stderr": "error: b\n"},
            "count": 3,
        });
        let summary = value(&payload);
        assert!(summary.contains("test a ... FAILED"), "{summary}");
        assert!(summary.contains("error: b"), "{summary}");
    }

    #[test]
    fn an_empty_output_is_handled() {
        assert!(text("").contains("0 bytes and 0 lines"));
        assert!(value(&json!({})).contains("0 bytes"));
    }

    #[test]
    fn a_large_input_is_quick() {
        let log = "ordinary output line of some length\n".repeat(200_000);
        let start = std::time::Instant::now();
        let _ = text(&log);
        assert!(start.elapsed().as_secs() < 20, "{:?}", start.elapsed());
    }
}
