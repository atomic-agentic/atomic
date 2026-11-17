// SPDX-License-Identifier: Apache-2.0

//! Lowering a contract to a provider's generation profile, and back.
//!
//! The *contract* is full JSON Schema 2020-12 and is what atomic validates.
//! The *generation profile* is the subset a provider's constrained decoding
//! accepts. Lowering rewrites the contract into that subset:
//!
//! - every object gets `additionalProperties: false` and lists all of its
//!   properties in `required`;
//! - a property the contract leaves optional becomes required and nullable,
//!   and the place is recorded, so [`normalize`] can turn the model's `null`
//!   there back into "omitted";
//! - where `anyOf` alternatives disagree about what a `null` means (omitted in
//!   one, a value in another), a required `const` or `enum` property must tell
//!   them apart, and normalization follows the alternative it names; with no
//!   such property the schema is refused, since normalizing would have to
//!   guess;
//! - a keyword the profile does not support is removed and stated in the
//!   description instead, so the model still reads it;
//! - a schema the profile cannot express at all is refused, with the path of
//!   the part that fails, before any tokens are spent.
//!
//! Profiles are data, versioned by the date their rules were checked, because
//! providers change them.

use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// A provider's generation profile.
#[derive(Debug)]
pub struct Profile {
    /// Name and the date its rules were checked, for example
    /// `openai-strict@2026-11-17`.
    pub name: &'static str,
    /// Keywords kept as they are, for any schema.
    pub kept: &'static [&'static str],
    /// Keywords kept on strings.
    pub string: &'static [&'static str],
    /// `format` values kept on strings.
    pub formats: &'static [&'static str],
    /// Keywords kept on numbers and integers.
    pub number: &'static [&'static str],
    /// Keywords kept on arrays.
    pub array: &'static [&'static str],
    /// Keywords that make a schema impossible to lower.
    pub refused: &'static [&'static str],
}

/// OpenAI Structured Outputs in strict mode, from the "Supported schemas"
/// section of OpenAI's guide, read 2026-11-17.
pub const OPENAI_STRICT: Profile = Profile {
    name: "openai-strict@2026-11-17",
    kept: &[
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "const",
        "anyOf",
        "$ref",
        "$defs",
        "description",
        "title",
        "$schema",
        "$id",
    ],
    string: &["pattern", "format"],
    formats: &[
        "date-time",
        "time",
        "date",
        "duration",
        "email",
        "hostname",
        "ipv4",
        "ipv6",
        "uuid",
    ],
    number: &[
        "multipleOf",
        "maximum",
        "exclusiveMaximum",
        "minimum",
        "exclusiveMinimum",
    ],
    array: &["minItems", "maxItems"],
    refused: &[
        "allOf",
        "not",
        "if",
        "then",
        "else",
        "dependentRequired",
        "dependentSchemas",
        "oneOf",
        "patternProperties",
        "unevaluatedProperties",
        "propertyNames",
    ],
};

/// A lowered schema and what lowering did to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Lowered {
    /// The schema to constrain generation with.
    pub schema: Value,
    /// Where lowering made an optional property nullable, for reports: an
    /// instance path with `*` for any array index, or, inside a definition,
    /// `#` and the definition's schema path. [`normalize`] does not read this
    /// list; it follows the schema, so a definition is normalized wherever it
    /// is used.
    pub nulls_added: Vec<String>,
    /// Constraints moved into descriptions, by schema path.
    pub hints: Vec<(String, String)>,
    /// Optional properties the contract also allows to be null: the model
    /// cannot express "omitted" there, so its null is kept as a value.
    pub warnings: Vec<String>,
    /// How to undo lowering on a generated value.
    plan: Plan,
    /// The plans of definitions, by schema path (`/$defs/name`).
    defs: BTreeMap<String, Plan>,
}

/// How to normalize the values one schema node describes; it mirrors the
/// contract, so definitions and alternatives are followed where they apply.
#[derive(Clone, Debug, Default, PartialEq)]
struct Plan {
    /// The node's schema path, for errors.
    path: String,
    /// Properties to descend into.
    props: Vec<(String, Plan)>,
    /// Optional properties lowering made nullable: a null there is "omitted".
    drop_null: Vec<String>,
    /// Properties where the contract itself allows null: a null is a value.
    null_ok: Vec<String>,
    /// Required properties with a fixed set of values (`const` or `enum`),
    /// which can tell `anyOf` alternatives apart.
    tags: Vec<(String, Vec<Value>)>,
    items: Option<Box<Plan>>,
    any_of: Vec<Plan>,
    /// When the alternatives disagree about a null: the property whose value
    /// names the alternative, and each alternative's values for it.
    select: Option<(String, Vec<Vec<Value>>)>,
    /// A local `$ref`, as the schema path of its target.
    reference: Option<String>,
}

