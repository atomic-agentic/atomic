// SPDX-License-Identifier: Apache-2.0

//! Whether the tags of an `.astro` template balance. The Astro compiler
//! closes an element left open at the end, drops a closing tag that closes
//! nothing, and lets a closing tag of another name close the innermost
//! element, all without a word. Tags are a prompt's structure, so atomic
//! refuses each of these when the file loads, naming the tag and its line.
//!
//! The template is read as Astro's tokenizer reads it, with one stack of
//! what is open: markup, where quotes and slashes are text and `{` opens an
//! expression; an expression, read as TypeScript, where `<` before a letter,
//! `>` or `/` starts a tag; a template literal; and a start tag's
//! attributes. An element's children are markup again, wherever it opens,
//! so tags balance inside an expression too. The frontmatter, HTML
//! comments, a ```` ```mermaid ```` fence and the text of a raw text element
//! (`<style>`, `<textarea>`, `<title>`, an element marked `is:raw`, …) are
//! not read for tags. A void element (`<br>`) and a self-closing tag
//! (`<Note />`) open nothing. Names match exactly: Astro reads a
//! capitalised tag as a component, so `<Input>` is not `<input>`.

/// HTML's void elements, which never have content or a closing tag.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "command", "embed", "hr", "img", "input", "keygen", "link",
    "meta", "param", "source", "track", "wbr",
];

/// HTML's elements whose content is text up to their own closing tag.
const RAW: &[&str] = &[
    "script",
    "style",
    "textarea",
    "title",
    "iframe",
    "noembed",
    "noframes",
    "plaintext",
    "xmp",
];

/// An unbalanced tag: its one-based line in the source, and what is wrong.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unbalanced {
    pub line: usize,
    pub message: String,
}

/// What is open where the scan is.
enum Frame {
    /// An element, whose children are markup: its name, and where it opened.
    Element(String, usize),
    /// A `{…}` expression, and how many plain braces are open inside it.
    Expr(usize),
    /// A template literal's text.
    Template,
    /// A start tag's attributes: its name, where it opened, and whether it
    /// is marked `is:raw`.
    Tag(String, usize, bool),
}

/// The first unbalanced tag in `source`, an `.astro` file, if any.
pub(crate) fn unbalanced(source: &str) -> Option<Unbalanced> {
    let start = template_start(source);
    let literal = literal_fences(source, start);
    Scan {
        text: &literal,
        bytes: literal.as_bytes(),
        at: start,
        open: Vec::new(),
    }
    .run()
}

struct Scan<'a> {
    text: &'a str,
    bytes: &'a [u8],
    at: usize,
    open: Vec<Frame>,
}

