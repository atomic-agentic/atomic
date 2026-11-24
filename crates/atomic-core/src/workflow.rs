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
use serde::{Deserialize, Serialize};
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
/// it declares none. A `trust` that is not one of [`TRUST`], as an atom a
/// run recorded before the load refused one might hold, is an error that
/// carries the value as written: such an atom accepts nothing.
pub fn accepted_trust(atom: &Atom) -> Result<&'static str, String> {
    match atom.frontmatter.get("trust") {
        None => Ok("external"),
        Some(Value::String(s)) => TRUST
            .iter()
            .find(|t| **t == s.as_str())
            .copied()
            .ok_or_else(|| format!("'{s}'")),
        Some(other) => Err(other.to_string()),
    }
}

/// Why `atom` accepts no input at all, if its `trust` is not one of
/// [`TRUST`] (see [`accepted_trust`]).
pub fn unknown_trust(atom: &Atom) -> Option<String> {
    accepted_trust(atom).err().map(|written| {
        format!(
            "atom `{}` declares `trust: {written}`, which is not external, agent or team, so \
             it accepts no input",
            atom.name
        )
    })
}

/// What to change for an atom refused by [`unknown_trust`].
pub fn unknown_trust_advice(atom: &Atom) -> String {
    format!(
        "set atom `{}`'s `trust` to external, agent or team",
        atom.name
    )
}

/// Why `source`, which is `origin` data, may not bind an input of `atom`, if
/// it may not: an atom takes no input less trusted than it accepts (see
/// [`accepted_trust`]). A workflow's step and a direct run of the atom are
/// held to this alike.
pub fn untrusted_binding(atom: &Atom, source: &str, origin: &str) -> Option<String> {
    // An unknown origin ranks as external, the least trusted; an unknown
    // trust accepts nothing rather than everything.
    let accepts = match accepted_trust(atom) {
        Ok(accepts) => accepts,
        Err(_) => return unknown_trust(atom),
    };
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
pub(crate) const STEP: &[&str] = &[
    "atom", "gate", "after", "with", "origins", "worktree", "coding", "review", "criteria",
];
/// Maximum number of criteria on one step. Every executable one consumes a
/// bounded runner call before review.
pub const MAX_CRITERIA: usize = 16;
/// One criterion in an operator-authored workflow. Imported issue prose is
/// never parsed into this type or executed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Criterion {
    /// Prose for the reviewer and operator; it does not execute.
    Text {
        /// Bounded prose, shown as data but never executed.
        text: String,
    },
    /// Direct argv in the confined ExecutionBackend, with no shell wrapper.
    Test {
        /// Direct program and arguments, with no shell interpolation.
        argv: Vec<String>,
        /// Wall-clock limit, from 1 to 300,000 milliseconds.
        timeout_ms: u64,
    },
    /// A CEL boolean over the typed candidate and visible workflow scope.
    Cel {
        /// Boolean expression over the typed candidate and visible scope.
        expr: String,
    },
    /// A bounded GET/HEAD probe through the runner, restricted to one host.
    Http {
        /// `GET` or `HEAD`.
        method: String,
        /// Absolute HTTP or HTTPS URL, without embedded credentials.
        url: String,
        /// The one exact host allowed through the runner's network policy.
        allowed_host: String,
        /// Required response status.
        status: u16,
        /// Wall-clock limit, from 1 to 300,000 milliseconds.
        timeout_ms: u64,
    },
}

