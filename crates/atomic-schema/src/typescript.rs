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
    }
    .ty(schema, 0)
}

type Resolve<'a> = dyn FnMut(&str) -> Result<String, TypeError> + 'a;

struct Projector<'a, 'b> {
    resolve: &'a mut Resolve<'b>,
    docs: bool,
}

/// A JSON value as a TypeScript literal type.
fn literal(value: &Value) -> String {
    value.to_string()
}

/// A doc comment for the keywords TypeScript cannot express, or "".
fn doc(schema: &Value) -> String {
    let Some(map) = schema.as_object() else {
        return String::new();
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
                    .map(|branch| self.ty(branch, depth + 1))
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
            let name = (self.resolve)(reference)?;
            let mut rest = map.clone();
            rest.remove("$ref");
            for key in ANNOTATIONS {
                rest.remove(key);
            }
            if rest.is_empty() {
                return Ok(name);
            }
            // Keywords beside a $ref apply as well as it (JSON Schema 2020-12).
            return Ok(format!(
                "({name} & {})",
                self.ty(&Value::Object(rest), depth + 1)?
            ));
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
                let comment = if self.docs { doc(value) } else { String::new() };
                members.push(format!(
                    "{comment}{}{optional}: {};",
                    literal(&Value::String(key.clone())),
                    self.ty(value, depth + 1)?
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
                self.ty(items.unwrap_or(&Value::Bool(true)), depth + 1)?
            ));
        };
        let prefix = prefix
            .as_array()
            .ok_or_else(|| TypeError("prefixItems must be an array".into()))?;
        let mut members = prefix
            .iter()
            .map(|item| self.ty(item, depth + 1))
            .collect::<Result<Vec<_>, _>>()?;
        match items {
            Some(Value::Bool(false)) => {}
            Some(rest) => members.push(format!("...Array<{}>", self.ty(rest, depth + 1)?)),
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
/// its `$defs` becomes that type, imported from its module. With `docs`,
/// descriptions and constraints TypeScript cannot express become doc
/// comments. Schemas that are neither requested nor referenced are not read.
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
        let mut imports: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
        let body = {
            let mut resolve = |reference: &str| -> Result<String, TypeError> {
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
                let name =
                    if fragment.is_empty() {
                        target_names.root.clone()
                    } else if let Some(key) = fragment.strip_prefix("/$defs/") {
                        let key = key.replace("~1", "/").replace("~0", "~");
                        target_names.defs.get(&key).cloned().ok_or_else(|| {
                            TypeError(format!("{reference}: no $defs entry {key}"))
                        })?
                    } else {
                        return fail(format!(
                            "{reference}: only a whole schema or one of its $defs can be typed"
                        ));
                    };
                if owner != index {
                    imports.entry(owner).or_default().insert(name.clone());
                }
                Ok(name)
            };
            let mut projector = Projector {
                resolve: &mut resolve,
                docs,
            };
            let lead = |schema: &Value| {
                let comment = if docs { doc(schema) } else { String::new() };
                if comment.is_empty() {
                    comment
                } else {
                    format!("{}\n", comment.trim_end())
                }
            };
            let mut body = format!(
                "{}export type {} = {};\n",
                lead(&module.entry.schema),
                own.root,
                projector.ty(&strip_defs(&module.entry.schema), 0)?
            );
            if let Some(definitions) = module.entry.schema.get("$defs").and_then(Value::as_object) {
                for (key, schema) in definitions {
                    body.push_str(&format!(
                        "{}export type {} = {};\n",
                        lead(schema),
                        own.defs[key],
                        projector.ty(schema, 0)?
                    ));
                }
            }
            body
        };
        let mut text = format!(
            "// Generated by atomic from {}. Do not edit.\n",
            module.relative.display()
        );
        let from = module.output.parent().unwrap_or(root);
        for (owner, names) in &imports {
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let target = &modules[*owner].output;
            let target = target.with_file_name(
                target
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".d.ts"))
                    .unwrap_or(""),
            );
            text.push_str(&format!(
                "import type {{ {} }} from '{}';\n",
                names.join(", "),
                specifier(from, &target)
            ));
            queue.push(*owner);
        }
        text.push_str(&body);
        out.insert(module.output.clone(), text);
    }
    Ok(out)
}

/// One atom's declarations: a type for each of its resolved contracts
/// (`Input`, `Output`, `Context`, `Case`, `Judges`), and the `Props`, `Reply`,
/// `EvalCase` and `Immutable` names atom sources use.
pub fn atom_declarations(schemas: &Map<String, Value>) -> Result<String, TypeError> {
    let mut types = Vec::new();
    for (key, schema) in schemas {
        let name = if key == "Context" { "ContextData" } else { key };
        types.push(format!(
            "export type {name} = {};",
            type_expression(schema)?
        ));
    }
    Ok(format!(
        "// Generated from the full runtime contracts. Do not edit.\n{}\n\
         export type Context = Immutable<ContextData>;\n\
         export type Reply = Output;\n\
         export type EvalCase = Case;\n\
         export type Immutable<T> = T extends object ? {{ readonly [K in keyof T]: Immutable<T[K]> }} : T;\n\
         export interface Props {{ input: Input; readonly context: Immutable<Context>; }}\n",
        types.join("\n")
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
            "// Generated by atomic from schemas/change.schema.json. Do not edit.\n\
             export type Change = { \"id\": string; \"tier\"?: Tier; };\n\
             export type Tier = \"tier-1\" | \"tier-2\";\n"
        );
        assert_eq!(
            out[&types.join("schemas/tree.d.ts")],
            "// Generated by atomic from schemas/tree.schema.json. Do not edit.\n\
             export type Outline = Node;\n\
             export type Node = { \"label\": string; \"children\"?: Array<Node>; };\n"
        );
        assert_eq!(
            out[&types.join("atoms/assess.input.d.ts")],
            "// Generated by atomic from atoms/assess.input.schema.json. Do not edit.\n\
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
            change.contains("/** The change ID. @pattern ^CHG-[0-9]+$ */ \"id\": string;"),
            "{change}"
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
