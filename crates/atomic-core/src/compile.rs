// SPDX-License-Identifier: Apache-2.0

//! Compiling an atom's body.
//!
//! After an atom loads, its body is compiled once per file version, with the
//! `markdown` crate as the only Markdown parser:
//!
//! 1. **Partials** are resolved, recursively and relative to the file that
//!    includes them; an include cycle fails.
//! 2. **XML-style sections** are checked: a lowercase section tag alone on
//!    its line (`<rubric>`, `</rubric>`) must balance; an angle bracket inside
//!    a line of prose is plain text (`Vec<String>`); the `<data-…>` prefix is
//!    reserved for the values atomic inserts. A section opened without a blank
//!    line after it gets a warning, because Markdown then reads the block as
//!    raw HTML.
//! 3. **Examples** are read: each `{% example %}` holds a `json input` and a
//!    `json output` fenced block, which must validate against the atom's
//!    input and output contracts.
//! 4. **Known sections** (`## Rubric`, `## Out of scope`) are kept as
//!    metadata.
//! 5. A **token estimate** and a **content hash** are computed. The hash
//!    covers the atom file and every partial it uses, so editing a partial
//!    changes the hash of every atom that includes it.

use crate::atom::{Atom, AtomError};
use crate::tags::{self, Piece, Tag};
use atomic_schema::registry::Registry;
use markdown::mdast::Node;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A few-shot example, also run as an eval case.
#[derive(Clone, Debug, PartialEq)]
pub struct Example {
    /// Its name.
    pub name: String,
    /// The input.
    pub input: Value,
    /// The expected output.
    pub output: Value,
}

/// A compiled atom body.
#[derive(Clone, Debug)]
pub struct Compiled {
    /// SHA-256 over the atom file and every partial it uses.
    pub hash: String,
    /// The body with partials expanded, as text and tags.
    pub pieces: Vec<Piece>,
    /// The examples, in order.
    pub examples: Vec<Example>,
    /// Known sections by heading, for example `Rubric`.
    pub sections: BTreeMap<String, String>,
    /// A rough token count of the expanded body (characters / 4).
    pub token_estimate: usize,
    /// Partial files used, in first-use order.
    pub partials: Vec<PathBuf>,
    /// Problems that do not stop the load.
    pub warnings: Vec<String>,
}

/// Headings whose sections atomic keeps as metadata.
pub const KNOWN_SECTIONS: &[&str] = &["Rubric", "Out of scope"];

/// Compile `atom`'s body.
pub fn compile(atom: &Atom, registry: &Registry) -> Result<Compiled, AtomError> {
    let fail = |file: &Path, line: usize, message: String| AtomError {
        file: file.to_owned(),
        line,
        path: "body".into(),
        message,
    };
    let source =
        std::fs::read(&atom.file).map_err(|e| fail(&atom.file, 1, format!("cannot read: {e}")))?;

    // 1. Partials.
    let mut partials = Vec::new();
    let mut texts: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let pieces = expand(
        &atom.body,
        &atom.file,
        atom.body_line,
        &mut vec![atom.file.clone()],
        &mut partials,
        &mut texts,
        &fail,
    )?;

    // 2. Sections.
    let mut warnings = Vec::new();
    check_sections(
        &atom.body_text,
        &atom.file,
        atom.body_line,
        &mut warnings,
        &fail,
    )?;
    for (path, bytes) in &texts {
        check_sections(
            &String::from_utf8_lossy(bytes),
            path,
            1,
            &mut warnings,
            &fail,
        )?;
    }

    // 3. Examples, and 4. known sections, from the Markdown tree.
    let tree =
        markdown::to_mdast(&atom.body_text, &markdown::ParseOptions::default()).map_err(|e| {
            fail(
                &atom.file,
                atom.body_line,
                format!("cannot parse the body: {e}"),
            )
        })?;
    let examples = read_examples(atom, &tree, registry, &fail)?;
    let sections = read_sections(&atom.body_text, &tree);

    // 5. Estimate and hash.
    let chars: usize = pieces
        .iter()
        .map(|p| match p {
            Piece::Text(t) => t.chars().count(),
            Piece::Tag(..) => 8,
        })
        .sum();
    let mut h = Sha256::new();
    h.update(b"atomic-compile-1\0");
    h.update(&source);
    for (path, bytes) in &texts {
        h.update(b"\0");
        h.update(path.to_string_lossy().as_bytes());
        h.update(b"\0");
        h.update(bytes);
    }

    Ok(Compiled {
        hash: format!("{:x}", h.finalize()),
        pieces,
        examples,
        sections,
        token_estimate: chars.div_ceil(4),
        partials,
        warnings,
    })
}