/// Parse and bound a workflow step's criteria before anything may run.
pub fn parse_criteria(value: &Value) -> Result<Vec<Criterion>, String> {
    let entries = value.as_array().ok_or("`criteria` is a list")?;
    if entries.len() > MAX_CRITERIA {
        return Err(format!("`criteria` allows at most {MAX_CRITERIA} entries"));
    }
    let mut out = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let criterion: Criterion = serde_json::from_value(entry.clone())
            .map_err(|error| format!("criteria[{index}]: {error}"))?;
        match &criterion {
            Criterion::Text { text } => {
                if text.trim().is_empty() || text.len() > 8192 {
                    return Err(format!("criteria[{index}]: text must have 1..8192 bytes"));
                }
            }
            Criterion::Test { argv, timeout_ms } => {
                if argv.is_empty()
                    || argv.len() > 32
                    || argv
                        .iter()
                        .any(|arg| arg.is_empty() || arg.len() > 4096 || arg.contains('\0'))
                    || argv.iter().map(String::len).sum::<usize>() > 16_384
                {
                    return Err(format!(
                        "criteria[{index}]: test argv must have 1..32 bounded, nonempty arguments"
                    ));
                }
                let program = std::path::Path::new(&argv[0])
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                if [
                    "sh",
                    "bash",
                    "dash",
                    "zsh",
                    "fish",
                    "cmd",
                    "powershell",
                    "pwsh",
                ]
                .contains(&program)
                {
                    return Err(format!(
                        "criteria[{index}]: a directly named shell is refused; test argv runs without an implicit shell"
                    ));
                }
                check_timeout(*timeout_ms, index)?;
            }
            Criterion::Cel { expr } => {
                if expr.trim().is_empty() || expr.len() > 4096 {
                    return Err(format!(
                        "criteria[{index}]: CEL expression must have 1..4096 bytes"
                    ));
                }
                expr::parse(expr).map_err(|error| format!("criteria[{index}]: {error}"))?;
            }
            Criterion::Http {
                method,
                url,
                allowed_host,
                status,
                timeout_ms,
            } => {
                if !matches!(method.as_str(), "GET" | "HEAD") {
                    return Err(format!(
                        "criteria[{index}]: HTTP method must be GET or HEAD"
                    ));
                }
                if url.len() > 4096
                    || allowed_host.is_empty()
                    || allowed_host.len() > 253
                    || allowed_host.contains('*')
                {
                    return Err(format!(
                        "criteria[{index}]: HTTP URL or allowed host is invalid"
                    ));
                }
                let parsed = url::Url::parse(url)
                    .map_err(|error| format!("criteria[{index}]: invalid HTTP URL: {error}"))?;
                if !matches!(parsed.scheme(), "http" | "https")
                    || parsed.host_str() != Some(allowed_host.as_str())
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || parsed.fragment().is_some()
                {
                    return Err(format!(
                        "criteria[{index}]: HTTP URL must name exactly the allowed host with no credentials or fragment"
                    ));
                }
                if !(100..=599).contains(status) {
                    return Err(format!("criteria[{index}]: HTTP status must be 100..599"));
                }
                check_timeout(*timeout_ms, index)?;
            }
        }
        out.push(criterion);
    }
    Ok(out)
}

fn check_timeout(timeout_ms: u64, index: usize) -> Result<(), String> {
    if !(1..=300_000).contains(&timeout_ms) {
        return Err(format!("criteria[{index}]: timeout_ms must be 1..300000"));
    }
    Ok(())
}
/// The fields of an atom step's `review`.
pub(crate) const REVIEW: &[&str] = &[
    "reviewer",
    "reviewers",
    "require",
    "with",
    "rounds",
    "self_review",
    "trial_merge",
];
/// The most repair rounds a review may give its author.
pub const MAX_REVIEW_ROUNDS: u64 = 5;
/// The verdicts a reviewer may give, as atomic's examples name them:
/// `approve` accepts the output; `request_changes` and `revise` ask the
/// author for a repair round; `reject`, `escalate` and
/// `insufficient_context` leave the decision to a person.
pub const VERDICTS: [&str; 6] = [
    "approve",
    "request_changes",
    "revise",
    "reject",
    "escalate",
    "insufficient_context",
];

/// What a reviewer's verdict asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The output is accepted.
    Approve,
    /// The author gets a repair round, if any are left.
    Changes,
    /// A person decides.
    Person,
}

