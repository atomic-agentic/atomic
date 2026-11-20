// SPDX-License-Identifier: Apache-2.0

//! `run_pipeline`: one durable workflow that runs any workflow file.
//!
//! A workflow file is data. [`prepare`] turns a loaded and checked workflow,
//! and the atoms it names, into a [`Pipeline`] that carries them by value:
//! the steps in order, each `with` mapping and the `returns` expression as CEL
//! source, the contracts, and every atom with its sources and compiled
//! contracts. A run takes the pipeline as its input, so the durable record
//! holds exactly what the run started with, and nothing is rebuilt to run a
//! new workflow.
//!
//! The interpreter walks the steps in order. Each atom step evaluates its
//! mapping against the workflow's `input` and the published outputs of the
//! steps before it, then runs the atom through [`run_atom_at`]: its turn and its
//! publication are sibling durable steps, so a step that finished before a
//! crash is never run again. CEL has no side effects, so mappings evaluated
//! again on replay give the same values.
//!
//! A run is pinned to what it started with. The pipeline records the
//! SHA-256 of the workflow file and of every source of every atom it uses,
//! and the run's first durable step stores the whole pipeline in the artifact
//! store. Editing a file afterwards changes new runs only: a run in flight,
//! or one recovered after a crash, uses the definitions in its own record.
//!
//! A gate step opens a gate through [`atomic_durable::gate`]: a row in the
//! gate store and a durable wait for a person's decision. Its title and what
//! it shows are CEL over the steps before it; who may decide comes only from
//! the deployment `run_pipeline` is registered with and the groups the
//! workflow file names, never from the run's input or a model. Only an
//! approval lets the run go on, and the step's `decision` records it; a
//! rejected or expired gate fails the run, so no step after it runs.
//!
//! A `worktree: true` step runs a codex atom in a worktree of its own,
//! checked out at the commit its mapped `base` names. Every attempt starts
//! from that base, and atomic, not the model, commits what the atom left once
//! its turn ends; the step's metadata records the commit as `commit`, beside
//! `base` and `commit_ref`, for later steps to review. A finished run's
//! worktrees are removed; its accepted commits keep their refs.
//!
//! An exec atom's command may reach systems atomic cannot observe, so a
//! step that runs one is guarded ([`atomic_durable::attempt`]): if execution
//! stopped after the command began and before the step's checkpoint, whether
//! its effects happened is unknown. A command declared `effects.external:
//! read-only` or `idempotent` then runs again. One with neither is parked as
//! `outcome_unknown`: the step fails without running the command again, and
//! nothing retries it automatically.
//!
//! v0.1 runs its steps in order: model atoms, exec atoms, which run a
//! command with no model, and gates.

use crate::metering::{BudgetPlan, RunBudget};
use crate::workflows::propose_at;
use atomic_core::artifact::{ArtifactError, ArtifactRef, ArtifactStore, LocalStore, Provenance};
use atomic_core::astro::StepClock;
use atomic_core::atom::Atom;
use atomic_core::budget::{Budget, BudgetSettings};
use atomic_core::git::{Git, Signature};
use atomic_core::render::RunInput;
use atomic_core::runtime::{Runtimes, contained, run_declarative_with_context};
use atomic_core::workflow::Workflow;
use atomic_core::worktree::{StepError, StepRequest, Worktrees};
use atomic_core::{AtomError, compile};
use atomic_durable::attempt;
use atomic_durable::gate::{self, Deployment, GateSpec, Resolution, Tier};
use atomic_durable::usage::Ledger;
use atomic_durable::{
    DurableError, DurableRuntime, Proposal, Result, WorkflowContext, failpoint, run_atom_at,
};
use atomic_schema::expr::Shape;
use atomic_schema::registry::Registry;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// The name `run_pipeline` is registered under.
pub const PIPELINE: &str = "atomic.pipeline";

/// The layout of the durable steps `run_pipeline` records, stamped on every
/// run as its version. The engine recovers and dequeues only runs of its own
/// version, so any build of atomic with the same layout continues the same
/// runs, and one with another layout leaves them alone. Change it exactly
/// when a run started under the old layout would replay differently under
/// the new one: a step added, removed, renamed or moved, a checkpoint's or
/// the [`PipelineRun`]'s shape changed, or the conversion of recorded values
/// to CEL changed, so that a mapping over them could give another answer.
/// The pipeline tests hold the
/// golden layout of each version, under `tests/layout/`: the steps two runs
/// record, through an approved and a rejected gate, with the shape of each
/// checkpoint, and the run's own shape. They fail when the layout no longer
/// matches this version's, whose file is never rewritten.
pub const LAYOUT_VERSION: &str = "atomic.pipeline.3";

/// How many times one process recovers one run of `run_pipeline` at the
/// same step before it fails the run as dead-lettered: enough for a crash
/// or two, few enough that a step whose process keeps dying, such as an
/// idempotent command whose runner always dies, stops soon. A run that gets
/// further counts afresh, and so does each process, so restarting
/// `atomic serve` never counts against a healthy run; the engine's own cap,
/// which counts every recovery claim, stays at its default.
pub const RECOVERY_LIMIT: u32 = 5;

/// For each run this process has executed: the furthest position it has
/// reached (the pin is 0, each step one more), and how many of this
/// process's executions of the run have reached it.
static PROGRESS: std::sync::Mutex<BTreeMap<String, (usize, u32)>> =
    std::sync::Mutex::new(BTreeMap::new());

/// The runs this process has dead-lettered.
static DEAD: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Whether this process failed run `id` as dead-lettered.
pub fn dead_lettered(id: &str) -> bool {
    DEAD.lock().unwrap_or_else(|e| e.into_inner()).contains(id)
}

/// Count an execution of run `id` reaching `position`, and fail the run
/// as dead-lettered once more executions than [`RECOVERY_LIMIT`] allows
/// have reached it without getting further. Each execution reaches each
/// position once; one replaying positions short of the furthest counts
/// nothing.
fn reached(id: &str, position: usize) -> Result<()> {
    let entries = {
        let mut progress = PROGRESS.lock().unwrap_or_else(|e| e.into_inner());
        let (top, entries) = progress.entry(id.to_owned()).or_insert((position, 0));
        if position > *top {
            *top = position;
            *entries = 0;
        }
        if position == *top {
            *entries += 1;
        }
        *entries
    };
    if entries > 1 + RECOVERY_LIMIT {
        DEAD.lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_owned());
        let why = format!(
            "{DEAD_LETTERED} run {id} stopped {RECOVERY_LIMIT} times in this process at the same \
             step without finishing it, so it is not run again"
        );
        atomic_core::log_line!("{why}");
        return Err(failed(why));
    }
    Ok(())
}

/// Take back the count [`reached`] made for this execution of run `id` at
/// `position`: it stopped there for a reason outside the run, such as a
/// store held elsewhere, and must not bring the run nearer dead-lettering.
fn not_reached(id: &str, position: usize) {
    let mut progress = PROGRESS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((top, entries)) = progress.get_mut(id)
        && *top == position
    {
        *entries = entries.saturating_sub(1);
    }
}

