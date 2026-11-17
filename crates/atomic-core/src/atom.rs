// SPDX-License-Identifier: Apache-2.0

//! Loading an atom file (`*.atom.md`).
//!
//! An atom file is YAML frontmatter between `---` lines, then a Markdown body
//! with `{% … %}` tags. Loading parses and checks everything that can be
//! checked without running it:
//!
//! - the frontmatter, against the atom field reference: unknown fields, fields
//!   of later versions and wrong types fail with their line;
//! - `input` and `output`: an inline schema, a `$ref` to a schema file
//!   (relative to the atom), or a sidecar beside the atom
//!   (`<name>.input.schema.yaml`); an explicit declaration wins over a sidecar,
//!   and a reference to a missing file fails;
//! - the body's tags: value paths and `if` conditions against the input
//!   schema, and partial files on disk;
//! - `validate` rules, as CEL over `input` and `output`.
//!
//! Every error names the file, the line and the path (a JSON Pointer into the
//! frontmatter, or the tag in the body).

use crate::tags::{self, Piece, Tag};
use atomic_schema::expr::{self, Scope};
use atomic_schema::registry::{self, Registry};
use atomic_schema::yaml;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The kinds of atom this version runs.
pub const KINDS: &[&str] = &["codex", "lite", "exec"];

/// Frontmatter fields this version reads.
const FIELDS: &[&str] = &[
    "atomic",
    "description",
    "name",
    "version",
    "kind",
    "input",
    "output",
    "model",
    "sandbox",
    "network",
    "tools",
    "trust",
    "approvals",
    "budget",
    "retry",
    "validate",
    "evals",
    "exec",
    "effects",
    "env",
];

/// Fields of the published format that later versions read.
const LATER: &[&str] = &[
    "extends",
    "review",
    "credentials",
    "runtime",
    "codex",
    "skills",
    "remote",
    "auth",
];

/// A loaded atom.
#[derive(Clone, Debug)]
pub struct Atom {
    /// The file.
    pub file: PathBuf,
    /// Its name: `name`, else the file name without `.atom.md`.
    pub name: String,
    /// Its version, default 1.
    pub version: u64,
    /// `codex`, `lite` or `exec`.
    pub kind: String,
    /// One sentence on what it does.
    pub description: String,
    /// The registry id of its input schema.
    pub input: String,
    /// The registry id of its output schema.
    pub output: String,
    /// The whole frontmatter, as JSON.
    pub frontmatter: Value,
    /// The body, split into text and tags.
    pub body: Vec<Piece>,
    /// The body as written.
    pub body_text: String,
    /// The file line the body starts on.
    pub body_line: usize,
}

/// Why an atom file failed to load.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{}:{line}: {path}: {message}", file.display())]
pub struct AtomError {
    /// The atom file, or the schema file the problem is in.
    pub file: PathBuf,
    /// The line, from 1.
    pub line: usize,
    /// A JSON Pointer into the frontmatter, or `body` for the body.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

/// Split `text` into frontmatter (with the line its first line is on) and body
/// (with the line it starts on).
pub(crate) fn split(text: &str) -> Option<(&str, &str, usize)> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let mut offset = 0;
    for (i, line) in rest.split_inclusive('\n').enumerate() {
        if line.trim_end() == "---" {
            let fm = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Some((fm, body, i + 3));
        }
        offset += line.len();
    }
    None
}

