// SPDX-License-Identifier: Apache-2.0

//! Whether the tags of an `.astro` template balance. The Astro compiler
//! closes an element left open at the end, drops a closing tag that closes
//! nothing, and lets a closing tag of another name close the innermost
//! element, all without a word. Tags are a prompt's structure, so atomic
//! refuses each of these when the file loads, naming the tag and its line.
//!
//! Only the template is read: the frontmatter, an HTML comment, and an
//! `{…}` expression are passed over (TypeScript checks the markup inside an
//! expression), as is the raw text of `<script>`, `<style>`, `<textarea>`,
//! `<title>` and of an element marked `is:raw`. A void element (`<br>`) and
//! a self-closing tag (`<Note />`) open nothing.

/// Elements that never have content or a closing tag.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose content is text up to their own closing tag.
const RAW: &[&str] = &["script", "style", "textarea", "title"];

/// An unbalanced tag: its one-based line in the source, and what is wrong.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unbalanced {
    pub line: usize,
    pub message: String,
}

/// The first unbalanced tag in `source`, an `.astro` file, if any.
pub(crate) fn unbalanced(source: &str) -> Option<Unbalanced> {
    let mut at = template_start(source);
    let literal = literal_fences(source, at);
    let bytes = literal.as_bytes();
    // Open elements: name and where each opened.
    let mut open: Vec<(String, usize)> = Vec::new();
    let line_at = |at: usize| 1 + bytes[..at].iter().filter(|b| **b == b'\n').count();
    let shown = |name: &str| format!("<{name}>");
    while at < bytes.len() {
        match bytes[at] {
            b'{' => at = past_expression(bytes, at),
            b'<' if bytes[at..].starts_with(b"<!--") => {
                at = find(bytes, at + 4, b"-->").map_or(bytes.len(), |end| end + 3);
            }
            b'<' if bytes.get(at + 1) == Some(&b'!') => {
                // `<!DOCTYPE …>` and the like.
                at = find(bytes, at, b">").map_or(bytes.len(), |end| end + 1);
            }
            b'<' if bytes.get(at + 1) == Some(&b'/') => {
                let start = at;
                let (name, end) = tag_name(bytes, at + 2);
                let ends = if name.is_empty() {
                    // `</>` closes a fragment.
                    bytes.get(end) == Some(&b'>')
                } else {
                    matches!(bytes.get(end), Some(b'>' | b' ' | b'\t' | b'\n' | b'\r'))
                };
                if !ends {
                    // `a </ b`: text.
                    at += 1;
                    continue;
                }
                at = find(bytes, end, b">").map_or(bytes.len(), |e| e + 1);
                match open.pop() {
                    Some((opened, _)) if opened == name => {}
                    Some((opened, opened_at)) => {
                        let opened_line = line_at(opened_at);
                        return Some(Unbalanced {
                            line: line_at(start),
                            message: format!(
                                "`</{name}>` closes {}, opened on line {opened_line}; close it with `</{opened}>`",
                                shown(&opened),
                            ),
                        });
                    }
                    None => {
                        return Some(Unbalanced {
                            line: line_at(start),
                            message: format!("`</{name}>` closes no open element; remove it"),
                        });
                    }
                }
            }
            b'<' => {
                let start = at;
                let (name, end) = tag_name(bytes, at + 1);
                let fragment = bytes.get(at + 1) == Some(&b'>');
                if name.is_empty() && !fragment {
                    // `a < b`: text.
                    at += 1;
                    continue;
                }
                let (end, self_closing, raw) = past_attributes(bytes, end);
                at = end;
                if self_closing || VOID.contains(&name.to_ascii_lowercase().as_str()) {
                    continue;
                }
                if raw || RAW.contains(&name.to_ascii_lowercase().as_str()) {
                    let close = format!("</{}", name.to_ascii_lowercase());
                    let lower = bytes.to_ascii_lowercase();
                    match find(&lower, at, close.as_bytes()) {
                        Some(e) => {
                            at = find(bytes, e, b">").map_or(bytes.len(), |e| e + 1);
                            continue;
                        }
                        None => {
                            return Some(Unbalanced {
                                line: line_at(start),
                                message: format!(
                                    "{} is never closed; close it with `</{name}>`",
                                    shown(&name)
                                ),
                            });
                        }
                    }
                }
                open.push((name, start));
            }
            _ => at += 1,
        }
    }
    open.pop().map(|(name, opened_at)| Unbalanced {
        line: line_at(opened_at),
        message: format!(
            "{} is never closed; close it with `</{name}>`",
            shown(&name)
        ),
    })
}