/// Set once this process is stopping: no run starts another step.
static STOPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Start no more steps in this process: every run waits before its next
/// step until the process exits, so a step that was running may still reach
/// its checkpoint while nothing new begins. Call it when a stop begins.
pub fn stop_starting_steps() {
    STOPPING.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Before a run's next piece of work: go on counted as working, or wait for
/// good if this process is stopping.
async fn keep_working(working: &mut Option<Running>) {
    match working {
        None => *working = Some(begin_work().await),
        Some(_) if STOPPING.load(std::sync::atomic::Ordering::SeqCst) => {
            *working = None;
            std::future::pending::<()>().await;
        }
        Some(_) => {}
    }
}

/// Count this run as working in [`steps_running`], then wait for good if
/// this process is stopping. Counted first, so a stop that finds nothing
/// running has seen every run that could still begin work.
async fn begin_work() -> Running {
    let running = Running::start();
    if STOPPING.load(std::sync::atomic::Ordering::SeqCst) {
        drop(running);
        return std::future::pending::<Running>().await;
    }
    running
}

/// How many atom steps this process is running now.
static RUNNING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many atom steps this process is running now: a stopping process
/// waits for them to reach their checkpoints.
pub fn steps_running() -> usize {
    RUNNING
        .load(std::sync::atomic::Ordering::SeqCst)
        .saturating_sub(SET_ASIDE.load(std::sync::atomic::Ordering::SeqCst))
}

/// How many of the [`RUNNING`] runs wait for good, set aside by
/// [`crate::workflows::settled`] in `atomic serve`.
static SET_ASIDE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Stop counting, in [`steps_running`], a run that waits for good.
pub(crate) fn set_aside() {
    SET_ASIDE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Counts one running atom step in [`steps_running`] while it lives.
struct Running;

impl Running {
    fn start() -> Self {
        RUNNING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The marker a dead-lettered run's error starts with, for readers in
/// other processes; this one asks [`dead_lettered`].
pub const DEAD_LETTERED: &str = "dead-lettered:";

/// One atom step, as the interpreter runs it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AtomStep {
    /// The step's id in the workflow.
    pub id: String,
    /// The atom it runs, by name.
    pub atom: String,
    /// Each input field and the CEL expression that computes it.
    pub with: BTreeMap<String, String>,
    /// Whether it runs in a worktree of its own, at its mapped `base`.
    #[serde(default)]
    pub worktree: bool,
    /// Each mapped field's origin: what its sources allow, or lower when the
    /// workflow says so (see `atomic_core::workflow::binding_origin`).
    #[serde(default)]
    pub origins: BTreeMap<String, String>,
}

use atomic_core::workflow::{binding_origin as origin_of, trust_rank as rank};

/// One gate step, as the interpreter runs it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateStep {
    /// The step's id in the workflow.
    pub id: String,
    /// The CEL expression for what the person is asked.
    pub title: String,
    /// The CEL expression for what they are shown, if any.
    pub show: Option<String>,
    /// How long the gate waits, in seconds, or `None` for no deadline.
    pub timeout: Option<u64>,
    /// The approver groups the workflow file names, if it names any.
    pub approvers: Option<Vec<String>>,
}

/// One step of a pipeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Step {
    /// Run an atom.
    Atom(AtomStep),
    /// Wait for a person's decision.
    Gate(GateStep),
}

impl Step {
    /// The step's id in the workflow.
    pub fn id(&self) -> &str {
        match self {
            Self::Atom(s) => &s.id,
            Self::Gate(s) => &s.id,
        }
    }
}

/// A workflow and the atoms it uses, by value: what one run executes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pipeline {
    /// The workflow's name.
    pub name: String,
    /// Its steps, in an order that respects `after`.
    pub steps: Vec<Step>,
    /// The CEL expression for its result.
    pub returns: String,
    /// Its input contract.
    pub input: Value,
    /// Its output contract.
    pub output: Value,
    /// Every atom a step names, by name.
    pub atoms: BTreeMap<String, Atom>,
    /// The limits the workflow's `budget` block writes for the whole run.
    #[serde(default)]
    pub budget: Budget,
    /// The run's budgets, resolved before it starts (see [`pin_budgets`]).
    /// A run without them resolves them from its services' settings in its
    /// first step; either way the run keeps what it began with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budgets: Option<BudgetPlan>,
    /// The SHA-256 of each file the run uses, by path: the workflow file and
    /// every source of every atom, as they were read when the run was
    /// prepared.
    pub pins: BTreeMap<String, String>,
}

/// The media type a pipeline snapshot is stored under.
pub const SNAPSHOT_MEDIA_TYPE: &str = "application/vnd.atomic.pipeline+json";

/// What a run's first step records: where its definitions were stored, and
/// the hash of every file they came from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pinned {
    /// The artifact holding the pipeline, as JSON.
    pub snapshot: String,
    /// The SHA-256 of each file, by path.
    pub files: BTreeMap<String, String>,
    /// The helper runtime the run started on (see
    /// `atomic_core::astro::Program::helper_runtime`). A step whose atom
    /// captures helpers runs only on the same one.
    #[serde(default)]
    pub helper_runtime: Option<String>,
    /// The budgets the run is metered under, as resolved when it started.
    pub budgets: BudgetPlan,
}

/// One run of a pipeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PipelineRun {
    /// What to run.
    pub pipeline: Pipeline,
    /// The workflow's input.
    pub input: Value,
}

/// Why a workflow cannot run.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{workflow}: {message}")]
pub struct PrepareError {
    /// The workflow's name.
    pub workflow: String,
    /// What is wrong.
    pub message: String,
}

/// Turn a loaded, checked workflow and the project's atoms into a pipeline,
/// refusing what the interpreter cannot run yet and any gate whose approvers
/// do not resolve in `deployment`.
pub fn prepare(
    workflow: &Workflow,
    atoms: &BTreeMap<String, Atom>,
    deployment: &Deployment,
) -> std::result::Result<Pipeline, PrepareError> {
    let fail = |message: String| PrepareError {
        workflow: workflow.name.clone(),
        message,
    };
    let definition = &workflow.definition;
    let mut steps = Vec::new();
    // Each step's kind, for the origin of what it outputs.
    let mut kinds: BTreeMap<String, String> = BTreeMap::new();
    let mut used = BTreeMap::new();
    let mut pins = BTreeMap::new();
    pins.insert(workflow.file.display().to_string(), workflow.hash.clone());
    for id in &workflow.order {
        let step = &definition["steps"][id];
        if let Some(g) = step.get("gate") {
            steps.push(Step::Gate(gate_step(id, g, deployment).map_err(fail)?));
            kinds.insert(id.clone(), "gate".into());
            continue;
        }
        let worktree = step.get("worktree").and_then(Value::as_bool) == Some(true);
        let name = step
            .get("atom")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(format!("step `{id}` names no atom")))?;
        let atom = atoms
            .get(name)
            .ok_or_else(|| fail(format!("step `{id}` names `{name}`, which is not loaded")))?;
        if !matches!(atom.kind.as_str(), "lite" | "codex" | "exec") {
            return Err(fail(format!(
                "step `{id}` runs `{name}`, a {} atom, which run_pipeline cannot run",
                atom.kind
            )));
        }
        if worktree && atom.kind != "codex" {
            return Err(fail(format!(
                "step `{id}` asks for a worktree for `{name}`, a {} atom; only a codex atom \
                 works in one",
                atom.kind
            )));
        }
        let mut with = BTreeMap::new();
        if let Some(map) = step.get("with").and_then(Value::as_object) {
            for (field, source) in map {
                let source = source.as_str().ok_or_else(|| {
                    fail(format!(
                        "step `{id}` maps `{field}` to a value that is not a CEL expression"
                    ))
                })?;
                with.insert(field.clone(), source.to_owned());
            }
        }
        let mut origins = BTreeMap::new();
        for (field, source) in &with {
            let allowed = origin_of(source, &kinds)
                .map_err(|e| fail(format!("step `{id}`: `with.{field}`: {e}")))?;
            let declared = step["origins"].get(field).and_then(Value::as_str);
            let origin = match declared {
                Some(declared) if rank(declared) > rank(allowed) => {
                    return Err(fail(format!(
                        "step `{id}` marks `{field}` as {declared}, but `{source}` is {allowed} \
                         data at most; a binding may lower an origin, never raise it"
                    )));
                }
                Some(declared) => declared,
                None => allowed,
            };
            origins.insert(field.clone(), origin.to_owned());
        }
        if worktree && !with.contains_key("base") {
            return Err(fail(format!(
                "step `{id}` asks for a worktree but maps no `base`, the commit it starts from"
            )));
        }
        for (source, text) in &atom.program.sources {
            pins.insert(source.clone(), sha256(text.as_bytes()));
        }
        used.insert(name.to_owned(), atom.clone());
        kinds.insert(id.clone(), atom.kind.clone());
        steps.push(Step::Atom(AtomStep {
            id: id.clone(),
            atom: name.to_owned(),
            with,
            worktree,
            origins,
        }));
    }
    let returns = definition
        .get("returns")
        .and_then(Value::as_str)
        .ok_or_else(|| fail("`returns` is not a CEL expression".into()))?
        .to_owned();
    Ok(Pipeline {
        name: workflow.name.clone(),
        steps,
        returns,
        input: workflow.input.clone(),
        output: workflow.output.clone(),
        atoms: used,
        budget: workflow.budget,
        budgets: None,
        pins,
    })
}

