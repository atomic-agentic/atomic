// SPDX-License-Identifier: Apache-2.0

//! Running atoms on Codex.
//!
//! Every atom run follows the same contract:
//!
//! 0. the input is checked against its full contract; an input that does not
//!    fit fails with [`AtomError::Input`] before anything is rendered or sent;
//! 1. the output contract is lowered to the generation profile, which becomes
//!    the turn's structured-output schema;
//! 2. the instructions are the developer instructions and the input is the
//!    user turn; a Codex atom on an endpoint other than OpenAI's API also gets
//!    the lowered schema written into its instructions, since Codex sends the
//!    schema on the Responses API, which such servers may accept and ignore;
//! 3. the final message is parsed (an answer that is exactly one fenced code
//!    block, as models that are not held to the schema often write, is read
//!    as its contents), the nulls lowering introduced are removed,
//!    and the result is validated against the **full** contract (strict mode
//!    drops constraints such as `maxLength`, so this is where they are
//!    enforced), every artifact it references is looked up in the runtime's
//!    store (it must exist, with the media type its schema names), and then
//!    the atom's own checks run;
//! 4. an answer that does not fit gets one repair turn on the same thread,
//!    quoting exactly what was wrong; if the repaired answer does not fit
//!    either, the atom fails with [`AtomError::Schema`] (or
//!    [`AtomError::Validation`] when only its own check failed).
//!
//! The normalized, validated value is what a durable step checkpoints.
//! [`run_codex_atom`] runs a typed Rust atom on a Codex thread,
//! [`run_lite_atom`] runs one as a single model call (see [`crate::lite`]),
//! and [`run_declarative`] runs a loaded `.atom.astro` on the runtime its
//! `kind` names.

use crate::atom::Atom as AtomFile;
use crate::codex::{self, CodexPaths, ModelEndpoint, TurnSpec};
use crate::compile::Compiled;
use crate::lite::{self, LiteRuntime};
use crate::render::{self, RunInput};
use crate::typed::{Atom, AtomConfig, AtomError, AtomKind};
use atomic_schema::artifact::{ArtifactLookup, ArtifactRef};
use atomic_schema::lower::{self, OPENAI_STRICT};
use atomic_schema::registry::Registry;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

/// Where a runtime looks up the artifacts an output references.
#[derive(Clone)]
pub struct Artifacts(Arc<dyn ArtifactLookup + Send + Sync>);

impl Artifacts {
    /// Look artifacts up in `lookup`, such as a [`crate::artifact::LocalStore`].
    pub fn new(lookup: impl ArtifactLookup + Send + Sync + 'static) -> Self {
        Self(Arc::new(lookup))
    }

    /// No store: every reference fails (see [`NoArtifacts`]).
    pub fn none() -> Self {
        Self::new(NoArtifacts)
    }
}

impl std::fmt::Debug for Artifacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Artifacts(..)")
    }
}

/// A lookup that knows no artifacts: an output that references one fails, so
/// a runtime with no store can never accept a reference it cannot check.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoArtifacts;

impl ArtifactLookup for NoArtifacts {
    fn media_types(&self, _: &ArtifactRef) -> Option<Vec<String>> {
        None
    }
}

/// Where and with what model atoms run.
#[derive(Clone, Debug)]
pub struct CodexRuntime {
    /// Paths for Codex's helpers.
    pub paths: CodexPaths,
    /// Codex's home for atom threads, inside the project's state directory.
    pub codex_home: PathBuf,
    /// The working directory threads run in.
    pub cwd: PathBuf,
    /// The model endpoint.
    pub endpoint: ModelEndpoint,
    /// The project's default run settings.
    pub defaults: AtomConfig,
    /// The artifact store outputs' references are checked against.
    pub artifacts: Artifacts,
}

/// The runtimes atoms run on, one for each kind of atom that calls a model.
/// A declarative atom runs on the one its `kind` names.
#[derive(Clone, Debug)]
pub struct Runtimes {
    /// For `kind: codex`.
    pub codex: CodexRuntime,
    /// For `kind: lite`.
    pub lite: LiteRuntime,
}

