// SPDX-License-Identifier: Apache-2.0

//! Loading the sole declarative atom format, `*.atom.astro`.
//! Frontmatter is an interpreted literal configuration; atom modules never execute.

use crate::astro::{self, Program};
use crate::budget::Budget;
use atomic_schema::registry::{self, Registry};
use atomic_schema::typescript;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The kinds of atom this version runs.
pub const KINDS: &[&str] = &["codex", "lite", "exec"];

/// The configuration fields this version reads; any other field is refused.
pub const FIELDS: &[&str] = &[
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
    "scorers",
    "examples",
    "context",
    "case",
    "judges",
];

/// Fields of the published format that later versions read; this version
/// refuses them by name.
pub const LATER: &[&str] = &[
    "extends",
    "review",
    "credentials",
    "runtime",
    "codex",
    "skills",
    "remote",
    "auth",
];

/// The typed `AtomConfig` declaration the compiler checks configuration
/// against, generated into each project as `.atomic/types/authoring.d.ts`.
pub const AUTHORING_DECLARATIONS: &str = include_str!("../../../authoring/authoring.d.ts");

/// A loaded, schema-checked Astro atom and its immutable source graph. It
/// serializes whole, sources and contracts included, so a run can carry the
/// atom it started with.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Atom {
    /// The source file.
    pub file: PathBuf,
    /// Explicit name or filename-derived identity.
    pub name: String,
    /// Declaration version, defaulting to one.
    pub version: u64,
    /// Model or execution kind.
    pub kind: String,
    /// Human-readable purpose.
    pub description: String,
    /// Registered full input schema.
    pub input: String,
    /// Registered full output schema.
    pub output: String,
    /// The limits its `budget` block writes; the rest inherit defaults (see
    /// [`crate::budget`]).
    #[serde(default)]
    pub budget: Budget,
    /// Parsed literal configuration and interpreted contract descriptors.
    pub frontmatter: Value,
    /// Immutable entry and component sources.
    pub program: Program,
    /// Full optional runtime context contract.
    pub context: Option<Value>,
    /// Full evaluation case schema.
    pub case: Value,
    /// Full judge input/output contracts keyed by the declared judge.
    pub judges: std::collections::BTreeMap<String, Value>,
    /// Generated declarations checked before model dispatch.
    pub declarations: Value,
}

/// What an exec atom declares about effects atomic cannot observe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum External {
    /// The command only reads.
    ReadOnly,
    /// It writes, and repeating it is safe.
    Idempotent,
}

impl Atom {
    /// Its `effects.external` declaration, if it makes one.
    pub fn external(&self) -> Option<External> {
        match self.frontmatter.pointer("/effects/external")?.as_str()? {
            "read-only" => Some(External::ReadOnly),
            "idempotent" => Some(External::Idempotent),
            _ => None,
        }
    }
}

/// A file-local loading diagnostic.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{}:{line}: {path}: {message}", file.display())]
pub struct AtomError {
    /// Source file.
    pub file: PathBuf,
    /// One-based source line.
    pub line: usize,
    /// Configuration or body path.
    pub path: String,
    /// Diagnostic message.
    pub message: String,
}