/// Resolve `pipeline`'s budgets against `settings`, for its atoms on
/// `runtimes`, and keep them in it, so the run is metered as configured
/// now and a budget that cannot hold, such as a dollar cap on an unpriced
/// model or a cap smaller than one call, fails before the run starts.
pub fn pin_budgets(
    pipeline: &mut Pipeline,
    runtimes: &Runtimes,
    settings: &BudgetSettings,
) -> std::result::Result<(), PrepareError> {
    let plan = plan_budgets(pipeline, runtimes, settings).map_err(|message| PrepareError {
        workflow: pipeline.name.clone(),
        message,
    })?;
    pipeline.budgets = Some(plan);
    Ok(())
}

/// A gate step from its definition, which the workflow loader has checked.
fn gate_step(
    id: &str,
    gate: &Value,
    deployment: &Deployment,
) -> std::result::Result<GateStep, String> {
    let text = |key: &str| gate.get(key).and_then(Value::as_str).map(str::to_owned);
    let title = text("title").ok_or_else(|| format!("gate `{id}` has no `title`"))?;
    let timeout = text("timeout")
        .map(|t| atomic_core::workflow::parse_timeout(&t))
        .transpose()
        .map_err(|e| format!("gate `{id}`: {e}"))?
        .map(|d| d.as_secs());
    let approvers = gate.get("approvers").map(|a| {
        a.as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    });
    gate::resolve(id, approvers.as_deref(), deployment).map_err(|e| e.to_string())?;
    Ok(GateStep {
        id: id.to_owned(),
        title,
        show: text("show"),
        timeout,
        approvers,
    })
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn failed(message: String) -> DurableError {
    DurableError::Failed(message)
}

/// How many more times a write tries for a store whose lock another holder
/// keeps, within one execution of the run, before it stops the run to be
/// run again: each try waits as long as the store waits for its lock, and
/// the wait between tries doubles up to that long. With the store's 30 s,
/// an execution waits about 7.4 minutes (11 tries of 30 s, and 111 s
/// between them) before it spends one of durare's recoveries.
const LOCKED_RETRIES: u32 = 10;
const LOCKED_BACKOFF: std::time::Duration = std::time::Duration::from_millis(200);

/// How many times this process has found the artifact store held elsewhere
/// for each run, try by try.
static STORE_WAITS: std::sync::Mutex<BTreeMap<String, u32>> =
    std::sync::Mutex::new(BTreeMap::new());

/// How many times this process has found the artifact store held
/// elsewhere while pinning run `id`.
pub fn store_waits(id: &str) -> u32 {
    STORE_WAITS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .copied()
        .unwrap_or(0)
}

/// Put `content` in `store` for run `id`, trying again with backoff, up to
/// a bound, while the store's lock is held elsewhere, and saying so once.
fn put_waiting_out_the_lock(
    id: &str,
    store: &LocalStore,
    content: &[u8],
    provenance: Provenance,
) -> std::result::Result<atomic_core::artifact::Manifest, ArtifactError> {
    let cap = store.lock_wait().max(LOCKED_BACKOFF);
    let mut backoff = LOCKED_BACKOFF.min(cap);
    let mut retries = 0;
    loop {
        match store.put(&mut &content[..], provenance.clone()) {
            Err(ArtifactError::Locked { .. }) if retries < LOCKED_RETRIES => {
                let waits = {
                    let mut all = STORE_WAITS.lock().unwrap_or_else(|e| e.into_inner());
                    let n = all.entry(id.to_owned()).or_insert(0);
                    *n += 1;
                    *n
                };
                if waits == 1 {
                    atomic_core::log_line!(
                        "atomic: run {id}: the artifact store is held by another process, such \
                         as a sweep; the run waits for it and tries again"
                    );
                }
                retries += 1;
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(cap);
            }
            stored => return stored,
        }
    }
}

/// Check `value` against `contract`, naming every place it fails.
fn fits(what: &str, contract: &Value, value: &Value) -> Result<()> {
    let validator = Registry::new()
        .validator_for(contract)
        .map_err(|e| failed(format!("{what}: the contract is not valid: {e}")))?;
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|e| {
            let at = e.instance_path().to_string();
            format!("{}: {e}", if at.is_empty() { "/" } else { &at })
        })
        .collect();
    if !errors.is_empty() {
        return Err(failed(format!(
            "{what} does not fit its contract: {}",
            errors.join("; ")
        )));
    }
    // CEL reads it as its contract types it; refuse here what it cannot read.
    atomic_schema::expr::convertible(value, contract)
        .map_err(|e| failed(format!("{what} cannot be read in CEL: {e}")))
}

/// The model a declarative atom's calls go to, as its runtime picks it: the
/// atom's own, then the project default, then the endpoint's. `None` for an
/// atom that calls no model.
fn model_of(atom: &Atom, runtimes: &Runtimes) -> Option<String> {
    let (defaults, endpoint) = match atom.kind.as_str() {
        "codex" => (&runtimes.codex.defaults, &runtimes.codex.endpoint),
        "lite" => (&runtimes.lite.defaults, &runtimes.lite.endpoint),
        _ => return None,
    };
    Some(
        atom.frontmatter
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| defaults.model.clone())
            .unwrap_or_else(|| endpoint.model.clone()),
    )
}