/// What a run records beside its output.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunMetadata {
    /// Codex's id for the thread the atom ran on; for a lite atom, the
    /// provider's id for its last response.
    pub thread_id: String,
    /// The model the turn used.
    pub model: String,
    /// Tokens used, over every turn, as the provider reported them.
    pub input_tokens: i64,
    /// Output tokens, over every turn.
    pub output_tokens: i64,
    /// Total tokens, over every turn.
    pub total_tokens: i64,
    /// Human actions taken during the turn, such as approvals. Unattended
    /// atoms decline every in-turn approval, so in v0.1 this is empty.
    pub human_actions: Vec<String>,
    /// SHA-256 over the developer instructions, the user turn and the output
    /// schema, so a run can be matched to exactly what was asked.
    pub prompt_hash: String,
    /// How many turns it took (more than one when a repair turn ran).
    pub turns: u32,
}

/// What runs an atom's turns.
#[derive(Clone, Copy)]
enum Backend<'a> {
    /// A Codex thread.
    Codex(&'a CodexRuntime),
    /// One model call per turn.
    Lite(&'a LiteRuntime),
}

impl Backend<'_> {
    fn endpoint(&self) -> &ModelEndpoint {
        match self {
            Self::Codex(rt) => &rt.endpoint,
            Self::Lite(rt) => &rt.endpoint,
        }
    }

    fn defaults(&self) -> &AtomConfig {
        match self {
            Self::Codex(rt) => &rt.defaults,
            Self::Lite(rt) => &rt.defaults,
        }
    }

    fn artifacts(&self) -> &(dyn ArtifactLookup + Send + Sync) {
        match self {
            Self::Codex(rt) => rt.artifacts.0.as_ref(),
            Self::Lite(rt) => rt.artifacts.0.as_ref(),
        }
    }
}

/// The atom's own check on a value that already fits the contract.
type Check<'a> = dyn Fn(&Value) -> Result<(), AtomError> + Send + Sync + 'a;
type AsyncCheck<'a> = dyn Fn(
        Value,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AtomError>> + Send + 'a>>
    + Send
    + Sync
    + 'a;

/// One run of the contract: what to ask, and how to check the answer.
struct Request<'a> {
    developer: String,
    prompt: String,
    model: Option<String>,
    sandbox: crate::typed::Sandbox,
    contract: &'a Value,
    validator: &'a jsonschema::Validator,
    name: &'a str,
}