/// Load the atom at `file`, registering its schemas in `registry`.
pub fn load(file: &Path, registry: &mut Registry) -> Result<Atom, AtomError> {
    let file = std::fs::canonicalize(file).map_err(|e| AtomError {
        file: file.to_owned(),
        line: 1,
        path: String::new(),
        message: format!("cannot read: {e}"),
    })?;
    let text = std::fs::read_to_string(&file).map_err(|e| AtomError {
        file: file.clone(),
        line: 1,
        path: String::new(),
        message: format!("cannot read: {e}"),
    })?;
    let fail = |line: usize, path: &str, message: String| AtomError {
        file: file.clone(),
        line,
        path: path.to_owned(),
        message,
    };
    let (fm_text, body_text, body_line) = split(&text).ok_or_else(|| {
        fail(
            1,
            "",
            "an atom file starts with frontmatter between --- lines".into(),
        )
    })?;

    let doc = yaml::load(&file, fm_text).map_err(|e| fail(e.at.line + 1, "", e.message))?;
    let line_of = |pointer: &str| doc.position(pointer).map_or(2, |p| p.line + 1);
    let Value::Object(fm) = &doc.value else {
        return Err(fail(
            2,
            "",
            "the frontmatter is a mapping, starting with `atomic: 1`".into(),
        ));
    };

    for key in fm.keys() {
        let pointer = format!("/{key}");
        if LATER.contains(&key.as_str()) {
            return Err(fail(
                line_of(&pointer),
                &pointer,
                format!("`{key}` is not supported by this version of atomic yet"),
            ));
        }
        if !FIELDS.contains(&key.as_str()) {
            return Err(fail(
                line_of(&pointer),
                &pointer,
                format!("unknown field `{key}`"),
            ));
        }
    }
    match fm.get("atomic") {
        Some(Value::Number(n)) if n.as_u64() == Some(crate::project::FORMAT_VERSION) => {}
        Some(other) => {
            return Err(fail(
                line_of("/atomic"),
                "/atomic",
                format!("format version {other} is not one this atomic reads"),
            ));
        }
        None => {
            return Err(fail(
                2,
                "",
                "missing the format version: add `atomic: 1`".into(),
            ));
        }
    }
    let description = match fm.get("description") {
        Some(Value::String(d)) if !d.trim().is_empty() => d.clone(),
        Some(_) => {
            return Err(fail(
                line_of("/description"),
                "/description",
                "`description` is one sentence of text".into(),
            ));
        }
        None => {
            return Err(fail(
                2,
                "",
                "missing `description`: say in one sentence what the atom does".into(),
            ));
        }
    };
    let file_name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match fm.get("name") {
        None => file_name
            .strip_suffix(".atom.md")
            .unwrap_or(&file_name)
            .to_owned(),
        Some(Value::String(n)) if !n.is_empty() => n.clone(),
        Some(_) => {
            return Err(fail(
                line_of("/name"),
                "/name",
                "`name` is a non-empty string".into(),
            ));
        }
    };
    let version = match fm.get("version") {
        None => 1,
        Some(v) => v.as_u64().filter(|v| *v >= 1).ok_or_else(|| {
            fail(
                line_of("/version"),
                "/version",
                "`version` is a whole number from 1".into(),
            )
        })?,
    };
    let kind = match fm.get("kind") {
        None => "codex".to_owned(),
        Some(Value::String(k)) if KINDS.contains(&k.as_str()) => k.clone(),
        Some(other) => {
            return Err(fail(
                line_of("/kind"),
                "/kind",
                format!("`kind` is one of {}, not {other}", KINDS.join(", ")),
            ));
        }
    };

    // Run settings: read-only, no network and no optional tools unless the
    // atom says otherwise, and only what this version can enforce.
    match fm.get("sandbox") {
        None => {}
        Some(Value::String(s)) if s == "read-only" || s == "workspace-write" => {}
        Some(other) => {
            return Err(fail(
                line_of("/sandbox"),
                "/sandbox",
                format!("`sandbox` is read-only or workspace-write, not {other}"),
            ));
        }
    }
    let allows_network = match fm.get("network") {
        None | Some(Value::Bool(false)) => false,
        Some(Value::Object(n)) => n
            .get("allow")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty()),
        Some(_) => true,
    };
    if allows_network {
        return Err(fail(
            line_of("/network"),
            "/network",
            "network access for atoms is not supported by this version of atomic yet; atoms run with no network"
                .into(),
        ));
    }
    let has_tools = match fm.get("tools") {
        None | Some(Value::Null) => false,
        Some(Value::Object(o)) => o
            .values()
            .any(|v| v.as_array().is_none_or(|a| !a.is_empty())),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(_) => true,
    };
    if has_tools {
        return Err(fail(
            line_of("/tools"),
            "/tools",
            "atomic, OpenAPI and MCP tools for atoms are not supported by this version of atomic yet".into(),
        ));
    }

    let mut ids = Vec::new();
    for role in ["input", "output"] {
        let pointer = format!("/{role}");
        let id = match fm.get(role) {
            Some(schema) => {
                schema_id(&file, role, schema, registry).map_err(|(sub, message)| {
                    fail(
                        line_of(&format!("{pointer}{sub}")),
                        &format!("{pointer}{sub}"),
                        message,
                    )
                })?
            }
            None => match registry::sidecar(&file, role) {
                Ok(Some(sidecar)) => registry
                    .load_file(&sidecar)
                    .map_err(|e| fail(1, &pointer, e.to_string()))?,
                Ok(None) => {
                    return Err(fail(
                        2,
                        &pointer,
                        format!(
                            "no `{role}` schema: declare one, or add {name}.{role}.schema.yaml beside the atom"
                        ),
                    ));
                }
                Err(e) => return Err(fail(1, &pointer, e.to_string())),
            },
        };
        ids.push(id);
    }
    let (input, output) = (ids.remove(0), ids.remove(0));
    registry.check().map_err(|e| fail(1, "", e.to_string()))?;
    let input_schema = registry
        .get(&input)
        .map(|e| e.schema.clone())
        .unwrap_or(Value::Null);
    let output_schema = registry
        .get(&output)
        .map(|e| e.schema.clone())
        .unwrap_or(Value::Null);

    // Lint: the output must lower to the generation profile, so a schema the
    // model cannot be constrained by fails now, before any tokens are spent.
    if kind != "exec" {
        // A reference to a place in a file is lowered as that place.
        let output_schema = if output_schema.get("$ref").is_some() {
            crate::workflow::inline(&output_schema, Path::new(""), registry, 0)
                .map_err(|m| fail(line_of("/output"), "/output", m))?
        } else {
            output_schema.clone()
        };
        let output_file = registry
            .get(&output)
            .map(|e| e.file.clone())
            .unwrap_or_else(|| file.clone());
        let in_atom = output_file
            .to_string_lossy()
            .starts_with(&*file.to_string_lossy());
        atomic_schema::lower::lower(&output_schema, &atomic_schema::lower::OPENAI_STRICT).map_err(
            |e| AtomError {
                file: if in_atom {
                    file.clone()
                } else {
                    output_file.clone()
                },
                line: if in_atom {
                    line_of(&format!("/output{}", e.path.trim_end_matches('/')))
                } else {
                    1
                },
                path: if in_atom {
                    format!("/output{}", e.path.trim_end_matches('/'))
                } else {
                    e.path.clone()
                },
                message: format!(
                    "the output schema cannot be lowered for generation: {}",
                    e.message
                ),
            },
        )?;
    }

    if let Some(rules) = fm.get("validate") {
        let Value::Array(rules) = rules else {
            return Err(fail(
                line_of("/validate"),
                "/validate",
                "`validate` is a list of CEL expressions".into(),
            ));
        };
        let scope = Scope::new()
            .with("input", &input_schema)
            .with("output", &output_schema);
        for (i, rule) in rules.iter().enumerate() {
            let pointer = format!("/validate/{i}");
            let source = rule.as_str().ok_or_else(|| {
                fail(
                    line_of(&pointer),
                    &pointer,
                    "a rule is a CEL expression in a string".into(),
                )
            })?;
            expr::parse(source)
                .and_then(|e| e.check(&scope))
                .map_err(|e| fail(line_of(&pointer), &pointer, e.to_string()))?;
        }
    }

    let body = tags::recognize(body_text).map_err(|e| {
        fail(
            body_line + e.at.line - 1,
            "body",
            format!("column {}: {}", e.at.column, e.message),
        )
    })?;
    let scope = Scope::new()
        .with("input", &input_schema)
        .with_any("frontmatter");
    for piece in &body {
        let Piece::Tag(tag, at) = piece else { continue };
        let line = body_line + at.line - 1;
        let checked = match tag {
            Tag::Value { root, path } if root == "input" => {
                let source = std::iter::once("input".to_owned())
                    .chain(path.iter().map(|p| {
                        if p.bytes().all(|b| b.is_ascii_digit()) {
                            format!("[{p}]")
                        } else {
                            format!(".{p}")
                        }
                    }))
                    .collect::<String>()
                    .replace(".[", "[");
                expr::parse(&source)
                    .and_then(|e| e.check(&scope))
                    .map_err(|e| e.message)
            }
            Tag::Value { root, .. } if root == "frontmatter" => Ok(()),
            Tag::Value { root, .. } => Err(format!(
                "`${root}` is not a value an atom body can read; use $input or $frontmatter"
            )),
            Tag::If { condition } => expr::parse(condition)
                .and_then(|e| e.check(&scope))
                .map_err(|e| e.to_string()),
            Tag::Partial { file: partial } => {
                let p = file.parent().unwrap_or(Path::new("")).join(partial);
                if p.is_file() {
                    Ok(())
                } else {
                    Err(format!(
                        "partial {partial} does not exist ({})",
                        p.display()
                    ))
                }
            }
            Tag::Else | Tag::EndIf | Tag::Example { .. } | Tag::EndExample => Ok(()),
        };
        checked
            .map_err(|message| fail(line, "body", format!("column {}: {message}", at.column)))?;
    }

    Ok(Atom {
        file,
        name,
        version,
        kind,
        description,
        input,
        output,
        frontmatter: doc.value.clone(),
        body,
        body_text: body_text.to_owned(),
        body_line,
    })
}