/// The run's caps and each model atom's: a budget that cannot be resolved,
/// such as a dollar cap on an unpriced model, is refused with where it is
/// written.
fn plan_budgets(
    pipeline: &Pipeline,
    runtimes: &Runtimes,
    settings: &BudgetSettings,
) -> std::result::Result<BudgetPlan, String> {
    let models: Vec<(&str, &Budget, String)> = pipeline
        .atoms
        .iter()
        .filter_map(|(name, atom)| Some((name.as_str(), &atom.budget, model_of(atom, runtimes)?)))
        .collect();
    let atoms: Vec<(&str, &Budget, &str)> = models
        .iter()
        .map(|(name, budget, model)| (*name, *budget, model.as_str()))
        .collect();
    BudgetPlan::resolve(settings, &pipeline.budget, &atoms).map_err(|(atom, e)| match atom {
        None => format!("workflow {}: budget: {e}", pipeline.name),
        Some(name) => format!("atom {name}: budget: {e}"),
    })
}

/// Run one pipeline. Workflow code: everything that touches a model or the
/// store goes through durable steps. Every model call is metered against
/// the run's and its atom's budget (see [`crate::metering`]).
async fn run_pipeline<C: WorkflowContext>(
    ctx: C,
    run: PipelineRun,
    services: Arc<Services>,
    ledger: Ledger,
    replay: Option<Arc<Replay>>,
) -> Result<Value> {
    let PipelineRun { pipeline, input } = run;
    let id = ctx.workflow_id().to_owned();
    // Before anything is recorded: which execution this is, so `runs show`
    // can tell what each one ran and what it found checkpointed. Only that
    // view depends on it, so a record that cannot be written costs the run
    // nothing but the view's accuracy; an execution that no longer owns its
    // run stops.
    match atomic_durable::history::begin(&ctx).await {
        Ok(_) => {}
        Err(e @ DurableError::Interrupted(_)) => return Err(e),
        Err(e) => atomic_core::log_line!(
            "atomic: run {id}: this execution of it could not be recorded ({e}); `atomic runs \
             show` may credit its steps to an earlier one"
        ),
    }
    // Working from here to its end, but for waits at gates: a stopping
    // process gives it its grace, and begins none of its steps.
    let mut working = Some(begin_work().await);
    reached(&id, 0)?;
    let store = services.store.clone();
    fits("the workflow's input", &pipeline.input, &input)?;
    plan(&pipeline, services.runtimes.codex.backend.statement())?;
    // The budgets the run keeps: pinned before it started, or resolved now,
    // once, and recorded with its definitions.
    let planned = match &pipeline.budgets {
        // A plan in the run's input came from outside: checked as one
        // resolved here would be.
        Some(plan) => {
            // Every atom of the run that calls a model needs its caps and
            // model in the plan, so a gap fails here, before any call.
            let model_atoms: Vec<&str> = pipeline
                .atoms
                .iter()
                .filter(|(_, atom)| model_of(atom, &services.runtimes).is_some())
                .map(|(name, _)| name.as_str())
                .collect();
            plan.validate()
                .and_then(|()| plan.covers(&model_atoms))
                .map(|()| plan.clone())
                .map_err(|e| format!("workflow {}: its pinned budget: {e}", pipeline.name))
        }
        None => plan_budgets(&pipeline, &services.runtimes, &services.budgets),
    };
    // Store the definitions the run uses before anything runs. The snapshot
    // is content-addressed, so a retried step stores the same artifact.
    let snapshot =
        serde_json::to_vec(&pipeline).map_err(|e| failed(format!("encode the pipeline: {e}")))?;
    let files = pipeline.pins.clone();
    let run_id = ctx.workflow_id().to_owned();
    let pinning = run_id.clone();
    let replaying = replay.is_some();
    let helper_runtime = atomic_core::astro::Program::current_helper_runtime();
    let pinned: Pinned = ctx
        .step("pin", move |_| async move {
            let budgets = planned.map_err(failed)?;
            let provenance = Provenance {
                media_type: SNAPSHOT_MEDIA_TYPE.into(),
                workflow: Some(run_id),
                step: Some("pin".into()),
                origin: Some("run_pipeline".into()),
                labels: Default::default(),
            };
            // A replay stores nothing new: its definitions are the recorded
            // run's, already in the store, and it waits for no lock to find
            // them. It marks them as just used, unless a sweep holds the
            // store, so a sweep's grace covers them. Gone, they are stored
            // again as any run's are.
            if replaying {
                let digest = sha256(&snapshot);
                if let Ok(stored) = ArtifactRef::from_digest(&digest)
                    && matches!(
                        store.refresh(&stored),
                        Ok(atomic_core::artifact::Refreshed::Now)
                            | Ok(atomic_core::artifact::Refreshed::Held)
                    )
                    && store.contains(&stored).unwrap_or(false)
                {
                    return Ok(Pinned {
                        snapshot: stored.to_string(),
                        files,
                        helper_runtime: Some(helper_runtime),
                        budgets,
                    });
                }
            }
            let pinning_id = pinning.clone();
            let stored = tokio::task::spawn_blocking(move || {
                put_waiting_out_the_lock(&pinning_id, &store, &snapshot, provenance)
            })
            .await
            .map_err(|e| failed(format!("store the pipeline: {e}")))?;
            let manifest = match stored {
                Ok(manifest) => manifest,
                // A store held elsewhere is in the way for now, not a fault
                // of the run: it stops without recording anything, and
                // recovery runs it again.
                Err(e @ ArtifactError::Locked { .. }) => {
                    not_reached(&pinning, 0);
                    return Err(DurableError::Interrupted(format!(
                        "store the pipeline: {e}; the run is run again, and once durare's \
                         recoveries of it run out, it waits for `atomic runs resume`"
                    )));
                }
                Err(e) => return Err(failed(format!("store the pipeline: {e}"))),
            };
            Ok(Pinned {
                snapshot: manifest.artifact.to_string(),
                files,
                helper_runtime: Some(helper_runtime),
                budgets,
            })
        })
        .await?;
    let plan = pinned.budgets.clone();
    let budget = RunBudget::pinned(ledger, &plan);
    let (caps, models) = (plan.atoms, plan.models);
    let mut steps = Map::new();
    // What each finished step's output is typed by, for CEL.
    let mut shapes = BTreeMap::new();
    let input_shape = Shape::Schema(&pipeline.input);
    // What atomic itself records beside each step.
    let metadata_schema = json!({"type": "object", "properties": {
        "input_tokens": {"type": "integer"}, "output_tokens": {"type": "integer"},
        "total_tokens": {"type": "integer"}, "turns": {"type": "integer"}
    }});
    let decision_schema = json!({"type": "object", "properties": {"at": {"type": "integer"}}});
    let mut used_worktrees = false;
    let mut accepted = Vec::new();
    for (position, step) in pipeline.steps.iter().enumerate() {
        // Before its attempt is recorded: a stopping process begins nothing.
        keep_working(&mut working).await;
        reached(&id, position + 1)?;
        let scope = json!(steps);
        let steps_shape = Shape::Fields(shapes.clone());
        let values = [
            ("input", &input, &input_shape),
            ("steps", &scope, &steps_shape),
        ];
        let step = match step {
            Step::Atom(step) => step,
            Step::Gate(g) => {
                // Waiting for a person is not work a stop waits for.
                working = None;
                let replaying = replay.as_ref().map(|r| r.of.as_str());
                let decision = run_gate(
                    &ctx,
                    &pipeline.name,
                    g,
                    &values,
                    &services.deployment,
                    replaying,
                );
                let decision = crate::telemetry::traced(
                    crate::telemetry::gate_span(&g.id, ctx.workflow_id()),
                    decision,
                )
                .await?;
                failpoint(&format!("pipeline:after:{}", g.id));
                steps.insert(g.id.clone(), json!({ "decision": decision }));
                shapes.insert(
                    g.id.clone(),
                    Shape::fields([("decision", Shape::Schema(&decision_schema))]),
                );
                continue;
            }
        };
        let mut mapped = Map::new();
        for (field, source) in &step.with {
            let value = atomic_schema::expr::evaluate(source, &values)
                .map_err(|e| failed(format!("step `{}`: `with.{field}`: {e}", step.id)))?;
            mapped.insert(field.clone(), value);
        }
        let atom = pipeline
            .atoms
            .get(&step.atom)
            .cloned()
            .ok_or_else(|| failed(format!("step `{}`: no atom `{}`", step.id, step.atom)))?;
        let mut runtimes = (*services.runtimes).clone();
        // An atom with captured helpers gets its step's logical time and seed
        // from a step of its own, recorded once and read back on every retry,
        // so its helpers render the same prompt and reach the same validator
        // decisions after a crash.
        if !atom.program.helpers.is_empty() {
            let clock: StepClock = ctx
                .step(&format!("{}.clock", step.id), |_| async {
                    StepClock::fresh().map_err(|e| failed(format!("the step's clock: {e}")))
                })
                .await?;
            runtimes.clock = Some(clock);
        }
        let run_id = ctx.workflow_id().to_owned();
        // An atom with a public context gets it from a step of its own,
        // resolved once from the run's settings and read back on every retry.
        let snapshot = if atom.context.is_some() {
            // The settings are needed only to resolve the context; a recorded
            // one replays without them.
            let settings = services.context.clone();
            let (resolving, run, id) = (atom.clone(), run_id.clone(), step.id.clone());
            let recorded: Value = ctx
                .step(&format!("{}.context", step.id), move |_| async move {
                    let settings = settings.ok_or_else(|| {
                        failed(format!(
                            "step `{id}` runs `{}`, which reads a public context, and this run \
                             has no context settings",
                            resolving.name
                        ))
                    })?;
                    tokio::task::spawn_blocking(move || {
                        atomic_core::context::resolve_for_step(&resolving, &run, &id, &settings)
                            .map(|snapshot| snapshot.as_value().clone())
                    })
                    .await
                    .map_err(|e| failed(format!("resolve the context: {e}")))?
                    .map_err(|e| failed(format!("resolve the context: {e}")))
                })
                .await?;
            Some(
                atomic_core::context::ContextSnapshot::restore(&atom, &run_id, recorded).map_err(
                    |e| failed(format!("step `{}`: its recorded context: {e}", step.id)),
                )?,
            )
        } else {
            None
        };
        let atom_input = Value::Object(mapped);
        // Recorded with the step's result, so a replay serves it only to the
        // same input.
        let input_sha256 = sha256(
            &serde_json::to_vec(&atom_input)
                .map_err(|e| failed(format!("step `{}`: encode its input: {e}", step.id)))?,
        );
        let (budget, atom_caps) = (budget.clone(), caps.get(&step.atom).cloned());
        // A model atom runs on the model its budget was resolved for, and
        // only with caps: never unmetered, never at another model's price.
        if let Some(model) = model_of(&atom, &runtimes) {
            let pinned = match (&atom_caps, models.get(&step.atom)) {
                (Some(_), Some(pinned)) => pinned.clone(),
                _ => {
                    return Err(failed(format!(
                        "step `{}`: `{}` calls a model, and the run's budget has no caps or \
                         model for it",
                        step.id, atom.name
                    )));
                }
            };
            runtimes.codex.defaults.model = Some(pinned.clone());
            runtimes.lite.defaults.model = Some(pinned.clone());
            let now = model_of(&atom, &runtimes).unwrap_or(model);
            if now != pinned {
                return Err(failed(format!(
                    "step `{}`: `{}` calls {now:?}, and the run's budget was resolved for \
                     {pinned:?}",
                    step.id, atom.name
                )));
            }
        }
        let tree = if step.worktree {
            let worktrees = services.worktrees.clone().ok_or_else(|| {
                failed(format!(
                    "step `{}` asks for a worktree, and this project is not in a git repository",
                    step.id
                ))
            })?;
            let base = atom_input
                .get("base")
                .and_then(Value::as_str)
                .ok_or_else(|| failed(format!("step `{}`: `base` is not a commit", step.id)))?;
            used_worktrees = true;
            Some((
                worktrees,
                StepRequest {
                    run: run_id.clone(),
                    step: step.id.clone(),
                    base: base.to_owned(),
                    message: format!("{}: {}", pipeline.name, step.id),
                    signature: Signature::default(),
                },
            ))
        } else {
            None
        };
        // An exec step's workspace opens in the body of the step that records
        // its attempt, so the attempt is recorded only once the workspace is
        // open: on recovery, an attempt whose workspace never opened is not
        // one whose command may have run. A replayed attempt step runs no
        // body, so a step that finished never touches the backend again; one
        // whose turn did not finish opens a workspace in its turn.
        //
        // A replay of a recorded run runs no command: its step is served the
        // recorded result, so it records its attempt with no workspace, and
        // nothing is opened on the backend.
        let guard = if atom.kind == "exec" {
            let identity = atomic_core::runtime::EffectIdentity {
                run_id: run_id.clone(),
                step_id: step.id.clone(),
            };
            let (opening, who, backend) = (atom.clone(), identity.clone(), runtimes.exec.shared());
            let place = format!("run {run_id}, step {}", step.id);
            let attempt_step = format!("{}.attempt", step.id);
            let (begun, opened) = if replay.is_some() {
                (attempt::begin(&ctx, &attempt_step).await?, None)
            } else {
                attempt::begin_with(&ctx, &attempt_step, move || async move {
                    let opened =
                        atomic_core::runtime::open_exec_workspace(&opening, &who, backend).await;
                    crate::workflows::settled(opened, &place, true).await
                })
                .await?
            };
            // The step's body may run again (after a park is released): the
            // workspace opened with the attempt serves its first run only.
            let opened = Arc::new(std::sync::Mutex::new(opened));
            Some((begun, atom.external().is_some(), opened, identity))
        } else {
            None
        };
        let origins = step.origins.clone();
        let pinned_helpers = pinned.helper_runtime.clone();
        let (id, ran) = (step.id.clone(), format!("pipeline:ran:{}", step.id));
        let place = format!("run {run_id}, step {}", step.id);
        let serving = replay.clone();
        let published = run_atom_at(
            &ctx,
            &step.id,
            move |position| {
                // The turn runs again after a park is released, so each run
                // takes its own copies.
                let mut runtimes = runtimes.clone();
                let (atom, atom_input, origins) = (atom.clone(), atom_input.clone(), origins.clone());
                let (id, ran, run_id) = (id.clone(), ran.clone(), run_id.clone());
                let (snapshot, tree, guard) = (snapshot.clone(), tree.clone(), guard.clone());
                let pinned_helpers = pinned_helpers.clone();
                let place = place.clone();
                // In a replay, the step's recorded result, with no model call
                // and no command run; a step with none fails rather than run
                // live.
                let served = serving.as_ref().map(|r| r.serve(&id, &input_sha256));
                let input_sha256 = input_sha256.clone();
                // An exec atom calls no model and has no caps to meter.
                if let Some(atom_caps) = &atom_caps {
                    let meter = budget.meter(&position, atom_caps);
                    runtimes.codex.meter = meter.clone();
                    runtimes.lite.meter = meter;
                }
                // Made here, in the step's body, so a step that replays from
                // its checkpoint makes none.
                let span = crate::telemetry::step_span(&atom.name, &id, &run_id);
                let caller = (run_id.clone(), id.clone());
                let work = async move {
                if let Some(served) = served {
                    return served;
                }
                // The run's helpers run only as recorded, on the runtime the
                // run started on; anything else is refused rather than
                // recaptured. Checked only when the turn runs: a finished
                // turn replays from its checkpoint, whatever the runtime.
                if !atom.program.helpers.is_empty() {
                    atom.program
                        .verify_helpers()
                        .map_err(|e| failed(format!("step `{id}`: {e}")))?;
                    let now = atomic_core::astro::Program::current_helper_runtime();
                    if pinned_helpers.as_deref() != Some(now.as_str()) {
                        return Err(failed(format!(
                            "step `{id}`: this run started on another helper runtime ({}), and \
                             its helpers cannot resume on this one ({now}); their behaviour could \
                             differ from what the run already recorded. Start a new run",
                            pinned_helpers.as_deref().unwrap_or("not recorded"),
                        )));
                    }
                }
                if let Some((begun, declared, _, _)) = &guard
                    && begun.interrupted()
                    && !declared
                {
                    return Err(failed(format!(
                        "step `{id}`: outcome_unknown: an earlier attempt of `{}` was interrupted \
                         before its result was saved, so whether its command's effects happened \
                         is unknown. The step is parked and its command does not run again. If \
                         repeating it is safe, declare `effects: {{ external: 'idempotent' }}` \
                         (or `'read-only'` if it only reads)",
                        atom.name
                    )));
                }
                let proposal = match (tree, guard) {
                    (_, Some((_, _, opened, identity))) => {
                        let clock = runtimes.clock.clone();
                        let backend = runtimes.exec.shared();
                        // The store the model runtimes check references against.
                        let artifacts = runtimes.codex.artifacts.clone();
                        propose_at(&place, true, async move {
                            let compiled = compile::compile(&atom, &Registry::new())
                                .map_err(|e| AtomError::Runtime(e.to_string()))?;
                            // Opened with the attempt, or, when the attempt
                            // was recorded by an earlier execution, now.
                            let first = opened.lock().ok().and_then(|mut o| o.take());
                            let opened = match first {
                                Some(opened) => opened,
                                None => {
                                    atomic_core::runtime::open_exec_workspace(
                                        &atom, &identity, backend,
                                    )
                                    .await?
                                }
                            };
                            atomic_core::runtime::run_exec_opened(
                                &atom,
                                &compiled,
                                &atom_input,
                                &identity,
                                opened,
                                clock.as_ref(),
                                &artifacts,
                            )
                            .await
                        })
                        .await
                    }
                    (None, None) => {
                        propose_at(&place, true, async move {
                            run_in(&atom, &atom_input, &origins, &run_id, &id, snapshot.as_ref(), &runtimes)
                                .await
                        })
                            .await
                    }
                    (Some((worktrees, request)), None) => {
                        let runtimes = Arc::new(runtimes);
                        let call = StepCall {
                            atom,
                            input: atom_input,
                            origins,
                            snapshot,
                            run_id,
                        };
                        in_worktree(worktrees, request, call, runtimes, &place).await
                    }
                }?;
                let mut proposal = proposal;
                let Value::Object(metadata) = &mut proposal.metadata else {
                    return Err(failed("an atom step's metadata is not an object".into()));
                };
                metadata.insert("input_sha256".into(), Value::String(input_sha256));
                failpoint(&ran);
                Ok(proposal)
                };
                // Model calls name the run and step on their spans.
                crate::telemetry::traced(span, atomic_core::usage::for_step(caller.0, caller.1, work))
            },
            Vec::new(),
        )
        .await?;
        if step.worktree
            && let Some(commit) = published.metadata.get("commit").and_then(Value::as_str)
        {
            accepted.push(commit.to_owned());
        }
        failpoint(&format!("pipeline:after:{}", step.id));
        // A replay keeps the recorded metadata in `steps`, thread and all, so
        // a later step that maps it is given what it was recorded with; only
        // what is published here leaves the thread out.
        let metadata = replay
            .as_ref()
            .and_then(|r| r.recorded.get(&step.id))
            .map_or(published.metadata, |r| r.proposal.metadata.clone());
        steps.insert(
            step.id.clone(),
            json!({"output": published.output, "metadata": metadata}),
        );
        let output = match pipeline
            .atoms
            .get(&step.atom)
            .and_then(|a| a.program.contracts.get("Output"))
        {
            Some(contract) => Shape::Schema(contract),
            None => Shape::Untyped,
        };
        shapes.insert(
            step.id.clone(),
            Shape::fields([
                ("output", output),
                ("metadata", Shape::Schema(&metadata_schema)),
            ]),
        );
    }
    keep_working(&mut working).await;
    let scope = json!(steps);
    let steps_shape = Shape::Fields(shapes);
    let result = atomic_schema::expr::evaluate(
        &pipeline.returns,
        &[
            ("input", &input, &input_shape),
            ("steps", &scope, &steps_shape),
        ],
    )
    .map_err(|e| failed(format!("`returns`: {e}")))?;
    fits("the workflow's result", &pipeline.output, &result)?;
    if used_worktrees && let Some(worktrees) = services.worktrees.clone() {
        // The run's worktrees go; its accepted commits keep their refs. A
        // repeat finds less to remove and removes it.
        let run_id = ctx.workflow_id().to_owned();
        let _removed: usize = ctx
            .step("cleanup", move |_| async move {
                tokio::task::spawn_blocking(move || worktrees.cleanup(&run_id, &accepted))
                    .await
                    .map_err(|e| failed(format!("clean up the worktrees: {e}")))?
                    .map(|c| c.refs.len() + c.worktrees.len())
                    .map_err(|e| failed(format!("clean up the worktrees: {e}")))
            })
            .await?;
    }
    drop(working);
    Ok(result)
}

