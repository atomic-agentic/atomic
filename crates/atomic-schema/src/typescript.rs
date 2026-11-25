// SPDX-License-Identifier: Apache-2.0

//! TypeScript declarations projected from JSON Schema.
//!
//! The declarations are editor and type-checking aids for atoms, components
//! and helpers. They describe the shape a schema accepts as closely as
//! TypeScript can. Keywords TypeScript cannot express (patterns, lengths,
//! formats, uniqueness) become doc comments, and atomic still validates every
//! value against the full schema.
//!
//! [`type_expression`] is the inline projection the authoring compiler uses
//! for one atom's resolved contracts. [`declarations`] writes one module per
//! schema file in a project, with named types for each schema and its
//! `$defs`, and imports between modules for `$ref`s.
//!
//! Every generated module, and every type and field in one, names the schema
//! file it was authored in and the JSON Pointer inside it, as an
//! `@atomic-schema` doc comment. That is how an editor shows where a shared
//! type came from, including a field reached through a local `$ref` from a
//! prompt component, and tells two `$defs` of one name in different files
//! apart. [`generated_origin`] reads a module's origin back and
//! [`stale_declarations`] uses it for the modules a schema rename left behind.

use crate::registry::{Entry, Registry};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

/// Why a schema has no TypeScript projection.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TypeError(pub String);

fn fail<T>(message: impl Into<String>) -> Result<T, TypeError> {
    Err(TypeError(message.into()))
}

const DEPTH: usize = 64;
const ANNOTATIONS: [&str; 7] = [
    "$id",
    "$schema",
    "$comment",
    "title",
    "description",
    "examples",
    "default",
];

/// The inline projection of a schema with no references other than
/// `atomic:artifact`. It matches the authoring compiler's projection except
/// where that one is wrong (see `tests/typescript/projection.json`).
pub fn type_expression(schema: &Value) -> Result<String, TypeError> {
    let mut unresolved = |_: &str| fail("type generation requires resolved schema references");
    Projector {
        resolve: &mut unresolved,
        docs: false,
        origin: None,
        path: Vec::new(),
        resolved: None,
    }
    .ty(schema, 0)
}

type Resolve<'a> = dyn FnMut(&str) -> Result<(String, SchemaOrigin), TypeError> + 'a;

struct Projector<'a, 'b> {
    resolve: &'a mut Resolve<'b>,
    docs: bool,
    /// The named type being projected, and [`Projector::path`] how far below
    /// it the projection has gone: together they let each field name the
    /// origin its schema is written at. `None` for an inline projection, which
    /// names no origin.
    origin: Option<SchemaOrigin>,
    path: Vec<String>,
    /// Where the `$ref` of the schema node last projected points. A field that
    /// is a reference names that, because its schema is authored in the file
    /// the reference reaches, not in the file that points at it.
    resolved: Option<SchemaOrigin>,
}

impl Projector<'_, '_> {
    /// Where the field `key` of the type in hand is written, or `None` when no
    /// origin is carried.
    fn field_origin(&self, key: &str) -> Option<SchemaOrigin> {
        let root = self.origin.as_ref()?;
        let mut tokens = self.path.clone();
        tokens.extend(["properties".to_owned(), key.to_owned()]);
        Some(SchemaOrigin {
            file: root.file.clone(),
            pointer: child_pointer(&root.pointer, &tokens),
        })
    }

    fn ty_below(
        &mut self,
        schema: &Value,
        depth: usize,
        tokens: &[String],
    ) -> Result<String, TypeError> {
        let previous = self.path.len();
        self.path.extend_from_slice(tokens);
        let result = self.ty(schema, depth);
        self.path.truncate(previous);
        result
    }
}

/// A JSON value as a TypeScript literal type.
fn literal(value: &Value) -> String {
    value.to_string()
}

/// A doc comment for the keywords TypeScript cannot express, where a schema
/// was authored, and "".
fn doc(schema: &Value, origin: Option<&SchemaOrigin>) -> String {
    let map = match schema.as_object() {
        Some(map) => map,
        // A boolean schema has nothing to carry a comment; an origin still
        // earns one.
        None => {
            return match origin.and_then(SchemaOrigin::written) {
                Some(origin) => format!("/** {ORIGIN_TAG}{origin} */ "),
                None => String::new(),
            };
        }
    };
    let mut parts = Vec::new();
    if let Some(text) = map.get("description").and_then(Value::as_str) {
        parts.push(text.trim().to_owned());
    }
    for key in [
        "format",
        "pattern",
        "minLength",
        "maxLength",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minItems",
        "maxItems",
        "uniqueItems",
        "minProperties",
        "maxProperties",
    ] {
        if let Some(value) = map.get(key) {
            let shown = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            parts.push(format!("@{key} {shown}"));
        }
    }
    if let Some(origin) = origin.and_then(SchemaOrigin::written) {
        parts.push(format!("{ORIGIN_TAG}{origin}"));
    }
    if parts.is_empty() {
        return String::new();
    }
    // A comment cannot contain its own terminator.
    format!("/** {} */ ", parts.join(" ").replace("*/", "*\\/"))
}