impl Scan<'_> {
    fn run(mut self) -> Option<Unbalanced> {
        while self.at < self.bytes.len() {
            let found = match self.open.last() {
                None | Some(Frame::Element(..)) => self.markup(),
                Some(Frame::Expr(_)) => self.expression(),
                Some(Frame::Template) => {
                    self.template();
                    None
                }
                Some(Frame::Tag(..)) => {
                    self.attributes();
                    None
                }
            };
            if found.is_some() {
                return found;
            }
        }
        // The innermost element still open, if any; an expression, tag or
        // template literal left open is the compiler's to refuse.
        self.open.iter().rev().find_map(|frame| match frame {
            Frame::Element(name, at) => Some(Unbalanced {
                line: self.line(*at),
                message: format!("{} is never closed; close it with `</{name}>`", shown(name)),
            }),
            _ => None,
        })
    }

    fn line(&self, at: usize) -> usize {
        1 + self.bytes[..at].iter().filter(|b| **b == b'\n').count()
    }

    fn rest(&self) -> &[u8] {
        &self.bytes[self.at..]
    }

    /// Whether the `<` at the scan starts a tag: before a name, which may
    /// be in any script (`<règles>`), `/` or `>`.
    fn starts_tag(&self) -> bool {
        self.text[self.at + 1..]
            .chars()
            .next()
            .is_some_and(opens_tag)
    }

    /// One step of markup: text, a comment, a tag, or an expression.
    fn markup(&mut self) -> Option<Unbalanced> {
        let rest = self.rest();
        if rest.starts_with(b"<!--") {
            self.at = find(self.bytes, self.at + 4, b"-->").map_or(self.bytes.len(), |e| e + 3);
        } else if rest.starts_with(b"<!") {
            self.at = find(self.bytes, self.at, b">").map_or(self.bytes.len(), |e| e + 1);
        } else if rest.starts_with(b"<") && self.starts_tag() {
            return self.tag();
        } else if rest.starts_with(b"{") {
            self.open.push(Frame::Expr(0));
            self.at += 1;
        } else {
            self.at += 1;
        }
        None
    }

    /// One step of an expression, read as TypeScript.
    fn expression(&mut self) -> Option<Unbalanced> {
        let rest = self.rest();
        match rest[0] {
            b'<' if self.starts_tag() => return self.tag(),
            b'{' => {
                if let Some(Frame::Expr(depth)) = self.open.last_mut() {
                    *depth += 1;
                }
                self.at += 1;
            }
            b'}' => {
                self.at += 1;
                match self.open.last_mut() {
                    Some(Frame::Expr(depth)) if *depth > 0 => *depth -= 1,
                    _ => {
                        self.open.pop();
                    }
                }
            }
            b'"' | b'\'' => self.at = past_string(self.bytes, self.at),
            b'`' => {
                self.open.push(Frame::Template);
                self.at += 1;
            }
            b'/' if rest.get(1) == Some(&b'/') => {
                self.at = find(self.bytes, self.at, b"\n").map_or(self.bytes.len(), |e| e + 1);
            }
            b'/' if rest.get(1) == Some(&b'*') => {
                self.at = find(self.bytes, self.at + 2, b"*/").map_or(self.bytes.len(), |e| e + 2);
            }
            _ => self.at += 1,
        }
        None
    }

    /// One step of a template literal's text.
    fn template(&mut self) {
        let rest = self.rest();
        match rest[0] {
            b'\\' => self.at += 2,
            b'`' => {
                self.open.pop();
                self.at += 1;
            }
            b'$' if rest.get(1) == Some(&b'{') => {
                self.open.push(Frame::Expr(0));
                self.at += 2;
            }
            _ => self.at += 1,
        }
    }

    /// One step of a start tag's attributes, up to the tag's end.
    fn attributes(&mut self) {
        let rest = self.rest();
        match rest[0] {
            b'"' | b'\'' => {
                let quote = rest[0];
                self.at = rest[1..]
                    .iter()
                    .position(|b| *b == quote)
                    .map_or(self.bytes.len(), |p| self.at + p + 2);
            }
            b'{' => {
                self.open.push(Frame::Expr(0));
                self.at += 1;
            }
            b'`' => {
                self.open.push(Frame::Template);
                self.at += 1;
            }
            b'/' if rest.get(1) == Some(&b'>') => {
                // Self-closing: it opens nothing.
                self.open.pop();
                self.at += 2;
            }
            b'>' => {
                self.at += 1;
                if let Some(Frame::Tag(name, at, raw)) = self.open.pop() {
                    self.opened(name, at, raw);
                }
            }
            b if b.is_ascii_whitespace() || b == b'=' || b == b'/' => self.at += 1,
            _ => {
                let len = rest
                    .iter()
                    .take_while(|b| {
                        !b.is_ascii_whitespace()
                            && !matches!(b, b'=' | b'>' | b'/' | b'"' | b'\'' | b'{' | b'`')
                    })
                    .count()
                    .max(1);
                if &rest[..len] == b"is:raw"
                    && let Some(Frame::Tag(_, _, raw)) = self.open.last_mut()
                {
                    *raw = true;
                }
                self.at += len;
            }
        }
    }

    /// A start tag has ended: what it opens.
    fn opened(&mut self, name: String, at: usize, raw: bool) {
        if VOID.contains(&name.as_str()) {
            return;
        }
        if name == "plaintext" {
            self.at = self.bytes.len();
            return;
        }
        if raw || RAW.contains(&name.as_str()) {
            // Text up to its own closing tag, the whole name matched.
            let close = format!("</{name}");
            let mut from = self.at;
            while let Some(e) = find(self.bytes, from, close.as_bytes()) {
                let after = self.bytes.get(e + close.len());
                if after.is_none_or(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/') {
                    self.at = find(self.bytes, e, b">").map_or(self.bytes.len(), |e| e + 1);
                    return;
                }
                from = e + close.len();
            }
            // Never closed: the end of the file is reported below.
            self.at = self.bytes.len();
        }
        self.open.push(Frame::Element(name, at));
    }

    /// A tag starting at `<`: a start tag, a closing tag or a fragment's.
    fn tag(&mut self) -> Option<Unbalanced> {
        let start = self.at;
        if self.bytes.get(start + 1) == Some(&b'/') {
            let (name, end) = name_at(self.bytes, start + 2);
            self.at = find(self.bytes, end, b">").map_or(self.bytes.len(), |e| e + 1);
            return self.closed(&name, start);
        }
        if self.bytes.get(start + 1) == Some(&b'>') {
            // A fragment.
            self.at = start + 2;
            self.open.push(Frame::Element(String::new(), start));
            return None;
        }
        let (name, end) = name_at(self.bytes, start + 1);
        self.at = end;
        self.open.push(Frame::Tag(name, start, false));
        None
    }

    /// A closing tag for `name` at `start`: it must close the innermost
    /// element open.
    fn closed(&mut self, name: &str, start: usize) -> Option<Unbalanced> {
        let line = || self.line(start);
        match self.open.last() {
            Some(Frame::Element(opened, _)) if opened == name => {
                self.open.pop();
                None
            }
            Some(Frame::Element(opened, at)) => Some(Unbalanced {
                line: line(),
                message: format!(
                    "`</{name}>` closes {}, opened on line {}; close it with `</{opened}>`",
                    shown(opened),
                    self.line(*at),
                ),
            }),
            _ if VOID.contains(&name) => Some(Unbalanced {
                line: line(),
                message: format!(
                    "`</{name}>` closes nothing: <{name}> is a void HTML element, which has no \
                     closing tag and cannot be a section's name"
                ),
            }),
            _ => Some(Unbalanced {
                line: line(),
                message: format!("`</{name}>` closes no open element; remove it"),
            }),
        }
    }
}