/// Refuse, before anything runs, a step the execution backend's statement
/// cannot carry. The plan follows what the backend stated when it connected,
/// never which backend it is.
///
/// A worktree step's commit is made from its workspace after its turn, so a
/// backend whose workspaces may not survive its runner (ephemeral, which a
/// statement it does not recognize also means) could lose the step's work
/// before atomic commits it.
fn plan(pipeline: &Pipeline, statement: atomic_exec::Statement) -> Result<()> {
    if statement.durability == atomic_exec::Durability::Durable {
        return Ok(());
    }
    let worktree = pipeline.steps.iter().find_map(|step| match step {
        Step::Atom(step) if step.worktree => Some(&step.id),
        _ => None,
    });
    match worktree {
        Some(id) => Err(failed(format!(
            "step `{id}` runs in a worktree, and the execution backend states that its \
             workspaces are {}: they may not survive its runner, and the step's change is \
             committed from its workspace after its turn. Run this workflow on a backend \
             whose workspaces are durable, such as the local one",
            statement.durability.as_str()
        ))),
        None => Ok(()),
    }
}

/// Run `atom` on `input` with `runtimes`.
async fn run_in(
    atom: &Atom,
    input: &Value,
    origins: &BTreeMap<String, String>,
    run_id: &str,
    step: &str,
    snapshot: Option<&atomic_core::context::ContextSnapshot>,
    runtimes: &Runtimes,
) -> std::result::Result<(Value, atomic_core::runtime::RunMetadata), AtomError> {
    let mut registry = Registry::new();
    let compiled =
        compile::compile(atom, &registry).map_err(|e| AtomError::Runtime(e.to_string()))?;
    // Each mapped field carries the origin its binding allows; anything else
    // is external.
    run_declarative_with_context(
        atom,
        &compiled,
        &mut registry,
        &RunInput {
            run_id,
            input,
            frontmatter: &atom.frontmatter,
            origin: "external",
            origins: Some(origins),
            step: Some(step),
        },
        snapshot.map(|snapshot| atomic_core::runtime::DispatchContext {
            snapshot,
            step_id: step,
        }),
        runtimes,
    )
    .await
}