/// Why a schema cannot be lowered.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("schema path {path}: {message} (profile {profile})")]
pub struct LowerError {
    /// JSON Pointer into the schema.
    pub path: String,
    /// What cannot be expressed.
    pub message: String,
    /// The profile.
    pub profile: &'static str,
}

/// Lower `contract` to `profile`.
pub fn lower(contract: &Value, profile: &Profile) -> Result<Lowered, LowerError> {
    let err = |path: &str, message: String| LowerError {
        path: if path.is_empty() {
            "/".into()
        } else {
            path.to_owned()
        },
        message,
        profile: profile.name,
    };
    if contract.get("anyOf").is_some() {
        return Err(err("", "the root must be an object, not anyOf".into()));
    }
    if !is_object_schema(contract) {
        return Err(err("", "the root must be an object schema".into()));
    }
    let mut out = Lowered {
        schema: Value::Null,
        nulls_added: Vec::new(),
        hints: Vec::new(),
        warnings: Vec::new(),
        plan: Plan::default(),
        defs: BTreeMap::new(),
    };
    let (schema, plan) = node(contract, "", "", profile, &mut out, &err)?;
    out.schema = schema;
    out.plan = plan;
    // Settle every anyOf now that every definition's plan is known.
    let defs = out.defs.clone();
    settle(&mut out.plan, &defs, &err)?;
    let mut settled = BTreeMap::new();
    for (k, mut d) in std::mem::take(&mut out.defs) {
        settle(&mut d, &defs, &err)?;
        settled.insert(k, d);
    }
    out.defs = settled;
    out.nulls_added.sort();
    out.warnings.sort();
    out.hints.sort();
    Ok(out)
}

fn is_object_schema(s: &Value) -> bool {
    s.get("type").and_then(Value::as_str) == Some("object") || s.get("properties").is_some()
}

fn allows_null(s: &Value) -> bool {
    match s.get("type") {
        Some(Value::String(t)) => t == "null",
        Some(Value::Array(ts)) => ts.iter().any(|t| t == "null"),
        _ => {
            s.get("enum")
                .and_then(Value::as_array)
                .is_some_and(|e| e.contains(&Value::Null))
                || s.get("anyOf")
                    .and_then(Value::as_array)
                    .is_some_and(|bs| bs.iter().any(allows_null))
        }
    }
}

/// Make `s` also accept null.
fn nullable(s: &Value) -> Value {
    let mut s = s.clone();
    if let Some(obj) = s.as_object_mut() {
        if obj.contains_key("$ref") || obj.contains_key("anyOf") {
            let branches = match obj.remove("anyOf") {
                Some(Value::Array(b)) if obj.is_empty() => b,
                Some(Value::Array(b)) => vec![json!({"anyOf": b})],
                _ => vec![Value::Object(std::mem::take(obj))],
            };
            let mut branches = branches;
            branches.push(json!({"type": "null"}));
            return json!({ "anyOf": branches });
        }
        match obj.get_mut("type") {
            Some(Value::String(t)) => {
                let t = std::mem::take(t);
                obj.insert("type".into(), json!([t, "null"]));
            }
            Some(Value::Array(ts)) => ts.push(json!("null")),
            _ => {}
        }
        if let Some(Value::Array(e)) = obj.get_mut("enum") {
            e.push(Value::Null);
        }
    }
    s
}

