// SPDX-License-Identifier: Apache-2.0

//! Loading and checking a workflow file (`*.workflow.yaml`).
//!
//! A workflow names atom steps and gate steps, orders them with `after`, maps
//! values into each atom's input with CEL `with` expressions, and computes its
//! result with a CEL `returns` expression. Loading checks, before anything
//! runs:
//!
//! - the shape: `atomic: 1`, explicit `input` and `output`, steps with known
//!   fields, `after` naming existing steps with no cycle;
//! - every `atom` names a loaded atom;
//! - every `with` key is a field of the atom's input, every required field is
//!   bound, and every expression type-checks against what the step can see:
//!   the workflow `input`, and the steps it runs after;
//! - a conservative structural fit: where the schema of an expression is known
//!   and plainly cannot fit the target field (a string where an integer is
//!   required, an object missing a required field), the load fails; where
//!   the fit cannot be proven either way, it is a warning.

use crate::atom::Atom;
use atomic_schema::expr::{self, Scope};
use atomic_schema::registry::Registry;
use atomic_schema::yaml;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A loaded workflow.
#[derive(Clone, Debug)]
pub struct Workflow {
    /// The file.
    pub file: PathBuf,
    /// Its name: `name`, else the file name without `.workflow.yaml`.
    pub name: String,
    /// Its version, default 1.
    pub version: u64,
    /// The steps in an order that respects `after`.
    pub order: Vec<String>,
    /// The workflow as written, as JSON.
    pub definition: Value,
    /// Fits the check could not prove, each with its path.
    pub warnings: Vec<String>,
}