impl Verdict {
    /// What `verdict` asks for; one atomic does not know is for a person.
    pub fn of(verdict: &str) -> Self {
        match verdict {
            "approve" => Self::Approve,
            "request_changes" | "revise" => Self::Changes,
            _ => Self::Person,
        }
    }
}
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
const LATER_STEP: &[&str] = &["foreach", "concurrency", "merge_queue", "retry"];

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
        // atomic names a step's durable operations after it (`<id>.turn`,
        // `<id>.review.<n>`, `<id>.repair.<n>`): a dot would let one
        // step's names be another's.
        if id.contains('.') {
            return Err(fail(
                &sp,
                format!(
                    "step `{id}` has a dot in its name; name steps with letters, digits, `-` \
                     and `_`"
                ),
            ));
        }
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
            (None, Some(_)) if s.contains_key("review") => {
                return Err(fail(
                    &format!("{sp}/review"),
                    "`review` belongs to an atom step: a gate is decided by a person".into(),
                ));
            }
            (None, Some(_)) if s.contains_key("criteria") => {
                return Err(fail(
                    &format!("{sp}/criteria"),
                    "`criteria` belongs to an atom step: a human gate is decided by a person"
                        .into(),
                ));
            }
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
                    // The step's own `origins` entry may be what lowered it.
                    let lowered = if origin != allowed {
                        format!(", remove the `origins` entry that marks it {origin}")
                    } else {
                        String::new()
                    };
                    let action = match accepted_trust(atom) {
                        Ok(accepts) => format!(
                            "bind it to {} instead{lowered}, or declare a lower `trust` on the \
                             atom if it is built for {origin} input",
                            trusted_sources(accepts)
                        ),
                        Err(_) => unknown_trust_advice(atom),
                    };
                    return Err(WorkflowError {
                        action: Some(action),
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
            if let Some(coding) = s.get("coding") {
                if !coding.is_boolean() {
                    return Err(fail(
                        &format!("{sp}/coding"),
                        "`coding` is true or false".into(),
                    ));
                }
                if coding.as_bool() == Some(true)
                    && (s.get("worktree") != Some(&Value::Bool(true))
                        || s.get("review")
                            .and_then(|review| review.get("self_review"))
                            .is_none())
                {
                    return Err(fail(
                        &format!("{sp}/coding"),
                        "a coding step needs a worktree and self-review evidence before independent review".into(),
                    ));
                }
                let coding_without_trial = coding.as_bool() == Some(true)
                    && s.get("review")
                        .and_then(|review| review.get("trial_merge"))
                        .is_none();
                if coding_without_trial {
                    // CONTROL_OFF: coding-review-needs-trial-merge-at-load
                    return Err(fail(
                        &format!("{sp}/coding"),
                        "a coding review needs a trial merge and checks before reviewers run"
                            .into(),
                    ));
                }
                if coding.as_bool() == Some(true)
                    && s.get("review")
                        .and_then(|review| review.get("with"))
                        .and_then(|with| with.get("diff"))
                        .and_then(Value::as_str)
                        != Some("candidate.metadata.diff")
                {
                    return Err(fail(
                        &format!("{sp}/coding"),
                        "a coding review must bind `diff` to the host's `candidate.metadata.diff` beside self-review evidence".into(),
                    ));
                }
                if coding.as_bool() == Some(true)
                    && s.get("review")
                        .and_then(|review| review.get("with"))
                        .and_then(Value::as_object)
                        .is_some_and(|with| with.len() != 1)
                {
                    return Err(fail(
                        &format!("{sp}/coding"),
                        "a coding reviewer receives the host diff and self-review evidence, without author reasoning".into(),
                    ));
                }
                if coding.as_bool() == Some(true)
                    && s.get("review")
                        .and_then(|review| review.get("self_review"))
                        .and_then(|self_review| self_review.get("with"))
                        .and_then(|with| with.get("diff"))
                        .and_then(Value::as_str)
                        != Some("candidate.metadata.diff")
                {
                    return Err(fail(
                        &format!("{sp}/coding"),
                        "a coding self-review must read the host's `candidate.metadata.diff`"
                            .into(),
                    ));
                }
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
            if let Some(criteria) = s.get("criteria") {
                let parsed =
                    parse_criteria(criteria).map_err(|why| fail(&format!("{sp}/criteria"), why))?;
                let candidate = candidate_schema(
                    &out,
                    s.get("worktree").and_then(Value::as_bool) == Some(true),
                    s.get("coding").and_then(Value::as_bool) == Some(true),
                );
                let criterion_scope = Scope::new()
                    .with("input", &input)
                    .with("steps", &steps_schema)
                    .with("candidate", &candidate);
                for (index, criterion) in parsed.iter().enumerate() {
                    if let Criterion::Cel { expr } = criterion {
                        expr::parse(expr)
                            .and_then(|parsed| parsed.check_boolean(&criterion_scope))
                            .map_err(|error| {
                                fail(&format!("{sp}/criteria/{index}/expr"), error.to_string())
                            })?;
                    }
                }
            }
            if let Some(review) = s.get("review") {
                check_review(
                    review,
                    &format!("{sp}/review"),
                    Reviewed {
                        name: atom_name,
                        atom,
                        output: &out,
                        worktree: s.get("worktree").and_then(Value::as_bool) == Some(true),
                        coding: s.get("coding").and_then(Value::as_bool) == Some(true),
                    },
                    atoms,
                    &scope,
                    &Roots {
                        input: &input,
                        steps: &steps_schema,
                    },
                    &kinds,
                    &mut warnings,
                    &fail,
                )?;
            }
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

/// The step a review is on: its atom, by name, the schema of its output and
/// whether it runs in a worktree.
#[derive(Clone, Copy)]
struct Reviewed<'a> {
    name: &'a str,
    atom: &'a Atom,
    output: &'a Value,
    worktree: bool,
    coding: bool,
}

/// The schemas a step's expressions read from.
struct Roots<'a> {
    input: &'a Value,
    steps: &'a Value,
}

/// The schema of what a reviewer sees as `candidate`: the output under
/// review, for a worktree step its `metadata.commit`, and for a coding step
/// the host-computed `metadata.diff`.
pub fn candidate_schema(output: &Value, worktree: bool, coding: bool) -> Value {
    let mut properties = Map::new();
    properties.insert("output".into(), output.clone());
    if worktree {
        let mut metadata = Map::new();
        metadata.insert("commit".into(), json!({"type": "string"}));
        if coding {
            metadata.insert("diff".into(), json!({"type": "string"}));
        }
        properties.insert(
            "metadata".into(),
            json!({"type": "object", "properties": metadata}),
        );
    }
    json!({"type": "object", "properties": properties, "additionalProperties": false})
}

/// The origin a review binding's `source` carries: what it reads of the
/// step's scope, as [`binding_origin`] has it, and anything of `candidate`
/// as external, since the candidate is the author's output on data that may
/// itself be external.
pub fn review_origin(
    source: &str,
    kinds: &BTreeMap<String, String>,
) -> Result<&'static str, String> {
    binding_origin(source, kinds)
}