/// Whether `c`, after a `<`, starts a tag.
fn opens_tag(c: char) -> bool {
    if c == '/' || c == '>' {
        return true;
    }
    c.is_alphabetic()
}

/// An element's name as a message shows it.
fn shown(name: &str) -> String {
    format!("<{name}>")
}

/// A tag's name from `at`, running to white space, `/` or `>`, and where
/// it ends.
fn name_at(bytes: &[u8], at: usize) -> (String, usize) {
    let end = at
        + bytes[at.min(bytes.len())..]
            .iter()
            .take_while(|b| !b.is_ascii_whitespace() && !matches!(b, b'/' | b'>'))
            .count();
    (
        String::from_utf8_lossy(&bytes[at.min(end)..end]).into_owned(),
        end,
    )
}

/// Where the template starts: after the frontmatter, if the file has one.
/// As in Astro, white space, a byte order mark or HTML comments may come
/// before its opening `---`, and a `---` inside one of its strings,
/// template literals or comments does not end it. Its closing `---` starts
/// a line; anything after it on that line is template.
fn template_start(source: &str) -> usize {
    let bytes = source.as_bytes();
    let mut at = 0;
    loop {
        if bytes[at..].starts_with("\u{feff}".as_bytes()) {
            at += 3;
        } else if bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        } else if bytes[at..].starts_with(b"<!--") {
            match find(bytes, at + 4, b"-->") {
                Some(e) => at = e + 3,
                None => return 0,
            }
        } else {
            break;
        }
    }
    if !bytes[at..].starts_with(b"---") {
        return 0;
    }
    at += 3;
    let mut line_start = false;
    while at < bytes.len() {
        if line_start {
            line_start = false;
            let indent = bytes[at..]
                .iter()
                .take_while(|b| matches!(b, b' ' | b'\t'))
                .count();
            if bytes[at + indent..].starts_with(b"---") {
                return at + indent + 3;
            }
        }
        match bytes[at] {
            b'\n' => {
                line_start = true;
                at += 1;
            }
            b'"' | b'\'' => at = past_string(bytes, at),
            b'`' => at = past_frontmatter_template(bytes, at + 1),
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                at = find(bytes, at, b"\n").unwrap_or(bytes.len());
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at = find(bytes, at + 2, b"*/").map_or(bytes.len(), |e| e + 2);
            }
            _ => at += 1,
        }
    }
    // A frontmatter with no end: no template to read.
    bytes.len()
}

/// Past a template literal in the frontmatter from just after its opening
/// backtick, its `${…}` parts and the literals in them included.
fn past_frontmatter_template(bytes: &[u8], mut at: usize) -> usize {
    let mut depth = 0usize;
    // Inside `${…}` from depth one on.
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'`' if depth == 0 => return at + 1,
            b'`' => at = past_frontmatter_template(bytes, at + 1),
            b'$' if depth == 0 && bytes.get(at + 1) == Some(&b'{') => {
                depth = 1;
                at += 2;
            }
            b'{' if depth > 0 => {
                depth += 1;
                at += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                at += 1;
            }
            b'"' | b'\'' if depth > 0 => at = past_string(bytes, at),
            _ => at += 1,
        }
    }
    bytes.len()
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

#[cfg(test)]
mod tests {
    use super::*;