/// Where the template starts: after the frontmatter, between a first line
/// of `---` and the next, if the file has one.
fn template_start(source: &str) -> usize {
    let mut at = 0;
    for (i, line) in source.split_inclusive('\n').enumerate() {
        at += line.len();
        match (i, line.trim() == "---") {
            (0, false) => return 0,
            (i, true) if i > 0 => return at,
            _ => {}
        }
    }
    // A frontmatter with no end: no template to read.
    source.len()
}

/// `source` with the brackets and braces of each ```` ```mermaid ```` fence
/// in its template from `start` blanked: a fence is read literally, as
/// diagram syntax, never as tags or expressions. Every byte keeps its place.
fn literal_fences(source: &str, start: usize) -> String {
    let mut out = String::with_capacity(source.len());
    out.push_str(&source[..start]);
    let mut open = false;
    for line in source[start..].split_inclusive('\n') {
        let trimmed = line.trim();
        if !open
            && trimmed
                .strip_prefix("```mermaid")
                .is_some_and(|rest| rest.trim().is_empty())
        {
            open = true;
        } else if open && trimmed == "```" {
            open = false;
        } else if open {
            out.extend(line.chars().map(|c| {
                if matches!(c, '<' | '>' | '{' | '}') {
                    ' '
                } else {
                    c
                }
            }));
            continue;
        }
        out.push_str(line);
    }
    out
}

/// The first index at or after `from` where `needle` starts.
fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

/// A tag's name from `at`, and where it ends.
fn tag_name(bytes: &[u8], at: usize) -> (String, usize) {
    if !bytes.get(at).is_some_and(u8::is_ascii_alphabetic) {
        return (String::new(), at);
    }
    let end = at
        + bytes[at..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
            .count();
    (String::from_utf8_lossy(&bytes[at..end]).into_owned(), end)
}

/// Past a start tag's attributes from `at`: where the tag ends, whether it
/// closes itself, and whether it is marked `is:raw`.
fn past_attributes(bytes: &[u8], mut at: usize) -> (usize, bool, bool) {
    let mut raw = false;
    while at < bytes.len() {
        match bytes[at] {
            b'>' => return (at + 1, at > 0 && bytes[at - 1] == b'/', raw),
            b'"' | b'\'' | b'`' => {
                let quote = bytes[at];
                at = bytes[at + 1..]
                    .iter()
                    .position(|b| *b == quote)
                    .map_or(bytes.len(), |p| at + 1 + p + 1);
            }
            b'{' => at = past_expression(bytes, at),
            _ => {
                if bytes[at..].starts_with(b"is:raw")
                    && !bytes
                        .get(at + 6)
                        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'-')
                {
                    raw = true;
                }
                at += 1;
            }
        }
    }
    (bytes.len(), false, raw)
}

/// Past the `{…}` expression that starts at `at`: its braces balanced,
/// with what is in its strings, template literals and comments passed over.
fn past_expression(bytes: &[u8], mut at: usize) -> usize {
    // Open braces, and for each whether it opened a template literal's
    // `${`, to go back into the literal when it closes.
    let mut depth: Vec<bool> = Vec::new();
    while at < bytes.len() {
        match bytes[at] {
            b'{' => {
                depth.push(false);
                at += 1;
            }
            b'}' => {
                at += 1;
                match depth.pop() {
                    Some(true) => at = past_template(bytes, at, &mut depth),
                    Some(false) => {}
                    None => return at,
                }
                if depth.is_empty() {
                    return at;
                }
            }
            b'"' | b'\'' => at = past_string(bytes, at),
            b'`' => at = past_template(bytes, at + 1, &mut depth),
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                at = find(bytes, at, b"\n").map_or(bytes.len(), |e| e + 1);
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at = find(bytes, at + 2, b"*/").map_or(bytes.len(), |e| e + 2);
            }
            _ => at += 1,
        }
    }
    bytes.len()
}

