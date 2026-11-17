// SPDX-License-Identifier: Apache-2.0

//! The tag recognizer for atom bodies (`.atom.md`).
//!
//! `{% … %}` tags are found on the source text before any Markdown parsing,
//! so Markdown never reinterprets what is inside a tag: an underscore or an
//! asterisk in a path or an expression stays literal. Inside a fenced code
//! block (```` ``` ```` or `~~~`) `{%` is plain text. The grammar is published
//! in `docs/reference/atom-tags.md`; any tag outside it fails the load with
//! its line and column.
//!
//! This module only recognizes and balances tags. Checking them against
//! schemas and files is the job of the compile passes.

/// A position in the body, both 1-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct At {
    /// Line, from 1.
    pub line: usize,
    /// Column in characters, from 1.
    pub column: usize,
}

/// A recognized tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tag {
    /// `{% $root.path %}`: insert a value. `path` holds the segments after the
    /// root, exactly as written (`$input.file_path` is root `input`, path
    /// `["file_path"]`).
    Value {
        /// `input`, `frontmatter`, or a loop or partial variable.
        root: String,
        /// The field names and list indexes after the root.
        path: Vec<String>,
    },
    /// `{% if <CEL> %}`.
    If {
        /// The condition, as written.
        condition: String,
    },
    /// `{% else /%}`.
    Else,
    /// `{% /if %}`.
    EndIf,
    /// `{% partial file="…" /%}`.
    Partial {
        /// The included file, relative to the atom.
        file: String,
    },
    /// `{% example name="…" %}`.
    Example {
        /// The example's name.
        name: String,
    },
    /// `{% /example %}`.
    EndExample,
}

/// A piece of the body: literal text, or a tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    /// Text, passed through as written.
    Text(String),
    /// A tag, and where it starts.
    Tag(Tag, At),
}

/// Why a body's tags could not be recognized.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{}:{}: {message}", at.line, at.column)]
pub struct TagError {
    /// Where the problem is.
    pub at: At,
    /// What is wrong.
    pub message: String,
}

/// Tags the published grammar defines that this version does not run yet.
const LATER: &[&str] = &["tool", "schema", "for", "render", "file", "slot"];

/// Split `body` into text and tags, and check that blocks balance.
pub fn recognize(body: &str) -> Result<Vec<Piece>, TagError> {
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut open: Vec<(&'static str, At)> = Vec::new();
    let mut fence: Option<String> = None;

    for (index, line) in body.split_inclusive('\n').enumerate() {
        let line_no = index + 1;
        let trimmed = line.trim_start();
        // Fenced code blocks are literal.
        let marker: String = trimmed
            .chars()
            .take_while(|c| *c == '`' || *c == '~')
            .collect();
        if let Some(f) = &fence {
            if marker.len() >= f.len()
                && marker.starts_with(&f[..1])
                && trimmed[marker.len()..].trim().is_empty()
            {
                fence = None;
            }
            text.push_str(line);
            continue;
        }
        if marker.len() >= 3 && marker.chars().all(|c| c == marker.as_bytes()[0] as char) {
            fence = Some(marker);
            text.push_str(line);
            continue;
        }

        let mut rest = line;
        let mut column = 1;
        while let Some(start) = rest.find("{%") {
            let before = &rest[..start];
            text.push_str(before);
            column += before.chars().count();
            let at = At {
                line: line_no,
                column,
            };
            let after_open = &rest[start + 2..];
            let end = close_of(after_open).ok_or_else(|| TagError {
                at,
                message: "a tag opened with {% is not closed with %} on the same line".into(),
            })?;
            let inner = &after_open[..end];
            if !text.is_empty() {
                pieces.push(Piece::Text(std::mem::take(&mut text)));
            }
            let tag = parse(inner, at)?;
            balance(&tag, at, &mut open)?;
            pieces.push(Piece::Tag(tag, at));
            let consumed = start + 2 + end + 2;
            column += rest[start..consumed].chars().count();
            rest = &rest[consumed..];
        }
        text.push_str(rest);
    }
    if let Some((name, at)) = open.pop() {
        return Err(TagError {
            at,
            message: format!("{{% {name} %}} is never closed with {{% /{name} %}}"),
        });
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    Ok(pieces)
}

/// Where the closing `%}` is, skipping quoted strings.
fn close_of(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' {
                    i += 1;
                } else if b == q {
                    quote = None;
                }
            }
            None => {
                if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == b'%' && bytes.get(i + 1) == Some(&b'}') {
                    return Some(i);
                } else if b == b'\n' {
                    return None;
                }
            }
        }
        i += 1;
    }
    None
}

