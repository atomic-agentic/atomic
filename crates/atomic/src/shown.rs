// SPDX-License-Identifier: Apache-2.0

//! What atomic prints from a gate, made safe for a terminal, and how a typed
//! answer to a question gate is read, the same way at the prompt and in
//! `atomic gates answer`.
//!
//! A gate's title, what it shows and its choices can come from a model, so
//! they are untrusted text: printed as they are, control sequences in them
//! could move the cursor, rewrite or hide the lines around them, set the
//! window title or make the terminal answer back into the prompt's input.
//! Everything printed from a gate goes through [`line`], which replaces C0
//! and C1 controls, DEL, and Unicode format and bidirectional controls with
//! visible escapes, one line at a time.

use atomic_durable::gate::Question;
use serde_json::Value;

/// Whether `c` must not reach a terminal as it is.
fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            // Zero-width and directional marks, bidi embeddings, overrides
            // and isolates, line and paragraph separators, the BOM.
            '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
}

fn escape(c: char, out: &mut String) {
    match c {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        c => out.push_str(&format!("\\u{{{:04x}}}", u32::from(c))),
    }
}

/// `text` on one line, with every unsafe character escaped.
pub fn line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if unsafe_char(c) {
            escape(c, &mut out);
        } else {
            out.push(c);
        }
    }
    out
}

/// JSON text, such as `serde_json` writes, with every unsafe character in
/// it written as a JSON escape: still valid JSON, and the same value, but
/// safe on a terminal. `serde_json` escapes C0 controls itself; DEL, C1
/// controls and the Unicode format and bidirectional controls it writes as
/// they are. A pretty form's own line breaks stay.
pub fn json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c != '\n' && unsafe_char(c) {
            // Every unsafe character is in the Basic Multilingual Plane, so
            // one `\uXXXX` escape writes it.
            out.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

/// A JSON value for a person: a string as its text, anything else as JSON,
/// on one line, safe.
pub fn value(v: &Value) -> String {
    match v {
        Value::String(s) => line(s),
        other => line(&other.to_string()),
    }
}

/// The label a choice is shown with: `a`, `b`, … `z`, then `aa`, `ab`, ….
pub fn label(index: usize) -> String {
    let letters = b"abcdefghijklmnopqrstuvwxyz";
    if index < letters.len() {
        char::from(letters[index]).to_string()
    } else {
        format!(
            "{}{}",
            label(index / letters.len() - 1),
            label(index % letters.len())
        )
    }
}

/// The answer `text` gives `question`: a choice by its exact value first,
/// then by its label; for a free text question the text itself; otherwise
/// the text as JSON if it is JSON, as a string if not. Whether it fits is
/// for the gate store to say.
pub fn answer(question: &Question, text: &str) -> Value {
    let text = text.trim();
    let typed: Value =
        serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()));
    if let Some(choices) = question.choices() {
        if let Some(exact) = choices
            .iter()
            .find(|c| c.as_str() == Some(text) || **c == typed)
        {
            return exact.clone();
        }
        if let Some(i) = (0..choices.len()).find(|i| label(*i) == text) {
            return choices[i].clone();
        }
        return typed;
    }
    if question.answer.get("type").and_then(Value::as_str) == Some("string") {
        return Value::String(text.to_owned());
    }
    typed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn controls_reach_no_terminal() {
        let hostile =
            "Ship\u{1b}[2J\u{1b}]0;owned\u{7}\u{9b}31m\u{202e}txt.exe\u{200b}\r\nnow\u{7f}";
        let safe = line(hostile);
        assert!(safe.chars().all(|c| !unsafe_char(c)), "{safe}");
        assert_eq!(
            safe,
            "Ship\\u{001b}[2J\\u{001b}]0;owned\\u{0007}\\u{009b}31m\\u{202e}txt.exe\\u{200b}\\r\\nnow\\u{007f}"
        );
        assert_eq!(
            line("café · 日本"),
            "café · 日本",
            "ordinary text is untouched"
        );
    }

    /// JSON for a program, shown on a terminal: no unsafe character reaches
    /// it as it is, and it still reads as the same value.
    #[test]
    fn json_reaches_no_terminal_with_a_control_in_it() {
        let hostile = json!({
            "title": "Ship\u{1b}[2J\u{7f}\u{9b}31m\u{85}\u{202e}gpj.exe\u{2066}\u{feff}",
            "show": {"line\u{202e}": ["\u{200b}", 1]},
        });
        let written = json(&serde_json::to_string_pretty(&hostile).expect("JSON"));
        assert!(
            written.chars().all(|c| c == '\n' || !unsafe_char(c)),
            "{written:?}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&written).expect("still JSON"),
            hostile
        );
        assert!(
            written.contains("\\u007f\\u009b31m\\u0085\\u202egpj.exe"),
            "{written}"
        );
        // Ordinary text, and the pretty form's line breaks, stay.
        let plain = serde_json::to_string_pretty(&json!({"a": "café · 日本"})).expect("JSON");
        assert_eq!(json(&plain), plain);
    }

    #[test]
    fn an_answer_is_a_choice_by_value_first_then_by_label() {
        let numbers = Question {
            answer: json!({"enum": [10, 20, 1]}),
            remember: false,
            default: None,
        };
        assert_eq!(
            answer(&numbers, "1"),
            json!(1),
            "the value 1, not the first choice"
        );
        assert_eq!(answer(&numbers, "b"), json!(20));
        let words = Question {
            answer: json!({"enum": ["a", "beta"]}),
            remember: false,
            default: None,
        };
        assert_eq!(answer(&words, "a"), json!("a"), "the value a, not a label");
        assert_eq!(answer(&words, "b"), json!("beta"));
        assert_eq!(answer(&words, "nightly"), json!("nightly"));
        let free = Question {
            answer: json!({"type": "string"}),
            remember: false,
            default: None,
        };
        assert_eq!(answer(&free, "12"), json!("12"));
        assert_eq!(label(0), "a");
        assert_eq!(label(26), "aa");
    }
}