    const FRONT: &str = "---\nconst { input } = Astro.props;\nconst x = '<section>';\n---\n";

    fn checked(body: &str) -> Option<Unbalanced> {
        unbalanced(&format!("{FRONT}{body}"))
    }

    /// Templates the Astro compiler reads as balanced pass, including the
    /// text of markup inside an expression, where quotes and slashes are
    /// prose.
    #[test]
    fn balanced_templates_pass() {
        for body in [
            "Note: {input.text}",
            "<section>\nx\n</section>",
            "<section><notes>a</notes><br><hr/><img src=\"a>b\"></section>",
            "<Note title={`a ${'}'} </x>`} />\n<Note>{input.text}</Note>",
            "<!-- <section> -->\n<p>a < b and c > d</p>",
            "{input.items.map((i) => <li>{i}</li>)}",
            "<section>{input.urgent && <note>Don't delay.</note>}</section>",
            "<section>{input.urgent && <note>See https://example.com/a and src/**/*.ts, \"quoted` too</note>}</section>",
            "{input.urgent ? <note>It's here</note> : <note>It isn't</note>}",
            "<section>{input.items.map((i) => <li>{i.name}: don't {i.done ? <b>done</b> : 'open'}</li>)}</section>",
            "<style>p > a { color: red; } </p></style>",
            "<pre is:raw><section></pre>",
            "<title>a</titles> b</title>",
            "<>\n<Section>a</Section>\n</>",
            "<Source>a</Source><Input>b</Input><Title><b>c</b></Title><Link>d</Link><Meta>e</Meta><Textarea><f>g</f></Textarea>",
            "<règles>Be brief.</règles><éléments>Short.</éléments>",
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

    /// Inside an expression, tags balance too.
    #[test]
    fn a_mismatch_inside_an_expression_is_refused() {
        let found = checked("<section>\n{input.urgent && <note>Don't\ndelay.</notes>}\n</section>")
            .expect("refused");
        assert_eq!(found.line, 7);
        assert_eq!(
            found.message,
            "`</notes>` closes <note>, opened on line 6; close it with `</note>`"
        );
    }

    /// Prose in markup inside an expression ends nothing early: what comes
    /// after the expression is still read.
    #[test]
    fn prose_inside_an_expression_does_not_hide_what_follows() {
        for prose in [
            "Don't delay.",
            "See https://example.com/x",
            "Match src/*.ts",
        ] {
            let found = checked(&format!(
                "{{input.urgent && <note>{prose}</note>}}\n</section>"
            ))
            .expect("refused");
            assert_eq!(found.line, 6, "{prose}");
            assert_eq!(
                found.message, "`</section>` closes no open element; remove it",
                "{prose}"
            );
        }
    }

    #[test]
    fn an_unclosed_element_is_refused_where_it_opens() {
        let found = checked("a\n<section>\nx\n").expect("refused");
        assert_eq!(found.line, 6);
        assert_eq!(
            found.message,
            "<section> is never closed; close it with `</section>`"
        );
        let found = checked("<règles>\nx").expect("refused");
        assert_eq!(
            found.message,
            "<règles> is never closed; close it with `</règles>`"
        );
        // A component named as a void element is a component.
        let found = checked("<Source>\nx").expect("refused");
        assert_eq!(
            found.message,
            "<Source> is never closed; close it with `</Source>`"
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

    /// `<input>` is void, so it cannot wrap a section.
    #[test]
    fn a_void_element_used_as_a_section_is_refused() {
        let found = checked("<input>x</input>").expect("refused");
        assert_eq!(
            found.message,
            "`</input>` closes nothing: <input> is a void HTML element, which has no closing \
             tag and cannot be a section's name"
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

    /// The frontmatter is found as Astro finds it: after a byte order mark,
    /// blank lines or a comment, and ended only by a `---` that starts a
    /// line outside its strings and comments.
    #[test]
    fn the_frontmatter_is_found_as_astro_finds_it() {
        let found = |source: &str| unbalanced(source).map(|u| (u.line, u.message));
        let stray = "`</a>` closes no open element; remove it".to_owned();
        assert_eq!(
            found("\u{feff}\n<!-- c -->\n---\nconst a = 1;\n---\n</a>"),
            Some((6, stray.clone()))
        );
        assert_eq!(found("---\nconst s = `\n---\n</a>`;\n---\n<b>x</b>"), None);
        assert_eq!(
            found("---\n// ---\n/* \n---\n</a> */\n---\n</a>"),
            Some((7, stray.clone()))
        );
        assert_eq!(found("---\nconst a = 1;\n---</a>"), Some((3, stray)));
    }
}