/// Run `request`, with one repair turn.
async fn run_contract(
    backend: Backend<'_>,
    request: Request<'_>,
    check: &AsyncCheck<'_>,
) -> Result<(Value, RunMetadata), AtomError> {
    let lowered = lower::lower(request.contract, &OPENAI_STRICT).map_err(|e| {
        AtomError::Runtime(format!(
            "the output schema of {} cannot be lowered: {e}",
            request.name
        ))
    })?;
    let developer = match backend {
        Backend::Codex(rt) if !lite::is_openai_api(&rt.endpoint.base_url) => {
            schema_in_instructions(&request.developer, &lowered.schema)
        }
        _ => request.developer,
    };
    let spec = TurnSpec {
        developer_instructions: Some(developer),
        prompt: request.prompt,
        output_schema: Some(lowered.schema.clone()),
        model: request.model,
        sandbox: request.sandbox,
        ..TurnSpec::default()
    };
    let model = spec
        .model
        .clone()
        .unwrap_or_else(|| backend.endpoint().model.clone());
    let mut h = Sha256::new();
    for part in [
        spec.developer_instructions.as_deref().unwrap_or_default(),
        &spec.prompt,
        &lowered.schema.to_string(),
    ] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    let prompt_hash = format!("{:x}", h.finalize());

    // The answer is checked against the contract, the artifact store and the
    // atom's own validators, which may run in the compiler worker.
    let outcome = std::sync::Arc::new(std::sync::Mutex::new((None, None, 1)));
    let artifacts = backend.artifacts();
    let next = |answer: String| {
        let outcome = outcome.clone();
        let lowered = &lowered;
        let validator = request.validator;
        let contract = request.contract;
        async move {
            let result = match accept(
                &answer,
                lowered,
                validator,
                contract,
                artifacts,
                &|_| Ok(()),
            ) {
                Ok(value) => check(value.clone()).await.map(|()| value),
                Err(error) => Err(error),
            };
            let mut outcome = outcome.lock().expect("contract outcome lock");
            match result {
                Ok(value) => {
                    outcome.0 = Some(value);
                    None
                }
                Err(e @ (AtomError::Schema { .. } | AtomError::Validation(_))) if outcome.2 > 0 => {
                    outcome.2 -= 1;
                    let problem = match &e {
                        AtomError::Schema { message, .. } => message.clone(),
                        other => other.to_string(),
                    };
                    Some(TurnSpec {
                        prompt: format!(
                            "Your answer did not fit the required output: {problem}\n\
                             Reply again with only the corrected JSON, following the same schema."
                        ),
                        output_schema: Some(lowered.schema.clone()),
                        ..TurnSpec::default()
                    })
                }
                Err(e) => {
                    outcome.1 = Some(e);
                    None
                }
            }
        }
    };
    let (_, report) = match backend {
        Backend::Codex(rt) => {
            codex::converse_async(
                &rt.paths,
                &rt.codex_home,
                &rt.cwd,
                &rt.endpoint,
                &spec,
                next,
            )
            .await
        }
        Backend::Lite(rt) => lite::converse_async(rt, &spec, next).await,
    }
    .map_err(|e| AtomError::Model(format!("{e:#}")))?;
    let metadata = RunMetadata {
        thread_id: report.thread_id,
        model,
        input_tokens: report.input_tokens,
        output_tokens: report.output_tokens,
        total_tokens: report.total_tokens,
        human_actions: Vec::new(),
        prompt_hash,
        turns: report.turns,
    };
    let (accepted, failure, _) =
        std::mem::take(&mut *outcome.lock().expect("contract outcome lock"));
    match (accepted, failure) {
        (Some(v), _) => Ok((v, metadata)),
        (None, Some(e)) => Err(e),
        (None, None) => Err(AtomError::Runtime(
            "the turn ended without an answer".into(),
        )),
    }
}

/// Run a typed Codex atom on `input`, on a fresh Codex thread.
pub async fn run_codex_atom<A: Atom>(
    atom: &A,
    input: &A::In,
    runtime: &CodexRuntime,
) -> Result<(A::Out, RunMetadata), AtomError> {
    run_typed(atom, input, AtomKind::Codex, Backend::Codex(runtime)).await
}

/// Run a typed lite atom on `input`: one schema-constrained model call, and
/// one more for the repair turn when the first answer does not fit.
pub async fn run_lite_atom<A: Atom>(
    atom: &A,
    input: &A::In,
    runtime: &LiteRuntime,
) -> Result<(A::Out, RunMetadata), AtomError> {
    run_typed(atom, input, AtomKind::Lite, Backend::Lite(runtime)).await
}

