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
use crate::budget::Budget;
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
    /// The limits its `budget` block writes for the whole run, children
    /// included; the rest inherit defaults (see [`crate::budget`]).
    pub budget: Budget,
    /// The workflow as written, as JSON.
    pub definition: Value,
    /// Its input contract, with every reference inlined.
    pub input: Value,
    /// Its output contract, with every reference inlined.
    pub output: Value,
    /// SHA-256 of the file's text, as it was read and parsed.
    pub hash: String,
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
    /// What to change to fix it, when the check knows.
    pub action: Option<String>,
}

/// How far a value may be trusted, least first.
pub const TRUST: [&str; 3] = ["external", "agent", "team"];

/// Where `origin` sits in [`TRUST`]; an unknown origin ranks as external.
pub fn trust_rank(origin: &str) -> usize {
    TRUST.iter().position(|o| *o == origin).unwrap_or(0)
}

/// The riskiest input origin `atom` accepts: its `trust`, or external when
/// it declares none.
pub fn accepted_trust(atom: &Atom) -> &str {
    atom.frontmatter
        .get("trust")
        .and_then(Value::as_str)
        .unwrap_or("external")
}

/// Why `source`, which is `origin` data, may not bind an input of `atom`, if
/// it may not: an atom takes no input less trusted than it accepts (see
/// [`accepted_trust`]). A workflow's step and a direct run of the atom are
/// held to this alike.
pub fn untrusted_binding(atom: &Atom, source: &str, origin: &str) -> Option<String> {
    let accepts = accepted_trust(atom);
    (trust_rank(origin) < trust_rank(accepts)).then(|| {
        format!(
            "`{source}` is {origin} data, and atom `{}` accepts {accepts} input at most \
             (`trust: '{accepts}'`)",
            atom.name
        )
    })
}

/// Where data `trust` accepts can come from: what a binding refused by
/// [`untrusted_binding`] could read instead. Only a person's decision at a
/// gate, or a literal the workflow file writes, is team data; a model's
/// output is agent data as well.
pub fn trusted_sources(trust: &str) -> &'static str {
    match trust {
        "team" => "a gate a person decides, or a literal in the workflow file",
        _ => "a model step's output, a gate a person decides, or a literal in the workflow file",
    }
}

/// The most a binding's sources allow it to be trusted: the least trusted
/// of what its CEL expression reads. `kinds` gives each earlier step's kind
/// (`gate`, `lite`, `codex` or `exec`). The workflow's input is external
/// (nothing yet marks a field of it as coming from trusted configuration); a
/// model's output is agent; an exec atom's output came from outside code and
/// is external; a gate's decision came from a person and is team; and a
/// literal the workflow file writes is team. Every value, team included,
/// still reaches the model as data, never as instructions.
pub fn binding_origin(
    source: &str,
    kinds: &BTreeMap<String, String>,
) -> Result<&'static str, String> {
    let parsed = expr::parse(source).map_err(|e| e.to_string())?;
    let mut least = "team";
    for path in parsed.references() {
        let origin = match (path.first().map(String::as_str), path.get(1)) {
            (Some("steps"), Some(step)) => match kinds.get(step).map(String::as_str) {
                Some("gate") => "team",
                Some("lite" | "codex") => "agent",
                _ => "external",
            },
            _ => "external",
        };
        if trust_rank(origin) < trust_rank(least) {
            least = origin;
        }
    }
    Ok(least)
}

