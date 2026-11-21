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
//! steps before it, then runs the atom's turn (`<id>.turn`, which
//! checkpoints a private candidate) and publishes it (`<id>.publish`) as
//! sibling durable steps, so a step that finished before a crash is never
//! run again. CEL has no side effects, so mappings evaluated again on replay
//! give the same values.
//!
//! A step with a `review` has its candidate reviewed before it is
//! published: each verdict (`<id>.review.<n>`) and each repair round of the
//! author (`<id>.repair.<n>`) is a sibling step of its own, and a review that
//! does not approve goes to a person at gate `<id>.review`. What accepted
//! the candidate is recorded as `<id>.accepted`.
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
use atomic_core::runtime::{Runtimes, contained, run_declarative_repairing};
use atomic_core::workflow::Workflow;
use atomic_core::worktree::{StepError, StepRequest, Worktrees};
use atomic_core::{AtomError, compile};
use atomic_durable::attempt;
use atomic_durable::gate::{self, Deployment, GateSpec, Resolution, Tier};
use atomic_durable::usage::Ledger;
use atomic_durable::{
    Acceptance, Candidate, DurableError, DurableRuntime, Proposal, Published, Result,
    WorkflowContext, execute_candidate_told, failpoint, publish_candidate, review_candidate,
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
pub const LAYOUT_VERSION: &str = "atomic.pipeline.7";

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
        // A step that stopped for an artifact store held elsewhere, as a pin
        // does, takes nothing toward dead-lettering, nor does it get further.
        if position == *top && !atomic_core::runtime::take_held_stop(id) {
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
    // A step waiting for room on the execution backend gives up its wait.
    atomic_core::runtime::stop_waiting_for_backend();
}

/// Before a run's next piece of work: go on counted as working, or wait for
/// good if this process is stopping.
async fn keep_working(working: &mut Option<Running>, run: &str) {
    match working {
        None => *working = Some(begin_work(run).await),
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
async fn begin_work(run: &str) -> Running {
    let running = Running::start(run);
    if STOPPING.load(std::sync::atomic::Ordering::SeqCst) {
        drop(running);
        return std::future::pending::<Running>().await;
    }
    running
}

/// How many atom steps this process is running now.
/// The runs working now, each as many times as it counts.
static RUNNING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// How many atom steps this process is running now: a stopping process
/// waits for them to reach their checkpoints. A run whose step is set aside,
/// waiting past its limit for room ([`atomic_core::runtime`]), is not one;
/// a set-aside step of a run not counted here takes nothing off.
pub fn steps_running() -> usize {
    let aside = atomic_core::runtime::runs_set_aside_for_room();
    let counted = RUNNING
        .lock()
        .map(|runs| runs.iter().filter(|run| !aside.contains(*run)).count())
        .unwrap_or_default();
    counted.saturating_sub(SET_ASIDE.load(std::sync::atomic::Ordering::SeqCst))
}

/// How many of the [`RUNNING`] runs wait for good, set aside by
/// [`crate::workflows::settled`] in `atomic serve`.
static SET_ASIDE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Stop counting, in [`steps_running`], a run that waits for good.
pub(crate) fn set_aside() {
    SET_ASIDE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Counts one run's work in [`steps_running`] while it lives.
struct Running(String);

impl Running {
    fn start(run: &str) -> Self {
        if let Ok(mut runs) = RUNNING.lock() {
            runs.push(run.to_owned());
        }
        Self(run.to_owned())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Ok(mut runs) = RUNNING.lock()
            && let Some(at) = runs.iter().position(|run| *run == self.0)
        {
            runs.swap_remove(at);
        }
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
    /// The review its output gets before it is published, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewStep>,
}

/// An atom step's review: a reviewer atom's typed verdict on each candidate,
/// a repair round for the author on a rejection, and a person's decision on
/// an escalation or when the rounds run out. Nothing passes it silently.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewStep {
    /// The reviewer atom, by name.
    pub reviewer: String,
    /// Each of the reviewer's input fields and the CEL expression that
    /// computes it, over the step's scope and `candidate`.
    pub with: BTreeMap<String, String>,
    /// Each mapped field's origin.
    #[serde(default)]
    pub origins: BTreeMap<String, String>,
    /// How many repair rounds the author gets before the review escalates.
    pub rounds: u64,
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
    /// For a question gate (`ask` in the workflow file, which is then the
    /// `title` above), what it asks for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<QuestionStep>,
}

/// What a question gate asks for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionStep {
    /// The JSON Schema the answer must fit.
    pub answer: Value,
    /// Whether the answer goes into the feedback log.
    pub remember: bool,
    /// The answer the step resumes with if nobody answers in time; without
    /// one, an unanswered question stops the run as an expired gate does.
    /// `Some(null)` is a null default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub default: Option<Value>,
}

/// A field that is present, even as `null`, is `Some`.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Value>, D::Error> {
    <Value as serde::Deserialize>::deserialize(d).map(Some)
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
    /// The fixture repository a coding run is bound to (see
    /// [`atomic_core::fixture`]): its worktree steps run in worktrees of the
    /// fixture's snapshot, never of the developer's checkout. Absent for a
    /// run that changes no code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixture: Option<atomic_core::fixture::FixtureBinding>,
}

impl Pipeline {
    /// Whether a step of it changes code, in a worktree.
    pub fn changes_code(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s, Step::Atom(a) if a.worktree))
    }
}

/// The worktrees a run's worktree steps use: those of its fixture's
/// snapshot when it is bound to one, else the services' own.
fn worktrees_for(pipeline: &Pipeline, services: &Services) -> Result<Option<Arc<Worktrees>>> {
    match &pipeline.fixture {
        Some(fixture) => {
            let git = atomic_core::git::Git::find()
                .map_err(|e| failed(format!("the fixture's snapshot: {e}")))?;
            let repo = git
                .repo(&fixture.snapshot)
                .without_filters()
                .map_err(|e| failed(format!("the fixture's snapshot: {e}")))?;
            Ok(Some(Arc::new(Worktrees::new(
                repo,
                fixture.worktrees.clone(),
            ))))
        }
        None => Ok(services.worktrees.clone()),
    }
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
    /// What names the run's build caches beside its id, chosen when it
    /// started (see `Worktrees::caches`); `None` in a run recorded before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caches: Option<String>,
}

