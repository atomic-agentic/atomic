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
//! and [`run_declarative`] runs a loaded `.atom.md`.

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
use std::path::{Path, PathBuf};
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
    check: &Check<'_>,
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

    let mut accepted = None;
    let mut failure = None;
    let mut repairs_left = 1;
    let artifacts = backend.artifacts();
    let next = |answer: &str| match accept(
        answer,
        &lowered,
        request.validator,
        request.contract,
        artifacts,
        check,
    ) {
        Ok(v) => {
            accepted = Some(v);
            None
        }
        Err(e @ (AtomError::Schema { .. } | AtomError::Validation(_))) if repairs_left > 0 => {
            repairs_left -= 1;
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
            failure = Some(e);
            None
        }
    };
    let (_, report) = match backend {
        Backend::Codex(rt) => {
            codex::converse(
                &rt.paths,
                &rt.codex_home,
                &rt.cwd,
                &rt.endpoint,
                &spec,
                next,
            )
            .await
        }
        Backend::Lite(rt) => lite::converse(rt, &spec, next).await,
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
    let (value, metadata) = run_contract(backend, request, &check).await?;
    let out = serde_json::from_value(value)
        .map_err(|e| AtomError::Runtime(format!("deserialize the accepted output: {e}")))?;
    Ok((out, metadata))
}

/// Run a loaded `.atom.md` on `input`, on the runtime its `kind` names.
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
    let backend = match atom.kind.as_str() {
        "codex" => Backend::Codex(&runtimes.codex),
        "lite" => Backend::Lite(&runtimes.lite),
        other => {
            return Err(AtomError::Runtime(format!(
                "{} is a {other} atom, which calls no model",
                atom.name
            )));
        }
    };
    let input_validator = registry
        .validator(&atom.input)
        .map_err(|e| AtomError::Runtime(e.to_string()))?;
    check_input(&input_validator, input)?;
    let rendered = render::render(
        compiled,
        &RunInput {
            run_id,
            input,
            frontmatter: &atom.frontmatter,
            origin,
        },
    )
    .map_err(|e| AtomError::Runtime(e.to_string()))?;
    let registered = registry
        .get(&atom.output)
        .map(|e| e.schema.clone())
        .ok_or_else(|| AtomError::Runtime(format!("{}: no output schema registered", atom.name)))?;
    let validator = registry
        .validator(&atom.output)
        .map_err(|e| AtomError::Runtime(e.to_string()))?;
    // The generation profile cannot follow file references: inline them.
    let dir = atom.file.parent().unwrap_or(Path::new("")).to_owned();
    let contract =
        crate::workflow::inline(&registered, &dir, registry, 0).map_err(AtomError::Runtime)?;

    let rules: Vec<String> = atom
        .frontmatter
        .get("validate")
        .and_then(Value::as_array)
        .map(|r| {
            r.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let check = |output: &Value| -> Result<(), AtomError> {
        for rule in &rules {
            let program = cel::Program::compile(rule)
                .map_err(|e| AtomError::Runtime(format!("validate rule {rule}: {e}")))?;
            let mut context = cel::Context::default();
            context
                .add_variable("input", input)
                .and_then(|()| context.add_variable("output", output))
                .map_err(|e| AtomError::Runtime(e.to_string()))?;
            match program.execute(&context) {
                Ok(cel::Value::Bool(true)) => {}
                Ok(other) => return Err(AtomError::Validation(format!("`{rule}` is {other:?}"))),
                Err(e) => return Err(AtomError::Validation(format!("`{rule}`: {e}"))),
            }
        }
        Ok(())
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