/// Check an exec atom's command and environment, and that no other atom
/// declares them. `exec.command` is a literal argument array, run with no
/// shell; `env` is the whole environment the command receives.
fn check_exec(kind: &str, fm: &serde_json::Map<String, Value>) -> Result<(), (String, String)> {
    let refuse = |pointer: &str, message: &str| Err((pointer.to_owned(), message.to_owned()));
    if kind != "exec" {
        for key in ["exec", "env", "effects"] {
            if fm.contains_key(key) {
                return refuse(
                    &format!("/{key}"),
                    &format!("`{key}` is for `kind: 'exec'` atoms"),
                );
            }
        }
        return Ok(());
    }
    let Some(Value::Object(exec)) = fm.get("exec") else {
        return refuse(
            "/exec",
            "an exec atom needs `exec: { command: [program, ...args] }`",
        );
    };
    if exec.contains_key("script") {
        return refuse(
            "/exec/script",
            "`exec.script` runs on a managed runtime, which this version of atomic does not provide yet; use `exec.command`",
        );
    }
    if let Some(key) = exec.keys().find(|key| *key != "command") {
        return refuse(&format!("/exec/{key}"), &format!("unknown field `{key}`"));
    }
    match exec.get("command") {
        Some(Value::Array(args))
            if !args.is_empty()
                && args
                    .iter()
                    .all(|a| a.as_str().is_some_and(|a| !a.is_empty())) => {}
        _ => {
            return refuse(
                "/exec/command",
                "`exec.command` is a nonempty list of nonempty strings: the program, then its arguments; no shell runs it",
            );
        }
    }
    match fm.get("env") {
        None => {}
        Some(Value::Object(env)) => {
            for (name, value) in env {
                let valid = !name.is_empty()
                    && !name.starts_with(|c: char| c.is_ascii_digit())
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
                if !valid {
                    return refuse(
                        &format!("/env/{name}"),
                        "an environment variable name is letters, digits and `_`",
                    );
                }
                if !value.is_string() {
                    return refuse(&format!("/env/{name}"), "an environment value is a string");
                }
            }
        }
        Some(_) => return refuse("/env", "`env` maps variable names to string values"),
    }
    match fm.get("effects") {
        None => {}
        Some(Value::Object(effects)) => {
            if let Some(key) = effects.keys().find(|key| *key != "external") {
                return refuse(
                    &format!("/effects/{key}"),
                    &format!(
                        "`effects.{key}` is not supported yet; this version reads only `effects.external`"
                    ),
                );
            }
            if let Some(external) = effects.get("external")
                && !matches!(external.as_str(), Some("read-only" | "idempotent"))
            {
                return refuse(
                    "/effects/external",
                    "`effects.external` is `read-only` (the command only reads) or `idempotent` (it writes, and repeating it is safe)",
                );
            }
        }
        Some(_) => {
            return refuse(
                "/effects",
                "`effects` is a mapping, such as `{ external: 'idempotent' }`",
            );
        }
    }
    Ok(())
}