/// What one step dispatches: its atom, its mapped input with each field's
/// origin, its recorded public context, and its run.
struct StepCall {
    atom: Atom,
    input: Value,
    origins: BTreeMap<String, String>,
    snapshot: Option<atomic_core::context::ContextSnapshot>,
    run_id: String,
}

/// Run a step's atom in its worktree, reset to the step's base, and commit
/// what it left. The commit is recorded in the step's metadata by atomic,
/// over anything the atom reported.
async fn in_worktree(
    worktrees: Arc<Worktrees>,
    request: StepRequest,
    call: StepCall,
    runtimes: Arc<Runtimes>,
    place: &str,
) -> Result<Proposal> {
    let StepCall {
        atom,
        input,
        origins,
        snapshot,
        run_id,
    } = call;
    let step = request.step.clone();
    let finished = contained(async move {
        let subdirectory = source_subdirectory(runtimes.codex.cwd.clone()).await?;
        worktrees
            .run_step(request, |tree| async move {
                let mut here = (*runtimes).clone();
                here.codex.cwd = tree.join(subdirectory);
                run_in(
                    &atom,
                    &input,
                    &origins,
                    &run_id,
                    &step,
                    snapshot.as_ref(),
                    &here,
                )
                .await
            })
            .await
            .map_err(|e| match e {
                StepError::Body(e) => e,
                StepError::Worktree(e) => AtomError::Runtime(format!("the step's worktree: {e}")),
            })
    })
    .await;
    let ((output, metadata), commit) = crate::workflows::settled(finished, place, true).await?;
    failpoint("pipeline:after-commit");
    let mut metadata = serde_json::to_value(metadata)
        .map_err(|e| failed(format!("encode the atom's metadata: {e}")))?;
    commit.record(&mut metadata);
    Ok(Proposal {
        output,
        metadata,
        writes: Vec::new(),
    })
}