async fn run_typed<A: Atom>(
    atom: &A,
    input: &A::In,
    kind: AtomKind,
    backend: Backend<'_>,
) -> Result<(A::Out, RunMetadata), AtomError> {
    if atom.kind() != kind {
        return Err(AtomError::Runtime(format!(
            "{} is a {:?} atom and cannot run as a {kind:?} atom",
            A::NAME,
            atom.kind()
        )));
    }
    let input_value = serde_json::to_value(input)
        .map_err(|e| AtomError::Runtime(format!("serialize the input: {e}")))?;
    let input_validator = Registry::new()
        .validator_for(&atom.input_schema())
        .map_err(|e| {
            AtomError::Runtime(format!("the input schema of {} is not valid: {e}", A::NAME))
        })?;
    check_input(&input_validator, &input_value)?;
    let mut config = backend.defaults().clone();
    atom.configure(&mut config);
    let contract = atom.output_schema();
    let validator = Registry::new().validator_for(&contract).map_err(|e| {
        AtomError::Runtime(format!(
            "the output schema of {} is not valid: {e}",
            A::NAME
        ))
    })?;
    let prompt = serde_json::to_string_pretty(input)
        .map_err(|e| AtomError::Runtime(format!("serialize the input: {e}")))?;
    let check = |v: &Value| -> Result<(), AtomError> {
        let out: A::Out = serde_json::from_value(v.clone()).map_err(|e| AtomError::Schema {
            message: format!("the answer does not deserialize: {e}"),
            answer: v.to_string(),
        })?;
        atom.validate(&out)
    };
    let request = Request {
        developer: atom.instructions().into_owned(),
        prompt,
        model: config.model.clone(),
        sandbox: config.sandbox,
        contract: &contract,
        validator: &validator,
        name: A::NAME,
    };
    let (value, metadata) = run_contract(backend, request, &|value| {
        Box::pin(std::future::ready(check(&value)))
    })
    .await?;
    let out = serde_json::from_value(value)
        .map_err(|e| AtomError::Runtime(format!("deserialize the accepted output: {e}")))?;
    Ok((out, metadata))
}

/// Run a loaded `.atom.astro` on `input`, on the runtime its `kind` names.
/// Returns the normalized, validated output, which is what a durable step
/// checkpoints.
pub async fn run_declarative(
    atom: &AtomFile,
    compiled: &Compiled,
    registry: &mut Registry,
    input: &Value,
    run_id: &str,
    origin: &str,
    runtimes: &Runtimes,
) -> Result<(Value, RunMetadata), AtomError> {
    run_declarative_with_context(
        atom,
        compiled,
        registry,
        &RunInput {
            run_id,
            input,
            frontmatter: &atom.frontmatter,
            origin,
        },
        None,
        runtimes,
    )
    .await
}

/// A frozen host snapshot and the current trusted dispatch step.
pub struct DispatchContext<'a> {
    /// Verified context checkpoint.
    pub snapshot: &'a crate::context::ContextSnapshot,
    /// Current step identity, assigned by the caller.
    pub step_id: &'a str,
}