/// Load and type-check an Astro atom, registering its full schemas.
pub fn load(file: &Path, registry: &mut Registry) -> Result<Atom, AtomError> {
    let fail = |line, path: &str, message: String| AtomError {
        file: file.to_owned(),
        line,
        path: path.to_owned(),
        message,
    };
    if !file.to_string_lossy().ends_with(".atom.astro") {
        return Err(fail(
            1,
            "",
            "declarative atoms use .atom.astro; Markdown and MDX atom files have been replaced"
                .into(),
        ));
    }
    let file = std::fs::canonicalize(file).map_err(|e| fail(1, "", e.to_string()))?;
    let fail = |line, path: &str, message: String| AtomError {
        file: file.clone(),
        line,
        path: path.to_owned(),
        message,
    };
    // A compiler diagnostic keeps the file and line the compiler found.
    let located = |path: &str, error: astro::Diagnostic| {
        let mut message = error.message;
        for detail in error.details.iter().skip(1) {
            message.push('\n');
            message.push_str(detail);
        }
        AtomError {
            file: error.file.map_or_else(|| file.clone(), PathBuf::from),
            line: error.line.unwrap_or(1),
            path: path.to_owned(),
            message,
        }
    };
    let root = astro::project_root(&file).map_err(|e| fail(1, "project", e))?;
    let request = json!({
        "action": "load",
        "file": astro::path_text(&file).map_err(|e| fail(1, "", e))?,
        "root": astro::path_text(&root).map_err(|e| fail(1, "project", e))?,
    });
    let parsed = astro::worker_located(request).map_err(|e| located("body", e))?;
    let config = parsed["config"].clone();
    let fm = config
        .as_object()
        .ok_or_else(|| fail(1, "config", "config must be an object".into()))?;
    let source = std::fs::read_to_string(&file).map_err(|e| fail(1, "", e.to_string()))?;
    let line_of = |pointer: &str| {
        let key = pointer
            .trim_start_matches('/')
            .split('/')
            .next()
            .unwrap_or("");
        source
            .lines()
            .position(|line| {
                line.contains(&format!("{key}:")) || line.contains(&format!("\"{key}\":"))
            })
            .map_or(1, |line| line + 1)
    };
    for key in fm.keys() {
        if LATER.contains(&key.as_str()) {
            return Err(fail(
                line_of(key),
                &format!("/{key}"),
                format!("`{key}` is not supported by this version of atomic yet"),
            ));
        }
        if !FIELDS.contains(&key.as_str()) {
            return Err(fail(
                line_of(key),
                &format!("/{key}"),
                format!("unknown field `{key}`"),
            ));
        }
    }
    if fm.get("atomic").and_then(Value::as_u64) != Some(crate::project::FORMAT_VERSION) {
        return Err(fail(1, "/atomic", "unsupported atom format version".into()));
    }
    let name = fm
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail(1, "/name", "name must be nonempty".into()))?
        .to_owned();
    let version = fm
        .get("version")
        .map_or(Some(1), Value::as_u64)
        .filter(|n| *n > 0)
        .ok_or_else(|| fail(1, "/version", "version must be a positive integer".into()))?;
    let kind = fm
        .get("kind")
        .and_then(Value::as_str)
        .filter(|s| KINDS.contains(s))
        .ok_or_else(|| {
            let message = if fm.get("kind").and_then(Value::as_str) == Some("a2a") {
                "kind a2a (remote atoms) is not available yet; kind must be codex, lite or exec"
            } else {
                "kind must be codex, lite or exec"
            };
            fail(line_of("/kind"), "/kind", message.into())
        })?
        .to_owned();
    let description = fm
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or(&name)
        .to_owned();
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
    check_exec(&kind, fm)
        .map_err(|(pointer, message)| fail(line_of(&pointer), &pointer, message))?;

    // A budget limit that is not written inherits the configured default
    // when the atom runs; what is written must be well formed now.
    let budget = match fm.get("budget") {
        None => Budget::default(),
        Some(b) => Budget::parse(b).map_err(|(sub, message)| {
            let pointer = format!("/budget{sub}");
            fail(line_of(&pointer), &pointer, message)
        })?,
    };

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
    let mut program = Program {
        file: file.clone(),
        root,
        contracts: Value::Null,
        source_paths: serde_json::from_value(parsed["sourcePaths"].clone())
            .map_err(|e| fail(1, "body", e.to_string()))?,
        sources: serde_json::from_value(parsed["sources"].clone())
            .map_err(|e| fail(1, "body", e.to_string()))?,
    };
    let mut schemas = serde_json::Map::new();
    for (key, id) in [("Input", &input), ("Output", &output)] {
        schemas.insert(
            key.into(),
            registry
                .inline(id)
                .map_err(|e| fail(1, key, e.to_string()))?,
        );
    }
    let output_schema = schemas["Output"].clone();
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

    let context = match fm.get("context").cloned() {
        Some(schema) => Some(schema),
        None => registry::sidecar(&file, "context")
            .map_err(|e| fail(1, "/context", e.to_string()))?
            .map(|path| {
                registry
                    .load_file(&path)
                    .map_err(|e| fail(1, "/context", e.to_string()))
            })
            .transpose()?
            .and_then(|id| registry.get(&id).map(|entry| entry.schema.clone())),
    };
    let context = context
        .map(|schema| {
            crate::workflow::inline(
                &schema,
                file.parent().unwrap_or(Path::new("/")),
                registry,
                0,
            )
            .map_err(|e| fail(1, "/context", e))
        })
        .transpose()?;
    if parsed["needsContext"].as_bool() == Some(true) && context.is_none() {
        return Err(fail(
            1,
            "/context",
            "context binding requires a declared context schema".into(),
        ));
    }
    schemas.insert(
        "Context".into(),
        context
            .clone()
            .unwrap_or(json!({"type":"object","additionalProperties":false})),
    );
    let mut schema_number = 0;
    let mut resolve_schema = |schema: &Value| -> Result<Value, AtomError> {
        schema_number += 1;
        let id = schema_id(
            &file,
            &format!("evaluation-{schema_number}"),
            schema,
            registry,
        )
        .map_err(|(path, message)| fail(1, &path, message))?;
        registry
            .inline(&id)
            .map_err(|e| fail(1, "/case", e.to_string()))
    };
    let case = fm
        .get("case")
        .map(&mut resolve_schema)
        .transpose()?
        .unwrap_or(json!(true));
    let mut judges = std::collections::BTreeMap::new();
    let mut judge_properties = serde_json::Map::new();
    if let Some(declared) = fm.get("judges") {
        for (name, judge) in declared
            .as_object()
            .ok_or_else(|| fail(1, "/judges", "judges must be a mapping".into()))?
        {
            let input = resolve_schema(
                judge
                    .get("input")
                    .ok_or_else(|| fail(1, "/judges", "judge needs input".into()))?,
            )?;
            let output = resolve_schema(
                judge
                    .get("output")
                    .ok_or_else(|| fail(1, "/judges", "judge needs output".into()))?,
            )?;
            judges.insert(name.clone(), json!({"input":input,"output":output}));
            judge_properties.insert(name.clone(), json!({"type":"object","additionalProperties":false,"required":["input","output"],"properties":{"input":input,"output":output}}));
        }
    }
    if let Some(scorers) = fm.get("scorers") {
        for scorer in scorers
            .as_object()
            .ok_or_else(|| fail(1, "/scorers", "scorers must be a mapping".into()))?
            .values()
        {
            let judge = scorer
                .get("judge")
                .and_then(Value::as_str)
                .ok_or_else(|| fail(1, "/scorers", "scorer needs a declared judge".into()))?;
            if !judges.contains_key(judge) {
                return Err(fail(1, "/scorers", format!("undeclared judge {judge}")));
            }
            if scorer
                .get("min_mean")
                .and_then(Value::as_f64)
                .is_none_or(|value| !(0.0..=1.0).contains(&value))
            {
                return Err(fail(
                    1,
                    "/scorers",
                    "min_mean must be between zero and one".into(),
                ));
            }
        }
    }
    schemas.insert("Case".into(), case.clone());
    let required: Vec<_> = judge_properties.keys().cloned().collect();
    schemas.insert("Judges".into(), json!({"type":"object","additionalProperties":false,"required":required,"properties":judge_properties}));
    program.contracts = Value::Object(schemas.clone());
    let generated = declarations_for(
        &program.root,
        &file,
        &parsed["typeImports"],
        &schemas,
        registry,
    )
    .map_err(|e| fail(1, "types", e))?;
    let declarations = program
        .invoke_located("check", json!({ "declarations": generated }))
        .map_err(|e| located("types", e))?;
    if let Some(files) = declarations.as_object() {
        for (name, source) in files {
            program.sources.insert(
                name.clone(),
                source
                    .as_str()
                    .ok_or_else(|| fail(1, "types", "invalid declaration".into()))?
                    .to_owned(),
            );
        }
    }
    Ok(Atom {
        file,
        name,
        version,
        kind,
        description,
        input,
        output,
        budget,
        frontmatter: config,
        program,
        context,
        case,
        judges,
        declarations,
    })
}