/// Check an atom step's `review` (at `rp`): a lite or codex reviewer, not
/// the author, whose output is a verdict; a `with` that maps the reviewer's
/// input, typed and no less trusted than the reviewer accepts, from what
/// the step sees and the `candidate` under review; and how many repair
/// rounds the author gets, none for an author that accepts more trusted
/// input than a reviewer's findings are.
#[allow(clippy::too_many_arguments)]
fn check_review(
    review: &Value,
    rp: &str,
    reviewed: Reviewed<'_>,
    atoms: &BTreeMap<String, Atom>,
    scope: &Scope,
    roots: &Roots<'_>,
    kinds: &BTreeMap<String, String>,
    warnings: &mut Vec<String>,
    fail: &dyn Fn(&str, String) -> WorkflowError,
) -> Result<(), WorkflowError> {
    let author_name = reviewed.name;
    let Value::Object(r) = review else {
        return Err(fail(
            rp,
            "`review` is a mapping: `reviewer`, `with` and `rounds`".into(),
        ));
    };
    for key in r.keys() {
        if !REVIEW.contains(&key.as_str()) {
            return Err(fail(
                &format!("{rp}/{key}"),
                format!(
                    "unknown review field `{key}`; a review's fields are {}",
                    REVIEW.join(", ")
                ),
            ));
        }
    }
    if let Some(panel) = r.get("reviewers") {
        if !reviewed.coding || r.contains_key("reviewer") {
            return Err(fail(
                &format!("{rp}/reviewers"),
                "`reviewers` is for a coding step and cannot be combined with `reviewer`".into(),
            ));
        }
        if r.get("require").and_then(Value::as_str) != Some("all") {
            return Err(fail(
                &format!("{rp}/require"),
                "a fixed reviewer panel requires `require: all`".into(),
            ));
        }
        let names = panel
            .as_array()
            .filter(|names| names.len() == 2)
            .ok_or_else(|| {
                fail(
                    &format!("{rp}/reviewers"),
                    "`reviewers` names exactly two reviewer atoms".into(),
                )
            })?;
        let mut seen = std::collections::BTreeSet::new();
        for (index, entry) in names.iter().enumerate() {
            let name = entry
                .as_str()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    fail(
                        &format!("{rp}/reviewers/{index}"),
                        "a reviewer is an atom name".into(),
                    )
                })?;
            if !seen.insert(name) {
                return Err(fail(
                    &format!("{rp}/reviewers/{index}"),
                    format!("reviewer `{name}` appears twice"),
                ));
            }
            let mut single = r.clone();
            single.remove("reviewers");
            single.remove("require");
            single.insert("reviewer".into(), Value::String(name.to_owned()));
            check_review(
                &Value::Object(single),
                rp,
                reviewed,
                atoms,
                scope,
                roots,
                kinds,
                warnings,
                fail,
            )?;
        }
        return Ok(());
    }
    if r.contains_key("require") {
        return Err(fail(
            &format!("{rp}/require"),
            "`require` is only used with a reviewer panel".into(),
        ));
    }
    if reviewed.atom.kind == "exec" {
        return Err(fail(
            rp,
            format!(
                "`{author_name}` is an exec atom, and a review's repair rounds run the author \
                 again with the review, which a command cannot read; review a model atom"
            ),
        ));
    }
    if let Some(trial) = r.get("trial_merge") {
        if !cfg!(target_os = "linux") {
            return Err(fail(
                &format!("{rp}/trial_merge"),
                "trial-merge checks need a Linux execution sandbox".into(),
            ));
        }
        if !reviewed.coding || !reviewed.worktree {
            return Err(fail(
                &format!("{rp}/trial_merge"),
                "a trial merge requires a coding step in a worktree".into(),
            ));
        }
        let Some(spec) = trial.as_object() else {
            return Err(fail(
                &format!("{rp}/trial_merge"),
                "`trial_merge` maps `main` and `checks`".into(),
            ));
        };
        if spec
            .keys()
            .any(|key| !["main", "checks"].contains(&key.as_str()))
        {
            return Err(fail(
                &format!("{rp}/trial_merge"),
                "unknown trial-merge field".into(),
            ));
        }
        let main = spec.get("main").and_then(Value::as_str).ok_or_else(|| {
            fail(
                &format!("{rp}/trial_merge/main"),
                "`main` names a branch".into(),
            )
        })?;
        if !main.starts_with("refs/heads/") || !crate::git::is_ref_name(main) {
            return Err(fail(
                &format!("{rp}/trial_merge/main"),
                "`main` is a full `refs/heads/` branch name".into(),
            ));
        }
        let Some(checks) = spec
            .get("checks")
            .and_then(Value::as_array)
            .filter(|v| !v.is_empty())
        else {
            return Err(fail(
                &format!("{rp}/trial_merge/checks"),
                "`checks` is a nonempty list of command argument lists".into(),
            ));
        };
        for (index, check) in checks.iter().enumerate() {
            let Some(args) = check.as_array().filter(|args| !args.is_empty()) else {
                return Err(fail(
                    &format!("{rp}/trial_merge/checks/{index}"),
                    "a check is a nonempty command argument list".into(),
                ));
            };
            if args.len() > 32
                || args.iter().any(|arg| {
                    arg.as_str().is_none_or(|s| {
                        s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control)
                    })
                })
            {
                return Err(fail(
                    &format!("{rp}/trial_merge/checks/{index}"),
                    "check arguments must be nonempty plain text, at most 32 arguments of 4096 bytes".into(),
                ));
            }
        }
    }
    let name = r
        .get("reviewer")
        .and_then(Value::as_str)
        .ok_or_else(|| fail(&format!("{rp}/reviewer"), "`reviewer` names an atom".into()))?;
    if name == author_name {
        return Err(fail(
            &format!("{rp}/reviewer"),
            format!("`{name}` would review its own output; name another atom as the reviewer"),
        ));
    }
    let reviewer = atoms.get(name).ok_or_else(|| {
        let near = crate::names::nearest(name, atoms.keys().map(String::as_str));
        fail(
            &format!("{rp}/reviewer"),
            format!(
                "no atom named `{name}`{}",
                near.map(|n| format!("; did you mean {n}?"))
                    .unwrap_or_default()
            ),
        )
    })?;
    if !matches!(reviewer.kind.as_str(), "lite" | "codex") {
        return Err(fail(
            &format!("{rp}/reviewer"),
            format!(
                "`{name}` is a {} atom; a reviewer is a lite or codex atom",
                reviewer.kind
            ),
        ));
    }
    let verdict = reviewer.program.contracts.get("Output").ok_or_else(|| {
        fail(
            &format!("{rp}/reviewer"),
            format!("atom `{name}` has no compiled output contract"),
        )
    })?;
    is_verdict(verdict).map_err(|why| {
        fail(
            &format!("{rp}/reviewer"),
            format!(
                "`{name}`'s output is not a verdict: {why}. A reviewer outputs `verdict`, one \
                 of {}, with `approve` among them; the rest of its output (findings, a \
                 reason) is handed to the author with a request for changes",
                VERDICTS.join(", ")
            ),
        )
    })?;
    let rounds = match r.get("rounds") {
        None => 1,
        Some(n) => n
            .as_u64()
            .filter(|n| *n <= MAX_REVIEW_ROUNDS)
            .ok_or_else(|| {
                fail(
                    &format!("{rp}/rounds"),
                    format!("`rounds` is a whole number from 0 to {MAX_REVIEW_ROUNDS}"),
                )
            })?,
    };
    // The reviewer's input, from what the step sees and the candidate.
    let candidate = candidate_schema(reviewed.output, reviewed.worktree, reviewed.coding);
    let scope = scope.clone().with("candidate", &candidate);
    let self_output = if let Some(spec) = r.get("self_review") {
        let Value::Object(spec) = spec else {
            return Err(fail(
                &format!("{rp}/self_review"),
                "`self_review` is a mapping with `atom` and `with`".into(),
            ));
        };
        for key in spec.keys() {
            if ![
                "atom",
                "with",
                "decision_points",
                "selected_controls",
                "controls_atom",
            ]
            .contains(&key.as_str())
            {
                return Err(fail(
                    &format!("{rp}/self_review/{key}"),
                    "unknown self-review field".into(),
                ));
            }
        }
        for list in ["decision_points", "selected_controls"] {
            if let Some(value) = spec.get(list) {
                let Some(entries) = value.as_array() else {
                    return Err(fail(
                        &format!("{rp}/self_review/{list}"),
                        "expected a list of names".into(),
                    ));
                };
                let mut seen = std::collections::BTreeSet::new();
                for entry in entries {
                    let Some(name) = entry.as_str().filter(|name| !name.trim().is_empty()) else {
                        return Err(fail(
                            &format!("{rp}/self_review/{list}"),
                            "each name must be nonempty text".into(),
                        ));
                    };
                    if !seen.insert(name) {
                        return Err(fail(
                            &format!("{rp}/self_review/{list}"),
                            format!("`{name}` appears twice"),
                        ));
                    }
                }
            }
        }
        let selected = spec
            .get("selected_controls")
            .and_then(Value::as_array)
            .is_some_and(|rows| !rows.is_empty());
        let controls_name = match spec.get("controls_atom") {
            None => None,
            Some(Value::String(name)) if !name.is_empty() => Some(name.as_str()),
            _ => {
                return Err(fail(
                    &format!("{rp}/self_review/controls_atom"),
                    "`controls_atom` names an exec atom".into(),
                ));
            }
        };
        if selected && controls_name.is_none() {
            return Err(fail(
                &format!("{rp}/self_review/controls_atom"),
                "selected controls need a `controls_atom` that proves each row before review"
                    .into(),
            ));
        }
        if let Some(name) = controls_name {
            if !cfg!(target_os = "linux") {
                return Err(fail(
                    &format!("{rp}/self_review/controls_atom"),
                    "controls proof requires the Linux exec sandbox".into(),
                ));
            }
            if !reviewed.coding || !selected {
                return Err(fail(
                    &format!("{rp}/self_review/controls_atom"),
                    "`controls_atom` needs a coding worktree and selected controls".into(),
                ));
            }
            let control = atoms.get(name).ok_or_else(|| {
                fail(
                    &format!("{rp}/self_review/controls_atom"),
                    format!("no atom named `{name}`"),
                )
            })?;
            if control.kind != "exec" {
                return Err(fail(
                    &format!("{rp}/self_review/controls_atom"),
                    format!("atom `{name}` must be an exec atom"),
                ));
            }
            if control.external().is_none() {
                return Err(fail(
                    &format!("{rp}/self_review/controls_atom"),
                    format!(
                        "atom `{name}` must declare an idempotent or read-only external effect for replay"
                    ),
                ));
            }
            if control
                .frontmatter
                .pointer("/network/allow")
                .and_then(Value::as_array)
                .is_some_and(|hosts| !hosts.is_empty())
            {
                return Err(fail(
                    &format!("{rp}/self_review/controls_atom"),
                    format!("atom `{name}` must declare no network hosts for control proof"),
                ));
            }
            let input = control.program.contracts.get("Input").ok_or_else(|| {
                fail(
                    &format!("{rp}/self_review/controls_atom"),
                    format!("atom `{name}` has no input contract"),
                )
            })?;
            let supplied = json!({
                "type": "object",
                "required": ["source", "commit", "bound", "rows"],
                "properties": {
                    "source": {"type": "string"},
                    "commit": {"type": "string"},
                    "bound": {"type": "object", "required": ["reference", "base"],
                        "properties": {"reference": {"type": "string"}, "base": {"type": "string"}}},
                    "rows": {"type": "array", "items": {"type": "string"}}
                }
            });
            if let Fit::No(why) = fits(Some(&supplied), input) {
                return Err(fail(
                    &format!("{rp}/self_review/controls_atom"),
                    format!("the host's controls input cannot fit `{name}`: {why}"),
                ));
            }
        }
        let self_name = spec.get("atom").and_then(Value::as_str).ok_or_else(|| {
            fail(
                &format!("{rp}/self_review/atom"),
                "`atom` names a self-review atom".into(),
            )
        })?;
        let self_atom = atoms.get(self_name).ok_or_else(|| {
            fail(
                &format!("{rp}/self_review/atom"),
                format!("no atom named `{self_name}`"),
            )
        })?;
        if !matches!(self_atom.kind.as_str(), "lite" | "codex") {
            return Err(fail(
                &format!("{rp}/self_review/atom"),
                "a self-reviewer is a lite or codex atom".into(),
            ));
        }
        let self_input = self_atom.program.contracts.get("Input").ok_or_else(|| {
            fail(
                &format!("{rp}/self_review/atom"),
                format!("atom `{self_name}` has no input contract"),
            )
        })?;
        let self_fields = self_input
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                fail(
                    &format!("{rp}/self_review/atom"),
                    format!("atom `{self_name}`'s input is not an object"),
                )
            })?;
        let self_with = match spec.get("with") {
            Some(Value::Object(with)) => with,
            _ => {
                return Err(fail(
                    &format!("{rp}/self_review/with"),
                    "`with` maps every self-review input field to a CEL expression".into(),
                ));
            }
        };
        if controls_name.is_some() {
            if self_with.contains_key("control_results") {
                return Err(fail(
                    &format!("{rp}/self_review/with/control_results"),
                    "`control_results` is supplied by the host's exec proof".into(),
                ));
            }
            let target = self_fields.get("control_results").ok_or_else(|| {
                fail(
                    &format!("{rp}/self_review/atom"),
                    format!("atom `{self_name}` needs an input field `control_results`"),
                )
            })?;
            let supplied = json!({"type": "array", "items": {"type": "object", "required": ["row", "result"],
                "properties": {"row": {"type": "string"}, "result": {"type": "string"}}}});
            if let Fit::No(why) = fits(Some(&supplied), target) {
                return Err(fail(
                    &format!("{rp}/self_review/atom"),
                    format!("host control results cannot fit `{self_name}`'s input: {why}"),
                ));
            }
        }
        let self_accepts = accepted_trust(self_atom).map_err(|_| {
            fail(
                &format!("{rp}/self_review/atom"),
                unknown_trust(self_atom).unwrap_or_default(),
            )
        })?;
        if controls_name.is_some() && trust_rank("external") < trust_rank(self_accepts) {
            return Err(fail(
                &format!("{rp}/self_review/atom"),
                format!(
                    "host control results are external data, and `{self_name}` accepts {self_accepts} input at most"
                ),
            ));
        }
        for (field, source) in self_with {
            let at = format!("{rp}/self_review/with/{field}");
            let target = self_fields.get(field).ok_or_else(|| {
                fail(
                    &at,
                    format!("atom `{self_name}` has no input field `{field}`"),
                )
            })?;
            let source = source
                .as_str()
                .ok_or_else(|| fail(&at, "a mapping is a CEL expression in a string".into()))?;
            expr::parse(source)
                .and_then(|p| p.check(&scope))
                .map_err(|e| fail(&at, e.to_string()))?;
            let from = path_schema_in(
                source,
                &[
                    ("input", roots.input),
                    ("steps", roots.steps),
                    ("candidate", &candidate),
                ],
            );
            if let Fit::No(why) = fits(from.as_ref(), target) {
                return Err(fail(
                    &at,
                    format!("`{source}` cannot fit `{self_name}`'s input field `{field}`: {why}"),
                ));
            }
            let origin = review_origin(source, kinds).map_err(|e| fail(&at, e))?;
            if trust_rank(origin) < trust_rank(self_accepts) {
                return Err(fail(
                    &at,
                    format!(
                        "`{source}` is {origin} data, and self-reviewer `{self_name}` accepts {self_accepts} input at most"
                    ),
                ));
            }
        }
        for required in self_input
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !(self_with.contains_key(required)
                || required == "control_results" && controls_name.is_some())
            {
                return Err(fail(
                    &format!("{rp}/self_review/with"),
                    format!(
                        "atom `{self_name}` requires input field `{required}`, which `with` does not bind"
                    ),
                ));
            }
        }
        Some(self_atom.program.contracts.get("Output").ok_or_else(|| {
            fail(
                &format!("{rp}/self_review/atom"),
                format!("atom `{self_name}` has no output contract"),
            )
        })?)
    } else {
        None
    };
    let target = reviewer.program.contracts.get("Input").ok_or_else(|| {
        fail(
            &format!("{rp}/reviewer"),
            format!("atom `{name}` has no compiled input contract"),
        )
    })?;
    let fields = target
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let with = match r.get("with") {
        None => Map::new(),
        Some(Value::Object(w)) => w.clone(),
        Some(_) => {
            return Err(fail(
                &format!("{rp}/with"),
                "`with` maps the reviewer's input fields to CEL expressions".into(),
            ));
        }
    };
    if self_output.is_some() && with.contains_key("self_review") {
        return Err(fail(
            &format!("{rp}/with/self_review"),
            "`self_review` is supplied by the self-review step".into(),
        ));
    }
    if reviewed.coding && self_output.is_some() && with.contains_key("host_review") {
        return Err(fail(
            &format!("{rp}/with/host_review"),
            "`host_review` is supplied by the host's reviewed inventory".into(),
        ));
    }
    if let Some(output) = self_output {
        let field = fields.get("self_review").ok_or_else(|| {
            fail(
                &format!("{rp}/reviewer"),
                format!("atom `{name}` needs an input field `self_review` for the evidence"),
            )
        })?;
        if let Fit::No(why) = fits(Some(output), field) {
            return Err(fail(
                &format!("{rp}/reviewer"),
                format!(
                    "self-review evidence cannot fit `{name}`'s input field `self_review`: {why}"
                ),
            ));
        }
    }
    if reviewed.coding && self_output.is_some() {
        let field = fields.get("host_review").ok_or_else(|| {
            fail(
                &format!("{rp}/reviewer"),
                format!("atom `{name}` needs an input field `host_review` for host inventory"),
            )
        })?;
        let supplied = json!({
            "type": "object",
            "required": ["decision_points", "selected_controls", "control_results"],
            "properties": {
                "decision_points": {"type": "array", "items": {"type": "string"}},
                "selected_controls": {"type": "array", "items": {"type": "string"}},
                "control_results": {"type": "array", "items": {"type": "object",
                    "required": ["row", "result"],
                    "properties": {"row": {"type": "string"}, "result": {"type": "string"}}}}
            }
        });
        if let Fit::No(why) = fits(Some(&supplied), field) {
            return Err(fail(
                &format!("{rp}/reviewer"),
                format!("host inventory cannot fit `{name}`'s input field `host_review`: {why}"),
            ));
        }
    }
    // An atom whose `trust` is none of the three accepts nothing: refused
    // here as any binding into it would be.
    let accepts = |atom: &Atom, at: &str| {
        accepted_trust(atom).map_err(|_| WorkflowError {
            action: Some(unknown_trust_advice(atom)),
            ..fail(at, unknown_trust(atom).unwrap_or_default())
        })
    };
    let reviewer_accepts = accepts(reviewer, &format!("{rp}/reviewer"))?;
    // What the reviewer's findings carry: a model's output on what it read.
    let mut findings = "agent";
    if self_output.is_some() {
        if trust_rank("external") < trust_rank(reviewer_accepts) {
            return Err(fail(
                &format!("{rp}/reviewer"),
                format!(
                    "self-review evidence is external data, and reviewer `{name}` accepts {reviewer_accepts} input at most"
                ),
            ));
        }
        findings = "external";
    }
    for (field, e) in &with {
        let wp = format!("{rp}/with/{field}");
        let Some(field_schema) = fields.get(field) else {
            let known: Vec<&str> = fields.keys().map(String::as_str).collect();
            return Err(fail(
                &wp,
                format!(
                    "atom `{name}` has no input field `{field}` (fields: {})",
                    known.join(", ")
                ),
            ));
        };
        let source = e
            .as_str()
            .ok_or_else(|| fail(&wp, "a mapping is a CEL expression in a string".into()))?;
        expr::parse(source)
            .and_then(|p| p.check(&scope))
            .map_err(|e| fail(&wp, e.to_string()))?;
        let from = path_schema_in(
            source,
            &[
                ("input", roots.input),
                ("steps", roots.steps),
                ("candidate", &candidate),
            ],
        );
        match fits(from.as_ref(), field_schema) {
            Fit::Yes => {}
            Fit::No(why) => {
                return Err(fail(
                    &wp,
                    format!("`{source}` cannot fit `{name}`'s input field `{field}`: {why}"),
                ));
            }
            Fit::Unknown => {
                warnings.push(format!("{wp}: could not prove `{source}` fits `{field}`"))
            }
        }
        let origin = review_origin(source, kinds).map_err(|m| fail(&wp, m))?;
        if trust_rank(origin) < trust_rank(reviewer_accepts) {
            return Err(fail(
                &wp,
                format!(
                    "`{source}` is {origin} data, and reviewer `{name}` accepts \
                     {reviewer_accepts} input at most (`trust: '{reviewer_accepts}'`); a \
                     candidate under review is external data"
                ),
            ));
        }
        if trust_rank(origin) < trust_rank(findings) {
            findings = origin;
        }
    }
    for required in target
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !(with.contains_key(required)
            || required == "self_review" && self_output.is_some()
            || required == "host_review" && reviewed.coding && self_output.is_some())
        {
            return Err(fail(
                &format!("{rp}/with"),
                format!(
                    "atom `{name}` requires input field `{required}`, which `with` does not bind"
                ),
            ));
        }
    }
    // A repair round hands the author the reviewer's findings: no more
    // trusted than what the reviewer read.
    let author_accepts = accepts(reviewed.atom, &format!("{rp}/rounds"))?;
    if rounds > 0 && trust_rank(findings) < trust_rank(author_accepts) {
        return Err(fail(
            &format!("{rp}/rounds"),
            format!(
                "a repair round hands `{author_name}` the reviewer's findings, which are \
                 {findings} data, and it accepts {author_accepts} input at most \
                 (`trust: '{author_accepts}'`); set `rounds: 0`, so a person decides on a \
                 rejection, or review it with what it can take"
            ),
        ));
    }
    Ok(())
}