/// Why a workflow failed to load.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{}:{line}: {path}: {message}", file.display())]
pub struct WorkflowError {
    /// The workflow file.
    pub file: PathBuf,
    /// The line, from 1.
    pub line: usize,
    /// A JSON Pointer into the workflow.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

const TOP: &[&str] = &[
    "atomic", "name", "version", "input", "output", "steps", "returns", "budget",
];
const STEP: &[&str] = &["atom", "gate", "after", "with", "worktree"];
const GATE: &[&str] = &["title", "show", "timeout", "on_timeout", "approvers"];
const LATER_STEP: &[&str] = &["foreach", "concurrency", "merge_queue", "review", "retry"];

/// Load the workflow at `file`, checking it against `atoms` (by name).
pub fn load(
    file: &Path,
    atoms: &BTreeMap<String, Atom>,
    registry: &mut Registry,
) -> Result<Workflow, WorkflowError> {
    let file = std::fs::canonicalize(file).map_err(|e| WorkflowError {
        file: file.to_owned(),
        line: 1,
        path: String::new(),
        message: format!("cannot read: {e}"),
    })?;
    let text = std::fs::read_to_string(&file).map_err(|e| WorkflowError {
        file: file.clone(),
        line: 1,
        path: String::new(),
        message: format!("cannot read: {e}"),
    })?;
    let doc = yaml::load(&file, &text).map_err(|e| WorkflowError {
        file: file.clone(),
        line: e.at.line,
        path: String::new(),
        message: e.message,
    })?;
    let line_of = |pointer: &str| {
        // The nearest enclosing node that has a position.
        let mut p = pointer.to_owned();
        loop {
            if let Some(pos) = doc.position(&p) {
                return pos.line;
            }
            match p.rfind('/') {
                Some(i) => p.truncate(i),
                None => return 1,
            }
        }
    };
    let fail = |pointer: &str, message: String| WorkflowError {
        file: file.clone(),
        line: line_of(pointer),
        path: pointer.to_owned(),
        message,
    };
    let Value::Object(top) = &doc.value else {
        return Err(fail(
            "",
            "a workflow is a mapping, starting with `atomic: 1`".into(),
        ));
    };
    for key in top.keys() {
        if !TOP.contains(&key.as_str()) {
            return Err(fail(&format!("/{key}"), format!("unknown field `{key}`")));
        }
    }
    match top.get("atomic") {
        Some(Value::Number(n)) if n.as_u64() == Some(crate::project::FORMAT_VERSION) => {}
        Some(other) => {
            return Err(fail(
                "/atomic",
                format!("format version {other} is not one this atomic reads"),
            ));
        }
        None => {
            return Err(fail(
                "",
                "missing the format version: add `atomic: 1`".into(),
            ));
        }
    }
    let file_name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match top.get("name") {
        None => file_name
            .strip_suffix(".workflow.yaml")
            .unwrap_or(&file_name)
            .to_owned(),
        Some(Value::String(n)) if !n.is_empty() => n.clone(),
        Some(_) => return Err(fail("/name", "`name` is a non-empty string".into())),
    };
    let version = match top.get("version") {
        None => 1,
        Some(v) => v
            .as_u64()
            .filter(|v| *v >= 1)
            .ok_or_else(|| fail("/version", "`version` is a whole number from 1".into()))?,
    };

    let dir = file.parent().unwrap_or(Path::new("")).to_owned();
    let mut contracts = BTreeMap::new();
    for role in ["input", "output"] {
        let declared = top.get(role).ok_or_else(|| {
            fail(
                "",
                format!("missing `{role}`: a workflow declares its {role} schema"),
            )
        })?;
        let schema =
            inline(declared, &dir, registry, 0).map_err(|m| fail(&format!("/{role}"), m))?;
        contracts.insert(role, schema);
    }
    let (input, output) = (contracts["input"].clone(), contracts["output"].clone());

    let Some(Value::Object(steps)) = top.get("steps") else {
        return Err(fail("/steps", "`steps` maps step names to steps".into()));
    };

    // Shape and `after`.
    let mut after: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, step) in steps {
        let sp = format!("/steps/{id}");
        let Value::Object(s) = step else {
            return Err(fail(&sp, "a step is a mapping".into()));
        };
        for key in s.keys() {
            let kp = format!("{sp}/{key}");
            if LATER_STEP.contains(&key.as_str()) {
                return Err(fail(
                    &kp,
                    format!("`{key}` is not supported by this version of atomic yet"),
                ));
            }
            if !STEP.contains(&key.as_str()) {
                return Err(fail(&kp, format!("unknown step field `{key}`")));
            }
        }
        match (s.get("atom"), s.get("gate")) {
            (Some(_), Some(_)) => {
                return Err(fail(
                    &sp,
                    "a step is either an atom or a gate, not both".into(),
                ));
            }
            (None, None) => return Err(fail(&sp, "a step needs `atom` or `gate`".into())),
            _ => {}
        }
        let deps = match s.get("after") {
            None => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let dp = format!("{sp}/after/{i}");
                    let d = d
                        .as_str()
                        .ok_or_else(|| fail(&dp, "`after` lists step names".into()))?;
                    if !steps.contains_key(d) {
                        return Err(fail(
                            &dp,
                            format!("`after` names `{d}`, which is not a step here"),
                        ));
                    }
                    Ok(d.to_owned())
                })
                .collect::<Result<_, _>>()?,
            Some(_) => {
                return Err(fail(
                    &format!("{sp}/after"),
                    "`after` is a list of step names".into(),
                ));
            }
        };
        after.insert(id.clone(), deps);
    }
    let order = topo(&after).map_err(|cycle| {
        fail(
            &format!("/steps/{cycle}/after"),
            format!("step `{cycle}` is part of a cycle of `after`"),
        )
    })?;

    // Every step's type, as seen by later steps.
    let mut step_schemas: BTreeMap<String, Value> = BTreeMap::new();
    let mut warnings = Vec::new();
    for id in &order {
        let sp = format!("/steps/{id}");
        let s = &steps[id];
        let visible = ancestors(id, &after);
        let mut props = Map::new();
        for v in &visible {
            props.insert(v.clone(), step_schemas[v].clone());
        }
        let steps_schema =
            json!({"type": "object", "properties": props, "additionalProperties": false});
        let scope = Scope::new()
            .with("input", &input)
            .with("steps", &steps_schema);

        if let Some(atom_name) = s.get("atom") {
            let ap = format!("{sp}/atom");
            let atom_name = atom_name
                .as_str()
                .ok_or_else(|| fail(&ap, "`atom` names an atom".into()))?;
            let Some(atom) = atoms.get(atom_name) else {
                let near: Vec<&str> = atoms
                    .keys()
                    .filter(|k| close(k, atom_name))
                    .map(String::as_str)
                    .collect();
                let hint = if near.is_empty() {
                    String::new()
                } else {
                    format!("; did you mean {}?", near.join(" or "))
                };
                return Err(fail(&ap, format!("no atom named `{atom_name}`{hint}")));
            };
            let target = registry
                .get(&atom.input)
                .map(|e| e.schema.clone())
                .ok_or_else(|| {
                    fail(
                        &ap,
                        format!("atom `{atom_name}` has no registered input schema"),
                    )
                })?;
            let target = inline(&target, Path::new(""), registry, 0).map_err(|m| fail(&ap, m))?;
            let fields = target
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let required: Vec<String> = target
                .get("required")
                .and_then(Value::as_array)
                .map(|r| {
                    r.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let with = match s.get("with") {
                None => Map::new(),
                Some(Value::Object(w)) => w.clone(),
                Some(_) => {
                    return Err(fail(
                        &format!("{sp}/with"),
                        "`with` maps input fields to CEL expressions".into(),
                    ));
                }
            };
            for (field, e) in &with {
                let wp = format!("{sp}/with/{field}");
                let Some(field_schema) = fields.get(field) else {
                    let known: Vec<&str> = fields.keys().map(String::as_str).collect();
                    return Err(fail(
                        &wp,
                        format!(
                            "atom `{atom_name}` has no input field `{field}` (fields: {})",
                            known.join(", ")
                        ),
                    ));
                };
                let source = e
                    .as_str()
                    .ok_or_else(|| fail(&wp, "a mapping is a CEL expression in a string".into()))?;
                let parsed = expr::parse(source).map_err(|e| fail(&wp, e.to_string()))?;
                parsed.check(&scope).map_err(|e| fail(&wp, e.to_string()))?;
                match fits(
                    path_schema(source, &input, &steps_schema).as_ref(),
                    field_schema,
                ) {
                    Fit::Yes => {}
                    Fit::No(why) => {
                        return Err(fail(&wp, format!("`{source}` cannot fit `{field}`: {why}")));
                    }
                    Fit::Unknown => {
                        warnings.push(format!("{wp}: could not prove `{source}` fits `{field}`"))
                    }
                }
            }
            for r in &required {
                if !with.contains_key(r) {
                    return Err(fail(
                        &format!("{sp}/with"),
                        format!(
                            "atom `{atom_name}` requires input field `{r}`, which `with` does not bind"
                        ),
                    ));
                }
            }
            if let Some(w) = s.get("worktree")
                && !w.is_boolean()
            {
                return Err(fail(
                    &format!("{sp}/worktree"),
                    "`worktree` is true or false".into(),
                ));
            }
            let out = registry
                .get(&atom.output)
                .map(|e| e.schema.clone())
                .unwrap_or(json!({}));
            let out = inline(&out, Path::new(""), registry, 0).map_err(|m| fail(&ap, m))?;
            step_schemas.insert(
                id.clone(),
                json!({"type": "object", "properties": {"output": out, "metadata": {"type": "object"}}, "additionalProperties": false}),
            );
        } else if let Some(gate) = s.get("gate") {
            let gp = format!("{sp}/gate");
            let Value::Object(g) = gate else {
                return Err(fail(
                    &gp,
                    "`gate` is a mapping with at least a title".into(),
                ));
            };
            for key in g.keys() {
                if !GATE.contains(&key.as_str()) {
                    return Err(fail(
                        &format!("{gp}/{key}"),
                        format!("unknown gate field `{key}`"),
                    ));
                }
            }
            for key in ["title", "show"] {
                let kp = format!("{gp}/{key}");
                match g.get(key) {
                    None if key == "title" => {
                        return Err(fail(
                            &gp,
                            "a gate needs a `title` (a CEL expression)".into(),
                        ));
                    }
                    None => {}
                    Some(e) => {
                        let source = e
                            .as_str()
                            .ok_or_else(|| fail(&kp, "a CEL expression in a string".into()))?;
                        expr::parse(source)
                            .and_then(|p| p.check(&scope))
                            .map_err(|e| fail(&kp, e.to_string()))?;
                    }
                }
            }
            if let Some(t) = g.get("on_timeout")
                && !matches!(t.as_str(), Some("reject"))
            {
                return Err(fail(
                    &format!("{gp}/on_timeout"),
                    "`on_timeout` is `reject`: an expired gate never approves".into(),
                ));
            }
            step_schemas.insert(
                id.clone(),
                json!({"type": "object", "properties": {"decision": {"type": "object"}}, "additionalProperties": false}),
            );
        }
    }

    // The result.
    let all: Map<String, Value> = step_schemas.into_iter().collect();
    let steps_schema = json!({"type": "object", "properties": all, "additionalProperties": false});
    let scope = Scope::new()
        .with("input", &input)
        .with("steps", &steps_schema);
    let returns = top.get("returns").ok_or_else(|| {
        fail(
            "",
            "missing `returns`: a CEL expression for the result".into(),
        )
    })?;
    let source = returns.as_str().ok_or_else(|| {
        fail(
            "/returns",
            "`returns` is a CEL expression in a string".into(),
        )
    })?;
    expr::parse(source)
        .and_then(|p| p.check(&scope))
        .map_err(|e| fail("/returns", e.to_string()))?;
    match fits(path_schema(source, &input, &steps_schema).as_ref(), &output) {
        Fit::Yes => {}
        Fit::No(why) => {
            return Err(fail(
                "/returns",
                format!("`{source}` cannot fit the workflow's output: {why}"),
            ));
        }
        Fit::Unknown => warnings.push(format!(
            "/returns: could not prove `{source}` fits the output"
        )),
    }

    Ok(Workflow {
        file,
        name,
        version,
        order,
        definition: doc.value.clone(),
        warnings,
    })
}

/// Steps in an order respecting `after`, or a step on a cycle.
fn topo(after: &BTreeMap<String, Vec<String>>) -> Result<Vec<String>, String> {
    let mut done: Vec<String> = Vec::new();
    let mut left: BTreeSet<&String> = after.keys().collect();
    while !left.is_empty() {
        let ready: Vec<&String> = left
            .iter()
            .filter(|s| after[**s].iter().all(|d| done.contains(d)))
            .copied()
            .collect();
        if ready.is_empty() {
            return Err(left.iter().next().map(|s| (*s).clone()).unwrap_or_default());
        }
        for r in ready {
            left.remove(r);
            done.push(r.clone());
        }
    }
    Ok(done)
}

fn ancestors(id: &str, after: &BTreeMap<String, Vec<String>>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack: Vec<&String> = after[id].iter().collect();
    while let Some(s) = stack.pop() {
        if out.insert(s.clone()) {
            stack.extend(after[s].iter());
        }
    }
    out
}

/// Whether two names are one edit apart (for a "did you mean").
fn close(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()] <= 2
}