type Fail<'a> = dyn Fn(&Path, usize, String) -> AtomError + 'a;

/// Replace each partial tag with the partial's own pieces, recursively.
fn expand(
    pieces: &[Piece],
    file: &Path,
    first_line: usize,
    stack: &mut Vec<PathBuf>,
    used: &mut Vec<PathBuf>,
    texts: &mut Vec<(PathBuf, Vec<u8>)>,
    fail: &Fail<'_>,
) -> Result<Vec<Piece>, AtomError> {
    let mut out = Vec::new();
    for piece in pieces {
        let Piece::Tag(Tag::Partial { file: name }, at) = piece else {
            out.push(piece.clone());
            continue;
        };
        let line = first_line + at.line - 1;
        let path = file.parent().unwrap_or(Path::new("")).join(name);
        let path = std::fs::canonicalize(&path)
            .map_err(|_| fail(file, line, format!("partial {name} does not exist")))?;
        if let Some(i) = stack.iter().position(|p| *p == path) {
            let cycle: Vec<String> = stack[i..]
                .iter()
                .chain(std::iter::once(&path))
                .map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            return Err(fail(
                file,
                line,
                format!("include cycle: {}", cycle.join(" -> ")),
            ));
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| fail(file, line, format!("cannot read partial {name}: {e}")))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let inner = tags::recognize(&text).map_err(|e| {
            fail(
                &path,
                e.at.line,
                format!("column {}: {}", e.at.column, e.message),
            )
        })?;
        if !used.contains(&path) {
            used.push(path.clone());
            texts.push((path.clone(), bytes));
        }
        stack.push(path.clone());
        out.extend(expand(&inner, &path, 1, stack, used, texts, fail)?);
        stack.pop();
    }
    Ok(out)
}

/// The rules for XML-style section tags.
fn check_sections(
    text: &str,
    file: &Path,
    first_line: usize,
    warnings: &mut Vec<String>,
    fail: &Fail<'_>,
) -> Result<(), AtomError> {
    let mut open: Vec<(String, usize)> = Vec::new();
    let mut in_fence: Option<char> = None;
    let lines: Vec<&str> = text.lines().collect();
    for (i, raw) in lines.iter().enumerate() {
        let line = first_line + i;
        let t = raw.trim();
        if let Some(f) = in_fence {
            if t.starts_with(&f.to_string().repeat(3)) {
                in_fence = None;
            }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = t.chars().next();
            continue;
        }
        // The prefix atomic wraps inserted values in is reserved, anywhere.
        if let Some(pos) = raw.find("<data-").or_else(|| raw.find("</data-")) {
            return Err(fail(
                file,
                line,
                format!(
                    "column {}: <data-…> is reserved for the values atomic inserts",
                    pos + 1
                ),
            ));
        }
        let Some(tag) = section_tag(t) else { continue };
        match tag {
            (name, false) => {
                if lines.get(i + 1).is_some_and(|next| !next.trim().is_empty()) {
                    warnings.push(format!(
                        "{}:{line}: leave a blank line after <{name}>, or Markdown reads the block as raw HTML",
                        file.display()
                    ));
                }
                open.push((name, line));
            }
            (name, true) => match open.pop() {
                Some((o, _)) if o == name => {}
                Some((o, at)) => {
                    return Err(fail(
                        file,
                        line,
                        format!("</{name}> closes <{o}> opened at line {at}"),
                    ));
                }
                None => {
                    return Err(fail(
                        file,
                        line,
                        format!("</{name}> has no opening <{name}>"),
                    ));
                }
            },
        }
    }
    match open.pop() {
        Some((name, at)) => Err(fail(
            file,
            at,
            format!("<{name}> is never closed with </{name}>"),
        )),
        None => Ok(()),
    }
}