/// A fresh name for a run's build caches: random, or, with no randomness,
/// the time.
fn cache_nonce() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        bytes = (now as u64).to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
        let review = match step.get("review") {
            None => None,
            Some(r) => {
                let reviewer = r
                    .get("reviewer")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(format!("step `{id}`: `review` names no reviewer")))?;
                let atom = atoms.get(reviewer).ok_or_else(|| {
                    fail(format!(
                        "step `{id}`'s review names `{reviewer}`, which is not loaded"
                    ))
                })?;
                let mut with = BTreeMap::new();
                let mut origins = BTreeMap::new();
                for (field, source) in r
                    .get("with")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    let source = source.as_str().ok_or_else(|| {
                        fail(format!(
                            "step `{id}`'s review maps `{field}` to a value that is not a CEL \
                             expression"
                        ))
                    })?;
                    // What it reads of the candidate, the author's output,
                    // is taken as external: the least trusted.
                    let allowed = atomic_core::workflow::review_origin(source, &kinds)
                        .map_err(|e| fail(format!("step `{id}`: `review.with.{field}`: {e}")))?;
                    with.insert(field.clone(), source.to_owned());
                    origins.insert(field.clone(), allowed.to_owned());
                }
                for (source, text) in &atom.program.sources {
                    pins.insert(source.clone(), sha256(text.as_bytes()));
                }
                used.insert(reviewer.to_owned(), atom.clone());
                // Who decides the review's gate, should it open, resolves now,
                // before anything runs, as any gate's approvers do.
                gate::resolve(&format!("{id}.review"), None, deployment)
                    .map_err(|e| fail(format!("step `{id}`'s review: {e}")))?;
                Some(ReviewStep {
                    reviewer: reviewer.to_owned(),
                    with,
                    origins,
                    rounds: r.get("rounds").and_then(Value::as_u64).unwrap_or(1),
                })
            }
        };
        kinds.insert(id.clone(), atom.kind.clone());
        steps.push(Step::Atom(AtomStep {
            id: id.clone(),
            atom: name.to_owned(),
            with,
            worktree,
            origins,
            review,
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
        fixture: None,
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
    let title = text("title")
        .or_else(|| text("ask"))
        .ok_or_else(|| format!("gate `{id}` has no `title`"))?;
    let question = match gate.get("answer") {
        None => None,
        Some(answer) => Some(QuestionStep {
            answer: atomic_core::workflow::gate_answer(answer)
                .map_err(|e| format!("gate `{id}`: {e}"))?,
            remember: gate
                .get("remember")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            default: gate
                .get("on_timeout")
                .and_then(|t| t.get("default"))
                .cloned(),
        }),
    };
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
        question,
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
/// between them) before it spends one of durare's recoveries. An exec
/// step giving its command its inputs waits the same.
const LOCKED_RETRIES: u32 = atomic_core::runtime::STORE_HELD_RETRIES;
const LOCKED_BACKOFF: std::time::Duration = atomic_core::runtime::STORE_HELD_BACKOFF;

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
            Err(e @ ArtifactError::Locked { .. }) if retries < LOCKED_RETRIES => {
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
                // A stopping process waits no longer: the run stops as it
                // does once the tries run out.
                if atomic_core::runtime::sleep_unless_stopping(backoff) {
                    return Err(e);
                }
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
    let exec: Vec<(&str, &Budget)> = pipeline
        .atoms
        .iter()
        .filter(|(_, atom)| atom.kind == "exec")
        .map(|(name, atom)| (name.as_str(), &atom.budget))
        .collect();
    BudgetPlan::resolve(settings, &pipeline.budget, &atoms, &exec).map_err(|(atom, e)| match atom {
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
    why: atomic_core::usage::Why,
) -> Result<Value> {
    let id = ctx.workflow_id().to_owned();
    let mut caches = None;
    let result = run_pipeline_in(ctx, run, services, ledger, replay, &why, &mut caches).await;
    if let Some(caches) = caches {
        settle_caches(&id, caches, &result).await;
    }
    result
}

/// Remove run `id`'s build caches if the run ended as `result` says: only
/// a run that finished or failed, whose outcome is recorded, leaves none
/// (one resumed builds cold). Any other end keeps them: a run stopped for
/// recovery, parked for a person, or one whose outcome was not recorded,
/// such as an execution that lost a race to another still building in the
/// same caches.
async fn settle_caches(id: &str, caches: atomic_core::worktree::Caches, result: &Result<Value>) {
    if matches!(result, Ok(_) | Err(DurableError::Failed(_))) {
        remove_caches(id, caches).await;
    }
}

/// Remove run `id`'s build caches, saying so if they cannot be: what is
/// left costs disk, never a later run its isolation.
async fn remove_caches(id: &str, caches: atomic_core::worktree::Caches) {
    let dir = caches.dir().to_owned();
    let e = match tokio::task::spawn_blocking(move || caches.remove()).await {
        Ok(Ok(())) => return,
        Ok(Err(e)) => e.to_string(),
        Err(e) => e.to_string(),
    };
    {
        atomic_core::log_line!(
            "atomic: run {id}: its build caches at {} could not be removed ({e}); remove them by \
             hand",
            dir.display()
        );
    }
}

/// [`run_pipeline`]'s work, leaving in `run_caches` the run's build caches,
/// named before its first step, for [`run_pipeline`] to settle when the run
/// ends.
async fn run_pipeline_in<C: WorkflowContext>(
    ctx: C,
    run: PipelineRun,
    services: Arc<Services>,
    ledger: Ledger,
    replay: Option<Arc<Replay>>,
    why: &atomic_core::usage::Why,
    run_caches: &mut Option<atomic_core::worktree::Caches>,
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
    let mut working = Some(begin_work(&id).await);
    reached(&id, 0)?;
    let store = services.store.clone();
    fits("the workflow's input", &pipeline.input, &input)?;
    check_reviews(&pipeline)?;
    plan(&pipeline, services.runtimes.backend.statement())?;
    // Every step is held to its atom's trust before the run does anything.
    trusted(&pipeline).map_err(failed)?;
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
            // And every exec atom its caps, which its command is held to.
            let exec_atoms: Vec<&str> = pipeline
                .atoms
                .iter()
                .filter(|(_, atom)| atom.kind == "exec")
                .map(|(name, _)| name.as_str())
                .collect();
            // And held to the ceilings and prices of the person running it
            // now, as well as to the ceilings it carries: a plan made
            // elsewhere, or with none, cannot raise what their own
            // configuration caps, nor price a call below what it is here.
            services
                .budgets
                .validate()
                .map_err(|e| e.to_string())
                .and_then(|()| plan.validate())
                .and_then(|()| {
                    plan.held_to(
                        &services.budgets.ceilings,
                        "your own configuration's ceilings",
                    )
                })
                .and_then(|()| plan.priced_no_lower(&services.budgets))
                .and_then(|()| plan.covers(&model_atoms, &exec_atoms))
                .map(|()| BudgetPlan {
                    held: true,
                    ..plan.clone()
                })
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
                        caches: Some(cache_nonce()),
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
                caches: Some(cache_nonce()),
            })
        })
        .await?;
    let plan = pinned.budgets.clone();
    // A plan recorded before plans were held to the person's ceilings and
    // prices, as a run resumed or recovered from then has, is held to those
    // of the person running it now. A plan held when it was made keeps its
    // own, so a resumed run keeps the ceilings and prices it began under.
    if !plan.held {
        let now = &services.budgets;
        now.validate()
            .map_err(|e| e.to_string())
            .and_then(|()| plan.held_to(&now.ceilings, "your own configuration's ceilings"))
            .and_then(|()| plan.priced_no_lower(now))
            .map_err(|e| {
                failed(format!(
                    "workflow {}: its budget, recorded before budgets were held to your own \
                     ceilings and prices: {e}; start the run again to plan it under your \
                     current configuration",
                    pipeline.name
                ))
            })?;
    }
    // Under a dollar cap of the person's own, a model only a layer above them prices
    // is said, so a price they did not set is never counted unseen.
    let unvouched: Vec<&str> = plan
        .models
        .values()
        .filter(|model| services.budgets.priced_by_project.contains(*model))
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if !unvouched.is_empty() {
        atomic_core::log_line!(
            "atomic: run {}: under your own dollar cap, the run calls {} at prices only a \
             layer above your own configuration sets; `atomic config --explain` says more",
            ctx.workflow_id(),
            unvouched.join(", ")
        );
    }
    let budget = RunBudget::pinned(ledger, &plan);
    let (caps, models, exec_caps) = (plan.atoms, plan.models, plan.exec);
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
    // The run's worktrees, found once and off the async workers, since
    // finding them runs git.
    let run_worktrees = if pipeline.changes_code() {
        let (p, s) = (pipeline.clone(), services.clone());
        tokio::task::spawn_blocking(move || worktrees_for(&p, &s))
            .await
            .map_err(|e| failed(format!("the run's worktrees: {e}")))??
    } else {
        None
    };
    // The run's build caches, named once for all its worktree and exec
    // steps from what its first step recorded, with the person's rustup
    // toolchains found once; named before any step, so a run that ends
    // before the first step that uses them still removes what an earlier
    // execution left. Made only when a step uses them.
    let rustup = || atomic_core::worktree::rustup_home(&|n| std::env::var(n).ok());
    let named = match (&services.caches, &run_worktrees) {
        (Some(root), _) => Some(atomic_core::worktree::Caches::for_run(
            root,
            ctx.workflow_id(),
            pinned.caches.as_deref(),
            rustup(),
        )),
        (None, Some(worktrees)) => {
            Some(worktrees.caches(ctx.workflow_id(), pinned.caches.as_deref(), rustup()))
        }
        (None, None) => None,
    };
    if let Some(named) = named {
        *run_caches = Some(named.map_err(|e| failed(format!("the run's build caches: {e}")))?);
    }
    let mut accepted = Vec::new();
    for (position, step) in pipeline.steps.iter().enumerate() {
        // Before its attempt is recorded: a stopping process begins nothing.
        keep_working(&mut working, &id).await;
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
                    why.clone(),
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
        let atom_input = Value::Object(mapped);
        let run = RunScope {
            pipeline: &pipeline,
            services: &services,
            budget: &budget,
            caps: &caps,
            exec_caps: &exec_caps,
            models: &models,
            replay: &replay,
            pinned: &pinned,
            worktrees: run_worktrees.as_ref(),
            accepted: &accepted,
            caches: run_caches.as_ref(),
            why,
        };
        used_worktrees |= step.worktree;
        let (candidate, told) = turn(
            &ctx,
            &run,
            Turn {
                name: step.id.clone(),
                atom: step.atom.clone(),
                input: atom_input.clone(),
                origins: step.origins.clone(),
                worktree: step.worktree,
                repair: None,
                charge: None,
            },
        )
        .await?;
        // A replay serves what review accepted when the run was recorded,
        // and reviews nothing again.
        let (candidate, acceptance) = match (&step.review, &replay) {
            (Some(review), None) => {
                let (candidate, acceptance) = reviewed(
                    &ctx,
                    &run,
                    &mut working,
                    step,
                    review,
                    &values,
                    atom_input,
                    (candidate, told),
                )
                .await?;
                // What accepted it, kept with the run's record.
                let kept = acceptance.clone();
                let acceptance: Acceptance = ctx
                    .step(&format!("{}.accepted", step.id), move |_| async move {
                        Ok(kept)
                    })
                    .await?;
                (candidate, acceptance)
            }
            _ => {
                let acceptance = review_candidate(&candidate);
                (candidate, acceptance)
            }
        };
        let output = publish_candidate(
            &ctx,
            &format!("{}.publish", step.id),
            &candidate,
            &acceptance,
            Vec::new(),
        )
        .await?;
        let published = Published {
            candidate: candidate.id,
            output,
            metadata: candidate.proposal.metadata,
        };
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
    keep_working(&mut working, &id).await;
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
    if used_worktrees && let Some(worktrees) = run_worktrees.clone() {
        // The run's worktrees and build caches go; its accepted commits keep
        // their refs. A repeat finds less to remove and removes it.
        let run_id = ctx.workflow_id().to_owned();
        let caches = run_caches.clone();
        let _removed: usize = ctx
            .step("cleanup", move |_| async move {
                if let Some(caches) = caches {
                    // Never in the way of removing the worktrees.
                    remove_caches(&run_id, caches).await;
                }
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

/// What one atom turn needs of the run it is a step of.
#[derive(Clone, Copy)]
struct RunScope<'a> {
    pipeline: &'a Pipeline,
    services: &'a Arc<Services>,
    budget: &'a RunBudget,
    caps: &'a BTreeMap<String, atomic_core::budget::Resolved>,
    /// Each exec atom's caps, which its command's active time is charged to.
    exec_caps: &'a BTreeMap<String, atomic_core::budget::Resolved>,
    models: &'a BTreeMap<String, String>,
    replay: &'a Option<Arc<Replay>>,
    pinned: &'a Pinned,
    /// The run's worktrees: its fixture's, or the project's.
    worktrees: Option<&'a Arc<Worktrees>>,
    /// The commits its earlier worktree steps made.
    accepted: &'a [String],
    /// The run's build caches, shared by its worktree and exec turns,
    /// repairs and reviews included.
    caches: Option<&'a atomic_core::worktree::Caches>,
    /// What this run's spans say of a step that failed (see
    /// [`atomic_core::usage::Why`]).
    why: &'a atomic_core::usage::Why,
}

/// One turn of an atom: a step's author, a reviewer, or the author's repair
/// round, each its own durable operations under `name`.
struct Turn {
    /// The operations' name: the step's id, `<id>.review.<n>` or
    /// `<id>.repair.<n>`.
    name: String,
    /// The atom, by name.
    atom: String,
    /// Its input, mapped.
    input: Value,
    /// Each input field's origin.
    origins: BTreeMap<String, String>,
    /// Whether it runs in a worktree of its own.
    worktree: bool,
    /// For a repair round: the rejected output and the verdict asking for
    /// changes.
    repair: Option<Value>,
    /// The position of the turn whose atom allowance this one is charged to,
    /// if not its own: a repair round's is the author's first turn, a later
    /// verdict's the first verdict.
    charge: Option<i32>,
}

/// Run turn `t` of an atom as a step of `run` would: its clock and context,
/// its caps and pinned model, an exec atom's attempt guard, a worktree, a
/// replay's recorded result, and metering. Checkpoints the turn's result as
/// a private candidate under `<name>.turn`; the caller publishes it.
async fn turn<C: WorkflowContext>(
    ctx: &C,
    run: &RunScope<'_>,
    t: Turn,
) -> Result<(Candidate, i32)> {
    let RunScope {
        pipeline,
        services,
        budget,
        caps,
        models,
        replay,
        pinned,
        why,
        ..
    } = *run;
    // A fixture is its own repository (see `in_worktree`).
    let bound = pipeline.fixture.is_some();
    let atom = pipeline
        .atoms
        .get(&t.atom)
        .cloned()
        .ok_or_else(|| failed(format!("step `{}`: no atom `{}`", t.name, t.atom)))?;
    let mut runtimes = (*services.runtimes).clone();
    // What this run's spans say of a failure. Each run of this step notes on a
    // handle of its own, which the run takes up only when that step failed
    // (see [`atomic_core::usage::Why`] and [`crate::telemetry::traced_step`]).
    let run_why = (*why).clone();
    // An atom with captured helpers gets its step's logical time and seed
    // from a step of its own, recorded once and read back on every retry,
    // so its helpers render the same prompt and reach the same validator
    // decisions after a crash.
    if !atom.program.helpers.is_empty() {
        let clock: StepClock = ctx
            .step(&format!("{}.clock", t.name), |_| async {
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
        let (resolving, run, id) = (atom.clone(), run_id.clone(), t.name.clone());
        let recorded: Value = ctx
            .step(&format!("{}.context", t.name), move |_| async move {
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
            atomic_core::context::ContextSnapshot::restore(&atom, &run_id, recorded)
                .map_err(|e| failed(format!("step `{}`: its recorded context: {e}", t.name)))?,
        )
    } else {
        None
    };
    let atom_input = t.input.clone();
    // Recorded with the step's result, so a replay serves it only to the
    // same input.
    let input_sha256 = sha256(
        &serde_json::to_vec(&atom_input)
            .map_err(|e| failed(format!("step `{}`: encode its input: {e}", t.name)))?,
    );
    // A model atom's caps, or an exec atom's, which its command's active
    // time is charged to.
    let (budget, atom_caps) = (
        budget.clone(),
        caps.get(&t.atom)
            .or_else(|| run.exec_caps.get(&t.atom))
            .cloned(),
    );
    // A model atom runs on the model its budget was resolved for, and
    // only with caps: never unmetered, never at another model's price.
    if let Some(model) = model_of(&atom, &runtimes) {
        let pinned = match (&atom_caps, models.get(&t.atom)) {
            (Some(_), Some(pinned)) => pinned.clone(),
            _ => {
                return Err(failed(format!(
                    "step `{}`: `{}` calls a model, and the run's budget has no caps or \
                         model for it",
                    t.name, atom.name
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
                t.name, atom.name
            )));
        }
    }
    // An exec atom's command runs only with caps, never unmetered.
    if atom.kind == "exec" && atom_caps.is_none() {
        return Err(failed(format!(
            "step `{}`: `{}` runs a command, and the run's budget has no caps for it",
            t.name, atom.name
        )));
    }
    let tree = if t.worktree {
        let worktrees = run.worktrees.cloned().ok_or_else(|| {
            failed(format!(
                "step `{}` asks for a worktree, and the run is bound to no fixture \
                 repository: a run started before atomic bound coding runs to one cannot \
                 continue; start it again with --fixture and --fixture-rev",
                t.name
            ))
        })?;
        let base = atom_input
            .get("base")
            .and_then(Value::as_str)
            .ok_or_else(|| failed(format!("step `{}`: `base` is not a commit", t.name)))?;
        // A bound run starts each worktree from its fixture's commit or
        // one an earlier step of its own made, never from another commit
        // the snapshot holds, such as another run's.
        if let Some(fixture) = &pipeline.fixture {
            let (fixture, base, own) = (fixture.clone(), base.to_owned(), run.accepted.to_vec());
            tokio::task::spawn_blocking(move || {
                atomic_core::fixture::check_base(&fixture, &base, &own)
            })
            .await
            .map_err(|e| failed(format!("check the step's base: {e}")))?
            .map_err(|e| failed(format!("step `{}`: {e}", t.name)))?;
        }
        let caches = run
            .caches
            .cloned()
            .ok_or_else(|| failed("the run's build caches were never named".into()))?;
        Some((
            worktrees,
            caches,
            StepRequest {
                run: run_id.clone(),
                step: t.name.clone(),
                base: base.to_owned(),
                message: format!("{}: {}", pipeline.name, t.name),
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
            step_id: t.name.clone(),
        };
        let (opening, who, backend) = (atom.clone(), identity.clone(), runtimes.backend.shared());
        let (giving, artifacts) = (atom_input.clone(), runtimes.codex.artifacts.clone());
        // The run's own build caches, with a Cargo target of the turn's own.
        let exec_caches = run.caches.map(|c| c.for_exec(&t.name));
        let place = format!("run {run_id}, step {}", t.name);
        let attempt_step = format!("{}.attempt", t.name);
        let (begun, opened) = if replay.is_some() {
            (attempt::begin(ctx, &attempt_step).await?, None)
        } else {
            attempt::begin_with(ctx, &attempt_step, move || async move {
                let mut opened = atomic_core::runtime::open_exec_workspace(
                    &opening,
                    &who,
                    backend,
                    exec_caches.as_ref(),
                )
                .await;
                // Its inputs are given here, before the attempt is recorded:
                // a store held too long then stops the step with nothing
                // run and nothing recorded, and the run is run again.
                if let (Ok(workspace), Ok(compiled)) =
                    (&opened, compile::compile(&opening, &Registry::new()))
                    && let Err(e) = atomic_core::runtime::give_exec_inputs(
                        &opening, &compiled, &giving, &who, workspace, &artifacts,
                    )
                    .await
                {
                    opened = Err(e);
                }
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
    let origins = t.origins.clone();
    let pinned_helpers = pinned.helper_runtime.clone();
    let (id, ran) = (t.name.clone(), format!("pipeline:ran:{}", t.name));
    let place = format!("run {run_id}, step {}", t.name);
    let serving = replay.clone();
    let repair = t.repair.clone();
    let charge = t.charge;
    let step_caches = run.caches.map(|c| c.for_exec(&t.name));
    // What an exec step's command hands over is held until the turn has
    // recorded the output that references it, then let go; and let go too
    // when the turn is dropped or panics before that.
    let release = guard
        .as_ref()
        .map(|(_, _, _, identity)| runtimes.codex.artifacts.releasing(identity.clone()));
    let turn = execute_candidate_told(ctx, &format!("{}.turn", t.name), move |position| {
        // The turn runs again after a park is released, so each run
        // takes its own copies.
        let mut runtimes = runtimes.clone();
        // What this run of this step's model calls say of a failure of their
        // own, which its span says in turn.
        let why = atomic_core::usage::Why::for_step();
        let step_caches = step_caches.clone();
        let (atom, atom_input, origins) = (atom.clone(), atom_input.clone(), origins.clone());
        let repair = repair.clone();
        let (id, ran, run_id) = (id.clone(), ran.clone(), run_id.clone());
        let (snapshot, tree, guard) = (snapshot.clone(), tree.clone(), guard.clone());
        let pinned_helpers = pinned_helpers.clone();
        let place = place.clone();
        // In a replay, the step's recorded result, with no model call
        // and no command run; a step with none fails rather than run
        // live.
        let served = serving.as_ref().map(|r| r.serve(&id, &input_sha256));
        let input_sha256 = input_sha256.clone();
        // A model atom's calls, or an exec atom's command.
        if let Some(atom_caps) = &atom_caps {
            let meter = match charge {
                Some(allowance) => budget.meter_charged(&position, allowance, atom_caps),
                None => budget.meter(&position, atom_caps),
            };
            runtimes.codex.meter = meter.clone();
            runtimes.lite.meter = meter.clone();
            runtimes.exec_meter = meter;
        }
        // Made here, in the step's body, so a step that replays from
        // its checkpoint makes none.
        let span = crate::telemetry::step_span(&atom.name, &id, &run_id);
        let caller = (run_id.clone(), id.clone());
        let (ledgered, metered, at) = (budget.clone(), atom_caps.is_some(), position.clone());
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
            // The command may have run before when an earlier execution's
            // attempt replays as interrupted, or when, in this process, an
            // earlier run of the body took the workspace this attempt
            // opened. Either way it may never have started: a cap refused it
            // and the step parked. For a metered step the ledger says which:
            // only a command admitted may have run. An unmetered one is
            // taken to have run.
            let (replayed, taken) = match &guard {
                Some((begun, false, opened, _)) => {
                    let replayed = begun.interrupted();
                    // A replayed attempt opened no workspace here.
                    (
                        replayed,
                        !replayed && opened.lock().map_or(true, |o| o.is_none()),
                    )
                }
                _ => (false, false),
            };
            let admitted = if (replayed || taken) && metered {
                ledgered
                    .command_admitted(&at)
                    .await
                    .map_err(|e| failed(format!("step `{id}`: read its command's attempts: {e}")))?
            } else {
                true
            };
            if let Some(why) = ran_before(&atom.name, replayed && admitted, taken && admitted) {
                return Err(failed(format!(
                    "step `{id}`: outcome_unknown: {why}, so whether its command's effects \
                     happened is unknown. The step is parked and its command does not run \
                     again. If repeating it is safe, declare `effects: {{ external: \
                     'idempotent' }}` (or `'read-only'` if it only reads)"
                )));
            }
            let proposal = match (tree, guard) {
                (_, Some((_, _, opened, identity))) => {
                    let clock = runtimes.clock.clone();
                    let backend = runtimes.backend.shared();
                    let exec_caches = step_caches.clone();
                    // The store the model runtimes check references against.
                    let artifacts = runtimes.codex.artifacts.clone();
                    let meter = runtimes.exec_meter.clone();
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
                                    &atom,
                                    &identity,
                                    backend,
                                    exec_caches.as_ref(),
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
                            &meter,
                        )
                        .await
                    })
                    .await
                }
                (None, None) => {
                    propose_at(&place, true, async move {
                        run_in(
                            &atom,
                            &atom_input,
                            &origins,
                            &run_id,
                            &id,
                            snapshot.as_ref(),
                            repair.as_ref(),
                            &runtimes,
                        )
                        .await
                    })
                    .await
                }
                (Some((worktrees, caches, request)), None) => {
                    let runtimes = Arc::new(runtimes);
                    let call = StepCall {
                        atom,
                        input: atom_input,
                        origins,
                        snapshot,
                        run_id,
                        repair,
                    };
                    in_worktree(worktrees, caches, request, call, runtimes, &place, bound).await
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
        // Model calls name the run and step on their spans, and say why they
        // failed here and, if this step is what failed the run, on its span
        // too (see `mark_failed`).
        crate::telemetry::traced_step(
            span,
            why.clone(),
            &run_why,
            atomic_core::usage::for_step(caller.0, caller.1, why, work),
        )
    })
    .await;
    drop(release);
    turn
}

/// Review `candidate`, the author's turn of `step` on `input`: the reviewer
/// gives a typed verdict on it (`<id>.review.<n>`); a request for changes
/// with rounds left hands the verdict to the author for a repair round
/// (`<id>.repair.<n>`), whose candidate is reviewed in turn. Any other
/// verdict but an approval, or a request for changes with no rounds left,
/// opens gate `<id>.review` for a person: an approval accepts the
/// candidate, anything else fails the run. The candidate stays private
/// until it is accepted; nothing is accepted without a reviewer's approval
/// or a person's.
///
/// Each review and repair turn begins only while the process is not
/// stopping (see [`keep_working`]), and waiting for the person is not work
/// a stop waits for. Repair rounds are charged to the author's first turn's
/// atom allowance, later verdicts to the first verdict's, so the rounds
/// share one allowance each rather than taking a fresh one per round.
#[allow(clippy::too_many_arguments)]
async fn reviewed<C: WorkflowContext>(
    ctx: &C,
    run: &RunScope<'_>,
    working: &mut Option<Running>,
    step: &AtomStep,
    review: &ReviewStep,
    values: &[(&str, &Value, &Shape<'_>); 2],
    input: Value,
    (mut candidate, author_turn): (Candidate, i32),
) -> Result<(Candidate, Acceptance)> {
    let author = run
        .pipeline
        .atoms
        .get(&step.atom)
        .and_then(|a| a.program.contracts.get("Output"));
    let mut seen_shape = vec![("output", author.map_or(Shape::Untyped, Shape::Schema))];
    let commit_schema = json!({"type": "object", "properties": {"commit": {"type": "string"}}});
    if step.worktree {
        seen_shape.push(("metadata", Shape::Schema(&commit_schema)));
    }
    let candidate_shape = Shape::fields(seen_shape);
    let mut first_verdict = None;
    let mut round = 0;
    let (verdict, said) = loop {
        let mut seen = json!({"output": candidate.proposal.output});
        if step.worktree {
            seen["metadata"] = json!({
                "commit": candidate.proposal.metadata.get("commit").cloned().unwrap_or(Value::Null)
            });
        }
        let scope = [values[0], values[1], ("candidate", &seen, &candidate_shape)];
        let mut mapped = Map::new();
        for (field, source) in &review.with {
            let value = atomic_schema::expr::evaluate(source, &scope)
                .map_err(|e| failed(format!("step `{}`: `review.with.{field}`: {e}", step.id)))?;
            mapped.insert(field.clone(), value);
        }
        let name = format!("{}.review.{round}", step.id);
        keep_working(working, ctx.workflow_id()).await;
        let (verdict, told) = turn(
            ctx,
            run,
            Turn {
                name: name.clone(),
                atom: review.reviewer.clone(),
                input: Value::Object(mapped),
                origins: review.origins.clone(),
                worktree: false,
                repair: None,
                charge: first_verdict,
            },
        )
        .await?;
        first_verdict = first_verdict.or(Some(told));
        // The verdict is a reviewer's result like any atom's: published,
        // so the record shows what it decided.
        let verdict = publish_candidate(
            ctx,
            &format!("{name}.publish"),
            &verdict,
            &review_candidate(&verdict),
            Vec::new(),
        )
        .await?;
        let said = verdict
            .get("verdict")
            .and_then(Value::as_str)
            .unwrap_or("escalate")
            .to_owned();
        match atomic_core::workflow::Verdict::of(&said) {
            atomic_core::workflow::Verdict::Approve => {
                let acceptance = Acceptance {
                    candidate: candidate.id.clone(),
                    digest: candidate.digest.clone(),
                    accepted: true,
                    reason: Some(format!(
                        "approved by reviewer {} in review round {round}",
                        review.reviewer
                    )),
                };
                return Ok((candidate, acceptance));
            }
            atomic_core::workflow::Verdict::Changes if round < review.rounds => {
                round += 1;
                keep_working(working, ctx.workflow_id()).await;
                (candidate, _) = turn(
                    ctx,
                    run,
                    Turn {
                        name: format!("{}.repair.{round}", step.id),
                        atom: step.atom.clone(),
                        input: input.clone(),
                        origins: step.origins.clone(),
                        worktree: step.worktree,
                        repair: Some(json!({
                            "output": candidate.proposal.output,
                            "review": verdict,
                        })),
                        charge: Some(author_turn),
                    },
                )
                .await?;
            }
            _ => break (verdict, said),
        }
    };
    // A person decides. The gate's title is atomic's own words; what the
    // reviewer and the author said is what it shows.
    let id = format!("{}.review", step.id);
    let why = match (atomic_core::workflow::Verdict::of(&said), said.as_str()) {
        (atomic_core::workflow::Verdict::Changes, _) => {
            format!("asked for changes after {round} repair round(s), the most this step allows")
        }
        (_, "reject") => "rejected it".to_owned(),
        (_, "insufficient_context") => "could not judge it with the context it had".to_owned(),
        _ => "escalated it".to_owned(),
    };
    let approvers = gate::resolve(&id, None, &run.services.deployment)
        .map_err(|e| failed(format!("gate `{id}`: {e}")))?;
    let mut show = json!({"output": candidate.proposal.output, "verdict": verdict});
    if step.worktree {
        show["commit"] = candidate
            .proposal
            .metadata
            .get("commit")
            .cloned()
            .unwrap_or(Value::Null);
    }
    let spec = GateSpec {
        workflow: run.pipeline.name.clone(),
        title: format!(
            "Step {} of {}: reviewer {} {why}. Approve its output as it stands, or reject it",
            step.id, run.pipeline.name, review.reviewer
        ),
        show,
        evidence: Vec::new(),
        approvers,
        tier: Tier::High,
        timeout: None,
        on_timeout: gate::OnTimeout::Reject,
        question: None,
    };
    // Waiting for a person is not work a stop waits for.
    *working = None;
    let resolution = crate::telemetry::traced(
        crate::telemetry::gate_span(&id, ctx.workflow_id()),
        (*run.why).clone(),
        gate::gate(ctx, &id, spec),
    )
    .await?;
    failpoint(&format!("pipeline:after:{id}"));
    keep_working(working, ctx.workflow_id()).await;
    match resolution {
        Resolution::Decided(d) if d.verdict == gate::Verdict::Approve => {
            let acceptance = Acceptance {
                candidate: candidate.id.clone(),
                digest: candidate.digest.clone(),
                accepted: true,
                reason: Some(format!("approved by {} at gate {id}", d.by)),
            };
            Ok((candidate, acceptance))
        }
        other => Err(failed(format!(
            "step `{}`: its review went to gate `{id}`, which {}; the step's output is not \
             published",
            step.id,
            match other {
                Resolution::Decided(_) => "was rejected",
                _ => "expired",
            }
        ))),
    }
}

/// Refuse, before anything of the run is paid for, a reviewed step the
/// workflow loader would have refused, for a run whose pipeline came from
/// anywhere else: a dotted name, an exec author, a reviewer that is not a
/// model atom of the run, or more rounds than atomic allows.
fn check_reviews(pipeline: &Pipeline) -> Result<()> {
    for step in &pipeline.steps {
        let Step::Atom(step) = step else { continue };
        let Some(review) = &step.review else { continue };
        // As the loader refuses them, for a run whose pipeline came from
        // anywhere else.
        if step.id.contains('.') {
            return Err(failed(format!(
                "step `{}` has a dot in its name, which its review's durable names would share",
                step.id
            )));
        }
        let kind = |name: &str| pipeline.atoms.get(name).map(|a| a.kind.as_str());
        if kind(&step.atom) == Some("exec") {
            return Err(failed(format!(
                "step `{}`: an exec atom's output cannot be reviewed",
                step.id
            )));
        }
        if !matches!(kind(&review.reviewer), Some("lite" | "codex")) {
            return Err(failed(format!(
                "step `{}`: its reviewer `{}` is not a lite or codex atom of the run",
                step.id, review.reviewer
            )));
        }
        if review.rounds > atomic_core::workflow::MAX_REVIEW_ROUNDS {
            return Err(failed(format!(
                "step `{}`: its review gives {} repair rounds, more than the {} atomic allows",
                step.id,
                review.rounds,
                atomic_core::workflow::MAX_REVIEW_ROUNDS
            )));
        }
    }
    Ok(())
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
#[allow(clippy::too_many_arguments)]
async fn run_in(
    atom: &Atom,
    input: &Value,
    origins: &BTreeMap<String, String>,
    run_id: &str,
    step: &str,
    snapshot: Option<&atomic_core::context::ContextSnapshot>,
    repair: Option<&Value>,
    runtimes: &Runtimes,
) -> std::result::Result<(Value, atomic_core::runtime::RunMetadata), AtomError> {
    let mut registry = Registry::new();
    let compiled =
        compile::compile(atom, &registry).map_err(|e| AtomError::Runtime(e.to_string()))?;
    // Each mapped field carries the origin its binding allows; anything else
    // is external.
    run_declarative_repairing(
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
        repair,
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
    repair: Option<Value>,
}

/// Run a step's atom in its worktree, reset to the step's base, and commit
/// what it left. The commit is recorded in the step's metadata by atomic,
/// over anything the atom reported.
async fn in_worktree(
    worktrees: Arc<Worktrees>,
    caches: atomic_core::worktree::Caches,
    request: StepRequest,
    call: StepCall,
    runtimes: Arc<Runtimes>,
    place: &str,
    bound: bool,
) -> Result<Proposal> {
    let StepCall {
        atom,
        input,
        origins,
        snapshot,
        run_id,
        repair,
    } = call;
    let step = request.step.clone();
    let finished = contained(async move {
        // A fixture is its own repository: the atom works at the top of its
        // worktree, wherever the project is. Otherwise the worktree is of
        // the project's own checkout, at the project's place in it.
        let subdirectory = if bound {
            PathBuf::new()
        } else {
            source_subdirectory(runtimes.codex.cwd.clone()).await?
        };
        // The run's build caches, with a Cargo target directory of this
        // worktree's own.
        caches
            .create()
            .map_err(|e| AtomError::Runtime(format!("the build caches: {e}")))?;
        worktrees
            .run_step(request, |tree| async move {
                let mut here = (*runtimes).clone();
                here.codex.cwd = tree.join(subdirectory);
                here.codex.caches = Some(caches.in_worktree(&tree));
                run_in(
                    &atom,
                    &input,
                    &origins,
                    &run_id,
                    &step,
                    snapshot.as_ref(),
                    repair.as_ref(),
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
        on_timeout: match step.question.as_ref().and_then(|q| q.default.as_ref()) {
            Some(_) => gate::OnTimeout::Default,
            None => gate::OnTimeout::Reject,
        },
        question: step.question.as_ref().map(|q| gate::Question {
            answer: q.answer.clone(),
            remember: q.remember,
            default: q.default.clone(),
        }),
    };
    match gate::gate(ctx, &step.id, spec).await? {
        Resolution::Decided(d) if d.verdict == gate::Verdict::Approve => {
            let mut decision = json!({
                "verdict": "approve",
                "by": d.by.as_str(),
                "comment": d.comment,
                "at": d.at,
            });
            if step.question.is_some() {
                // The store checked the answer against the question.
                decision["answer"] = d
                    .answer
                    .ok_or_else(|| at("its decision carries no answer".into()))?;
            }
            Ok(decision)
        }
        // Nobody answered in time: silence selected the question's default,
        // as the gate store recorded it.
        Resolution::Defaulted { at: when, answer } => Ok(json!({
            "verdict": "default",
            "answer": answer,
            "by": null,
            "comment": null,
            "at": when,
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
    /// Where each run's build caches go, the state directory's `cache/`:
    /// a directory of the run's own there, which its worktree and exec
    /// steps may write and which goes when the run ends. `None` puts them
    /// beside the run's worktrees, and a run with neither gets none.
    pub caches: Option<PathBuf>,
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
        let id = ctx.workflow_id().to_owned();
        // One for this execution of the run, which its steps note when a
        // model call of theirs fails (see [`atomic_core::usage::Why`]).
        let why = atomic_core::usage::Why::default();
        let traced = crate::telemetry::traced(
            span,
            why.clone(),
            run_pipeline(ctx, run, services.clone(), ledger.clone(), None, why),
        );
        crate::panics::caught(id, traced)
    });
    rt.register(REPLAY, move |ctx: R::Context, replay: ReplayRun| {
        let ReplayRun { of, run, recorded } = replay;
        let span = crate::telemetry::workflow_span(&run.pipeline.name, ctx.workflow_id());
        let id = ctx.workflow_id().to_owned();
        let why = atomic_core::usage::Why::default();
        let traced = crate::telemetry::traced(
            span,
            why.clone(),
            run_pipeline(
                ctx,
                run,
                replaying.clone(),
                replay_ledger.clone(),
                Some(Arc::new(Replay { of, recorded })),
                why,
            ),
        );
        crate::panics::caught(id, traced)
    });
}

/// Hold every atom step's bindings to its atom's trust, and its review's to
/// the reviewer's, as the loader does, from the pipeline alone: before a run
/// does anything, so no step's effect lands before a later step is refused.
/// Each binding's origin is the lower of what the run recorded for it
/// (external, when it recorded nothing) and what its expression reads,
/// recomputed from the steps before it, so a pipeline built some other way,
/// or a run recorded before a check existed, cannot raise one. An atom whose
/// `trust` is none of the three accepts nothing. A resumed or served run is
/// checked on the pipeline it recorded.
fn trusted(pipeline: &Pipeline) -> std::result::Result<(), String> {
    use atomic_core::workflow::{
        accepted_trust, review_origin, trusted_sources, unknown_trust, unknown_trust_advice,
        untrusted_binding,
    };
    let atom_of = |id: &str, name: &str| {
        let atom = pipeline
            .atoms
            .get(name)
            .ok_or_else(|| format!("step `{id}`: no atom `{name}`"))?;
        let accepts = accepted_trust(atom).map_err(|_| {
            format!(
                "step `{id}`: {}; {}",
                unknown_trust(atom).unwrap_or_default(),
                unknown_trust_advice(atom)
            )
        })?;
        Ok::<_, String>((atom, accepts))
    };
    // A binding's origin at run time: the lower of what the run recorded
    // (external, when it recorded nothing) and what its expression reads;
    // and, when the run's `origins` entry is what lowered it, the advice to
    // remove that entry.
    let origin = |recorded: Option<&String>, read: &'static str| match recorded.map(String::as_str)
    {
        Some(entry) if rank(entry) < rank(read) => (
            entry.to_owned(),
            format!(", remove the `origins` entry that marks it {entry}"),
        ),
        None if rank("external") < rank(read) => ("external".to_owned(), String::new()),
        _ => (read.to_owned(), String::new()),
    };
    let mut kinds = BTreeMap::new();
    for step in &pipeline.steps {
        let step = match step {
            Step::Gate(gate) => {
                kinds.insert(gate.id.clone(), "gate".to_owned());
                continue;
            }
            Step::Atom(step) => step,
        };
        let (atom, accepts) = atom_of(&step.id, &step.atom)?;
        for (field, source) in &step.with {
            let read = origin_of(source, &kinds).unwrap_or("external");
            let (origin, entry) = origin(step.origins.get(field), read);
            if let Some(refused) = untrusted_binding(atom, source, &origin) {
                return Err(format!(
                    "step `{}`: `with.{field}`: {refused}; bind it to {} instead{entry}, or \
                     declare a lower `trust` on the atom if it is built for {origin} input",
                    step.id,
                    trusted_sources(accepts),
                ));
            }
        }
        if let Some(review) = &step.review {
            let (reviewer, reviewer_accepts) = atom_of(&step.id, &review.reviewer)?;
            // What the reviewer's findings carry: a model's output on what
            // it read.
            let mut findings = "agent".to_owned();
            for (field, source) in &review.with {
                let read = review_origin(source, &kinds).unwrap_or("external");
                let (origin, entry) = origin(review.origins.get(field), read);
                if let Some(refused) = untrusted_binding(reviewer, source, &origin) {
                    return Err(format!(
                        "step `{}`: `review.with.{field}`: {refused}; a candidate under review \
                         is external data; bind it to {} instead{entry}, or declare a lower \
                         `trust` on the reviewer if it is built for {origin} input",
                        step.id,
                        trusted_sources(reviewer_accepts),
                    ));
                }
                if rank(&origin) < rank(&findings) {
                    findings = origin;
                }
            }
            // A repair round hands the author the reviewer's findings: no
            // more trusted than what the reviewer read.
            if review.rounds > 0 && rank(&findings) < rank(accepts) {
                return Err(format!(
                    "step `{}`: `review.rounds`: a repair round hands `{}` the reviewer's \
                     findings, which are {findings} data, and it accepts {accepts} input at \
                     most (`trust: '{accepts}'`); set `rounds: 0`, so a person decides on a \
                     rejection, or review it with what it can take",
                    step.id, step.atom,
                ));
            }
        }
        kinds.insert(step.id.clone(), atom.kind.clone());
    }
    Ok(())
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

/// Why the command of exec atom `atom`, about to run in its step's body,
/// may have run already, so that running it again could repeat its
/// effects; `None` when this is its first run. Either an earlier execution
/// was `interrupted` after the step's attempt began, or, in this process,
/// an earlier run of the step's body took the workspace its attempt opened
/// (`workspace_taken`), and so ran its command or began to.
fn ran_before(atom: &str, interrupted: bool, workspace_taken: bool) -> Option<String> {
    if interrupted {
        Some(format!(
            "an earlier attempt of `{atom}` was interrupted before its result was saved"
        ))
    } else if workspace_taken {
        Some(format!(
            "this step already ran `{atom}`'s command, or began to, in this process"
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command runs again only through the effects rule: after an
    /// interrupted attempt, and after an earlier run of the step's body in
    /// this process took its attempt's workspace.
    #[test]
    fn an_exec_command_that_may_have_run_is_not_run_again_unchecked() {
        assert_eq!(ran_before("x", false, false), None, "its first run");
        assert!(ran_before("x", true, true).is_some_and(|why| why.contains("interrupted")));
        assert!(ran_before("x", true, false).is_some_and(|why| why.contains("interrupted")));
        assert!(ran_before("x", false, true).is_some_and(|why| why.contains("in this process")));
    }

    /// Only a recorded outcome, finished or failed, removes a run's caches:
    /// an execution that lost a race, or whose outcome went unrecorded,
    /// leaves them to the one still building there.
    #[test]
    fn a_runs_caches_go_only_with_an_outcome_that_was_recorded() {
        let ends: Vec<(Result<Value>, bool)> = vec![
            (Ok(json!({})), true),
            (Err(DurableError::Failed("a step failed".into())), true),
            (Err(DurableError::Conflict("raced".into())), false),
            (Err(DurableError::Interrupted("stopping".into())), false),
            (Err(DurableError::Parked("a cap".into())), false),
            (Err(DurableError::Engine("the store".into())), false),
            (Err(DurableError::Nested("a step".into())), false),
        ];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        for (i, (end, removed)) in ends.into_iter().enumerate() {
            let dir =
                std::env::temp_dir().join(format!("atomic-settle-{}-{i}", std::process::id()));
            let caches = atomic_core::worktree::Caches::new(dir.clone());
            caches.create().expect("create");
            std::fs::write(dir.join("built"), "x").expect("write");
            runtime.block_on(settle_caches("run-1", caches, &end));
            assert_eq!(!dir.exists(), removed, "{end:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

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