impl Projector<'_, '_> {
    fn ty(&mut self, schema: &Value, depth: usize) -> Result<String, TypeError> {
        if depth > DEPTH {
            return fail("schema type depth limit exceeded");
        }
        self.resolved = None;
        let map = match schema {
            Value::Bool(true) => return Ok("unknown".into()),
            Value::Bool(false) => return Ok("never".into()),
            Value::Object(map) => map,
            _ => return fail("invalid schema"),
        };
        if let Some(value) = map.get("const") {
            return Ok(literal(value));
        }
        if let Some(values) = map.get("enum") {
            let values = values
                .as_array()
                .ok_or_else(|| TypeError("enum must be an array".into()))?;
            if values.is_empty() {
                return Ok("never".into());
            }
            return Ok(values.iter().map(literal).collect::<Vec<_>>().join(" | "));
        }
        let combinators = [("anyOf", " | "), ("oneOf", " | "), ("allOf", " & ")];
        if combinators.iter().any(|(key, _)| map.contains_key(*key)) {
            let mut groups = Vec::new();
            for (key, separator) in combinators {
                let Some(branches) = map.get(key) else {
                    continue;
                };
                let branches = branches
                    .as_array()
                    .ok_or_else(|| TypeError(format!("{key} must be an array")))?;
                let types = branches
                    .iter()
                    .enumerate()
                    .map(|(index, branch)| {
                        self.ty_below(branch, depth + 1, &[key.to_owned(), index.to_string()])
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                groups.push(format!("({})", types.join(separator)));
            }
            let combined = if groups.len() == 1 {
                groups.remove(0)
            } else {
                format!("({})", groups.join(" & "))
            };
            let mut base = map.clone();
            for (key, _) in combinators {
                base.remove(key);
            }
            return Ok(
                if base.contains_key("type") || base.contains_key("properties") {
                    format!(
                        "({} & {combined})",
                        self.ty(&Value::Object(base), depth + 1)?
                    )
                } else {
                    combined
                },
            );
        }
        let reference = map.get("$ref").and_then(Value::as_str);
        if reference == Some("atomic:artifact") {
            return Ok("string".into());
        }
        if let Some(Value::Array(types)) = map.get("type") {
            let mut out = Vec::new();
            for item in types {
                if item == "null" {
                    out.push("null".to_owned());
                } else {
                    let mut single = map.clone();
                    single.insert("type".into(), item.clone());
                    out.push(self.ty(&Value::Object(single), depth + 1)?);
                }
            }
            return Ok(out.join(" | "));
        }
        if let Some(reference) = reference {
            let (name, origin) = (self.resolve)(reference)?;
            self.resolved = Some(origin.clone());
            // Keywords beside the reference are written at the same place as
            // the reference, so what is projected from them keeps the path the
            // field is at.
            let mut rest = map.clone();
            rest.remove("$ref");
            for key in ANNOTATIONS {
                rest.remove(key);
            }
            if rest.is_empty() {
                return Ok(name);
            }
            // Keywords beside a $ref apply as well as it (JSON Schema 2020-12).
            let siblings = self.ty(&Value::Object(rest), depth + 1)?;
            self.resolved = Some(origin);
            return Ok(format!("({name} & {siblings})"));
        }
        let kind = map.get("type").and_then(Value::as_str);
        if kind == Some("object") || map.contains_key("properties") {
            return self.object(map, depth);
        }
        if kind == Some("array") {
            return self.array(map, depth);
        }
        Ok(match kind {
            Some("string") => "string",
            Some("number" | "integer") => "number",
            Some("boolean") => "boolean",
            Some("null") => "null",
            _ => "unknown",
        }
        .into())
    }

    fn object(&mut self, map: &Map<String, Value>, depth: usize) -> Result<String, TypeError> {
        let required: BTreeSet<&str> = map
            .get("required")
            .and_then(Value::as_array)
            .map(|keys| keys.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut members = Vec::new();
        if let Some(properties) = map.get("properties") {
            let properties = properties
                .as_object()
                .ok_or_else(|| TypeError("properties must be an object".into()))?;
            for (key, value) in properties {
                let optional = if required.contains(key.as_str()) {
                    ""
                } else {
                    "?"
                };
                let text =
                    self.ty_below(value, depth + 1, &["properties".to_owned(), key.clone()])?;
                // A field that is a reference names the file the schema it
                // reaches is authored in; a field written here names this file,
                // below the type in hand.
                let direct_reference = value
                    .as_object()
                    .and_then(|schema| schema.get("$ref"))
                    .and_then(Value::as_str)
                    .is_some();
                let origin = (if direct_reference {
                    self.resolved.clone()
                } else {
                    None
                })
                .or_else(|| self.field_origin(key));
                let comment = if self.docs {
                    doc(value, origin.as_ref())
                } else {
                    String::new()
                };
                members.push(format!(
                    "{comment}{}{optional}: {text};",
                    literal(&Value::String(key.clone()))
                ));
            }
        }
        if map.get("additionalProperties") != Some(&Value::Bool(false)) {
            members.push("[key: string]: unknown;".into());
        }
        Ok(format!("{{ {} }}", members.join(" ")))
    }

    fn array(&mut self, map: &Map<String, Value>, depth: usize) -> Result<String, TypeError> {
        let items = map.get("items");
        let Some(prefix) = map.get("prefixItems") else {
            return Ok(format!(
                "Array<{}>",
                self.ty_below(
                    items.unwrap_or(&Value::Bool(true)),
                    depth + 1,
                    &["items".to_owned()],
                )?
            ));
        };
        let prefix = prefix
            .as_array()
            .ok_or_else(|| TypeError("prefixItems must be an array".into()))?;
        let mut members = prefix
            .iter()
            .enumerate()
            .map(|(index, item)| {
                self.ty_below(
                    item,
                    depth + 1,
                    &["prefixItems".to_owned(), index.to_string()],
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        match items {
            Some(Value::Bool(false)) => {}
            Some(rest) => members.push(format!(
                "...Array<{}>",
                self.ty_below(rest, depth + 1, &["items".to_owned()])?
            )),
            None => members.push("...Array<unknown>".into()),
        }
        Ok(format!("[{}]", members.join(", ")))
    }
}

/// Where a schema file's declaration module is written: its path below the
/// project root, under `.atomic/types/`, with `.schema.json` or `.schema.yaml`
/// replaced by `.d.ts`. Code imports `schemas/change.schema.json`'s types from
/// `<root>/.atomic/types/schemas/change`, a path that resolves in every editor
/// project, with or without a tsconfig.
pub fn declaration_path(root: &Path, schema_file: &Path) -> Option<PathBuf> {
    let relative = schema_file.strip_prefix(root).ok()?;
    let name = relative.file_name()?.to_str()?;
    let stem = name
        .strip_suffix(".schema.json")
        .or_else(|| name.strip_suffix(".schema.yaml"))?;
    Some(
        root.join(".atomic/types")
            .join(relative.parent().unwrap_or(Path::new("")))
            .join(format!("{stem}.d.ts")),
    )
}

/// Where a generated type or field was authored: a project-relative JSON
/// Schema file and a JSON Pointer inside it. A generated doc comment names it
/// as `@atomic-schema <file>#<pointer>`, so a stock editor shows, for a shared
/// type or a field reached through a reference, which schema file to open and
/// where in it the schema is written. It is an editor aid and nothing more:
/// the runtime validates every value against the JSON Schema itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaOrigin {
    /// The schema file, `/`-separated, relative to the project root.
    pub file: String,
    /// The JSON Pointer inside it, as a fragment: empty for the whole
    /// document, `/properties/id` for a field, and a `$defs` entry by its
    /// escaped key, as `/properties/a~1b` is a key holding a `/`.
    pub pointer: String,
}

impl SchemaOrigin {
    /// Where it is, as a doc comment names it. A whole document keeps its `#`,
    /// so a schema file and a pointer into it read differently.
    pub fn text(&self) -> String {
        if self.pointer.is_empty() {
            format!("{}#", self.file)
        } else {
            format!("{}#{}", self.file, self.pointer)
        }
    }

    /// The schema file alone as a generated module's header names it, or `None`
    /// when its name holds a control character. A schema file name is whatever
    /// the author called the file, and the header is a line comment: a newline
    /// in a name would start a line of the generated file's own. An origin is
    /// an editor aid, so such a name earns none rather than one escaped into
    /// the declarations. The header names the file, not a place inside it, so
    /// this carries no `#`.
    fn written_file(&self) -> Option<&str> {
        if self.file.chars().any(char::is_control) {
            return None;
        }
        Some(&self.file)
    }

    /// The origin as a generated doc comment names it, or `None` when either
    /// name in it holds a control character, for the same reason.
    fn written(&self) -> Option<String> {
        self.written_file()?;
        if self.pointer.chars().any(char::is_control) {
            return None;
        }
        Some(self.text())
    }
}

/// The `@atomic-schema` tag a generated doc comment carries an origin in.
const ORIGIN_TAG: &str = "@atomic-schema ";

/// How deep below `.atomic/types/` a generated module may be.
const GENERATED_DEPTH: usize = 16;

/// A JSON Pointer token.
fn pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// The pointer `tokens` below `pointer`, which may be empty for a document
/// root.
fn child_pointer(pointer: &str, tokens: &[String]) -> String {
    let mut out = pointer.to_owned();
    for token in tokens {
        out.push('/');
        out.push_str(&pointer_token(token));
    }
    out
}

/// The origin a generated declaration module names, or `None` when it names
/// none. An atom's own declarations, the authoring types and a file atomic did
/// not write name none, and so does a name that is absolute, holds `\` or `:`,
/// holds a control character, steps out of the project or is empty: only a
/// project-relative path is ever an origin, so nothing reading one back can
/// point outside the project. The first line naming an origin decides, which is
/// the header line every generated module starts with and no later comment can
/// speak over.
pub fn generated_origin(text: &str) -> Option<SchemaOrigin> {
    let line = text.lines().find_map(|line| {
        line.find(ORIGIN_TAG)
            .map(|at| line[at + ORIGIN_TAG.len()..].trim())
    })?;
    let (file, pointer) = match line.split_once('#') {
        Some((file, pointer)) => (file, pointer),
        None => (line, ""),
    };
    if file.is_empty()
        || file.contains(':')
        || file.contains('\\')
        || file.chars().any(char::is_control)
        || pointer.chars().any(char::is_control)
        || Path::new(file).is_absolute()
        || file
            .split('/')
            .any(|part| part.is_empty() || part == ".." || part == ".")
        || !(pointer.is_empty() || pointer.starts_with('/'))
    {
        return None;
    }
    Some(SchemaOrigin {
        file: file.to_owned(),
        pointer: pointer.to_owned(),
    })
}

/// The generated declaration modules under `<root>/.atomic/types/` naming a
/// schema file the project no longer has: the stale symbols a schema rename or
/// removal leaves behind. Left in place they keep an old type resolving in an
/// editor, so what a person imports from a renamed schema goes on checking
/// against the type that schema no longer describes.
///
/// A module naming no origin, one whose text cannot be read, and one whose
/// origin is present or unreadable rather than truly not there are all kept:
/// only a schema file that is missing, or whose directory path is missing,
/// makes a module stale. Symlinked modules and directories are passed over,
/// never followed or removed, and so is a module below the sixteenth directory
/// level, deeper than a project's generated tree is laid out — each of these
/// keeps a module rather than removing one it cannot see. A generated
/// directory that cannot be read is an error, so a tree this cannot see is
/// never reported as holding nothing stale, and a `.atomic/types` that is a
/// link rather than a directory is an error too, because everything below a
/// link belongs to something outside the project.
pub fn stale_declarations(root: &Path) -> Result<Vec<PathBuf>, TypeError> {
    let mut stale = Vec::new();
    let types = root.join(".atomic/types");
    // The generated tree is atomic's own, so a link at its top is not part of
    // it: everything below a link lives outside the project, and a caller that
    // removed what this returns would delete files the project does not own.
    // A tree that is not a plain directory is refused whole.
    match std::fs::symlink_metadata(&types) {
        Ok(kind) if kind.is_dir() => {}
        // Nothing has been generated, so nothing is stale.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(stale),
        Ok(_) => return Err(TypeError(format!("{}: not a directory", types.display()))),
        Err(error) => return Err(TypeError(format!("{}: {error}", types.display()))),
    }
    let mut stack = vec![(types, 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // No generated directory at all: nothing has been generated, so
            // nothing is stale.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(TypeError(format!("{}: {error}", dir.display()))),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => return Err(TypeError(format!("{}: {error}", dir.display()))),
            };
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    return Err(TypeError(format!("{}: {error}", entry.path().display())));
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if kind.is_dir() {
                if depth + 1 < GENERATED_DEPTH {
                    stack.push((entry.path(), depth + 1));
                }
                continue;
            }
            if !kind.is_file() || !name.ends_with(".d.ts") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Some(origin) = generated_origin(&text) else {
                continue;
            };
            match std::fs::symlink_metadata(root.join(&origin.file)) {
                // A schema file that is not there: the rename took it away.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    stale.push(entry.path())
                }
                // Present, unreadable, a link or anything else that is not a
                // true absence keeps the module.
                _ => {}
            }
        }
    }
    stale.sort();
    Ok(stale)
}

struct Names {
    root: String,
    defs: BTreeMap<String, String>,
}

struct Module<'a> {
    entry: &'a Entry,
    relative: PathBuf,
    output: PathBuf,
    // Naming fails only for a module that is generated or referenced.
    names: Result<Names, TypeError>,
}

/// A path below the project root, `/`-separated, as a schema origin names the
/// file it was authored in. This is the file a person opens, so an origin is
/// never a `$id` or a URL.
fn relative_text(relative: &Path) -> String {
    relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// A schema file as a TypeScript name for its module path without its
/// `.schema.json` or `.schema.yaml`: `schemas/lead.schema.json` is
/// `SchemasLead`. It qualifies an imported type whose plain name is already
/// taken in the module, so the name alone says which schema file the type came
/// from.
fn module_qualifier(file: &str) -> String {
    let stem = file
        .strip_suffix(".schema.json")
        .or_else(|| file.strip_suffix(".schema.yaml"))
        .unwrap_or(file);
    definition_name(stem)
}

/// A path or key as a TypeScript type name, or `text` itself when it could not
/// name one. Only names a person reads come from this, never a name a check
/// resolves.
pub(crate) fn definition_name(text: &str) -> String {
    type_name(text).unwrap_or_else(|_| text.to_owned())
}

/// A schema file name or `$defs` key as a TypeScript type name.
fn type_name(text: &str) -> Result<String, TypeError> {
    let name: String = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect();
    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return fail(format!("{text:?} cannot name a TypeScript type"));
    }
    Ok(name)
}

fn names(entry: &Entry, relative: &Path, stem: &str) -> Result<Names, TypeError> {
    let title = entry.schema.get("title").and_then(Value::as_str);
    let root = match title.map(type_name) {
        Some(Ok(name)) => name,
        _ => type_name(stem)?,
    };
    let mut defs = BTreeMap::new();
    let mut taken = BTreeSet::from([root.clone()]);
    if let Some(definitions) = entry.schema.get("$defs").and_then(Value::as_object) {
        for key in definitions.keys() {
            let name = type_name(key)?;
            if !taken.insert(name.clone()) {
                return fail(format!(
                    "{}: two types are named {name}",
                    relative.display()
                ));
            }
            defs.insert(key.clone(), name);
        }
    }
    Ok(Names { root, defs })
}

/// The relative module specifier from directory `from` to file `to`.
fn specifier(from: &Path, to: &Path) -> String {
    let from: Vec<Component> = from.components().collect();
    let to: Vec<Component> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = from[common..].iter().map(|_| "..".to_owned()).collect();
    parts.extend(
        to[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    let joined = parts.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

/// Declaration modules for the schema files `files` (canonical paths, loaded
/// in `registry`) and every schema file they reference, keyed by
/// [`declaration_path`]. The schema's own type is named by its `title`, or
/// else its file name (`review-packet.schema.json` is `ReviewPacket`); each
/// `$defs` entry is a type of its own. A `$ref` to a whole schema or one of
/// its `$defs` becomes that type, imported from its module; where that name is
/// already taken in the importing module, it is imported under a
/// `<Name><ModuleQualifier>` alias, so two schema files defining one `$defs`
/// name stay usable side by side.
///
/// Every module starts with a line naming the schema file it was made from
/// ([`generated_origin`]), and with `docs`, every type and every field carries
/// its [`SchemaOrigin`] as a doc comment beside any description and the
/// constraints TypeScript cannot express. A field that is a `$ref` names the
/// file the schema it reaches is authored in, so following a shared field out
/// of a prompt component lands on the schema file and pointer to change.
/// Schemas that are neither requested nor referenced are not read.
pub fn declarations(
    registry: &Registry,
    root: &Path,
    files: &[PathBuf],
    docs: bool,
) -> Result<BTreeMap<PathBuf, String>, TypeError> {
    let mut modules = Vec::new();
    for entry in registry.entries() {
        let Some(output) = declaration_path(root, &entry.file) else {
            continue;
        };
        let Ok(relative) = entry.file.strip_prefix(root) else {
            continue;
        };
        let name = relative.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let stem = name
            .strip_suffix(".schema.json")
            .or_else(|| name.strip_suffix(".schema.yaml"))
            .unwrap_or(name);
        modules.push(Module {
            entry,
            relative: relative.to_owned(),
            output,
            names: names(entry, relative, stem),
        });
    }
    let by_id: BTreeMap<&str, usize> = modules
        .iter()
        .enumerate()
        .map(|(index, module)| (module.entry.id.as_str(), index))
        .collect();
    let mut queue = Vec::new();
    for file in files {
        let index = modules
            .iter()
            .position(|module| &module.entry.file == file)
            .ok_or_else(|| {
                TypeError(format!(
                    "{} is not a loaded schema file in the project",
                    file.display()
                ))
            })?;
        queue.push(index);
    }
    let mut done = BTreeSet::new();
    let mut out = BTreeMap::new();
    while let Some(index) = queue.pop() {
        if !done.insert(index) {
            continue;
        }
        let module = &modules[index];
        let own = module.names.as_ref().map_err(Clone::clone)?;
        let base = url::Url::parse(&module.entry.id)
            .map_err(|e| TypeError(format!("{}: {e}", module.entry.id)))?;
        let file = relative_text(&module.relative);
        let root_origin = SchemaOrigin {
            file: file.clone(),
            pointer: String::new(),
        };
        let mut imports: BTreeMap<usize, BTreeMap<String, String>> = BTreeMap::new();
        // Every type name this module shows, and which schema file's type it
        // came from: its own types first, then each imported one under its
        // plain name or its alias.
        let mut taken: BTreeMap<String, usize> = own
            .defs
            .values()
            .map(|name| (name.clone(), index))
            .collect();
        taken.insert(own.root.clone(), index);
        let body = {
            let mut resolve = |reference: &str| -> Result<(String, SchemaOrigin), TypeError> {
                let target = base
                    .join(reference)
                    .map_err(|e| TypeError(format!("{reference}: {e}")))?;
                let fragment = target.fragment().unwrap_or("").to_owned();
                let mut document = target.clone();
                document.set_fragment(None);
                let owner = registry
                    .get(document.as_str())
                    .and_then(|entry| by_id.get(entry.id.as_str()).copied())
                    .ok_or_else(|| {
                        TypeError(format!(
                            "{}: {reference} names no schema file in the project",
                            module.relative.display()
                        ))
                    })?;
                let target_names = modules[owner].names.as_ref().map_err(Clone::clone)?;
                let key = fragment
                    .strip_prefix("/$defs/")
                    .map(|key| key.replace("~1", "/").replace("~0", "~"));
                let name =
                    match &key {
                        Some(key) => target_names.defs.get(key).cloned().ok_or_else(|| {
                            TypeError(format!("{reference}: no $defs entry {key}"))
                        })?,
                        None if fragment.is_empty() => target_names.root.clone(),
                        None => {
                            return fail(format!(
                                "{reference}: only a whole schema or one of its $defs can be typed"
                            ));
                        }
                    };
                // The pointer is the one written in the schema file, which is
                // not necessarily the spelling the `$ref` used: `$id` may
                // resolve it from anywhere, and `#` points at the root.
                let pointer = match &key {
                    Some(key) => format!("/$defs/{}", pointer_token(key)),
                    None => String::new(),
                };
                let origin = SchemaOrigin {
                    file: relative_text(&modules[owner].relative),
                    pointer,
                };
                if owner == index {
                    return Ok((name, origin));
                }
                // The name this module shows the type under, decided once per
                // referenced type so a module referencing it twice imports it
                // once. A name one of this module's own types, or a type from
                // another file, already holds is imported under a qualified
                // alias instead.
                let shown = match imports.get(&owner).and_then(|from| from.get(&name)) {
                    Some(shown) => shown.clone(),
                    None => {
                        let free = taken.get(&name).is_none_or(|from| *from == owner);
                        let shown = if free {
                            name.clone()
                        } else {
                            format!("{name}{}", module_qualifier(&origin.file))
                        };
                        if let Some(from) = taken.get(&shown) {
                            return fail(format!(
                                "{file}: {name} from {} and {} would both be shown as {shown} here; rename one schema file",
                                origin.file,
                                relative_text(&modules[*from].relative),
                            ));
                        }
                        taken.insert(shown.clone(), owner);
                        imports
                            .entry(owner)
                            .or_default()
                            .insert(name.clone(), shown.clone());
                        shown
                    }
                };
                Ok((shown, origin))
            };
            let mut projector = Projector {
                resolve: &mut resolve,
                docs,
                origin: Some(root_origin.clone()),
                path: Vec::new(),
                resolved: None,
            };
            let lead = |schema: &Value, origin: &SchemaOrigin| {
                // Nothing to say above a type writes no line at all, so a type
                // with no comment is not set off by an empty one.
                match (docs, doc(schema, Some(origin)).trim_end()) {
                    (true, comment) if !comment.is_empty() => format!("{comment}\n"),
                    _ => String::new(),
                }
            };
            projector.path.clear();
            let mut body = format!(
                "{}export type {} = {};\n",
                lead(&module.entry.schema, &root_origin),
                own.root,
                projector.ty(&strip_defs(&module.entry.schema), 0)?
            );
            if let Some(definitions) = module.entry.schema.get("$defs").and_then(Value::as_object) {
                for key in definitions.keys().cloned().collect::<Vec<_>>() {
                    let schema = definitions.get(&key).expect("just listed");
                    let origin = SchemaOrigin {
                        file: file.clone(),
                        pointer: format!("/$defs/{}", pointer_token(&key)),
                    };
                    projector.origin = Some(origin.clone());
                    projector.path.clear();
                    body.push_str(&format!(
                        "{}export type {} = {};\n",
                        lead(schema, &origin),
                        own.defs.get(&key).expect("named above"),
                        projector.ty(schema, 0)?
                    ));
                }
            }
            projector.origin = None;
            drop(projector);
            // A reference is only projected for a type that is projected, so
            // every module an import was recorded for is generated too.
            for owner in imports.keys() {
                queue.push(*owner);
            }
            body
        };
        // Every generated module names, in a comment on its own second line
        // and on every type it declares, the schema file it was made from.
        // `generated_origin` reads that line back, which is how a check finds
        // the modules a schema rename left behind. It is a comment because the
        // file is TypeScript, and the editor's checker reads it as one. A file
        // whose name cannot be written as an origin gets no origin line, so
        // half a name is never read back as one.
        let origin_line = match root_origin.written_file() {
            Some(file) => format!("// {ORIGIN_TAG}{file}\n"),
            None => String::new(),
        };
        let mut text = format!("// Generated by atomic. Do not edit.\n{origin_line}");
        let from = module.output.parent().unwrap_or(root);
        let mut lines = Vec::new();
        for (owner, imported) in &imports {
            // `imported` maps the name the other module exports to the name
            // this module shows it under, which differs when the plain name
            // was already taken here.
            let mut names: Vec<String> = imported
                .iter()
                .map(|(imported, shown)| {
                    if imported == shown {
                        imported.clone()
                    } else {
                        format!("{imported} as {shown}")
                    }
                })
                .collect();
            names.sort();
            let target = &modules[*owner].output;
            let target = target.with_file_name(
                target
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".d.ts"))
                    .unwrap_or(""),
            );
            lines.push(format!(
                "import type {{ {} }} from '{}';",
                names.join(", "),
                specifier(from, &target)
            ));
        }
        // Imports in path order, so a regenerated file differs from the last
        // one only where its schema changed.
        lines.sort();
        for line in lines {
            text.push_str(&line);
            text.push('\n');
        }
        text.push_str(&body);
        out.insert(module.output.clone(), text);
    }
    Ok(out)
}

/// One atom's declarations: a type for each of its resolved contracts
/// (`Input`, `Output`, `Context`, `Case`, `Judges`), the `Props`, `Reply`,
/// `EvalCase` and `Immutable` names atom sources use, and `Config` — that
/// atom's own configuration type, so an author annotates
/// `export const config: Config` and imports nothing but this module. The
/// older `AtomConfig<…>` spelling, imported from `authoring`, keeps working
/// beside it: both name the same contract types.
///
/// `Context` is projected only when the atom declares a context contract. An
/// atom that declares none has no `Context` type at all, so `Props` and
/// `Config` leave `context` out and every read of it — in a template or in a
/// validator or scorer — is a type error rather than a callback that quietly
/// sees an absent value.
pub fn atom_declarations(schemas: &Map<String, Value>) -> Result<String, TypeError> {
    let mut types = Vec::new();
    for (key, schema) in schemas {
        let name = if key == "Context" { "ContextData" } else { key };
        types.push(format!(
            "export type {name} = {};",
            type_expression(schema)?
        ));
    }
    let declared = schemas.contains_key("Context");
    Ok(format!(
        "// Generated from the full runtime contracts. Do not edit.\n\
         import type {{ AtomConfig }} from './authoring';\n{}\n\
         {}\
         export type Reply = Output;\n\
         export type EvalCase = Case;\n\
         export type Config = AtomConfig<Input, Reply, EvalCase, Judges{}>;\n\
         export type Immutable<T> = T extends object ? {{ readonly [K in keyof T]: Immutable<T[K]> }} : T;\n\
         export interface Props {{ input: Input;{} }}\n",
        types.join("\n"),
        if declared {
            "export type Context = Immutable<ContextData>;\n"
        } else {
            ""
        },
        if declared { ", Context" } else { "" },
        if declared {
            " readonly context: Immutable<Context>;"
        } else {
            ""
        },
    ))
}

/// The schema without `$defs`, which become types of their own.
fn strip_defs(schema: &Value) -> Value {
    let mut schema = schema.clone();
    if let Some(map) = schema.as_object_mut() {
        map.remove("$defs");
    }
    schema
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn expand(schema: &Value) -> Value {
        match schema.as_str().and_then(|s| s.strip_prefix("DEEP-")) {
            Some(n) => {
                let n: usize = n.parse().expect("depth");
                (0..n).fold(
                    json!({ "type": "string" }),
                    |inner, _| json!({ "type": "array", "items": inner }),
                )
            }
            None => schema.clone(),
        }
    }

    #[test]
    fn the_authoring_projection_cases_hold() {
        let cases: Value =
            serde_json::from_str(include_str!("../tests/typescript/projection.json"))
                .expect("cases");
        let cases = cases["cases"].as_array().expect("case list");
        assert!(cases.len() >= 40);
        for case in cases {
            let name = case["name"].as_str().expect("name");
            let result = type_expression(&expand(&case["schema"]));
            if let Some(error) = case["error"].as_str() {
                let message = result.expect_err(name).0;
                assert!(message.contains(error), "{name}: {message}");
            } else {
                let expected = case["rust"]["type"]
                    .as_str()
                    .or(case["type"].as_str())
                    .expect("expected type");
                assert_eq!(result.expect(name), expected, "{name}");
            }
        }
    }

    #[test]
    fn tuples_come_from_prefix_items() {
        let closed = json!({ "type": "array", "prefixItems": [{ "type": "string" }, { "type": "integer" }], "items": false });
        assert_eq!(type_expression(&closed).expect("tuple"), "[string, number]");
        let open = json!({ "type": "array", "prefixItems": [{ "type": "string" }], "items": { "type": "boolean" } });
        assert_eq!(
            type_expression(&open).expect("tuple"),
            "[string, ...Array<boolean>]"
        );
    }

    fn project(files: &[(&str, Value)]) -> (tempdir::Dir, Registry) {
        let dir = tempdir::Dir::new();
        let mut registry = Registry::new();
        for (name, schema) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(&path, schema.to_string()).expect("write");
        }
        for (name, _) in files {
            registry.load_file(&dir.path().join(name)).expect("load");
        }
        registry.check().expect("references resolve");
        (dir, registry)
    }

    fn change_project() -> (tempdir::Dir, Registry) {
        project(&[
            (
                "schemas/change.schema.json",
                json!({
                    "type": "object", "additionalProperties": false, "required": ["id"],
                    "properties": {
                        "id": { "type": "string", "pattern": "^CHG-[0-9]+$", "description": "The change ID." },
                        "tier": { "$ref": "#/$defs/tier" }
                    },
                    "$defs": { "tier": { "enum": ["tier-1", "tier-2"] } }
                }),
            ),
            (
                "schemas/tree.schema.json",
                json!({
                    "title": "Outline", "$ref": "#/$defs/node",
                    "$defs": { "node": { "type": "object", "additionalProperties": false, "required": ["label"],
                        "properties": { "label": { "type": "string" }, "children": { "type": "array", "items": { "$ref": "#/$defs/node" } } } } }
                }),
            ),
            (
                "atoms/assess.input.schema.json",
                json!({
                    "type": "object", "additionalProperties": false, "required": ["change"],
                    "properties": {
                        "change": { "$ref": "../schemas/change.schema.json" },
                        "tier": { "$ref": "../schemas/change.schema.json#/$defs/tier" },
                        "outline": { "$ref": "../schemas/tree.schema.json" }
                    }
                }),
            ),
        ])
    }

    fn canonical(dir: &tempdir::Dir) -> PathBuf {
        std::fs::canonicalize(dir.path()).expect("root")
    }

    fn all(registry: &Registry, root: &Path) -> Vec<PathBuf> {
        registry
            .entries()
            .filter(|entry| declaration_path(root, &entry.file).is_some())
            .map(|entry| entry.file.clone())
            .collect()
    }

    #[test]
    fn modules_name_types_and_import_references() {
        let (dir, registry) = change_project();
        let root = canonical(&dir);
        let out =
            declarations(&registry, &root, &all(&registry, &root), false).expect("declarations");
        let types = root.join(".atomic/types");
        assert_eq!(
            out[&types.join("schemas/change.d.ts")],
            "// Generated by atomic. Do not edit.\n\
             // @atomic-schema schemas/change.schema.json\n\
             export type Change = { \"id\": string; \"tier\"?: Tier; };\n\
             export type Tier = \"tier-1\" | \"tier-2\";\n"
        );
        assert_eq!(
            out[&types.join("schemas/tree.d.ts")],
            "// Generated by atomic. Do not edit.\n\
             // @atomic-schema schemas/tree.schema.json\n\
             export type Outline = Node;\n\
             export type Node = { \"label\": string; \"children\"?: Array<Node>; };\n"
        );
        assert_eq!(
            out[&types.join("atoms/assess.input.d.ts")],
            "// Generated by atomic. Do not edit.\n\
             // @atomic-schema atoms/assess.input.schema.json\n\
             import type { Change, Tier } from '../schemas/change';\n\
             import type { Outline } from '../schemas/tree';\n\
             export type AssessInput = { \"change\": Change; \"tier\"?: Tier; \"outline\"?: Outline; };\n"
        );
    }

    #[test]
    fn only_requested_schemas_and_their_references_are_generated() {
        let (dir, registry) = change_project();
        let root = canonical(&dir);
        let assess = root.join("atoms/assess.input.schema.json");
        let out = declarations(&registry, &root, &[assess], false).expect("declarations");
        assert_eq!(out.len(), 3);
        let tree = root.join("schemas/tree.schema.json");
        let out = declarations(&registry, &root, &[tree], false).expect("declarations");
        assert_eq!(
            out.keys().collect::<Vec<_>>(),
            [&root.join(".atomic/types/schemas/tree.d.ts")]
        );
    }

    #[test]
    fn an_unrequested_schema_cannot_break_generation() {
        let (dir, registry) = project(&[
            ("good.schema.json", json!({ "type": "string" })),
            (
                "pointer.schema.json",
                json!({ "type": "object", "properties": { "a": { "type": "string" }, "b": { "$ref": "#/properties/a" } } }),
            ),
        ]);
        let root = canonical(&dir);
        let out = declarations(&registry, &root, &[root.join("good.schema.json")], false)
            .expect("only the requested schema is projected");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn docs_carry_descriptions_and_constraints() {
        let (dir, registry) = change_project();
        let root = canonical(&dir);
        let out =
            declarations(&registry, &root, &all(&registry, &root), true).expect("declarations");
        let change = &out[&root.join(".atomic/types/schemas/change.d.ts")];
        assert!(
            change.contains("/** The change ID. @pattern ^CHG-[0-9]+$ @atomic-schema schemas/change.schema.json#/properties/id */ \"id\": string;"),
            "{change}"
        );
    }

    /// The whole generated module for `schemas/change.schema.json`, with the
    /// origin of its root type, its `$defs` type and each of its fields.
    fn change_module(docs: bool) -> String {
        let (dir, registry) = change_project();
        let root = canonical(&dir);
        declarations(&registry, &root, &all(&registry, &root), docs).expect("declarations")
            [&root.join(".atomic/types/schemas/change.d.ts")]
            .clone()
    }

    #[test]
    fn every_generated_type_and_field_names_its_schema_file_and_pointer() {
        assert_eq!(
            change_module(true),
            "// Generated by atomic. Do not edit.\n\
             // @atomic-schema schemas/change.schema.json\n\
             /** @atomic-schema schemas/change.schema.json# */\n\
             export type Change = { \
             /** The change ID. @pattern ^CHG-[0-9]+$ @atomic-schema schemas/change.schema.json#/properties/id */ \"id\": string; \
             /** @atomic-schema schemas/change.schema.json#/$defs/tier */ \"tier\"?: Tier; \
             };\n\
             /** @atomic-schema schemas/change.schema.json#/$defs/tier */\n\
             export type Tier = \"tier-1\" | \"tier-2\";\n"
        );
    }

    #[test]
    fn a_field_reached_through_a_reference_names_where_its_schema_is_authored() {
        // A prompt component or atom imports this module; each of its three
        // fields points at another schema file, and each says so.
        let (dir, registry) = change_project();
        let root = canonical(&dir);
        let out =
            declarations(&registry, &root, &all(&registry, &root), true).expect("declarations");
        assert_eq!(
            out[&root.join(".atomic/types/atoms/assess.input.d.ts")],
            "// Generated by atomic. Do not edit.\n\
             // @atomic-schema atoms/assess.input.schema.json\n\
             import type { Change, Tier } from '../schemas/change';\n\
             import type { Outline } from '../schemas/tree';\n\
             /** @atomic-schema atoms/assess.input.schema.json# */\n\
             export type AssessInput = { \
             /** @atomic-schema schemas/change.schema.json# */ \"change\": Change; \
             /** @atomic-schema schemas/change.schema.json#/$defs/tier */ \"tier\"?: Tier; \
             /** @atomic-schema schemas/tree.schema.json# */ \"outline\"?: Outline; \
             };\n"
        );
    }

    #[test]
    fn fields_below_a_named_definition_point_below_it() {
        let (dir, registry) = change_project();
        let root = canonical(&dir);
        let out =
            declarations(&registry, &root, &all(&registry, &root), true).expect("declarations");
        let tree = &out[&root.join(".atomic/types/schemas/tree.d.ts")];
        assert!(
            tree.contains("export type Node = { /** @atomic-schema schemas/tree.schema.json#/$defs/node/properties/label */ \"label\": string;"),
            "{tree}"
        );
        // An anonymous object inside a field points below that field.
        let (dir, registry) = project(&[(
            "packet.schema.json",
            json!({"type": "object", "additionalProperties": false, "properties": {
                "head": {"type": "object", "properties": {"title": {"type": "string"}}}
            }}),
        )]);
        let root = canonical(&dir);
        let out =
            declarations(&registry, &root, &all(&registry, &root), true).expect("declarations");
        let packet = &out[&root.join(".atomic/types/packet.d.ts")];
        assert!(
            packet.contains("@atomic-schema packet.schema.json#/properties/head/properties/title"),
            "{packet}"
        );
    }

    #[test]
    fn referenced_field_with_siblings_keeps_the_referenced_origin() {
        let (dir, registry) = project(&[
            (
                "schemas/root.schema.json",
                json!({"type": "object", "properties": {
                    "tier": {"$ref": "tier.schema.json", "minLength": 2},
                    "nested": {"type": "object", "properties": {
                        "tierRef": {"$ref": "tier.schema.json"}
                    }}
                }}),
            ),
            ("schemas/tier.schema.json", json!({"type": "string"})),
        ]);
        let root = canonical(&dir);
        let out =
            declarations(&registry, &root, &all(&registry, &root), true).expect("declarations");
        let generated = &out[&root.join(".atomic/types/schemas/root.d.ts")];
        assert!(
            generated.contains("@atomic-schema schemas/tier.schema.json# */ \"tier\""),
            "{generated}"
        );
        assert!(
            generated.contains(
                "@atomic-schema schemas/root.schema.json#/properties/nested */ \"nested\""
            ),
            "{generated}"
        );
        assert!(
            generated.contains("@atomic-schema schemas/tier.schema.json# */ \"tierRef\""),
            "{generated}"
        );
    }

    #[test]
    fn nested_fields_name_array_and_combinator_pointer_segments() {
        let (dir, registry) = project(&[(
            "schemas/root.schema.json",
            json!({"type": "object", "properties": {
                "rows": {"type": "array", "items": {"type": "object", "properties": {"id": {"type": "string"}}}},
                "tuple": {"type": "array", "prefixItems": [{"type": "object", "properties": {"id": {"type": "string"}}}], "items": false},
                "any": {"anyOf": [{"type": "object", "properties": {"id": {"type": "string"}}}]},
                "one": {"oneOf": [{"type": "object", "properties": {"id": {"type": "string"}}}]},
                "all": {"allOf": [{"type": "object", "properties": {"id": {"type": "string"}}}]}
            }}),
        )]);
        let root = canonical(&dir);
        let out =
            declarations(&registry, &root, &all(&registry, &root), true).expect("declarations");
        let generated = &out[&root.join(".atomic/types/schemas/root.d.ts")];
        for pointer in [
            "#/properties/rows/items/properties/id",
            "#/properties/tuple/prefixItems/0/properties/id",
            "#/properties/any/anyOf/0/properties/id",
            "#/properties/one/oneOf/0/properties/id",
            "#/properties/all/allOf/0/properties/id",
        ] {
            assert!(
                generated.contains(&format!("@atomic-schema schemas/root.schema.json{pointer}")),
                "missing {pointer}: {generated}"
            );
        }
    }

    #[test]
    fn a_definition_key_needing_pointer_escapes_names_its_pointer_exactly() {
        let (dir, registry) = project(&[(
            "odd.schema.json",
            json!({"type": "object", "$defs": {"a/b~c": {"type": "string"}}}),
        )]);
        let root = canonical(&dir);
        let out =
            declarations(&registry, &root, &all(&registry, &root), true).expect("declarations");
        let odd = &out[&root.join(".atomic/types/odd.d.ts")];
        assert!(
            odd.contains("/** @atomic-schema odd.schema.json#/$defs/a~1b~0c */\nexport type ABC"),
            "{odd}"
        );
    }

    #[test]
    fn equal_definition_names_in_two_files_keep_their_own_origins() {
        let (dir, registry) = project(&[
            (
                "schemas/lead.schema.json",
                json!({"$defs": {"status": {"type": "string", "description": "Who is on it."}}}),
            ),
            (
                "schemas/build.schema.json",
                json!({"$defs": {"status": {"enum": ["ok", "failed"]}}}),
            ),
        ]);
        let root = canonical(&dir);
        let out =
            declarations(&registry, &root, &all(&registry, &root), true).expect("declarations");
        let types = root.join(".atomic/types");
        // The module and the type name are the same; only the origin tells
        // them apart.
        assert_eq!(
            out[&types.join("schemas/lead.d.ts")],
            "// Generated by atomic. Do not edit.\n\
             // @atomic-schema schemas/lead.schema.json\n\
             /** @atomic-schema schemas/lead.schema.json# */\n\
             export type Lead = unknown;\n\
             /** Who is on it. @atomic-schema schemas/lead.schema.json#/$defs/status */\n\
             export type Status = string;\n"
        );
        assert_eq!(
            out[&types.join("schemas/build.d.ts")],
            "// Generated by atomic. Do not edit.\n\
             // @atomic-schema schemas/build.schema.json\n\
             /** @atomic-schema schemas/build.schema.json# */\n\
             export type Build = unknown;\n\
             /** @atomic-schema schemas/build.schema.json#/$defs/status */\n\
             export type Status = \"ok\" | \"failed\";\n"
        );
    }

    #[test]
    fn one_module_reaching_two_definitions_of_one_name_imports_both() {
        // `work` holds a `status` of its own and reaches two more, all named
        // `status`. One import of each under one name would not compile, so a
        // name already taken here is imported under a qualified alias that
        // says which schema file it came from.
        let (dir, registry) = project(&[
            (
                "schemas/lead.schema.json",
                json!({"$defs": {"status": {"type": "string", "description": "Who is on it."}}}),
            ),
            (
                "schemas/build.schema.json",
                json!({"$defs": {"status": {"enum": ["ok", "failed"]}}}),
            ),
            (
                "work.schema.json",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["lead", "build"],
                    "properties": {
                        "lead": {"$ref": "schemas/lead.schema.json#/$defs/status"},
                        "build": {"$ref": "schemas/build.schema.json#/$defs/status"},
                        "status": {"type": "string", "description": "This module's own."}
                    },
                    "$defs": {"status": {"type": "number"}}
                }),
            ),
        ]);
        let root = canonical(&dir);
        let out =
            declarations(&registry, &root, &all(&registry, &root), true).expect("declarations");
        assert_eq!(
            out[&root.join(".atomic/types/work.d.ts")],
            "// Generated by atomic. Do not edit.\n\
             // @atomic-schema work.schema.json\n\
             import type { Status as StatusSchemasBuild } from './schemas/build';\n\
             import type { Status as StatusSchemasLead } from './schemas/lead';\n\
             /** @atomic-schema work.schema.json# */\n\
             export type Work = { \
             /** @atomic-schema schemas/lead.schema.json#/$defs/status */ \"lead\": StatusSchemasLead; \
             /** @atomic-schema schemas/build.schema.json#/$defs/status */ \"build\": StatusSchemasBuild; \
             /** This module's own. @atomic-schema work.schema.json#/properties/status */ \"status\"?: string; \
             };\n\
             /** @atomic-schema work.schema.json#/$defs/status */\n\
             export type Status = number;\n"
        );
    }

    #[test]
    fn a_schema_named_by_its_id_still_names_its_file_and_pointer() {
        // `$id` decides how a `$ref` resolves; the origin is the file a person
        // opens, because that is where the schema is written.
        let (dir, registry) = project(&[
            (
                "schemas/named.schema.json",
                json!({"$id": "https://example.test/v1/status", "$defs": {"status": {"type": "string"}}}),
            ),
            (
                "request.schema.json",
                json!({"type": "object", "properties": {
                    "where": {"$ref": "https://example.test/v1/status#/$defs/status"}
                }}),
            ),
        ]);
        let root = canonical(&dir);
        let out =
            declarations(&registry, &root, &all(&registry, &root), true).expect("declarations");
        let named = &out[&root.join(".atomic/types/schemas/named.d.ts")];
        assert!(
            named.contains("// @atomic-schema schemas/named.schema.json\n"),
            "{named}"
        );
        assert!(
            named.contains(
                "/** @atomic-schema schemas/named.schema.json#/$defs/status */\nexport type Status"
            ),
            "{named}"
        );
        let request = &out[&root.join(".atomic/types/request.d.ts")];
        assert!(
            request.contains(
                "/** @atomic-schema schemas/named.schema.json#/$defs/status */ \"where\"?: Status;"
            ),
            "{request}"
        );
    }

    #[test]
    fn the_origin_is_read_back_from_a_generated_module() {
        assert_eq!(
            generated_origin(&change_module(true)),
            Some(SchemaOrigin {
                file: "schemas/change.schema.json".into(),
                pointer: String::new(),
            })
        );
        // A module names its schema file with docs off too, since that line,
        // not the doc comments, is what a check reads back to find a module a
        // rename left behind. A file atomic did not generate names nothing.
        assert_eq!(
            generated_origin(&change_module(false)),
            Some(SchemaOrigin {
                file: "schemas/change.schema.json".into(),
                pointer: String::new(),
            })
        );
        assert_eq!(generated_origin("export type A = string;\n"), None);
        // Only the header line atomic writes first counts: a tag inside a
        // comment, or a name that steps out of the project, names nothing, so
        // a file nobody generated is never called stale for its origin.
        for text in [
            "/** @atomic-schema elsewhere.schema.json# */\nexport type A = string;\n",
            "@atomic-schema ../outside.schema.json\nexport type A = string;\n",
            "@atomic-schema file:///etc/passwd\nexport type A = string;\n",
            "@atomic-schema \nexport type A = string;\n",
            // A control character means the name is not one whole path, so it
            // is not read as an origin.
            "// @atomic-schema bad\u{1}name.schema.json\nexport type A = string;\n",
            "// @atomic-schema real.schema.json#/properties/a\u{7}b\nexport type A = string;\n",
        ] {
            assert_eq!(generated_origin(text), None, "{text:?}");
        }
        // The header decides even where a schema description repeats the tag.
        assert_eq!(
            generated_origin(
                "// Generated by atomic. Do not edit.\n// @atomic-schema real.schema.json\n\
                 /** see @atomic-schema elsewhere.schema# */ export type Real = string;\n"
            ),
            Some(SchemaOrigin {
                file: "real.schema.json".into(),
                pointer: String::new(),
            })
        );
    }

    #[test]
    fn a_renamed_schema_file_is_the_only_one_a_stale_module_blames() {
        let (dir, registry) = project(&[
            (
                "schemas/tier.schema.json",
                json!({"$defs": {"level": {"type": "string"}}}),
            ),
            ("keep.schema.json", json!({"type": "string"})),
        ]);
        let root = canonical(&dir);
        let types = root.join(".atomic/types");
        std::fs::create_dir_all(types.join("schemas")).expect("generated dir");
        let out =
            declarations(&registry, &root, &all(&registry, &root), true).expect("declarations");
        for (path, text) in &out {
            std::fs::write(path, text).expect("generated");
        }
        // What a check writes for the atom and the authoring types, a
        // hand-written file, and a live module in a nested directory: none is
        // stale while its schema file is there.
        std::fs::write(types.join("authoring.d.ts"), "export type X = string;\n").expect("write");
        std::fs::write(types.join("notes.txt"), "not a module").expect("write");
        // A header whose name holds a control character names no file, so this
        // module is kept rather than pruned on a half-read name.
        std::fs::write(
            types.join("broken.d.ts"),
            "// generated\n// @atomic-schema bad\u{1}name.schema.json\nexport type Broken = string;\n",
        )
        .expect("write");
        std::fs::create_dir_all(root.join("sub")).expect("sub");
        std::fs::create_dir_all(types.join("sub")).expect("sub");
        std::fs::write(root.join("sub/old.schema.json"), "{}").expect("origin");
        std::fs::write(
            types.join("sub/old.d.ts"),
            "// generated\n// @atomic-schema sub/old.schema.json\nexport type Old = string;\n",
        )
        .expect("write");
        assert_eq!(
            stale_declarations(&root).expect("stale"),
            Vec::<PathBuf>::new()
        );
        std::fs::rename(
            root.join("schemas/tier.schema.json"),
            root.join("schemas/level.schema.json"),
        )
        .expect("rename");
        assert_eq!(
            stale_declarations(&root).expect("stale"),
            vec![types.join("schemas/tier.d.ts")],
            "only the renamed schema's module is stale"
        );
        // The origin file is gone, so a second pass with the module already
        // removed says nothing is.
        std::fs::remove_file(types.join("schemas/tier.d.ts")).expect("remove");
        assert_eq!(
            stale_declarations(&root).expect("stale"),
            Vec::<PathBuf>::new()
        );
        // A nested module whose schema file is gone is found too.
        std::fs::remove_file(root.join("sub/old.schema.json")).expect("remove origin");
        assert_eq!(
            stale_declarations(&root).expect("stale"),
            vec![types.join("sub/old.d.ts")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_module_whose_origin_cannot_be_read_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _registry) = project(&[("keep.schema.json", json!({"type": "string"}))]);
        let root = canonical(&dir);
        let types = root.join(".atomic/types");
        std::fs::create_dir_all(&types).expect("generated dir");
        std::fs::write(
            types.join("keep.d.ts"),
            "// generated\n// @atomic-schema keep.schema.json\nexport type Keep = string;\n",
        )
        .expect("write");
        // A module that names no origin, and one whose origin is unreadable
        // rather than absent, are both kept.
        std::fs::write(types.join("authoring.d.ts"), "export type A = string;\n").expect("write");
        std::fs::write(types.join("hand-written.d.ts"), "export type C = string;\n")
            .expect("write");
        let origin = root.join("secret/other.schema.json");
        std::fs::create_dir(root.join("secret")).expect("secret dir");
        std::fs::write(&origin, "{}").expect("origin");
        std::fs::write(
            types.join("unreadable.d.ts"),
            "// generated\n// @atomic-schema secret/other.schema.json\nexport type B = string;\n",
        )
        .expect("write");
        std::fs::set_permissions(&origin, std::fs::Permissions::from_mode(0o000)).expect("mode");
        assert!(
            std::fs::read_to_string(&origin).is_err(),
            "the mode-000 origin is not unreadable here, so this test would prove nothing"
        );
        assert_eq!(
            stale_declarations(&root).expect("stale"),
            Vec::<PathBuf>::new(),
            "an origin that cannot be read is not called absent"
        );
        std::fs::set_permissions(&origin, std::fs::Permissions::from_mode(0o644))
            .expect("restore mode");
        // The same module is stale once its schema file is truly gone, so the
        // kept result above came from the unreadable file and not from a
        // module that names no origin at all.
        std::fs::remove_file(&origin).expect("remove");
        assert_eq!(
            stale_declarations(&root).expect("stale"),
            vec![types.join("unreadable.d.ts")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_generated_directory_is_passed_over_and_a_linked_tree_refused() {
        // What this returns is meant to be removed, and the generated tree is
        // atomic's own. A link there would point the removal at files the
        // project does not own, so a linked directory inside the tree is passed
        // over and a tree that is a link is refused whole.
        let (dir, _registry) = project(&[("keep.schema.json", json!({"type": "string"}))]);
        let root = canonical(&dir);
        let types = root.join(".atomic/types");
        std::fs::create_dir_all(&types).expect("generated dir");
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(
            outside.join("stale.d.ts"),
            "// generated\n// @atomic-schema gone.schema.json\nexport type Stale = string;\n",
        )
        .expect("outside module");
        std::os::unix::fs::symlink(&outside, types.join("linked")).expect("inner link");
        assert_eq!(
            stale_declarations(&root).expect("inner link"),
            Vec::<PathBuf>::new(),
            "a linked directory in the generated tree is never listed through"
        );
        assert!(
            outside.join("stale.d.ts").is_file(),
            "nothing outside was touched"
        );
        std::fs::remove_file(types.join("linked")).expect("remove inner link");
        std::fs::remove_dir(&types).expect("remove generated dir");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        std::fs::write(
            elsewhere.join("gone.d.ts"),
            "// generated\n// @atomic-schema gone.schema.json\nexport type Stale = string;\n",
        )
        .expect("linked module");
        std::os::unix::fs::symlink(&elsewhere, &types).expect("outer link");
        let error = stale_declarations(&root).expect_err("a linked tree is refused");
        assert!(error.0.contains("not a directory"), "{error}");
        assert!(
            elsewhere.join("gone.d.ts").is_file(),
            "a refused tree keeps every file in it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_generated_tree_that_cannot_be_read_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        // The result of this function is a list to remove, and an unreadable
        // directory is not an empty one: a tree that cannot be listed is
        // refused rather than reported as holding nothing stale.
        let (dir, _registry) = project(&[("keep.schema.json", json!({"type": "string"}))]);
        let root = canonical(&dir);
        let types = root.join(".atomic/types");
        std::fs::create_dir_all(&types).expect("generated dir");
        std::fs::write(
            types.join("keep.d.ts"),
            "// generated\n// @atomic-schema gone.schema.json\nexport type Keep = string;\n",
        )
        .expect("write");
        std::fs::set_permissions(&types, std::fs::Permissions::from_mode(0o000)).expect("mode");
        assert!(
            std::fs::read_dir(&types).is_err(),
            "the mode-000 tree reads here, so this test would prove nothing"
        );
        let error = stale_declarations(&root).expect_err("an unreadable tree is not an empty one");
        assert!(error.0.contains("Permission denied"), "{error}");
        std::fs::set_permissions(&types, std::fs::Permissions::from_mode(0o755)).expect("mode");
        // The same tree lists once it can be read, so the refusal above came
        // from the directory and not from a rule that keeps this module.
        assert_eq!(
            stale_declarations(&root).expect("readable"),
            vec![types.join("keep.d.ts")]
        );
        // And with no generated tree at all there is nothing to remove.
        std::fs::remove_dir_all(&types).expect("remove tree");
        assert_eq!(
            stale_declarations(&root).expect("no tree"),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn a_name_holding_a_control_character_is_never_written_as_an_origin() {
        // A `$defs` key is whatever the author wrote, and an origin is written
        // into a doc comment. A key holding a line break is projected without
        // an origin rather than ending the comment early and putting a line of
        // the author's choosing into the generated file.
        let (dir, registry) = project(&[(
            "weird.schema.json",
            json!({"$defs": {"bad\nkey": {"type": "string"}}}),
        )]);
        let root = canonical(&dir);
        let out =
            declarations(&registry, &root, &all(&registry, &root), true).expect("declarations");
        assert_eq!(
            out[&root.join(".atomic/types/weird.d.ts")],
            "// Generated by atomic. Do not edit.\n\
             // @atomic-schema weird.schema.json\n\
             /** @atomic-schema weird.schema.json# */\n\
             export type Weird = unknown;\n\
             export type BadKey = string;\n"
        );
    }

    #[test]
    fn output_is_deterministic() {
        let (dir, registry) = change_project();
        let root = canonical(&dir);
        let files = all(&registry, &root);
        let first = declarations(&registry, &root, &files, true).expect("first");
        let second = declarations(&registry, &root, &files, true).expect("second");
        assert_eq!(first, second);
    }

    #[test]
    fn colliding_type_names_fail() {
        let (dir, registry) = project(&[(
            "change.schema.json",
            json!({ "type": "string", "$defs": { "change": { "type": "string" } } }),
        )]);
        let root = canonical(&dir);
        let error =
            declarations(&registry, &root, &all(&registry, &root), false).expect_err("collision");
        assert!(error.0.contains("two types are named Change"), "{error}");
    }

    #[test]
    fn references_into_other_parts_of_a_schema_fail() {
        let (dir, registry) = project(&[(
            "pointer.schema.json",
            json!({ "type": "object", "properties": { "a": { "type": "string" }, "b": { "$ref": "#/properties/a" } } }),
        )]);
        let root = canonical(&dir);
        let error =
            declarations(&registry, &root, &all(&registry, &root), false).expect_err("pointer");
        assert!(
            error.0.contains("only a whole schema or one of its $defs"),
            "{error}"
        );
    }

    /// The contracts of an atom with a `reply` output, as the loader holds them.
    fn contracts(context: bool) -> Map<String, Value> {
        let mut schemas = Map::new();
        schemas.insert(
            "Input".into(),
            json!({"type":"object","additionalProperties":false,"required":["email"],
                   "properties":{"email":{"type":"string"}}}),
        );
        schemas.insert(
            "Output".into(),
            json!({"type":"object","additionalProperties":false,"required":["reply"],
                   "properties":{"reply":{"type":"string"},"tone":{"enum":["brief","warm"]}}}),
        );
        if context {
            schemas.insert(
                "Context".into(),
                json!({"type":"object","additionalProperties":false,"required":["profile"],
                       "properties":{"profile":{"enum":["dev","prod"]},
                                      "maxReplyCharacters":{"type":"integer","minimum":1}}}),
            );
        }
        schemas.insert("Case".into(), json!(true));
        schemas.insert(
            "Judges".into(),
            json!({"type":"object","additionalProperties":false,"required":[],"properties":{}}),
        );
        schemas
    }

    /// An atom that declares no context gets a `Config` and a `Props` that hold
    /// `input` alone: nothing in the atom can read a `context` it never
    /// declared, because the type says there is none.
    #[test]
    fn an_atom_with_no_context_declares_no_context_type() {
        assert_eq!(
            atom_declarations(&contracts(false)).expect("declarations"),
            "// Generated from the full runtime contracts. Do not edit.\n\
             import type { AtomConfig } from './authoring';\n\
             export type Input = { \"email\": string; };\n\
             export type Output = { \"reply\": string; \"tone\"?: \"brief\" | \"warm\"; };\n\
             export type Case = unknown;\n\
             export type Judges = {  };\n\
             export type Reply = Output;\n\
             export type EvalCase = Case;\n\
             export type Config = AtomConfig<Input, Reply, EvalCase, Judges>;\n\
             export type Immutable<T> = T extends object ? { readonly [K in keyof T]: Immutable<T[K]> } : T;\n\
             export interface Props { input: Input; }\n"
        );
    }

    /// A declared context — even one with no required field — is the only thing
    /// that puts `context` in `Props` and `Config`, under both spellings.
    #[test]
    fn a_declared_context_reaches_props_and_the_per_atom_config() {
        let text = atom_declarations(&contracts(true)).expect("declarations");
        assert_eq!(
            text,
            "// Generated from the full runtime contracts. Do not edit.\n\
             import type { AtomConfig } from './authoring';\n\
             export type Input = { \"email\": string; };\n\
             export type Output = { \"reply\": string; \"tone\"?: \"brief\" | \"warm\"; };\n\
             export type ContextData = { \"profile\": \"dev\" | \"prod\"; \"maxReplyCharacters\"?: number; };\n\
             export type Case = unknown;\n\
             export type Judges = {  };\n\
             export type Context = Immutable<ContextData>;\n\
             export type Reply = Output;\n\
             export type EvalCase = Case;\n\
             export type Config = AtomConfig<Input, Reply, EvalCase, Judges, Context>;\n\
             export type Immutable<T> = T extends object ? { readonly [K in keyof T]: Immutable<T[K]> } : T;\n\
             export interface Props { input: Input; readonly context: Immutable<Context>; }\n"
        );
        // The per-atom `Config` names the same contract types the explicit
        // `AtomConfig` import is written with, so the two spellings agree.
        assert!(
            text.contains(
                "export type Config = AtomConfig<Input, Reply, EvalCase, Judges, Context>;"
            ),
            "{text}"
        );
    }

    #[test]
    fn atom_declarations_match_the_authoring_projection() {
        let cases: Value =
            serde_json::from_str(include_str!("../tests/typescript/projection.json"))
                .expect("cases");
        let atoms = cases["atoms"].as_array().expect("atom cases");
        assert!(!atoms.is_empty());
        for case in atoms {
            let schemas = case["schemas"].as_object().expect("schemas");
            assert_eq!(
                atom_declarations(schemas).expect("declarations"),
                case["declarations"].as_str().expect("expected"),
                "{}",
                case["name"]
            );
        }
    }

    /// A temporary directory removed on drop.
    mod tempdir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static COUNT: AtomicUsize = AtomicUsize::new(0);
                let path = std::env::temp_dir().join(format!(
                    "atomic-schema-ts-{}-{}",
                    std::process::id(),
                    COUNT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).expect("temp dir");
                Self(path)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