/// Past the quoted string that starts at `at`.
fn past_string(bytes: &[u8], at: usize) -> usize {
    let quote = bytes[at];
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote || b == b'\n' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// Past a template literal's text from `at`, just after its opening
/// backtick or a `${…}` it holds: to its closing backtick, or into its next
/// `${`, which `depth` then records.
fn past_template(bytes: &[u8], mut at: usize, depth: &mut Vec<bool>) -> usize {
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'`' => return at + 1,
            b'$' if bytes.get(at + 1) == Some(&b'{') => {
                depth.push(true);
                return at + 2;
            }
            _ => at += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRONT: &str = "---\nconst { input } = Astro.props;\nconst x = '<section>';\n---\n";

    fn checked(body: &str) -> Option<Unbalanced> {
        unbalanced(&format!("{FRONT}{body}"))
    }

    #[test]
    fn balanced_templates_pass() {
        for body in [
            "Note: {input.text}",
            "<section>\nx\n</section>",
            "<section><notes>a</notes><br><hr/><img src=\"a>b\"></section>",
            "<Note title={`a ${'}'} </x>`} />\n<Note>{input.text}</Note>",
            "<!-- <section> -->\n<p>a < b and c > d</p>",
            "{input.items.map((i) => <li>{i}</li>)}",
            "<style>p > a { color: red; } </p></style>",
            "<pre is:raw><section></pre>",
            "<>\n<Section>a</Section>\n</>",
            "<!DOCTYPE html>\n<p>x</p>",
            "```mermaid\nflowchart TD\n  A[Ticket] --> B{Urgent?}\n  B --> C<page>\n```\n<p>x</p>",
        ] {
            assert_eq!(checked(body), None, "{body}");
        }
    }

    #[test]
    fn a_mismatched_close_names_both_tags_and_its_line() {
        let found = checked("<section>\nx\n</notes>").expect("refused");
        assert_eq!(found.line, 7);
        assert_eq!(
            found.message,
            "`</notes>` closes <section>, opened on line 5; close it with `</section>`"
        );
    }

    #[test]
    fn an_unclosed_element_is_refused_where_it_opens() {
        let found = checked("a\n<section>\nx\n").expect("refused");
        assert_eq!(found.line, 6);
        assert_eq!(
            found.message,
            "<section> is never closed; close it with `</section>`"
        );
    }

    #[test]
    fn a_stray_close_is_refused() {
        let found = checked("x\n</section>").expect("refused");
        assert_eq!(found.line, 6);
        assert_eq!(
            found.message,
            "`</section>` closes no open element; remove it"
        );
    }

    /// Astro reads a capitalised tag as a component, so names match
    /// exactly.
    #[test]
    fn names_match_exactly() {
        let found = checked("<Note>x</note>").expect("refused");
        assert!(
            found.message.starts_with("`</note>` closes <Note>"),
            "{}",
            found.message
        );
    }

    #[test]
    fn an_unclosed_raw_element_is_refused() {
        let found = checked("<style>p {}").expect("refused");
        assert_eq!(
            found.message,
            "<style> is never closed; close it with `</style>`"
        );
    }

    /// A mermaid fence is diagram text, but after it tags count again.
    #[test]
    fn a_mermaid_fence_is_read_literally() {
        let found = checked("```mermaid\nA --> B<x>\n```\n</x>").expect("refused");
        assert_eq!(found.line, 8);
        assert_eq!(found.message, "`</x>` closes no open element; remove it");
    }

    #[test]
    fn a_file_with_no_frontmatter_is_read_whole() {
        assert_eq!(unbalanced("<a>x</a>"), None);
        assert!(unbalanced("<a>x").is_some());
    }
}