/// Execute a checked Astro atom with an optional frozen public context.
pub async fn run_declarative_with_context(
    atom: &AtomFile,
    compiled: &Compiled,
    _registry: &mut Registry,
    run: &RunInput<'_>,
    context: Option<DispatchContext<'_>>,
    runtimes: &Runtimes,
) -> Result<(Value, RunMetadata), AtomError> {
    let input = run.input;
    if compiled.hash != atom.program.hash() {
        return Err(AtomError::Runtime("compiled atom version mismatch".into()));
    }
    if atom.kind == "exec" {
        return run_exec(atom, compiled, input, Arc::new(atomic_exec::LocalBackend)).await;
    }
    let backend = match atom.kind.as_str() {
        "codex" => Backend::Codex(&runtimes.codex),
        "lite" => Backend::Lite(&runtimes.lite),
        other => {
            return Err(AtomError::Runtime(format!(
                "{} is a {other} atom, which this version cannot run",
                atom.name
            )));
        }
    };
    // The compiled contract, not the registry: a run carries its atoms with
    // their contracts, and checks against what it started with.
    let input_contract = compiled.program.contracts.get("Input").ok_or_else(|| {
        AtomError::Runtime(format!("{} has no compiled input contract", atom.name))
    })?;
    let input_validator = Registry::new()
        .validator_for(input_contract)
        .map_err(|e| AtomError::Runtime(e.to_string()))?;
    check_input(&input_validator, input)?;
    let snapshot = context.as_ref().map(|context| context.snapshot);
    let step_id = context.as_ref().map(|context| context.step_id);
    let render_program = compiled.clone();
    let render_input = input.clone();
    let render_frontmatter = run.frontmatter.clone();
    let render_run_id = run.run_id.to_owned();
    let render_origin = run.origin.to_owned();
    let render_snapshot = snapshot.cloned();
    let render_step = step_id.map(str::to_owned);
    let rendered = crate::astro::blocking(move || {
        render::render_with_context(
            &render_program,
            &RunInput {
                run_id: &render_run_id,
                input: &render_input,
                frontmatter: &render_frontmatter,
                origin: &render_origin,
            },
            render_snapshot.as_ref(),
            render_step.as_deref(),
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(AtomError::Runtime)?;
    let contract = compiled.program.contracts["Output"].clone();
    // A registry with atomic's built-in schemas, so an artifact reference in
    // the contract resolves.
    let validator = Registry::new()
        .validator_for(&contract)
        .map_err(|e| AtomError::Runtime(e.to_string()))?;

    let check = |output: Value| -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), AtomError>> + Send>,
    > {
        let program = compiled.program.clone();
        let values = serde_json::json!({"values":{"input":input,"output":output},"options":{"contextSnapshot":snapshot,"stepId":step_id}});
        let has_checks = atom
            .frontmatter
            .get("validate")
            .and_then(Value::as_array)
            .is_some_and(|checks| !checks.is_empty());
        Box::pin(async move { validate_output(&program, has_checks, values).await })
    };
    // As for a typed atom: the atom's own model, then the project's default,
    // then the endpoint's.
    let model = atom
        .frontmatter
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| backend.defaults().model.clone());
    // A declarative atom is read-only unless it says otherwise; a project
    // default never widens it.
    let sandbox = match atom.frontmatter.get("sandbox").and_then(Value::as_str) {
        Some("workspace-write") => crate::typed::Sandbox::WorkspaceWrite,
        _ => crate::typed::Sandbox::ReadOnly,
    };
    let request = Request {
        developer: rendered.developer,
        prompt: rendered.user,
        model,
        sandbox,
        contract: &contract,
        validator: &validator,
        name: &atom.name,
    };
    run_contract(backend, request, &check).await
}

/// How long an exec atom's command may run before it is stopped.
pub const EXEC_TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);
/// The most an exec atom's command may write to stdout, in bytes.
pub const EXEC_OUTPUT_LIMIT: usize = 8_388_608;