/// Whether `contract` is a reviewer's verdict: an object requiring
/// `verdict`, a string among [`VERDICTS`] that may be `approve`.
pub fn is_verdict(contract: &Value) -> Result<(), String> {
    let props = contract
        .get("properties")
        .and_then(Value::as_object)
        .ok_or("it is not an object with properties")?;
    let required = contract
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|r| r.iter().any(|f| f == "verdict"));
    if !required {
        return Err("it does not require `verdict`".into());
    }
    let choices = props
        .get("verdict")
        .and_then(|v| v.get("enum"))
        .and_then(Value::as_array)
        .filter(|c| !c.is_empty())
        .ok_or("`verdict` is not an enum")?;
    for c in choices {
        if !c.as_str().is_some_and(|c| VERDICTS.contains(&c)) {
            return Err(format!("`verdict` allows {c}"));
        }
    }
    if !choices.iter().any(|c| c == "approve") {
        return Err("`verdict` cannot be `approve`".into());
    }
    Ok(())
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
    path_schema_in(source, &[("input", input), ("steps", steps)])
}

/// [`path_schema`] over the named `roots`.
fn path_schema_in(source: &str, roots: &[(&str, &Value)]) -> Option<Value> {
    let source = source.trim();
    if !source
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
    {
        return None;
    }
    let mut parts = source.split('.');
    let root = parts.next()?;
    let mut cur = roots
        .iter()
        .find(|(name, _)| *name == root)
        .map(|(_, schema)| (*schema).clone())?;
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