/// Register an explicitly declared schema and return its id. Errors carry a
/// pointer below the field and a message.
fn schema_id(
    atom: &Path,
    role: &str,
    schema: &Value,
    registry: &mut Registry,
) -> Result<String, (String, String)> {
    let dir = atom.parent().unwrap_or(Path::new(""));
    // A lone $ref to a file: load that file.
    if let Some(Value::String(r)) = schema.get("$ref")
        && schema.as_object().is_some_and(|o| o.len() == 1)
        && !r.contains(':')
        && !r.starts_with('#')
    {
        let target = dir.join(r.split('#').next().unwrap_or(r));
        if !target.is_file() {
            return Err((
                "/$ref".into(),
                format!(
                    "dangling reference: {r} does not exist ({})",
                    target.display()
                ),
            ));
        }
        let id = registry
            .load_file(&target)
            .map_err(|e| ("/$ref".into(), e.to_string()))?;
        let fragment = r.split_once('#').map_or("", |(_, f)| f);
        if fragment.is_empty() {
            return Ok(id);
        }
        // A place inside the file: check it exists, then register a schema
        // that is exactly that place, by the file's absolute id, so the
        // registry, lowering and workflow mapping all reach the same target.
        let found = registry
            .get(&id)
            .is_some_and(|e| e.schema.pointer(fragment).is_some());
        if !found {
            return Err((
                "/$ref".into(),
                format!(
                    "dangling reference: {r}: {} has no #{fragment}",
                    target.display()
                ),
            ));
        }
        let uri_file = PathBuf::from(format!("{}?{role}", atom.display()));
        let wrapper = serde_json::json!({
            "$id": format!("file://{}", uri_file.display()),
            "$ref": format!("{id}#{fragment}"),
        });
        return registry
            .register(uri_file, wrapper)
            .map_err(|e| ("/$ref".into(), e.to_string()));
    }
    // An inline schema: registered under the atom's own URI, so relative
    // references resolve beside the atom.
    let uri_file = PathBuf::from(format!("{}?{role}", atom.display()));
    let mut schema = schema.clone();
    if schema.get("$id").is_none()
        && let Some(o) = schema.as_object_mut()
    {
        o.insert(
            "$id".into(),
            Value::String(format!("file://{}", uri_file.display())),
        );
    }
    for target in relative_refs(&schema) {
        let path = dir.join(&target);
        if !path.is_file() {
            return Err((
                String::new(),
                format!(
                    "dangling reference: {target} does not exist ({})",
                    path.display()
                ),
            ));
        }
        registry
            .load_file(&path)
            .map_err(|e| (String::new(), e.to_string()))?;
    }
    registry
        .register(uri_file, schema)
        .map_err(|e| (String::new(), e.to_string()))
}

/// File paths named by `$ref`s anywhere in `schema` (not URIs or fragments).
fn relative_refs(schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                if let Some(Value::String(r)) = m.get("$ref")
                    && !r.contains(':')
                    && !r.starts_with('#')
                {
                    out.push(r.split('#').next().unwrap_or(r).to_owned());
                }
                m.values().for_each(|c| walk(c, out));
            }
            Value::Array(a) => a.iter().for_each(|c| walk(c, out)),
            _ => {}
        }
    }
    walk(schema, &mut out);
    out
}

#[cfg(test)]
mod tests;