/// Run an exec atom: its command, with no model and no shell, on `backend`.
///
/// The command runs in the project's directory with exactly the environment
/// the atom declares, plus `HOME` and `TMPDIR` pointing at a scratch
/// directory of its own. It reads the input as one JSON value on stdin and
/// writes one JSON value to stdout; stderr is for diagnostics. A nonzero
/// exit fails the step, and so does output that breaks the contract or the
/// atom's validators: there is no model to repair it.
pub async fn run_exec(
    atom: &AtomFile,
    compiled: &Compiled,
    input: &Value,
    backend: Arc<dyn atomic_exec::ExecutionBackend>,
) -> Result<(Value, RunMetadata), AtomError> {
    let contract = |key: &str| {
        compiled.program.contracts.get(key).ok_or_else(|| {
            AtomError::Runtime(format!("{} has no compiled {key} contract", atom.name))
        })
    };
    let input_validator = Registry::new()
        .validator_for(contract("Input")?)
        .map_err(|e| AtomError::Runtime(e.to_string()))?;
    check_input(&input_validator, input)?;
    let strings = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    let mut command = strings(&atom.frontmatter["exec"]["command"]).into_iter();
    let program = command
        .next()
        .ok_or_else(|| AtomError::Runtime(format!("{} has no exec.command", atom.name)))?;
    let args: Vec<String> = command.collect();
    let scratch =
        scratch_dir().map_err(|e| AtomError::Runtime(format!("scratch directory: {e}")))?;
    let mut env: Vec<(String, String)> = atom.frontmatter["env"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
        .collect();
    let scratch_text = scratch.display().to_string();
    env.retain(|(k, _)| k != "HOME" && k != "TMPDIR");
    env.push(("HOME".into(), scratch_text.clone()));
    env.push(("TMPDIR".into(), scratch_text));
    let stdin = serde_json::to_vec(input).map_err(|e| AtomError::Runtime(e.to_string()))?;
    let mut h = Sha256::new();
    for part in std::iter::once(&program).chain(&args) {
        h.update(part.as_bytes());
        h.update([0]);
    }
    h.update(&stdin);
    let prompt_hash = format!("{:x}", h.finalize());
    let request = atomic_exec::Command {
        program: program.clone(),
        args,
        cwd: atom.program.root.clone(),
        env,
        stdin,
        time_limit: EXEC_TIME_LIMIT,
        output_limit: EXEC_OUTPUT_LIMIT,
    };
    // The command runs on the blocking pool, never on an executor thread.
    let done = tokio::task::spawn_blocking(move || backend.run(&request))
        .await
        .map_err(|e| AtomError::Runtime(format!("{}: {e}", atom.name)));
    let _ = std::fs::remove_dir_all(&scratch);
    let done = done?.map_err(|e| AtomError::Runtime(format!("{}: {e}", atom.name)))?;
    if !done.success() {
        let stderr = String::from_utf8_lossy(&done.stderr);
        let tail: String = stderr
            .trim()
            .chars()
            .rev()
            .take(2000)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(AtomError::Runtime(format!(
            "{}: {program} {}{}",
            atom.name,
            done.code.map_or_else(
                || "was stopped by a signal".to_owned(),
                |c| format!("exited with code {c}")
            ),
            if tail.is_empty() {
                String::new()
            } else {
                format!(": {tail}")
            }
        )));
    }
    let text = String::from_utf8_lossy(&done.stdout).into_owned();
    let schema_error = |message: String| AtomError::Schema {
        message,
        answer: text.chars().take(2000).collect(),
    };
    let output: Value = serde_json::from_str(text.trim())
        .map_err(|e| schema_error(format!("stdout is not one JSON value: {e}")))?;
    let validator = Registry::new()
        .validator_for(contract("Output")?)
        .map_err(|e| AtomError::Runtime(e.to_string()))?;
    let errors: Vec<String> = validator
        .iter_errors(&output)
        .map(|e| {
            let at = e.instance_path().to_string();
            format!("{}: {e}", if at.is_empty() { "/".to_owned() } else { at })
        })
        .collect();
    if !errors.is_empty() {
        return Err(schema_error(errors.join("; ")));
    }
    let has_checks = atom
        .frontmatter
        .get("validate")
        .and_then(Value::as_array)
        .is_some_and(|checks| !checks.is_empty());
    let values = serde_json::json!({"values":{"input":input,"output":output}});
    validate_output(&compiled.program, has_checks, values).await?;
    Ok((
        output,
        RunMetadata {
            thread_id: String::new(),
            model: "exec".into(),
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            human_actions: Vec::new(),
            prompt_hash,
            turns: 0,
        },
    ))
}

/// A new, empty scratch directory for one exec attempt.
fn scratch_dir() -> std::io::Result<PathBuf> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "atomic-exec-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir)?;
    Ok(dir)
}

async fn validate_output(
    program: &crate::astro::Program,
    has_checks: bool,
    values: Value,
) -> Result<(), AtomError> {
    if !has_checks {
        return Ok(());
    }
    validation_result(program.invoke_async("validate", values).await)
}

// Only an explicit predicate rejection is repairable. Transport failures and
// malformed protocol replies say nothing about the model's answer.
fn validation_result(result: Result<Value, String>) -> Result<(), AtomError> {
    let result = result.map_err(AtomError::Runtime)?;
    match result.get("valid").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        Some(false) => Err(AtomError::Validation(rejection(&result))),
        None => Err(AtomError::Runtime(format!(
            "invalid validation response: {result}"
        ))),
    }
}