/// A lowercase section tag that is the whole line: `<name>` or `</name>`.
fn section_tag(line: &str) -> Option<(String, bool)> {
    let inner = line.strip_prefix('<')?.strip_suffix('>')?;
    let (closing, name) = match inner.strip_prefix('/') {
        Some(n) => (true, n),
        None => (false, inner),
    };
    let valid = name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    valid.then(|| (name.to_owned(), closing))
}

fn read_examples(
    atom: &Atom,
    tree: &Node,
    registry: &Registry,
    fail: &Fail<'_>,
) -> Result<Vec<Example>, AtomError> {
    // Where each example's tags are, in body lines.
    let mut spans = Vec::new();
    let mut current: Option<(String, usize)> = None;
    for piece in &atom.body {
        match piece {
            Piece::Tag(Tag::Example { name }, at) => current = Some((name.clone(), at.line)),
            Piece::Tag(Tag::EndExample, at) => {
                if let Some((name, start)) = current.take() {
                    spans.push((name, start, at.line));
                }
            }
            _ => {}
        }
    }
    if spans.is_empty() {
        return Ok(Vec::new());
    }
    let mut blocks = Vec::new();
    collect_code(tree, &mut blocks);

    let input = registry
        .validator(&atom.input)
        .map_err(|e| fail(&atom.file, 1, e.to_string()))?;
    let output = registry
        .validator(&atom.output)
        .map_err(|e| fail(&atom.file, 1, e.to_string()))?;
    let mut out = Vec::new();
    for (name, start, end) in spans {
        let file_line = atom.body_line + start - 1;
        let mut found: BTreeMap<&str, (Value, usize)> = BTreeMap::new();
        for (lang, meta, value, line) in &blocks {
            if *line <= start || *line >= end || lang.as_deref() != Some("json") {
                continue;
            }
            let role = match meta.as_deref().map(str::trim) {
                Some("input") => "input",
                Some("output") => "output",
                _ => continue,
            };
            let at = atom.body_line + line - 1;
            let v: Value = serde_json::from_str(value).map_err(|e| {
                fail(
                    &atom.file,
                    at,
                    format!("example {name:?}: the json {role} block is not JSON: {e}"),
                )
            })?;
            found.insert(role, (v, at));
        }
        let mut pair = Vec::new();
        for (role, validator) in [("input", &input), ("output", &output)] {
            let (value, at) = found.remove(role).ok_or_else(|| {
                fail(
                    &atom.file,
                    file_line,
                    format!("example {name:?} needs a ```json {role} block"),
                )
            })?;
            if let Some(e) = validator.iter_errors(&value).next() {
                return Err(fail(
                    &atom.file,
                    at,
                    format!(
                        "example {name:?}: the {role} breaks the atom's {role} schema at {}: {e}",
                        e.instance_path()
                    ),
                ));
            }
            pair.push(value);
        }
        let output_value = pair.pop().unwrap_or_default();
        let input_value = pair.pop().unwrap_or_default();
        out.push(Example {
            name,
            input: input_value,
            output: output_value,
        });
    }
    Ok(out)
}

type Block = (Option<String>, Option<String>, String, usize);

fn collect_code(node: &Node, out: &mut Vec<Block>) {
    if let Node::Code(c) = node {
        let line = c.position.as_ref().map_or(0, |p| p.start.line);
        out.push((c.lang.clone(), c.meta.clone(), c.value.clone(), line));
    }
    if let Some(children) = node.children() {
        for c in children {
            collect_code(c, out);
        }
    }
}

fn read_sections(text: &str, tree: &Node) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(children) = tree.children() else {
        return out;
    };
    let headings: Vec<(usize, u8, String, usize)> = children
        .iter()
        .filter_map(|n| match n {
            Node::Heading(h) => {
                let p = h.position.as_ref()?;
                Some((p.start.offset, h.depth, n.to_string(), p.end.offset))
            }
            _ => None,
        })
        .collect();
    for (i, (_, depth, title, end)) in headings.iter().enumerate() {
        if *depth != 2 || !KNOWN_SECTIONS.contains(&title.trim()) {
            continue;
        }
        let stop = headings[i + 1..]
            .iter()
            .find(|(_, d, _, _)| *d <= 2)
            .map_or(text.len(), |(start, _, _, _)| *start);
        out.insert(title.trim().to_owned(), text[*end..stop].trim().to_owned());
    }
    out
}

#[cfg(test)]
mod tests;