/// The declarations an atom's sources are checked against: its own contract
/// types, the authoring types, and a module for each project schema file its
/// sources import types from, with the schema files those reference. Sources
/// import `schemas/change.schema.json`'s types from
/// `.atomic/types/schemas/change`. Only imported schemas are included, so
/// editing an unrelated schema leaves the compiled atom unchanged. An import
/// with no schema file behind it is left for the type checker to report.
fn declarations_for(
    root: &Path,
    file: &Path,
    type_imports: &Value,
    schemas: &serde_json::Map<String, Value>,
    registry: &mut Registry,
) -> Result<serde_json::Map<String, Value>, String> {
    let types = root.join(".atomic/types");
    let stem = file
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".atom.astro"))
        .ok_or("atom file name")?;
    let mut out = serde_json::Map::new();
    out.insert(
        astro::path_text(&types.join(format!("{stem}.d.ts")))?.to_owned(),
        json!(typescript::atom_declarations(schemas).map_err(|e| e.to_string())?),
    );
    out.insert(
        astro::path_text(&types.join("authoring.d.ts"))?.to_owned(),
        json!(AUTHORING_DECLARATIONS),
    );
    let mut wanted = Vec::new();
    for import in type_imports
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let import = import.strip_suffix(".d.ts").unwrap_or(import);
        let Ok(relative) = Path::new(import).strip_prefix(&types) else {
            continue;
        };
        let Some(name) = relative.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        for extension in ["json", "yaml"] {
            let schema = root
                .join(relative)
                .with_file_name(format!("{name}.schema.{extension}"));
            if schema.is_file() {
                registry.load_file(&schema).map_err(|e| e.to_string())?;
                wanted.push(std::fs::canonicalize(&schema).map_err(|e| e.to_string())?);
                break;
            }
        }
    }
    if wanted.is_empty() {
        return Ok(out);
    }
    for (path, text) in
        typescript::declarations(registry, root, &wanted, true).map_err(|e| e.to_string())?
    {
        let key = astro::path_text(&path)?.to_owned();
        if out.contains_key(&key) {
            return Err(format!(
                "{} would replace the atom's generated {}; rename the schema file",
                path.display(),
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("declarations")
            ));
        }
        out.insert(key, json!(text));
    }
    Ok(out)
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
        if !r.contains('#') || r.ends_with('#') {
            return Ok(id);
        }
    }
    // An inline schema: registered under the atom's own URI, so relative
    // references resolve beside the atom.
    let uri_file = PathBuf::from(format!("{}?{role}", atom.display()));
    registry
        .register(uri_file, schema.clone())
        .map_err(|e| (String::new(), e.to_string()))
}

#[cfg(test)]
pub(crate) mod tests;