// The repair prompt quotes each rule the answer broke, so the model can fix it.
fn rejection(result: &Value) -> String {
    let failed: Vec<String> = result
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("passed").and_then(Value::as_bool) != Some(true))
        .map(|item| {
            let rule = item.get("source").and_then(Value::as_str).map_or_else(
                || "the answer's size".to_owned(),
                |rule| format!("`{rule}`"),
            );
            match item.get("error").and_then(Value::as_str) {
                Some(error) => format!("- {rule} could not be checked: {error}"),
                None => format!("- {rule} returned false"),
            }
        })
        .collect();
    if failed.is_empty() {
        return "the atom's checks rejected the answer".into();
    }
    format!("the answer broke the atom's checks:\n{}", failed.join("\n"))
}

/// Run an atom's future on its own task, so a panic inside it fails that
/// atom's run with [`AtomError::Panicked`] instead of taking the process down.
pub async fn contained<T, F>(run: F) -> Result<T, AtomError>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, AtomError>> + Send + 'static,
{
    match tokio::spawn(run).await {
        Ok(result) => result,
        Err(e) if e.is_panic() => {
            let payload = e.into_panic();
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic with no message".to_owned());
            Err(AtomError::Panicked(message))
        }
        Err(e) => Err(AtomError::Runtime(format!(
            "the atom's task was cancelled: {e}"
        ))),
    }
}

/// `developer` with the output schema stated, for an endpoint that may not
/// enforce it.
fn schema_in_instructions(developer: &str, schema: &Value) -> String {
    let schema = serde_json::to_string_pretty(schema).unwrap_or_default();
    format!(
        "{developer}\n\nYour final answer is only a JSON object, with no other text and \
         no code fence, that follows this JSON Schema:\n\n{schema}"
    )
}

/// The contents of `answer` when it is exactly one fenced code block (with or
/// without a language tag), else `answer` trimmed. Anything outside the block
/// keeps the answer as it is, so it fails to parse and gets the repair turn.
fn unfence(answer: &str) -> &str {
    let trimmed = answer.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let Some((tag, body)) = rest.split_once('\n') else {
        return trimmed;
    };
    let Some(inner) = body.trim_end().strip_suffix("```") else {
        return trimmed;
    };
    let tag_ok = tag.trim().is_empty() || tag.trim().eq_ignore_ascii_case("json");
    if !tag_ok || inner.contains("```") {
        return trimmed;
    }
    inner.trim()
}

/// Check an input against its full contract, naming every place it fails.
fn check_input(validator: &jsonschema::Validator, input: &Value) -> Result<(), AtomError> {
    let errors: Vec<String> = validator
        .iter_errors(input)
        .map(|e| {
            let at = e.instance_path().to_string();
            format!("{}: {e}", if at.is_empty() { "/".to_owned() } else { at })
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AtomError::Input(errors.join("; ")))
    }
}

/// Turn a final message into a value that fits the contract, or say exactly
/// why it does not.
fn accept(
    answer: &str,
    lowered: &lower::Lowered,
    validator: &jsonschema::Validator,
    contract: &Value,
    artifacts: &(dyn ArtifactLookup + Send + Sync),
    check: &Check<'_>,
) -> Result<Value, AtomError> {
    let schema_error = |message: String| AtomError::Schema {
        message,
        answer: answer.to_owned(),
    };
    let mut value: Value = serde_json::from_str(unfence(answer))
        .map_err(|e| schema_error(format!("the answer is not JSON: {e}")))?;
    lower::normalize(&mut value, lowered);
    let errors: Vec<String> = validator
        .iter_errors(&value)
        .map(|e| {
            let at = e.instance_path().to_string();
            format!("{}: {e}", if at.is_empty() { "/".to_owned() } else { at })
        })
        .collect();
    if !errors.is_empty() {
        return Err(schema_error(errors.join("; ")));
    }
    // A reference must name an artifact that exists, of the right type: the
    // schema can only check its spelling.
    atomic_schema::artifact::check_refs(contract, &value, artifacts)
        .map_err(|e| schema_error(e.to_string()))?;
    match check(&value) {
        // A deserialization failure in the typed check is about this answer.
        Err(AtomError::Schema { message, .. }) => Err(schema_error(message)),
        Err(e) => Err(e),
        Ok(()) => Ok(value),
    }
}

#[cfg(test)]
mod tests;