/// Resolve the project's position within its actual source checkout. Git's
/// common metadata directory cannot identify the root of a linked checkout.
async fn source_subdirectory(cwd: PathBuf) -> std::result::Result<PathBuf, AtomError> {
    tokio::task::spawn_blocking(move || {
        let error = |e| AtomError::Runtime(format!("the source checkout: {e}"));
        let cwd = std::fs::canonicalize(cwd).map_err(|e| error(e.to_string()))?;
        let top = Git::find()
            .and_then(|git| git.worktree_root(&cwd))
            .map_err(|e| error(e.to_string()))?;
        cwd.strip_prefix(top)
            .map(PathBuf::from)
            .map_err(|e| error(e.to_string()))
    })
    .await
    .map_err(|e| AtomError::Runtime(format!("the source checkout: {e}")))?
}

/// Open gate `step`, wait for its resolution, and return the decision that
/// lets the run go on. Anything but an approval fails the run.
async fn run_gate<C: WorkflowContext>(
    ctx: &C,
    workflow: &str,
    step: &GateStep,
    values: &[(&str, &Value, &Shape<'_>)],
    deployment: &Deployment,
    replay_of: Option<&str>,
) -> Result<Value> {
    let at = |e: String| failed(format!("gate `{}`: {e}", step.id));
    let title = match atomic_schema::expr::evaluate(&step.title, values)
        .map_err(|e| at(format!("`title`: {e}")))?
    {
        // A replay's gate says so: its steps' results were served from a
        // recording, not made now.
        Value::String(title) => match replay_of {
            Some(of) => format!("Replay of run {of}: {title}"),
            None => title,
        },
        other => return Err(at(format!("`title` gave {other}, not a string"))),
    };
    let show = match &step.show {
        Some(source) => {
            atomic_schema::expr::evaluate(source, values).map_err(|e| at(format!("`show`: {e}")))?
        }
        None => Value::Null,
    };
    // Who may decide comes from the workflow's groups and the deployment
    // this process runs in; nothing in the run's input chooses it.
    let approvers = gate::resolve(&step.id, step.approvers.as_deref(), deployment)
        .map_err(|e| at(e.to_string()))?;
    let spec = GateSpec {
        workflow: workflow.to_owned(),
        title,
        show,
        evidence: Vec::new(),
        approvers,
        tier: Tier::High,
        timeout: step.timeout.map(std::time::Duration::from_secs),
        on_timeout: Default::default(),
    };
    match gate::gate(ctx, &step.id, spec).await? {
        Resolution::Decided(d) if d.verdict == gate::Verdict::Approve => Ok(json!({
            "verdict": "approve",
            "by": d.by.as_str(),
            "comment": d.comment,
            "at": d.at,
        })),
        Resolution::Decided(d) => Err(at(format!(
            "rejected by {}{}, so no step after it runs",
            d.by,
            d.comment.map(|c| format!(": {c}")).unwrap_or_default()
        ))),
        Resolution::Expired { .. } => Err(at(
            "its deadline passed undecided, so no step after it runs".into(),
        )),
    }
}

/// What `run_pipeline` runs with, fixed when it is registered and never
/// part of a run's input.
#[derive(Clone, Debug)]
pub struct Services {
    /// Where atoms run.
    pub runtimes: Arc<Runtimes>,
    /// Where each run's definitions are stored.
    pub store: LocalStore,
    /// Where the project runs, which decides who may decide its gates.
    pub deployment: Deployment,
    /// The project repository's worktrees, for `worktree: true` steps, or
    /// `None` when the project is not in a git repository.
    pub worktrees: Option<Arc<Worktrees>>,
    /// The budget settings every run's model calls are metered under.
    pub budgets: Arc<BudgetSettings>,
    /// The public context settings a step whose atom declares a context is
    /// resolved from, once per run and step; `None` refuses such a step.
    pub context: Option<atomic_core::context::DispatchSettings>,
}

/// Register `run_pipeline` on `rt`, metering model calls against `rt`'s
/// usage ledger.
pub fn register_pipeline<R: DurableRuntime>(rt: &mut R, services: Services) {
    let services = Arc::new(services);
    let ledger = rt.ledger();
    let (replaying, replay_ledger) = (services.clone(), ledger.clone());
    rt.register(PIPELINE, move |ctx: R::Context, run: PipelineRun| {
        let span = crate::telemetry::workflow_span(&run.pipeline.name, ctx.workflow_id());
        crate::telemetry::traced(
            span,
            run_pipeline(ctx, run, services.clone(), ledger.clone(), None),
        )
    });
    rt.register(REPLAY, move |ctx: R::Context, replay: ReplayRun| {
        let ReplayRun { of, run, recorded } = replay;
        let span = crate::telemetry::workflow_span(&run.pipeline.name, ctx.workflow_id());
        crate::telemetry::traced(
            span,
            run_pipeline(
                ctx,
                run,
                replaying.clone(),
                replay_ledger.clone(),
                Some(Arc::new(Replay { of, recorded })),
            ),
        )
    });
}

/// The name a replay of a recorded run is registered under.
pub const REPLAY: &str = "atomic.replay";

/// A replay of a recorded run: the same pipeline and input, with each atom
/// step served the result it published in the recording instead of calling
/// a model or running a command. An atom step the recording has no result
/// for fails the replay; nothing runs live. Every other step runs as it
/// would in a new run: a gate opens again, marked as a replay's, and waits
/// for a decision.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayRun {
    /// The recorded run.
    pub of: String,
    /// Its pipeline and input.
    pub run: PipelineRun,
    /// Each published atom step's recorded result, by step id.
    pub recorded: BTreeMap<String, Recorded>,
}

/// One atom step's recorded result, as a replay serves it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recorded {
    /// What the step published: its output and metadata, with no writes.
    pub proposal: Proposal,
    /// The SHA-256 of the input the step was given, which the replayed
    /// step's must match.
    pub input_sha256: String,
}