/// Replace `$ref`s with what they point at, so a schema can be embedded
/// elsewhere. File references resolve relative to `dir`; registered ids
/// through the registry. Recursion past a depth becomes an open schema.
pub(crate) fn inline(
    schema: &Value,
    dir: &Path,
    registry: &mut Registry,
    depth: usize,
) -> Result<Value, String> {
    inline_in(schema, schema, dir, registry, depth)
}

fn inline_in(
    root: &Value,
    schema: &Value,
    dir: &Path,
    registry: &mut Registry,
    depth: usize,
) -> Result<Value, String> {
    if depth > 16 {
        return Ok(json!({}));
    }
    match schema {
        Value::Object(map) => {
            // An artifact reference stays as it is: lowering inlines it with
            // its media type, and the runtime checks the store against it.
            if map.get("$ref").and_then(Value::as_str)
                == Some(atomic_schema::artifact::ARTIFACT_SCHEMA_ID)
            {
                return Ok(schema.clone());
            }
            if let Some(Value::String(r)) = map.get("$ref") {
                let (base, fragment) = r.split_once('#').map_or((r.as_str(), ""), |(b, f)| (b, f));
                let (doc, doc_dir) = if base.is_empty() {
                    (root.clone(), dir.to_owned())
                } else if base.contains(':') {
                    let e = registry
                        .get(base)
                        .ok_or_else(|| format!("no schema registered as {base}"))?;
                    (
                        e.schema.clone(),
                        e.file.parent().map(Path::to_owned).unwrap_or_default(),
                    )
                } else {
                    let path = dir.join(base);
                    if !path.is_file() {
                        return Err(format!(
                            "dangling reference: {r} does not exist ({})",
                            path.display()
                        ));
                    }
                    let id = registry.load_file(&path).map_err(|e| e.to_string())?;
                    let e = registry
                        .get(&id)
                        .ok_or_else(|| format!("no schema registered as {id}"))?;
                    (
                        e.schema.clone(),
                        path.parent().map(Path::to_owned).unwrap_or_default(),
                    )
                };
                let target = if fragment.is_empty() {
                    doc.clone()
                } else {
                    doc.pointer(fragment)
                        .cloned()
                        .ok_or_else(|| format!("{r}: no such place in the schema"))?
                };
                return inline_in(&doc, &target, &doc_dir, registry, depth + 1);
            }
            let mut out = Map::new();
            for (k, v) in map {
                if k == "$defs" || k == "$id" || k == "$schema" {
                    continue;
                }
                out.insert(k.clone(), inline_in(root, v, dir, registry, depth)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => items
            .iter()
            .map(|v| inline_in(root, v, dir, registry, depth))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

/// The schema of `source` when it is a plain path from `input` or `steps`.
fn path_schema(source: &str, input: &Value, steps: &Value) -> Option<Value> {
    let source = source.trim();
    if !source
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
    {
        return None;
    }
    let mut parts = source.split('.');
    let mut cur = match parts.next()? {
        "input" => input.clone(),
        "steps" => steps.clone(),
        _ => return None,
    };
    for p in parts {
        cur = cur.get("properties")?.get(p)?.clone();
    }
    Some(cur)
}

enum Fit {
    Yes,
    No(String),
    Unknown,
}

fn types_of(s: &Value) -> Option<BTreeSet<String>> {
    match s.get("type") {
        Some(Value::String(t)) => Some(BTreeSet::from([t.clone()])),
        Some(Value::Array(ts)) => Some(
            ts.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        ),
        _ => s.get("enum").and_then(Value::as_array).map(|vs| {
            vs.iter()
                .map(|v| {
                    match v {
                        Value::Null => "null",
                        Value::Bool(_) => "boolean",
                        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
                        Value::Number(_) => "number",
                        Value::String(_) => "string",
                        Value::Array(_) => "array",
                        Value::Object(_) => "object",
                    }
                    .to_owned()
                })
                .collect()
        }),
    }
}

/// Whether a value of schema `from` fits `to`, as far as can be proven.
fn fits(from: Option<&Value>, to: &Value) -> Fit {
    let Some(from) = from else {
        return Fit::Unknown;
    };
    let (Some(ft), Some(tt)) = (types_of(from), types_of(to)) else {
        return Fit::Unknown;
    };
    // An integer is a number.
    let widen = |t: &String| {
        if t == "integer" && tt.contains("number") {
            "number".to_owned()
        } else {
            t.clone()
        }
    };
    let ft: BTreeSet<String> = ft.iter().map(widen).collect();
    if ft.is_disjoint(&tt) {
        return Fit::No(format!(
            "it is {} and the field takes {}",
            join(&ft),
            join(&tt)
        ));
    }
    if !ft.is_subset(&tt) {
        return Fit::Unknown;
    }
    if ft.contains("object") {
        let have: BTreeSet<&str> = from
            .get("properties")
            .and_then(Value::as_object)
            .map(|p| p.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let open = from.get("properties").is_none();
        if let Some(Value::Array(req)) = to.get("required") {
            for r in req.iter().filter_map(Value::as_str) {
                if !open && !have.contains(r) {
                    return Fit::No(format!("it has no field `{r}`, which the target requires"));
                }
            }
        }
        if open {
            return Fit::Unknown;
        }
    }
    Fit::Yes
}

fn join(s: &BTreeSet<String>) -> String {
    s.iter().cloned().collect::<Vec<_>>().join(" or ")
}

#[cfg(test)]
mod tests;