pub(crate) const TOP: &[&str] = &[
    "atomic", "name", "version", "input", "output", "steps", "returns", "budget",
];
pub(crate) const STEP: &[&str] = &["atom", "gate", "after", "with", "origins", "worktree"];
pub(crate) const GATE: &[&str] = &[
    "title",
    "ask",
    "answer",
    "remember",
    "show",
    "timeout",
    "on_timeout",
    "approvers",
];
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
        action: None,
    })?;
    let text = std::fs::read_to_string(&file).map_err(|e| WorkflowError {
        file: file.clone(),
        line: 1,
        path: String::new(),
        message: format!("cannot read: {e}"),
        action: None,
    })?;
    let doc = yaml::load(&file, &text).map_err(|e| WorkflowError {
        file: file.clone(),
        line: e.at.line,
        path: String::new(),
        message: e.message,
        action: None,
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
        action: None,
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

    // The run's own limits; the rest inherit the configured defaults.
    let budget = match top.get("budget") {
        None => Budget::default(),
        Some(b) => {
            Budget::parse(b).map_err(|(sub, message)| fail(&format!("/budget{sub}"), message))?
        }
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
        let declared = atomic_schema::compact::to_json_schema(declared).map_err(|e| {
            fail(
                &format!("/{role}{}", e.path.trim_end_matches('/')),
                e.message,
            )
        })?;
        let schema =
            inline(&declared, &dir, registry, 0).map_err(|m| fail(&format!("/{role}"), m))?;
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
                let mut e = fail(&kp, format!("unknown step field `{key}`"));
                e.action = Some(match crate::names::nearest(key, STEP.iter().copied()) {
                    Some(field) => format!("change `{key}` to `{field}`"),
                    None => format!("remove `{key}`; a step's fields are {}", STEP.join(", ")),
                });
                return Err(e);
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
                        let near = crate::names::nearest(d, steps.keys().map(String::as_str));
                        let mut e = fail(
                            &dp,
                            format!(
                                "`after` names `{d}`, which is not a step here{}",
                                near.map(|n| format!("; did you mean {n}?"))
                                    .unwrap_or_default()
                            ),
                        );
                        e.action = Some(match near {
                            Some(n) => format!("change `{d}` in `after` to `{n}`"),
                            None => format!(
                                "name steps of this workflow in `after`: {}",
                                steps
                                    .keys()
                                    .map(String::as_str)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        });
                        return Err(e);
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

    // Every step's type, as seen by later steps, and its kind, for the origin
    // of what it outputs.
    let mut step_schemas: BTreeMap<String, Value> = BTreeMap::new();
    let mut kinds: BTreeMap<String, String> = BTreeMap::new();
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
                let near = crate::names::nearest(atom_name, atoms.keys().map(String::as_str));
                let hint = near
                    .map(|n| format!("; did you mean {n}?"))
                    .unwrap_or_default();
                let mut e = fail(&ap, format!("no atom named `{atom_name}`{hint}"));
                e.action = Some(match near {
                    Some(n) => format!("change `atom: {atom_name}` to `atom: {n}`"),
                    None if atoms.is_empty() => {
                        format!("add an atom named `{atom_name}` under atoms/")
                    }
                    None => format!(
                        "name one of the project's atoms: {}, or add one named `{atom_name}`",
                        atoms
                            .keys()
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
                return Err(e);
            };
            let target = atom
                .program
                .contracts
                .get("Input")
                .cloned()
                .ok_or_else(|| {
                    fail(
                        &ap,
                        format!("atom `{atom_name}` has no compiled input contract"),
                    )
                })?;
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
            // Each mapped field's origin, when the workflow lowers it; the
            // interpreter checks it against the origin its sources allow.
            match s.get("origins") {
                None => {}
                Some(Value::Object(origins)) => {
                    for (field, origin) in origins {
                        let op = format!("{sp}/origins/{field}");
                        if !with.contains_key(field) {
                            return Err(fail(
                                &op,
                                format!("`origins` names `{field}`, which `with` does not map"),
                            ));
                        }
                        if !matches!(origin.as_str(), Some("external" | "agent" | "team")) {
                            return Err(fail(
                                &op,
                                "an origin is `external`, `agent` or `team`".into(),
                            ));
                        }
                    }
                }
                Some(_) => {
                    return Err(fail(
                        &format!("{sp}/origins"),
                        "`origins` maps input fields to `external`, `agent` or `team`".into(),
                    ));
                }
            }
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
                        let (message, action) = mismatch(id, atom_name, field, source, &why, steps);
                        return Err(WorkflowError {
                            action: Some(action),
                            ..fail(&wp, message)
                        });
                    }
                    Fit::Unknown => {
                        warnings.push(format!("{wp}: could not prove `{source}` fits `{field}`"))
                    }
                }
            }
            // Each binding's origin: what its sources allow, lowered if the
            // step says so, and no less trusted than the atom accepts.
            for (field, e) in &with {
                let wp = format!("{sp}/with/{field}");
                let source = e.as_str().unwrap_or_default();
                let allowed = binding_origin(source, &kinds).map_err(|m| fail(&wp, m))?;
                let origin = match s
                    .get("origins")
                    .and_then(|o| o.get(field))
                    .and_then(Value::as_str)
                {
                    Some(declared) if trust_rank(declared) > trust_rank(allowed) => {
                        return Err(fail(
                            &format!("{sp}/origins/{field}"),
                            format!(
                                "`origins` marks `{field}` as {declared}, but `{source}` is {allowed} data at most; \
                                 a binding may lower an origin, never raise it"
                            ),
                        ));
                    }
                    Some(declared) => declared,
                    None => allowed,
                };
                if let Some(refused) = untrusted_binding(atom, source, origin) {
                    return Err(WorkflowError {
                        action: Some(format!(
                            "bind it to {} instead, or declare a lower `trust` on the atom if it \
                             is built for {origin} input",
                            trusted_sources(accepted_trust(atom))
                        )),
                        ..fail(&wp, refused)
                    });
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
            let out = atom
                .program
                .contracts
                .get("Output")
                .cloned()
                .ok_or_else(|| {
                    fail(
                        &ap,
                        format!("atom `{atom_name}` has no compiled output contract"),
                    )
                })?;
            step_schemas.insert(
                id.clone(),
                json!({"type": "object", "properties": {"output": out, "metadata": {"type": "object"}}, "additionalProperties": false}),
            );
            kinds.insert(id.clone(), atom.kind.clone());
        } else if let Some(gate) = s.get("gate") {
            kinds.insert(id.clone(), "gate".into());
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
            // An approval gate has a `title`; a question gate an `ask` and
            // the `answer` it takes.
            let question = match (g.get("title"), g.get("ask"), g.get("answer")) {
                (Some(_), None, None) => None,
                (None, Some(_), Some(answer)) => {
                    Some(gate_answer(answer).map_err(|e| fail(&format!("{gp}/answer"), e))?)
                }
                (None, Some(_), None) => {
                    return Err(fail(&gp, "a question gate needs an `answer`".into()));
                }
                (Some(_), Some(_), _) => {
                    return Err(fail(
                        &gp,
                        "a gate has a `title` (approve or reject) or an `ask` (a question), \
                         not both"
                            .into(),
                    ));
                }
                (_, None, Some(_)) => {
                    return Err(fail(
                        &gp,
                        "`answer` belongs to a question gate's `ask`".into(),
                    ));
                }
                (None, None, _) => {
                    return Err(fail(
                        &gp,
                        "a gate needs a `title` (a CEL expression), or an `ask` and an `answer` \
                         for a question"
                            .into(),
                    ));
                }
            };
            if g.get("remember").is_some_and(|r| !r.is_boolean()) {
                return Err(fail(
                    &format!("{gp}/remember"),
                    "`remember` is true or false".into(),
                ));
            }
            if g.contains_key("remember") && question.is_none() {
                return Err(fail(
                    &format!("{gp}/remember"),
                    "`remember` belongs to a question gate".into(),
                ));
            }
            for key in ["title", "ask", "show"] {
                let kp = format!("{gp}/{key}");
                match g.get(key) {
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
            if let Some(t) = g.get("timeout") {
                t.as_str()
                    .ok_or_else(|| "a duration in a string, such as `24h`".to_owned())
                    .and_then(parse_timeout)
                    .map_err(|e| fail(&format!("{gp}/timeout"), e))?;
            }
            if let Some(a) = g.get("approvers") {
                let groups = a.as_array().filter(|a| a.iter().all(Value::is_string));
                if groups.is_none() {
                    return Err(fail(
                        &format!("{gp}/approvers"),
                        "`approvers` is a list of group names".into(),
                    ));
                }
            }
            let tp = format!("{gp}/on_timeout");
            match (g.get("on_timeout"), &question) {
                (None, _) => {}
                (Some(t), None) if t.as_str() == Some("reject") => {}
                (Some(_), None) => {
                    return Err(fail(
                        &tp,
                        "`on_timeout` is `reject`: an expired gate never approves".into(),
                    ));
                }
                (Some(t), Some(_)) if t.as_str() == Some("reject") => {}
                (Some(t), Some(answer)) => {
                    let default = t
                        .as_object()
                        .filter(|o| o.len() == 1)
                        .and_then(|o| o.get("default"))
                        .ok_or_else(|| {
                            fail(
                                &tp,
                                "a question gate's `on_timeout` is `reject` or \
                                 `{ default: <answer> }`"
                                    .into(),
                            )
                        })?;
                    if !g.contains_key("timeout") {
                        return Err(fail(
                            &tp,
                            "a default answer takes effect when the `timeout` passes: give the \
                             gate a `timeout`"
                                .into(),
                        ));
                    }
                    let fits = jsonschema::validator_for(answer)
                        .map(|v| v.is_valid(default))
                        .unwrap_or(false);
                    if !fits {
                        return Err(fail(
                            &format!("{tp}/default"),
                            format!("the default {default} is not an answer `answer` allows"),
                        ));
                    }
                }
            }
            let decision = match &question {
                None => json!({"type": "object"}),
                Some(answer) => json!({
                    "type": "object",
                    "properties": {"answer": answer},
                }),
            };
            step_schemas.insert(
                id.clone(),
                json!({"type": "object", "properties": {"decision": decision}, "additionalProperties": false}),
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
        budget,
        definition: doc.value.clone(),
        input,
        output,
        hash: format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(text.as_bytes())
        ),
        warnings,
    })
}

/// Steps in an order respecting `after`, or a step on a cycle.
/// A question gate's `answer` as the JSON Schema an answer must fit: a list
/// of choices is shorthand for an `enum` of them; a mapping is a schema.
pub fn gate_answer(answer: &Value) -> Result<Value, String> {
    let schema = match answer {
        Value::Array(choices) if !choices.is_empty() => json!({"enum": choices}),
        Value::Object(_) => answer.clone(),
        _ => {
            return Err("`answer` is a list of choices or a JSON Schema for a free answer".into());
        }
    };
    jsonschema::validator_for(&schema).map_err(|e| format!("`answer` is not a schema: {e}"))?;
    Ok(schema)
}

/// A gate's `timeout`: a whole number and a unit, `s`, `m`, `h` or `d`,
/// such as `90s`, `30m`, `24h` or `7d`.
pub fn parse_timeout(text: &str) -> Result<std::time::Duration, String> {
    let bad = || format!("`{text}` is not a duration such as `30m`, `24h` or `7d`");
    let unit = text.chars().last().ok_or_else(bad)?;
    let scale: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        _ => return Err(bad()),
    };
    let digits = &text[..text.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let count: u64 = digits.parse().map_err(|_| bad())?;
    let seconds = count.checked_mul(scale).ok_or_else(bad)?;
    if seconds == 0 {
        return Err(format!(
            "`{text}` is no time at all; omit `timeout` for no deadline"
        ));
    }
    Ok(std::time::Duration::from_secs(seconds))
}

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

/// Replace `$ref`s with what they point at, so a schema can be embedded
/// elsewhere. File references resolve relative to `dir`; registered ids
/// through the registry. Recursion past a depth becomes an open schema.
pub(crate) fn inline(
    schema: &Value,
    dir: &Path,
    registry: &mut Registry,
    _depth: usize,
) -> Result<Value, String> {
    // Relative references resolve against `dir`, which may itself be relative.
    let dir = if dir.is_absolute() {
        dir.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(dir)
    };
    registry
        .inline_value(schema, &dir.join(".atomic-inline.schema.json"))
        .map_err(|e| e.to_string())
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

/// What a mapping whose value cannot fit its field says: both ends, named
/// (the step or workflow input it reads, and the step it feeds), and what to
/// change.
fn mismatch(
    to: &str,
    to_atom: &str,
    field: &str,
    source: &str,
    why: &str,
    steps: &Map<String, Value>,
) -> (String, String) {
    let mut parts = source.trim().split('.');
    let from = match (parts.next(), parts.next()) {
        (Some("steps"), Some(from)) => {
            let atom = steps
                .get(from)
                .and_then(|s| s.get("atom"))
                .and_then(Value::as_str);
            Some((from.to_owned(), atom.map(str::to_owned)))
        }
        _ => None,
    };
    let origin = match &from {
        Some((from, _)) => format!("step `{from}`'s output"),
        None => "the workflow's input".to_owned(),
    };
    let message = format!(
        "step `{to}` maps `{source}`, from {origin}, to its input `{field}`, but \
         `{source}` cannot fit `{field}`: {why}"
    );
    let producer = match &from {
        Some((from, Some(atom))) => format!("the output schema of atom `{atom}` (step `{from}`)"),
        Some((from, None)) => format!("what step `{from}` returns"),
        None => "the workflow's input schema".to_owned(),
    };
    let action = format!(
        "make the two agree: change `with.{field}` in step `{to}` to an expression whose \
         value fits `{field}`, or change {producer} or the input schema of atom `{to_atom}` \
         (step `{to}`)"
    );
    (message, action)
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