fn node(
    s: &Value,
    path: &str,
    instance: &str,
    profile: &Profile,
    out: &mut Lowered,
    err: &dyn Fn(&str, String) -> LowerError,
) -> Result<(Value, Plan), LowerError> {
    let mut plan = Plan {
        path: path_or_root(path),
        ..Plan::default()
    };
    // atomic's own artifact reference cannot be resolved by a provider:
    // inline it, keeping any mediaType as a hint.
    if s.get("$ref").and_then(Value::as_str) == Some(crate::artifact::ARTIFACT_SCHEMA_ID) {
        let mut inlined = crate::artifact::artifact_schema();
        if let Some(obj) = inlined.as_object_mut() {
            obj.remove("$id");
            if let Some(m) = s.get("mediaType").and_then(Value::as_str) {
                obj.insert(
                    "description".into(),
                    Value::String(format!(
                        "A reference to a stored {m} artifact: artifact://sha256/<digest>."
                    )),
                );
            }
        }
        return Ok((inlined, plan));
    }
    if let Some(target) = s.get("$ref").and_then(Value::as_str) {
        plan.reference = target.strip_prefix('#').map(str::to_owned);
    }
    let Value::Object(map) = s else {
        return match s {
            Value::Bool(true) => Err(err(
                path,
                "an unconstrained schema (true) cannot be generated".into(),
            )),
            _ => Err(err(path, format!("{s} is not a schema"))),
        };
    };
    for k in profile.refused {
        if map.contains_key(*k) {
            return Err(err(path, format!("`{k}` is not supported")));
        }
    }
    let types: Vec<&str> = match map.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let mut kept = Map::new();
    let mut hints = Vec::new();
    for (k, v) in map {
        let keep = profile.kept.contains(&k.as_str())
            || (types.contains(&"string") && profile.string.contains(&k.as_str()))
            || ((types.contains(&"number") || types.contains(&"integer"))
                && profile.number.contains(&k.as_str()))
            || (types.contains(&"array") && profile.array.contains(&k.as_str()));
        let keep =
            keep && (k != "format" || v.as_str().is_some_and(|f| profile.formats.contains(&f)));
        if keep {
            kept.insert(k.clone(), v.clone());
        } else if !matches!(
            k.as_str(),
            "default" | "examples" | "$comment" | "deprecated" | "readOnly" | "writeOnly"
        ) {
            hints.push(format!("{k}: {v}"));
        }
    }
    if !hints.is_empty() {
        // Sorted, so the text does not depend on map ordering.
        hints.sort();
        let text = format!("Constraints: {}.", hints.join("; "));
        let description = match kept.get("description").and_then(Value::as_str) {
            Some(d) => format!("{d} {text}"),
            None => text,
        };
        kept.insert("description".into(), Value::String(description));
        for h in hints {
            out.hints.push((path_or_root(path), h));
        }
    }

    if is_object_schema(s) {
        match map.get("additionalProperties") {
            None | Some(Value::Bool(false)) => {}
            Some(_) => {
                return Err(err(
                    path,
                    "an object that allows extra properties cannot be generated; list its properties".into(),
                ));
            }
        }
        let required: Vec<String> = map
            .get("required")
            .and_then(Value::as_array)
            .map(|r| {
                r.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let props = map
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut lowered = Map::new();
        for (name, prop) in &props {
            let child_path = format!("{path}/properties/{}", escape(name));
            let child_instance = format!("{instance}/{}", escape(name));
            let (mut p, child) = node(prop, &child_path, &child_instance, profile, out, err)?;
            plan.props.push((name.clone(), child));
            if allows_null(prop) {
                plan.null_ok.push(name.clone());
            }
            if required.contains(name) {
                let values = match (prop.get("const"), prop.get("enum")) {
                    (Some(c), _) => Some(vec![c.clone()]),
                    (None, Some(Value::Array(e))) => Some(e.clone()),
                    _ => None,
                };
                if let Some(values) = values {
                    plan.tags.push((name.clone(), values));
                }
            }
            if !required.contains(name) {
                if prop.get("anyOf").is_some() && allows_null(prop) {
                    // A null could be "omitted" or the value of the branch
                    // that allows null: normalization could not tell which.
                    return Err(err(
                        &child_path,
                        format!(
                            "`{name}` is optional and one of its anyOf branches allows null, so a null from the \
                             model is ambiguous; make `{name}` required, or drop null from its branches"
                        ),
                    ));
                }
                if allows_null(prop) {
                    out.warnings.push(child_instance.clone());
                } else {
                    p = nullable(&p);
                    plan.drop_null.push(name.clone());
                    out.nulls_added.push(child_instance);
                }
            }
            lowered.insert(name.clone(), p);
        }
        kept.insert("type".into(), json!("object"));
        kept.insert("properties".into(), Value::Object(lowered));
        let mut names: Vec<&String> = props.keys().collect();
        names.sort();
        kept.insert("required".into(), json!(names));
        kept.insert("additionalProperties".into(), json!(false));
    }
    if let Some(items) = map.get("items") {
        let (lowered, child) = node(
            items,
            &format!("{path}/items"),
            &format!("{instance}/*"),
            profile,
            out,
            err,
        )?;
        kept.insert("items".into(), lowered);
        plan.items = Some(Box::new(child));
    }
    if let Some(Value::Array(branches)) = map.get("anyOf") {
        let mut lowered = Vec::new();
        for (i, b) in branches.iter().enumerate() {
            let (l, child) = node(b, &format!("{path}/anyOf/{i}"), instance, profile, out, err)?;
            lowered.push(l);
            plan.any_of.push(child);
        }
        kept.insert("anyOf".into(), Value::Array(lowered));
    }
    if let Some(Value::Object(defs)) = map.get("$defs") {
        let mut lowered = Map::new();
        for (name, d) in defs {
            // Definitions are reached through $ref; their nulls are recorded
            // where the definition sits, not where it is used.
            let def_path = format!("{path}/$defs/{}", escape(name));
            let (l, child) = node(d, &def_path, &format!("#{def_path}"), profile, out, err)?;
            lowered.insert(name.clone(), l);
            out.defs.insert(def_path, child);
        }
        kept.insert("$defs".into(), Value::Object(lowered));
    }
    Ok((Value::Object(kept), plan))
}

/// How deep normalization follows references, which may be recursive.
const MAX_DEPTH: usize = 64;

/// The instance paths, relative to `plan`, where a null means "omitted"
/// (`null_ok` false) or is a value (`null_ok` true).
fn null_paths(
    plan: &Plan,
    defs: &BTreeMap<String, Plan>,
    prefix: &str,
    null_ok: bool,
    depth: usize,
    out: &mut BTreeSet<String>,
) {
    if depth > 16 {
        return;
    }
    if let Some(target) = plan.reference.as_ref().and_then(|r| defs.get(r)) {
        null_paths(target, defs, prefix, null_ok, depth + 1, out);
    }
    let names = if null_ok {
        &plan.null_ok
    } else {
        &plan.drop_null
    };
    for name in names {
        out.insert(format!("{prefix}/{}", escape(name)));
    }
    for (name, child) in &plan.props {
        let at = format!("{prefix}/{}", escape(name));
        null_paths(child, defs, &at, null_ok, depth + 1, out);
    }
    if let Some(items) = &plan.items {
        null_paths(items, defs, &format!("{prefix}/*"), null_ok, depth + 1, out);
    }
    for branch in &plan.any_of {
        null_paths(branch, defs, prefix, null_ok, depth + 1, out);
    }
}

/// A branch's tags, following a reference to its definition.
fn tags_of<'a>(plan: &'a Plan, defs: &'a BTreeMap<String, Plan>) -> &'a [(String, Vec<Value>)] {
    match plan.reference.as_ref().and_then(|r| defs.get(r)) {
        Some(target) if plan.tags.is_empty() => &target.tags,
        _ => &plan.tags,
    }
}

/// Decide, for every anyOf under `plan`, whether its alternatives can
/// disagree about a null, and if so which property tells them apart.
fn settle(
    plan: &mut Plan,
    defs: &BTreeMap<String, Plan>,
    err: &dyn Fn(&str, String) -> LowerError,
) -> Result<(), LowerError> {
    for (_, child) in &mut plan.props {
        settle(child, defs, err)?;
    }
    if let Some(items) = &mut plan.items {
        settle(items, defs, err)?;
    }
    for branch in &mut plan.any_of {
        settle(branch, defs, err)?;
    }
    if plan.any_of.len() < 2 {
        return Ok(());
    }
    let paths = |b: &Plan, null_ok: bool| {
        let mut out = BTreeSet::new();
        null_paths(b, defs, "", null_ok, 0, &mut out);
        out
    };
    let omitted: Vec<BTreeSet<String>> = plan.any_of.iter().map(|b| paths(b, false)).collect();
    let values: Vec<BTreeSet<String>> = plan.any_of.iter().map(|b| paths(b, true)).collect();
    let mut conflict = None;
    for (i, o) in omitted.iter().enumerate() {
        for (j, v) in values.iter().enumerate() {
            if i != j
                && let Some(at) = o.intersection(v).next()
            {
                conflict = Some((i, j, at.clone()));
            }
        }
    }
    let Some((i, j, at)) = conflict else {
        return Ok(());
    };
    // A required property whose values differ in every pair of alternatives.
    let first = tags_of(&plan.any_of[0], defs);
    let discriminator = first.iter().find_map(|(name, _)| {
        let sets: Option<Vec<Vec<Value>>> = plan
            .any_of
            .iter()
            .map(|b| {
                tags_of(b, defs)
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
            })
            .collect();
        let sets = sets?;
        let disjoint = sets.iter().enumerate().all(|(a, x)| {
            sets.iter()
                .skip(a + 1)
                .all(|y| x.iter().all(|v| !y.contains(v)))
        });
        disjoint.then(|| (name.clone(), sets))
    });
    match discriminator {
        Some(select) => {
            plan.select = Some(select);
            Ok(())
        }
        None => Err(err(
            &plan.path,
            format!(
                "a null at {at} means \"omitted\" in alternative {i} but is a value in alternative {j}, \
                 and no required const or enum property tells the alternatives apart; add one, or make \
                 the property required in alternative {i}"
            ),
        )),
    }
}

fn apply(plan: &Plan, defs: &BTreeMap<String, Plan>, value: &mut Value, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    if let Some(target) = plan.reference.as_ref().and_then(|r| defs.get(r)) {
        apply(target, defs, value, depth + 1);
    }
    if let Value::Object(map) = value {
        for name in &plan.drop_null {
            if map.get(name) == Some(&Value::Null) {
                map.remove(name);
            }
        }
        for (name, child) in &plan.props {
            if let Some(v) = map.get_mut(name) {
                apply(child, defs, v, depth + 1);
            }
        }
    }
    if let (Value::Array(items), Some(item)) = (&mut *value, &plan.items) {
        for v in items {
            apply(item, defs, v, depth + 1);
        }
    }
    match &plan.select {
        // Only the alternative the value names.
        Some((name, sets)) => {
            let tag = value.get(name).cloned();
            let chosen = tag.and_then(|t| sets.iter().position(|s| s.contains(&t)));
            if let Some(i) = chosen {
                apply(&plan.any_of[i], defs, value, depth + 1);
            }
        }
        // The alternatives agree about every null, so each may apply.
        None => {
            for branch in &plan.any_of {
                apply(branch, defs, value, depth + 1);
            }
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

fn path_or_root(p: &str) -> String {
    if p.is_empty() {
        "/".into()
    } else {
        p.to_owned()
    }
}

/// Check that `schema` keeps the rules of strict generation for `profile`:
/// every object closed and fully required, only supported keywords, the root
/// an object. Returns every violation with its schema path.
pub fn check(schema: &Value, profile: &Profile) -> Vec<String> {
    let mut problems = Vec::new();
    if schema.get("anyOf").is_some() || !is_object_schema(schema) {
        problems.push("/: the root must be an object".into());
    }
    walk_check(schema, "", profile, &mut problems);
    problems
}

fn walk_check(s: &Value, path: &str, profile: &Profile, problems: &mut Vec<String>) {
    let Value::Object(map) = s else {
        problems.push(format!("{}: not a schema object", path_or_root(path)));
        return;
    };
    let types: Vec<&str> = match map.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    for k in map.keys() {
        let ok = profile.kept.contains(&k.as_str())
            || (types.contains(&"string") && profile.string.contains(&k.as_str()))
            || ((types.contains(&"number") || types.contains(&"integer"))
                && profile.number.contains(&k.as_str()))
            || (types.contains(&"array") && profile.array.contains(&k.as_str()));
        if !ok {
            problems.push(format!("{}: unsupported keyword {k}", path_or_root(path)));
        }
    }
    if let Some(props) = map.get("properties").and_then(Value::as_object) {
        if map.get("additionalProperties") != Some(&Value::Bool(false)) {
            problems.push(format!(
                "{}: additionalProperties must be false",
                path_or_root(path)
            ));
        }
        let required: Vec<&str> = map
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        for (name, p) in props {
            if !required.contains(&name.as_str()) {
                problems.push(format!("{}: {name} is not required", path_or_root(path)));
            }
            walk_check(
                p,
                &format!("{path}/properties/{}", escape(name)),
                profile,
                problems,
            );
        }
    }
    if let Some(items) = map.get("items") {
        walk_check(items, &format!("{path}/items"), profile, problems);
    }
    for key in ["anyOf"] {
        if let Some(Value::Array(bs)) = map.get(key) {
            for (i, b) in bs.iter().enumerate() {
                walk_check(b, &format!("{path}/{key}/{i}"), profile, problems);
            }
        }
    }
    if let Some(Value::Object(defs)) = map.get("$defs") {
        for (name, d) in defs {
            walk_check(
                d,
                &format!("{path}/$defs/{}", escape(name)),
                profile,
                problems,
            );
        }
    }
}

/// Undo lowering on a generated value: where lowering made an optional
/// property nullable, a `null` means the property was omitted, so it is
/// removed. The walk follows the contract: a definition is normalized
/// wherever it is used, and among alternatives that disagree about a null,
/// only the one the value names applies. A null anywhere else, including one
/// the contract permits, is left alone.
pub fn normalize(value: &mut Value, lowered: &Lowered) {
    apply(&lowered.plan, &lowered.defs, value, 0);
}

#[cfg(test)]
mod tests;