fn parse(inner: &str, at: At) -> Result<Tag, TagError> {
    let err = |message: String| TagError { at, message };
    let body = inner.trim();
    let (body, self_closing) = match body.strip_suffix('/') {
        Some(b) => (b.trim_end(), true),
        None => (body, false),
    };
    if let Some(path) = body.strip_prefix('$') {
        return value(path).ok_or_else(|| {
            err(format!(
                "{{% ${path} %}} is not a value path such as $input.title"
            ))
        });
    }
    if let Some(name) = body.strip_prefix('/') {
        return match name.trim() {
            "if" => Ok(Tag::EndIf),
            "example" => Ok(Tag::EndExample),
            other => Err(err(format!("{{% /{other} %}} closes no tag atomic knows"))),
        };
    }
    let name: String = body
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let args = body[name.len()..].trim();
    match name.as_str() {
        "if" if !args.is_empty() && !self_closing => Ok(Tag::If {
            condition: args.to_owned(),
        }),
        "if" => Err(err(
            "{% if %} needs a condition and a body: {% if <CEL> %}…{% /if %}".into(),
        )),
        "else" if self_closing && args.is_empty() => Ok(Tag::Else),
        "else" => Err(err("write else as {% else /%}".into())),
        "partial" if self_closing => Ok(Tag::Partial {
            file: attribute(args, "file")
                .ok_or_else(|| err("{% partial file=\"…\" /%} needs a file".into()))?,
        }),
        "partial" => Err(err(
            "in this version a partial has no body: write {% partial file=\"…\" /%}".into(),
        )),
        "example" if !self_closing => Ok(Tag::Example {
            name: attribute(args, "name")
                .ok_or_else(|| err("{% example name=\"…\" %} needs a name".into()))?,
        }),
        "example" => Err(err(
            "an example has a body: {% example name=\"…\" %}…{% /example %}".into(),
        )),
        n if LATER.contains(&n) => Err(err(format!(
            "the {{% {n} %}} tag is not supported by this version of atomic yet"
        ))),
        n if body[n.len()..].starts_with('(') => Err(err(format!(
            "function calls such as {{% {n}(…) %}} are not supported by this version of atomic yet"
        ))),
        "" => Err(err(format!("{{% {body} %}} is not a tag"))),
        n => Err(err(format!("unknown tag {{% {n} %}}"))),
    }
}

/// `$root(.name | [index])*`, with names of letters, digits and underscores.
fn value(path: &str) -> Option<Tag> {
    let mut segments = Vec::new();
    let mut rest = path;
    let ident = |s: &str| -> usize {
        s.chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .count()
    };
    let n = ident(rest);
    if n == 0 || rest.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let root = rest[..n].to_owned();
    rest = &rest[n..];
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            let n = ident(r);
            if n == 0 {
                return None;
            }
            segments.push(r[..n].to_owned());
            rest = &r[n..];
        } else if let Some(r) = rest.strip_prefix('[') {
            let close = r.find(']')?;
            let index = &r[..close];
            if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            segments.push(index.to_owned());
            rest = &r[close + 1..];
        } else {
            return None;
        }
    }
    Some(Tag::Value {
        root,
        path: segments,
    })
}

/// The value of `name="…"` in `args`.
fn attribute(args: &str, name: &str) -> Option<String> {
    let start = args.find(&format!("{name}=\""))? + name.len() + 2;
    let len = args[start..].find('"')?;
    Some(args[start..start + len].to_owned())
}

fn balance(tag: &Tag, at: At, open: &mut Vec<(&'static str, At)>) -> Result<(), TagError> {
    let err = |message: String| TagError { at, message };
    match tag {
        Tag::If { .. } => open.push(("if", at)),
        Tag::Example { .. } => {
            if open.iter().any(|(n, _)| *n == "example") {
                return Err(err("an example cannot contain another example".into()));
            }
            open.push(("example", at));
        }
        Tag::Else => {
            if open.last().map(|(n, _)| *n) != Some("if") {
                return Err(err("{% else /%} outside an {% if %}".into()));
            }
        }
        Tag::EndIf | Tag::EndExample => {
            let want = if matches!(tag, Tag::EndIf) {
                "if"
            } else {
                "example"
            };
            match open.pop() {
                Some((n, _)) if n == want => {}
                Some((n, opened)) => {
                    return Err(err(format!(
                        "{{% /{want} %}} closes {{% {n} %}} opened at {}:{}",
                        opened.line, opened.column
                    )));
                }
                None => {
                    return Err(err(format!(
                        "{{% /{want} %}} has no matching {{% {want} %}}"
                    )));
                }
            }
        }
        Tag::Value { .. } | Tag::Partial { .. } => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests;