/// A replay as `run_pipeline` serves it.
struct Replay {
    of: String,
    recorded: BTreeMap<String, Recorded>,
}

impl Replay {
    /// Step `id`'s recorded result for the input whose SHA-256 is `input`:
    /// its output and metadata, without the model thread it ran on, which
    /// belongs to the recorded run and not to this one.
    fn serve(&self, id: &str, input: &str) -> Result<Proposal> {
        let of = &self.of;
        let recorded = self.recorded.get(id).ok_or_else(|| {
            failed(format!(
                "no recorded result for step {id}: run {of} did not publish one, and a replay                  never runs an atom live"
            ))
        })?;
        if recorded.input_sha256 != input {
            return Err(failed(format!(
                "step {id} has another input than it had in run {of}, so its recorded result                  does not answer it"
            )));
        }
        let mut proposal = recorded.proposal.clone();
        if let Value::Object(metadata) = &mut proposal.metadata {
            metadata.remove("thread_id");
        }
        Ok(proposal)
    }
}

/// The replay of run `of` in `rt`'s store: its pipeline and input, and the
/// result each of its atom steps published, with the input it was given.
/// Refused for a run recorded under another layout than `rt`'s, and for one
/// whose recorded results propose writes, which a replay never makes.
pub async fn replay_of(rt: &atomic_durable::Durare, of: &str) -> Result<ReplayRun> {
    let record = rt
        .run_record(of)
        .await?
        .ok_or_else(|| failed(format!("there is no run {of} to replay")))?;
    if record.version != rt.version() {
        return Err(failed(format!(
            "run {of} was recorded under layout {}, and this build replays only {}",
            record.version,
            rt.version()
        )));
    }
    let input = rt
        .run_input(of)
        .await?
        .ok_or_else(|| failed(format!("there is no run {of} to replay")))?;
    let run: PipelineRun = serde_json::from_value(input).map_err(|e| {
        failed(format!(
            "run {of} is not a workflow-file run this build can replay: {e}"
        ))
    })?;
    let recorded = recorded_results(of, &run, rt.recorded_candidates(of).await?)?;
    Ok(ReplayRun {
        of: of.to_owned(),
        run,
        recorded,
    })
}

/// What a replay of run `of`, a run of `run`, serves each atom step, from
/// the `candidates` the run published.
fn recorded_results(
    of: &str,
    run: &PipelineRun,
    mut candidates: BTreeMap<String, atomic_durable::Candidate>,
) -> Result<BTreeMap<String, Recorded>> {
    let mut recorded = BTreeMap::new();
    for step in &run.pipeline.steps {
        let Step::Atom(step) = step else { continue };
        let Some(candidate) = candidates.remove(&step.id) else {
            continue;
        };
        let id = &step.id;
        if !candidate.proposal.writes.is_empty() {
            return Err(failed(format!(
                "run {of}: step {id}'s recorded result makes writes when it is published, and a                  replay never makes them again"
            )));
        }
        let input_sha256 = candidate
            .proposal
            .metadata
            .get("input_sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| failed(format!("run {of}: step {id} recorded no input digest")))?
            .to_owned();
        recorded.insert(
            id.clone(),
            Recorded {
                proposal: candidate.proposal,
                input_sha256,
            },
        );
    }
    Ok(recorded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> PipelineRun {
        serde_json::from_value(json!({
            "pipeline": {
                "name": "notes",
                "steps": [
                    {"kind": "atom", "id": "read", "atom": "read-note", "with": {}},
                    {"kind": "atom", "id": "rank", "atom": "rank-note", "with": {}},
                ],
                "returns": "steps",
                "input": {},
                "output": {},
                "atoms": {},
                "pins": {},
            },
            "input": {},
        }))
        .expect("a run")
    }

    fn candidate(writes: Vec<atomic_durable::Write>) -> atomic_durable::Candidate {
        atomic_durable::Candidate {
            id: "recorded:3".into(),
            proposal: Proposal {
                output: json!({"title": "x"}),
                metadata: json!({"thread_id": "t-1", "input_sha256": "abc"}),
                writes,
            },
            digest: String::new(),
        }
    }

    #[test]
    fn a_replay_serves_only_what_was_recorded_for_the_same_input() {
        let recorded = recorded_results(
            "recorded",
            &run(),
            BTreeMap::from([
                ("read".to_owned(), candidate(Vec::new())),
                ("gone".to_owned(), candidate(Vec::new())),
            ]),
        )
        .expect("recorded");
        assert_eq!(recorded.keys().collect::<Vec<_>>(), ["read"]);
        let replay = Replay {
            of: "recorded".into(),
            recorded,
        };
        // The recorded output, without the recorded run's thread.
        let served = replay.serve("read", "abc").expect("served");
        assert_eq!(served.output, json!({"title": "x"}));
        assert_eq!(served.metadata, json!({"input_sha256": "abc"}));
        // Another input, or a step with no result, is refused.
        let e = replay.serve("read", "abd").expect_err("another input");
        assert!(e.to_string().contains("another input"), "{e}");
        let e = replay.serve("rank", "abc").expect_err("unrecorded");
        assert!(
            e.to_string().contains("no recorded result for step rank"),
            "{e}"
        );
    }

    #[test]
    fn a_recorded_result_that_writes_is_not_replayed() {
        let writes = vec![atomic_durable::Write {
            sql: "UPDATE t SET n = 1".into(),
            params: Vec::new(),
        }];
        let e = recorded_results(
            "recorded",
            &run(),
            BTreeMap::from([("read".to_owned(), candidate(writes))]),
        )
        .expect_err("writes");
        assert!(e.to_string().contains("never makes them again"), "{e}");
        let mut unhashed = candidate(Vec::new());
        unhashed.proposal.metadata = json!({});
        let e = recorded_results(
            "recorded",
            &run(),
            BTreeMap::from([("read".to_owned(), unhashed)]),
        )
        .expect_err("no digest");
        assert!(e.to_string().contains("no input digest"), "{e}");
    }
}
