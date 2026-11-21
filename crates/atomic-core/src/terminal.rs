// SPDX-License-Identifier: Apache-2.0

//! Text made safe for a terminal. What atomic writes for a person can carry
//! what a model, a command or a run's caller wrote: printed as it is, a
//! control sequence in it could move the cursor, rewrite or hide the lines
//! around it, set the window title or make the terminal answer back into a
//! prompt's input. [`line`] and [`block`] replace C0 and C1 controls, DEL,
//! and Unicode format and bidirectional controls with visible escapes.
//!
//! Text is redacted before it is escaped, never after: redaction reads a
//! terminal escape sequence around a token as it is written, not as it is
//! shown.

/// Whether `c` must not reach a terminal as it is.
pub fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            // Zero-width and directional marks, bidi embeddings, overrides
            // and isolates, line and paragraph separators, invisible
            // operators and deprecated format marks, the BOM, the soft
            // hyphen, the Mongolian vowel separator, and the tag
            // characters, which can hide text that a model wrote.
            '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}' | '\u{E0001}' | '\u{E0020}'..='\u{E007F}')
}

fn escape(c: char, out: &mut String) {
    match c {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        c => out.push_str(&format!("\\u{{{:04x}}}", u32::from(c))),
    }
}

/// `text` with every unsafe character escaped but those `keep` allows.
fn escaped(text: &str, keep: fn(char) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if keep(c) || !unsafe_char(c) {
            out.push(c);
        } else {
            escape(c, &mut out);
        }
    }
    out
}

/// `text` on one line, with every unsafe character escaped.
pub fn line(text: &str) -> String {
    escaped(text, |_| false)
}

/// `text` on as many lines as it has, its tabs kept, with every other
/// unsafe character escaped: for a model's answer, an error or a log line,
/// printed as text. Its output has nothing left to escape, so escaping it
/// again changes nothing.
pub fn block(text: &str) -> String {
    escaped(text, |c| ['\n', '\t'].contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_escaped_again_stays() {
        let hostile = "a\u{1b}[2J\tb\nc\u{202e}\r";
        let once = block(hostile);
        assert_eq!(once, "a\\u{001b}[2J\tb\nc\\u{202e}\\r");
        assert_eq!(block(&once), once);
    }
}
