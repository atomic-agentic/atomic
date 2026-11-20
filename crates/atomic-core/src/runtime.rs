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
//! Every run goes through the execution backend its runtime names (see
//! [`atomic_exec`]): it opens a workspace there before anything is sent, a
//! Codex turn runs its shell and file work where the workspace says, an exec
//! atom's command runs through the backend, and the workspace is closed when
//! the run ends, whether it succeeded or not.
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
use crate::usage::{MeterError, Metering};
use atomic_schema::artifact::{ArtifactLookup, ArtifactRef};
use atomic_schema::lower::{self, OPENAI_STRICT};
use atomic_schema::registry::Registry;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

use atomic_exec::{ExecutionBackend, Open, Shell, Workspace};

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
    /// What atoms' commands are kept out of: atomic's private state, for
    /// every runtime atoms run on (see [`codex::StateGuard`]).
    pub guard: codex::StateGuard,
    /// The working directory threads run in.
    pub cwd: PathBuf,
    /// The model endpoint.
    pub endpoint: ModelEndpoint,
    /// The project's default run settings.
    pub defaults: AtomConfig,
    /// The artifact store outputs' references are checked against.
    pub artifacts: Artifacts,
    /// What admits and settles each turn (see [`crate::usage`]).
    pub meter: Metering,
    /// The execution backend each run opens its workspace on, from
    /// [`cwd`](Self::cwd), and runs its turn's shell and file work through.
    pub backend: atomic_exec::Backend,
    /// The build caches a worktree step's turn shares with the project's
    /// other worktree steps: its commands are pointed at them, and may
    /// write them under `workspace-write`. `None` outside worktree steps.
    pub caches: Option<crate::worktree::Caches>,
}

/// The runtimes atoms run on, one for each kind of atom that calls a model.
/// A declarative atom runs on the one its `kind` names.
#[derive(Clone, Debug)]
pub struct Runtimes {
    /// For `kind: codex`.
    pub codex: CodexRuntime,
    /// For `kind: lite`.
    pub lite: LiteRuntime,
    /// Where `kind: exec` commands run.
    pub exec: atomic_exec::Backend,
    /// The durable step's recorded clock, for the atom's helper calls (see
    /// [`crate::astro::StepClock`]). Without one, each operation's helpers
    /// see the current time and a fresh seed.
    pub clock: Option<crate::astro::StepClock>,
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

    fn execution(&self) -> &atomic_exec::Backend {
        match self {
            Self::Codex(rt) => &rt.backend,
            Self::Lite(rt) => &rt.backend,
        }
    }

    /// What a run asks its backend for: a Codex atom's workspace starts
    /// from the runtime's directory, with the environment the atom declares,
    /// and a lite atom's has no files.
    fn open(&self, request: &Request<'_>) -> Open {
        let source = match self {
            Self::Codex(rt) => Some(rt.cwd.clone()),
            Self::Lite(_) => None,
        };
        let (mut env, mut writable_roots) = (std::collections::BTreeMap::new(), Vec::new());
        // A worktree step's build tools find the shared caches, and its
        // rustup proxies the person's toolchains; what the atom declares
        // wins.
        if let Self::Codex(rt) = self
            && let Some(caches) = &rt.caches
        {
            env.extend(caches.env());
            if let Some(toolchains) = crate::worktree::rustup_home(&|name| std::env::var(name).ok())
            {
                env.insert("RUSTUP_HOME".into(), toolchains.display().to_string());
            }
            if request.sandbox == crate::typed::Sandbox::WorkspaceWrite {
                writable_roots.push(caches.dir().to_owned());
            }
        }
        env.extend(request.env.clone());
        Open {
            label: request.name.to_owned(),
            run: request.place.run.clone(),
            step: request.place.step.clone(),
            attempt: new_attempt(),
            source,
            sandbox: request.sandbox,
            writable_roots,
            env,
        }
    }
}

/// Which run and step an atom's run belongs to, for its workspace.
#[derive(Clone, Debug)]
pub(crate) struct Place {
    run: String,
    step: String,
}

impl Place {
    /// A run atomic knows by its run and step.
    pub(crate) fn of(run: &str, step: &str) -> Self {
        Self {
            run: run.to_owned(),
            step: step.to_owned(),
        }
    }

    /// A typed atom run outside any durable run's record: a run of its own,
    /// so no other run's attempt is taken for an earlier one of it.
    fn unrecorded(name: &str) -> Self {
        Self {
            run: format!("unrecorded-{}", new_attempt()),
            step: name.to_owned(),
        }
    }
}

/// A fresh identity for one attempt.
fn new_attempt() -> String {
    let mut bytes = [0u8; 8];
    // With no randomness, the time still tells one attempt from another.
    if getrandom::fill(&mut bytes).is_err() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        bytes = (now as u64).to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The first wait after a backend reports it has no room, doubled after each
/// refusal up to [`OPEN_WAIT_MAX`].
const OPEN_WAIT_FIRST: std::time::Duration = std::time::Duration::from_millis(200);
/// The longest wait between tries.
const OPEN_WAIT_MAX: std::time::Duration = std::time::Duration::from_secs(30);
/// How often a run still waiting says so.
const OPEN_WAIT_REPORT: std::time::Duration = std::time::Duration::from_secs(600);

/// Open a workspace on `backend`, on the blocking pool. While the backend
/// reports [`atomic_exec::ExecError::Unavailable`] (no room, or a sandbox
/// that failed in a way that may pass, such as a full disk; nothing was
/// started) the run waits, trying again with a growing pause, for as long
/// as it takes: it records nothing and spends none of the durable engine's
/// recovery attempts, and it says on stderr that it is waiting, then again
/// every ten minutes. Stopping the run stops the wait.
pub(crate) async fn open_workspace(
    backend: Arc<dyn ExecutionBackend>,
    request: Open,
) -> Result<WorkspaceGuard, AtomError> {
    let label = request.label.clone();
    let mut wait = OPEN_WAIT_FIRST;
    let mut reported: Option<std::time::Instant> = None;
    loop {
        let (b, r) = (backend.clone(), request.clone());
        let opened = tokio::task::spawn_blocking(move || b.open(&r))
            .await
            .map_err(|e| AtomError::Runtime(format!("{label}: open its workspace: {e}")))?;
        match opened {
            Ok(workspace) => {
                if reported.is_some() {
                    crate::log_line!(
                        "atomic: {label}: the execution backend can open its workspace again"
                    );
                }
                return Ok(WorkspaceGuard(Some(Arc::new(Closer {
                    backend,
                    workspace: Some(workspace),
                }))));
            }
            Err(e @ atomic_exec::ExecError::Unavailable { .. }) => {
                if reported.is_none_or(|at| at.elapsed() >= OPEN_WAIT_REPORT) {
                    crate::log_line!(
                        "atomic: {label} is waiting for the execution backend \
                         (run {}, step {}): {e}",
                        request.run,
                        request.step
                    );
                    reported = Some(std::time::Instant::now());
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(OPEN_WAIT_MAX);
            }
            Err(atomic_exec::ExecError::Unconfinable { message }) => {
                return Err(AtomError::Unconfinable(message));
            }
            Err(e) => return Err(AtomError::Runtime(format!("{label}: {e}"))),
        }
    }
}

/// An open workspace, closed however the run that holds it ends: by
/// [`close`](Self::close) when it finishes, or when the guard is dropped,
/// as it is when the run panics or is cancelled. A command running in it
/// holds it open too ([`hold`](Self::hold)), so a cancelled run's workspace
/// closes only once its command has ended.
///
/// A run's outcome is settled before its workspace is closed, so a close
/// that fails changes nothing about the run; it is reported on stderr, and
/// a backend reclaims what it left when it next connects.
pub struct WorkspaceGuard(Option<Arc<Closer>>);

/// Closes its workspace when the last holder lets go.
pub(crate) struct Closer {
    backend: Arc<dyn ExecutionBackend>,
    workspace: Option<Workspace>,
}

impl WorkspaceGuard {
    /// The workspace.
    pub(crate) fn workspace(&self) -> &Workspace {
        // Present until the guard closes it.
        self.0
            .as_ref()
            .and_then(|closer| closer.workspace.as_ref())
            .expect("an open workspace")
    }

    /// Keep the workspace open while the holder lives, such as a command
    /// running in it.
    pub(crate) fn hold(&self) -> Arc<Closer> {
        self.0.clone().expect("an open workspace")
    }

    /// Close the workspace, on the blocking pool, unless something still
    /// holds it, which then closes it when it lets go.
    pub(crate) async fn close(mut self) {
        if let Some(closer) = self.0.take()
            && let Ok(closer) = Arc::try_unwrap(closer)
        {
            let _ = tokio::task::spawn_blocking(move || closer.close_now()).await;
        }
    }
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        drop(self.0.take());
    }
}

impl Closer {
    fn close_now(mut self) {
        if let Some(workspace) = self.workspace.take()
            && let Err(e) = self.backend.close(&workspace)
        {
            crate::log_line!(
                "atomic: could not close a workspace, which its backend reclaims later: {e}"
            );
        }
    }
}

impl Drop for Closer {
    fn drop(&mut self) {
        // Already closed by `close_now`, which takes the workspace.
        let Some(workspace) = self.workspace.take() else {
            return;
        };
        let closer = Closer {
            backend: self.backend.clone(),
            workspace: Some(workspace),
        };
        // Off the executor when there is one; here, when there is not.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(move || closer.close_now());
            }
            Err(_) => closer.close_now(),
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
    place: Place,
    /// The environment the atom declares for its commands.
    env: std::collections::BTreeMap<String, String>,
}

/// Run `request`, with one repair turn.
async fn run_contract(
    backend: Backend<'_>,
    request: Request<'_>,
    check: &AsyncCheck<'_>,
) -> Result<(Value, RunMetadata), AtomError> {
    let opening = backend.open(&request);
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
    let mut spec = TurnSpec {
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
    // Nothing is sent before the backend has opened the run's workspace,
    // and the workspace is closed however the run ends.
    let workspace = open_workspace(backend.execution().shared(), opening).await?;
    let conversation = match backend {
        Backend::Codex(rt) => match turn_in(workspace.workspace(), &mut spec) {
            Ok(cwd) => {
                codex::converse_async(
                    &rt.paths,
                    &rt.codex_home,
                    &cwd,
                    &rt.endpoint,
                    &rt.meter,
                    &rt.guard,
                    &spec,
                    next,
                )
                .await
            }
            Err(e) => {
                workspace.close().await;
                return Err(e);
            }
        },
        Backend::Lite(rt) => lite::converse_async(rt, &spec, next).await,
    };
    workspace.close().await;
    let (_, report) = conversation.map_err(|e| match crate::usage::meter_failure(&e) {
        // The meter refused the next call: nothing was sent for it.
        Some(MeterError::Exhausted(refusal)) => AtomError::Budget(refusal.to_string()),
        Some(failure) => AtomError::Runtime(failure.to_string()),
        None => AtomError::Model(format!("{e:#}")),
    })?;
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

/// Set `spec` up to run in `workspace` and return the directory its thread
/// runs in: the sandbox, the writable roots and the environment are the
/// workspace's, and its shell must be one this process can drive. Its
/// commands' `HOME`, `ZDOTDIR` and `TMPDIR` are the workspace's scratch
/// directory, so no shell reads the developer's startup files and nothing
/// is left in a shared temporary directory; a `workspace-write` turn may
/// write there.
fn turn_in(workspace: &Workspace, spec: &mut TurnSpec) -> Result<PathBuf, AtomError> {
    let cwd = workspace
        .require_root()
        .map_err(|e| AtomError::Runtime(e.to_string()))?
        .clone();
    match workspace.shell {
        Shell::Local => {}
        Shell::None => {
            return Err(AtomError::Runtime(format!(
                "the backend gave {}'s turn no shell, and a codex atom's turn needs one",
                workspace.label
            )));
        }
    }
    spec.sandbox = workspace.sandbox;
    spec.writable_roots = workspace.writable_roots.clone();
    spec.env = workspace.env.clone();
    let private = |name: &str, dir: &Option<PathBuf>, spec: &mut TurnSpec| {
        if let Some(dir) = dir {
            spec.env.insert(name.to_owned(), dir.display().to_string());
            if workspace.sandbox == crate::typed::Sandbox::WorkspaceWrite
                && !spec.writable_roots.contains(dir)
            {
                spec.writable_roots.push(dir.clone());
            }
        }
    };
    private("HOME", &workspace.scratch, spec);
    private("ZDOTDIR", &workspace.scratch, spec);
    // Shared by every turn on the backend: Codex's sandbox keeps its
    // registry of protected mount points in TMPDIR, and turns that share a
    // writable root must share that registry.
    private("TMPDIR", &workspace.tmp, spec);
    Ok(cwd)
}

/// Run `spec` as one turn on a fresh thread of `runtime`, in a workspace its
/// backend opens from the runtime's directory with the spec's sandbox,
/// writable roots and environment, and return the final message. For tools
/// and tests that need a bare turn; atoms run through [`run_codex_atom`] and
/// [`run_declarative`].
pub async fn run_turn(runtime: &CodexRuntime, spec: &TurnSpec) -> Result<String, AtomError> {
    let place = Place::unrecorded("turn");
    let opening = Open {
        label: "turn".into(),
        run: place.run,
        step: place.step,
        attempt: new_attempt(),
        source: Some(runtime.cwd.clone()),
        sandbox: spec.sandbox,
        writable_roots: spec.writable_roots.clone(),
        env: spec.env.clone(),
    };
    let workspace = open_workspace(runtime.backend.shared(), opening).await?;
    let mut spec = spec.clone();
    let answer = match turn_in(workspace.workspace(), &mut spec) {
        Ok(cwd) => codex::converse_async(
            &runtime.paths,
            &runtime.codex_home,
            &cwd,
            &runtime.endpoint,
            &runtime.meter,
            &runtime.guard,
            &spec,
            |_| std::future::ready(None),
        )
        .await
        .map(|(answer, _)| answer)
        .map_err(|e| AtomError::Model(format!("{e:#}"))),
        Err(e) => Err(e),
    };
    workspace.close().await;
    answer
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
    // A contract written in code is never inlined or checked at load: refuse
    // one whose artifact references could not be checked before any model
    // call, rather than at every answer.
    atomic_schema::artifact::check_contract(&contract).map_err(|e| {
        AtomError::Runtime(format!(
            "the output schema of {} cannot be checked for artifact references: {e}",
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
        place: Place::unrecorded(A::NAME),
        env: Default::default(),
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
            origins: None,
            step: None,
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
        // Outside a workflow, the atom's own name stands in for the step.
        let identity = EffectIdentity {
            run_id: run.run_id.to_owned(),
            step_id: run.step.unwrap_or(&atom.name).to_owned(),
        };
        return run_exec(
            atom,
            compiled,
            input,
            &identity,
            runtimes.exec.shared(),
            runtimes.clock.as_ref(),
            // The store the model runtimes check references against.
            &runtimes.codex.artifacts,
        )
        .await;
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
    let render_origins = run.origins.cloned();
    let render_snapshot = snapshot.cloned();
    let render_step = step_id.map(str::to_owned);
    let render_clock = runtimes.clock.clone();
    let rendered = crate::astro::blocking(move || {
        render::render_clocked(
            &render_program,
            &RunInput {
                run_id: &render_run_id,
                input: &render_input,
                frontmatter: &render_frontmatter,
                origin: &render_origin,
                origins: render_origins.as_ref(),
                step: None,
            },
            render_snapshot.as_ref(),
            render_step.as_deref(),
            render_clock.as_ref(),
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

    // Each validation round is its own operation within the step's clock.
    let rounds = std::sync::atomic::AtomicUsize::new(0);
    let check = |output: Value| -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), AtomError>> + Send>,
    > {
        let program = compiled.program.clone();
        let round = rounds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let clock = runtimes
            .clock
            .clone()
            .map(|clock| (clock, format!("validate:{round}")));
        let values = serde_json::json!({"values":{"input":input,"output":output},"options":{"contextSnapshot":snapshot,"stepId":step_id}});
        let has_checks = atom
            .frontmatter
            .get("validate")
            .and_then(Value::as_array)
            .is_some_and(|checks| !checks.is_empty());
        Box::pin(async move { validate_output(&program, has_checks, values, clock).await })
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
        // Outside a workflow, the atom's own name stands in for the step.
        place: Place::of(run.run_id, run.step.unwrap_or(&atom.name)),
        env: declared_env(atom),
    };
    run_contract(backend, request, &check).await
}

/// The names atomic sets in an exec command's environment, which an atom's
/// `env` may not declare.
pub const EFFECT_VARIABLES: [&str; 3] = ["ATOMIC_RUN_ID", "ATOMIC_STEP_ID", "ATOMIC_EFFECT_KEY"];

/// Who an exec command acts for: its run and step. The effect key is the
/// same on every attempt of that step and differs for any other step or run,
/// so a command that writes can make a retried write recognisable, by
/// sending it as an idempotency key or putting it in what it writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectIdentity {
    /// The run.
    pub run_id: String,
    /// The step within it.
    pub step_id: String,
}

impl EffectIdentity {
    /// SHA-256 of the run and step, in hex.
    pub fn key(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"atomic-effect-v1\0");
        digest.update(self.run_id.as_bytes());
        digest.update([0]);
        digest.update(self.step_id.as_bytes());
        format!("{:x}", digest.finalize())
    }

    fn environment(&self) -> [(String, String); 3] {
        [
            ("ATOMIC_RUN_ID".into(), self.run_id.clone()),
            ("ATOMIC_STEP_ID".into(), self.step_id.clone()),
            ("ATOMIC_EFFECT_KEY".into(), self.key()),
        ]
    }
}

/// How long an exec atom's command may run before it is stopped.
pub const EXEC_TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);
/// The most an exec atom's command may write to stdout, in bytes.
pub const EXEC_OUTPUT_LIMIT: usize = 8_388_608;

/// Run an exec atom: its command, with no model and no shell, on `backend`.
///
/// The command runs in a workspace the backend opens from the project's
/// directory, with exactly the environment the atom declares, plus `HOME`
/// and `TMPDIR` pointing at the workspace's scratch directory, which is the
/// attempt's own. It reads the input as one JSON value on stdin and
/// writes one JSON value to stdout; stderr is for diagnostics. A nonzero
/// exit fails the step, and so does output that breaks the contract, names
/// an artifact `artifacts` does not hold as the contract requires, or fails
/// the atom's validators: there is no model to repair it. The validators'
/// helpers follow `clock`, the step's recorded clock, when there is one.
pub async fn run_exec(
    atom: &AtomFile,
    compiled: &Compiled,
    input: &Value,
    identity: &EffectIdentity,
    backend: Arc<dyn atomic_exec::ExecutionBackend>,
    clock: Option<&crate::astro::StepClock>,
    artifacts: &Artifacts,
) -> Result<(Value, RunMetadata), AtomError> {
    let workspace = open_exec_workspace(atom, identity, backend).await?;
    run_exec_opened(atom, compiled, input, identity, workspace, clock, artifacts).await
}

/// Open the workspace an exec atom's command will run in, as [`run_exec`]
/// does. A workflow opens it before it records the step's attempt, so an
/// attempt whose workspace never opened never counts as one whose command
/// may have run.
pub async fn open_exec_workspace(
    atom: &AtomFile,
    identity: &EffectIdentity,
    backend: Arc<dyn atomic_exec::ExecutionBackend>,
) -> Result<WorkspaceGuard, AtomError> {
    let declared = declared_env(atom);
    open_workspace(
        backend,
        Open {
            label: atom.name.clone(),
            run: identity.run_id.clone(),
            step: identity.step_id.clone(),
            attempt: new_attempt(),
            source: Some(atom.program.root.clone()),
            env: declared,
            ..Open::default()
        },
    )
    .await
}

/// The environment an atom declares in its `env`, by name.
fn declared_env(atom: &AtomFile) -> std::collections::BTreeMap<String, String> {
    atom.frontmatter["env"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
        .collect()
}

/// [`run_exec`] in a workspace [`open_exec_workspace`] opened, which it
/// closes when the command has ended.
pub async fn run_exec_opened(
    atom: &AtomFile,
    compiled: &Compiled,
    input: &Value,
    identity: &EffectIdentity,
    workspace: WorkspaceGuard,
    clock: Option<&crate::astro::StepClock>,
    artifacts: &Artifacts,
) -> Result<(Value, RunMetadata), AtomError> {
    if compiled.hash != atom.program.hash() {
        return Err(AtomError::Runtime("compiled atom version mismatch".into()));
    }
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
    let stdin = serde_json::to_vec(input).map_err(|e| AtomError::Runtime(e.to_string()))?;
    let mut h = Sha256::new();
    for part in std::iter::once(&program).chain(&args) {
        h.update(part.as_bytes());
        h.update([0]);
    }
    h.update(&stdin);
    let prompt_hash = format!("{:x}", h.finalize());
    let done = run_command(
        &workspace,
        program.clone(),
        args,
        stdin,
        identity.environment(),
    )
    .await;
    workspace.close().await;
    let done = done.map_err(|e| {
        let message = format!("{}: {e}", atom.name);
        match e {
            CommandError::Exec(e) if e.outcome_unknown() => AtomError::OutcomeUnknown(message),
            _ => AtomError::Runtime(message),
        }
    })?;
    if !done.success() {
        let stderr = String::from_utf8_lossy(&done.stderr);
        // Redacted, then cut: a secret cut in half is never in the tail.
        let tail = crate::redact::tail(stderr.trim(), 2000);
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
    // As for a model's answer: a reference must name an artifact that exists,
    // of the right type.
    atomic_schema::artifact::check_refs(contract("Output")?, &output, artifacts.0.as_ref())
        .map_err(|e| schema_error(e.to_string()))?;
    atomic_schema::expr::convertible(&output, contract("Output")?).map_err(schema_error)?;
    let has_checks = atom
        .frontmatter
        .get("validate")
        .and_then(Value::as_array)
        .is_some_and(|checks| !checks.is_empty());
    let values = serde_json::json!({"values":{"input":input,"output":output}});
    // One validation round, the same operation on every attempt.
    let clock = clock.map(|clock| (clock.clone(), "validate:0".to_owned()));
    validate_output(&compiled.program, has_checks, values, clock).await?;
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

/// Run an exec atom's command in `workspace`: in its root, with its
/// environment, `HOME` and `TMPDIR` at its scratch directory, and the step's
/// effect identity (`effect`), which nothing the atom declares can replace.
/// The command runs on the blocking pool, never on an executor thread.
async fn run_command(
    guard: &WorkspaceGuard,
    program: String,
    args: Vec<String>,
    stdin: Vec<u8>,
    effect: [(String, String); 3],
) -> Result<atomic_exec::Completed, CommandError> {
    let workspace = guard.workspace();
    let cwd = workspace.require_root()?.clone();
    let scratch = workspace.require_scratch()?.display().to_string();
    let mut env: Vec<(String, String)> = workspace
        .env
        .iter()
        .filter(|(k, _)| *k != "HOME" && *k != "TMPDIR" && !EFFECT_VARIABLES.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.push(("HOME".into(), scratch.clone()));
    env.push(("TMPDIR".into(), scratch));
    env.extend(effect);
    let request = atomic_exec::Command {
        program,
        args,
        cwd,
        env,
        stdin,
        time_limit: EXEC_TIME_LIMIT,
        output_limit: EXEC_OUTPUT_LIMIT,
        // A confining backend lets it write its scratch directory only.
        writable: workspace.scratch.iter().cloned().collect(),
    };
    // The command holds its workspace open until it ends, even when the run
    // that started it is cancelled and drops its guard.
    let held = guard.hold();
    tokio::task::spawn_blocking(move || {
        let done = held.backend.run(&request);
        drop(held);
        done
    })
    .await
    .map_err(|e| CommandError::Join(e.to_string()))?
    .map_err(CommandError::Exec)
}

/// Why [`run_command`] has no result.
#[derive(Debug, thiserror::Error)]
enum CommandError {
    /// The backend's error, which says whether the command may have run.
    #[error(transparent)]
    Exec(#[from] atomic_exec::ExecError),
    /// The blocking task running it failed.
    #[error("{0}")]
    Join(String),
}

async fn validate_output(
    program: &crate::astro::Program,
    has_checks: bool,
    values: Value,
    clock: Option<(crate::astro::StepClock, String)>,
) -> Result<(), AtomError> {
    if !has_checks {
        return Ok(());
    }
    let program = program.clone();
    let result = crate::astro::blocking(move || {
        program
            .invoke_clocked(
                "validate",
                values,
                clock
                    .as_ref()
                    .map(|(clock, operation)| (clock, operation.as_str())),
            )
            .map_err(|e| e.to_string())
    })
    .await;
    validation_result(result)
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
    // The atom's task runs inside the caller's span, for the run and step
    // the caller runs, so its model calls' spans are children of the step's.
    let caller = crate::usage::caller();
    let held = crate::spans::hold(tracing::Span::current());
    let task = held.run(crate::usage::with_caller(caller, run));
    drop(held);
    match tokio::spawn(task).await {
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
    // Every later CEL expression reads the value as its contract types it, so
    // a number that conversion refuses is refused here, where it can be
    // repaired, and is never checkpointed.
    atomic_schema::expr::convertible(&value, contract).map_err(schema_error)?;
    match check(&value) {
        // A deserialization failure in the typed check is about this answer.
        Err(AtomError::Schema { message, .. }) => Err(schema_error(message)),
        Err(e) => Err(e),
        Ok(()) => Ok(value),
    }
}

#[cfg(test)]
mod tests;
