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

use crate::criteria::{self, CriterionOutcome};
use crate::graph_claim::{
    ClaimTurnLimits, bounded_claimed_turn, reclaim_review_gate, validate_before_turn,
};
use crate::metering::{BudgetPlan, RunBudget};
use crate::model_policy::{LaneChoice, LanePolicySource};
use crate::workflows::propose_at;
use atomic_core::artifact::{ArtifactError, ArtifactRef, ArtifactStore, LocalStore, Provenance};
use atomic_core::astro::StepClock;
use atomic_core::atom::Atom;
use atomic_core::budget::{Budget, BudgetSettings};
use atomic_core::git::{Git, Repo, Signature, TreeEntry};
use atomic_core::render::RunInput;
use atomic_core::runtime::{Runtimes, contained, run_declarative_repairing};
use atomic_core::workflow::{Criterion, Workflow, parse_criteria};
use atomic_core::worktree::{StepError, StepRequest, Worktrees};
use atomic_core::{AtomError, compile};
use atomic_durable::attempt;
use atomic_durable::gate::{self, Deployment, GateSpec, Resolution, Tier};
use atomic_durable::publication::Precondition;
use atomic_durable::usage::Ledger;
use atomic_durable::{
    Acceptance, Candidate, DurableError, DurableRuntime, Proposal, Published, Result,
    WorkflowContext, execute_candidate_told, failpoint, publish_candidate, review_candidate,
};
use atomic_graph::{
    store::Lease,
    transactional_claim::{self, ClaimNext, ReviewFollowup, WorkItem},
};
use atomic_schema::expr::Shape;
use atomic_schema::registry::Registry;
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The name `run_pipeline` is registered under.
pub const PIPELINE: &str = "atomic.pipeline";
/// A separate durable layout for a graph-driven run.
pub const GRAPH_PIPELINE: &str = "atomic.graph.pipeline.v1";
/// One concurrently running graph item, with its own durable step sequence.
pub const GRAPH_LANE: &str = "atomic.graph.lane.v1";

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
pub const LAYOUT_VERSION: &str = "atomic.pipeline.11";

/// Older runs projected an empty `Context` contract even when the atom had no
/// public context. Repair only that exact, inert projection in the in-memory
/// copy of a recorded run. The durable input remains unchanged, and a real or
/// damaged context contract still requires its verified snapshot.
fn normalize_legacy_context(
    name: &str,
    context: Option<&Value>,
    contracts: &mut Value,
) -> Result<()> {
    if context.is_some() {
        return Ok(());
    }
    let entries = contracts
        .as_object_mut()
        .ok_or_else(|| failed(format!("atom `{name}` has invalid recorded contracts")))?;
    let Some(schema) = entries.get("Context") else {
        return Ok(());
    };
    if *schema != json!({"type":"object","additionalProperties":false}) {
        return Err(failed(format!(
            "atom `{name}` records a public context contract without a declared context; recovery requires its verified snapshot"
        )));
    }
    entries.remove("Context");
    Ok(())
}

/// How many times one process recovers one run of `run_pipeline` at the
/// same step before it fails the run as dead-lettered: enough for a crash
/// or two, few enough that a step whose process keeps dying, such as an
/// idempotent command whose runner always dies, stops soon. A run that gets
/// further counts afresh, and so does each process, so restarting
/// `atomic serve` never counts against a healthy run; the engine's own cap,
/// which counts every recovery claim, stays at its default.
pub const RECOVERY_LIMIT: u32 = 5;

/// A run's recovery identity. A run ID, and even a project key, can be reused
/// in a different store served by the same process.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RecoveryKey {
    store: PathBuf,
    project: Option<String>,
    run: String,
}

impl RecoveryKey {
    fn new(project: Option<&str>, run: &str, store: &LocalStore) -> Self {
        Self {
            store: store
                .root()
                .canonicalize()
                .unwrap_or_else(|_| store.root().to_path_buf()),
            project: project.map(str::to_owned),
            run: run.to_owned(),
        }
    }
}

/// For each run this process has executed: the furthest position it has
/// reached (the pin is 0, each step one more), and how many of this
/// process's executions of the run have reached it.
static PROGRESS: std::sync::Mutex<BTreeMap<RecoveryKey, (usize, u32)>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Count an execution of run `id` reaching `position`, and fail the run
/// as dead-lettered once more executions than [`RECOVERY_LIMIT`] allows
/// have reached it without getting further. Each execution reaches each
/// position once; one replaying positions short of the furthest counts
/// nothing.
fn reached(key: &RecoveryKey, position: usize) -> Result<()> {
    let id = &key.run;
    let entries = {
        let mut progress = PROGRESS.lock().unwrap_or_else(|e| e.into_inner());
        let (top, entries) = progress.entry(key.clone()).or_insert((position, 0));
        if position > *top {
            *top = position;
            *entries = 0;
        }
        // A step that stopped for an artifact store held elsewhere, as a pin
        // does, takes nothing toward dead-lettering, nor does it get further.
        if position == *top && !atomic_core::runtime::take_held_stop(&key.store, id) {
            *entries += 1;
        }
        *entries
    };
    if entries > 1 + RECOVERY_LIMIT {
        let why = format!(
            "{DEAD_LETTERED} run {id} stopped {RECOVERY_LIMIT} times in this process at the same \
             step without finishing it, so it is not run again"
        );
        atomic_core::redact::Line::new()
            .plain(DEAD_LETTERED)
            .plain(" run ")
            .shown(id)
            .plain(" stopped ")
            .shown(&RECOVERY_LIMIT.to_string())
            .plain(" times in this process at the same step without finishing it, so it is not run again")
            .log();
        return Err(failed(why));
    }
    Ok(())
}

/// Take back the count [`reached`] made for this execution of run `id` at
/// `position`: it stopped there for a reason outside the run, such as a
/// store held elsewhere, and must not bring the run nearer dead-lettering.
fn not_reached(key: &RecoveryKey, position: usize) {
    let mut progress = PROGRESS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((top, entries)) = progress.get_mut(key)
        && *top == position
    {
        *entries = entries.saturating_sub(1);
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::{PROGRESS, RECOVERY_LIMIT, RecoveryKey, reached};
    use atomic_core::artifact::LocalStore;

    #[test]
    fn same_run_name_in_another_store_does_not_inherit_recovery_count() {
        let root = std::env::temp_dir().join(format!("atomic-recovery-key-{}", std::process::id()));
        let first_store = LocalStore::open(root.join("first")).expect("first artifact store");
        let second_store = LocalStore::open(root.join("second")).expect("second artifact store");
        let reopened = LocalStore::open(root.join("first")).expect("reopened artifact store");
        let run = format!("recovery-key-scope-{}", std::process::id());
        let first = RecoveryKey::new(Some("same-project"), &run, &first_store);
        let second = RecoveryKey::new(Some("same-project"), &run, &second_store);
        let another_project = RecoveryKey::new(Some("another-project"), &run, &first_store);
        assert_eq!(
            first,
            RecoveryKey::new(Some("same-project"), &run, &reopened)
        );
        for _ in 0..=RECOVERY_LIMIT {
            reached(&first, 0).expect("first store has its recovery allowance");
        }
        reached(&second, 0).expect("second store has its own recovery allowance");
        reached(&another_project, 0).expect("another project has its own recovery allowance");
        let failure = reached(&first, 0).expect_err("first store exhausts only its own allowance");
        assert!(failure.to_string().contains("dead-lettered:"));
        let mut progress = PROGRESS.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(progress.remove(&second), Some((0, 1)));
        assert_eq!(progress.remove(&another_project), Some((0, 1)));
        progress.remove(&first);
        drop(progress);
        drop((first_store, second_store, reopened));
        std::fs::remove_dir_all(root).expect("remove artifact stores");
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
    /// A coding step must pass a self-review before independent review.
    #[serde(default, skip_serializing_if = "is_false")]
    pub coding: bool,
    /// Each mapped field's origin: what its sources allow, or lower when the
    /// workflow says so (see `atomic_core::workflow::binding_origin`).
    #[serde(default)]
    pub origins: BTreeMap<String, String>,
    /// Frozen operator-authored checks evaluated before review or publication.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria: Vec<Criterion>,
    /// The review its output gets before it is published, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewStep>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// An atom step's review: a reviewer atom's typed verdict on each candidate,
/// a repair round for the author on a rejection, and a person's decision on
/// an escalation or when the rounds run out. Nothing passes it silently.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewStep {
    /// The reviewer atom, by name.
    pub reviewer: String,
    /// Fixed ordered panel for coding steps. Empty for a legacy single review.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<String>,
    /// Each of the reviewer's input fields and the CEL expression that
    /// computes it, over the step's scope and `candidate`.
    pub with: BTreeMap<String, String>,
    /// Each mapped field's origin.
    #[serde(default)]
    pub origins: BTreeMap<String, String>,
    /// How many repair rounds the author gets before the review escalates.
    pub rounds: u64,
    /// A separate model turn that records checklist evidence for each
    /// candidate before the independent reviewer sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_review: Option<SelfReviewStep>,
    /// Host preflight against the current main head before a coding candidate
    /// reaches either reviewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial_merge: Option<TrialMergeStep>,
}

/// A trusted review declaration for a scratch merge and its checks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrialMergeStep {
    /// The branch whose current head is merged into the private candidate.
    pub main: String,
    /// Commands run in order in the merged scratch worktree, without a shell.
    pub checks: Vec<Vec<String>>,
}

/// The optional checklist pass over a candidate before independent review.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SelfReviewStep {
    /// The atom that produces the checklist evidence.
    pub atom: String,
    /// CEL mappings from the candidate and the workflow scope into its input.
    pub with: BTreeMap<String, String>,
    /// The least trusted origin of each mapped field.
    #[serde(default)]
    pub origins: BTreeMap<String, String>,
    /// Decision points the trusted workflow requires the evidence to cover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_points: Vec<String>,
    /// Controls rows the trusted workflow requires the evidence to cover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_controls: Vec<String>,
    /// The exec atom that proves each selected row against the private coding
    /// candidate before either reviewer turn runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controls_atom: Option<String>,
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

/// Operator-selected, local Cargo inputs for a trial check. The registry is
/// never taken from a candidate's environment or from its Cargo config.
#[derive(Clone, Debug)]
pub struct TrialCargo {
    /// Canonical operator-owned registry directory, containing index, cache,
    /// and source trees. A trial check may read it but cannot write it.
    pub registry: PathBuf,
    /// The installed rustup toolchain selected by the operator.
    pub toolchain: String,
}

impl TrialCargo {
    /// Validate and pin an operator-selected local registry and toolchain.
    pub fn new(registry: PathBuf, toolchain: String) -> std::result::Result<Self, String> {
        let registry = std::fs::canonicalize(&registry).map_err(|e| {
            format!(
                "trial merge offline Cargo cache unavailable: {}: {e}",
                registry.display()
            )
        })?;
        if !registry.is_dir()
            || ["index", "cache", "src"]
                .iter()
                .any(|name| !registry.join(name).is_dir())
        {
            return Err(format!(
                "trial merge offline Cargo cache unavailable: {} needs index, cache, and src directories",
                registry.display()
            ));
        }
        if toolchain.is_empty()
            || toolchain.len() > 128
            || !toolchain
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err("trial merge Cargo toolchain is missing or invalid".into());
        }
        Ok(Self {
            registry,
            toolchain,
        })
    }
}

fn default_rustup_bin(home: &Path) -> PathBuf {
    home.join(".cargo/bin")
}

/// Choose only operator-installed Cargo/rustc shims. The trial's HOME and
/// CARGO_HOME are a fresh, candidate-writable cache, so this runs before the
/// sandbox command is built and reads the operator process environment.
fn trial_toolchain_bin(
    cargo_home: Option<&Path>,
    operator_home: Option<&Path>,
    scratch: &Path,
    cache: &Path,
) -> Option<PathBuf> {
    let scratch = std::fs::canonicalize(scratch).ok()?;
    let cache = std::fs::canonicalize(cache).ok()?;
    // CONTROL_OFF: trial-cargo-default-home
    let default_bin = operator_home.map(default_rustup_bin);
    let preferred = cargo_home.map(|home| home.join("bin")).or(default_bin);
    preferred
        .into_iter()
        .chain([PathBuf::from("/usr/local/cargo/bin")])
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .find(|bin| {
            !bin.starts_with(&scratch)
                && !bin.starts_with(&cache)
                && ["cargo", "rustc"].iter().all(|name| {
                    std::fs::canonicalize(bin.join(name)).is_ok_and(|shim| {
                        shim.is_file() && !shim.starts_with(&scratch) && !shim.starts_with(&cache)
                    })
                })
        })
}

fn trial_uses_cargo(pipeline: &Pipeline) -> bool {
    pipeline.steps.iter().any(|step| match step {
        Step::Atom(step) => step
            .review
            .as_ref()
            .and_then(|r| r.trial_merge.as_ref())
            .is_some_and(|trial| {
                trial.checks.iter().any(|argv| {
                    argv.first()
                        .is_some_and(|program| program == "cargo" || program.ends_with("/cargo"))
                })
            }),
        Step::Gate(_) => false,
    })
}

/// The bytes used for a controls proof come from atomic's build. The atom
/// supplies only its typed input/output contract; its command and environment
/// cannot decide how a selected control is proved.
const CONTROLS_DRIVER: &[u8] = include_bytes!("../../../scripts/controls-exec.py");
const CONTROLS_RUNNER: &[u8] = include_bytes!("../../../scripts/controls-check.sh");

fn trusted_controls_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => {
            return Err(failed(format!(
                "trusted controls proof path {} is not a regular file",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let nonce = atomic_core::credential::fresh_secret()
                .map_err(|error| failed(format!("name trusted controls proof file: {error}")))?;
            let staged = path.with_extension(format!("tmp-{nonce}"));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)
                .map_err(|error| failed(format!("stage trusted controls proof file: {error}")))?;
            let installed = file
                .write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(|error| failed(format!("write trusted controls proof file: {error}")))
                .and_then(|()| {
                    std::fs::rename(&staged, path).map_err(|error| {
                        failed(format!("install trusted controls proof file: {error}"))
                    })
                });
            let _ = std::fs::remove_file(&staged);
            installed?;
        }
        Err(error) => {
            return Err(failed(format!(
                "inspect trusted controls proof file {}: {error}",
                path.display()
            )));
        }
    }
    let meta = std::fs::symlink_metadata(path)
        .map_err(|error| failed(format!("inspect trusted controls proof file: {error}")))?;
    if !meta.file_type().is_file()
        || std::fs::read(path)
            .map_err(|error| failed(format!("read trusted controls proof file: {error}")))?
            != bytes
    {
        return Err(failed(format!(
            "trusted controls proof file {} differs from atomic's build",
            path.display()
        )));
    }
    Ok(())
}

fn trusted_controls_atom(
    atom: &Atom,
    caches: Option<&atomic_core::worktree::Caches>,
) -> Result<Atom> {
    for key in [
        "CARGO_HOME",
        "CARGO_TARGET_DIR",
        "CARGO_NET_OFFLINE",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
    ] {
        if atom.frontmatter["env"].get(key).is_some() {
            return Err(failed(format!(
                "{}: host Cargo input {key} cannot be declared by the controls atom",
                atom.name
            )));
        }
    }
    let caches = caches.ok_or_else(|| failed("controls proof has no private run cache".into()))?;
    let parent = caches
        .dir()
        .parent()
        .ok_or_else(|| failed("controls proof cache has no private parent".into()))?;
    let bundle = parent.join(format!(
        "controls-proof-{}-{}",
        &sha256(CONTROLS_DRIVER)[..24],
        &sha256(CONTROLS_RUNNER)[..24]
    ));
    let scripts = bundle.join("scripts");
    atomic_core::worktree::Caches::new(scripts.clone())
        .create()
        .map_err(|error| failed(format!("create trusted controls proof bundle: {error}")))?;
    trusted_controls_file(&scripts.join("controls-exec.py"), CONTROLS_DRIVER)?;
    trusted_controls_file(&scripts.join("controls-check.sh"), CONTROLS_RUNNER)?;
    let mut trusted = atom.clone();
    if trusted.kind != "exec" {
        return Err(failed(format!(
            "controls proof atom {} is not exec",
            atom.name
        )));
    }
    let env = json!({
        "PATH": std::env::var("PATH").map_err(|error| failed(format!("controls operator PATH: {error}")))?,
        "CONTROL_RUNNER": "controls-check.sh",
        "CONTROL_RUNNER_SHA256": sha256(CONTROLS_RUNNER),
        "CONTROL_DRIVER_SHA256": sha256(CONTROLS_DRIVER),
    });
    let frontmatter = trusted.frontmatter.as_object_mut().ok_or_else(|| {
        failed(format!(
            "controls proof atom {} has no configuration",
            atom.name
        ))
    })?;
    let exec = frontmatter
        .get_mut("exec")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            failed(format!(
                "controls proof atom {} has no exec command",
                atom.name
            ))
        })?;
    exec.insert(
        "command".into(),
        json!(["python3", "scripts/controls-exec.py"]),
    );
    frontmatter.insert("env".into(), env);
    trusted.program.root = bundle;
    Ok(trusted)
}

fn controls_uses_cargo(pipeline: &Pipeline) -> bool {
    pipeline.steps.iter().any(|step| match step {
        Step::Atom(step) => step
            .review
            .as_ref()
            .and_then(|review| review.self_review.as_ref())
            .and_then(|self_review| self_review.controls_atom.as_ref())
            .and_then(|name| pipeline.atoms.get(name))
            .is_some(),
        Step::Gate(_) => false,
    })
}

fn prepare_controls_cargo(
    caches: &atomic_core::worktree::Caches,
    selected: &TrialCargo,
    source: &Path,
) -> Result<atomic_core::runtime::ExecCargo> {
    let source = source
        .canonicalize()
        .map_err(|e| failed(format!("controls bound repository: {e}")))?;
    if !source.is_dir() {
        return Err(failed(
            "controls bound repository is not a directory".into(),
        ));
    }
    caches
        .create()
        .map_err(|e| failed(format!("controls offline Cargo cache: {e}")))?;
    let home = caches
        .prepare_exec_cargo(&selected.registry)
        .map_err(|e| failed(format!("controls offline Cargo cache: {e}")))?;
    let target = home.join("target");
    std::fs::create_dir(&target)
        .map_err(|e| failed(format!("controls offline Cargo target: {e}")))?;
    Ok(atomic_core::runtime::ExecCargo {
        home,
        target,
        registry: selected.registry.clone(),
        toolchain: selected.toolchain.clone(),
        read_only_bindings: vec![source],
    })
}

impl Pipeline {
    /// Whether a step of it changes code, in a worktree.
    pub fn changes_code(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s, Step::Atom(a) if a.worktree))
    }
}

/// What a run's `worktree: true` steps work in, or why the run has none.
/// The why is kept in the words that said it, because the run has to fail
/// with what was found: a repository atomic could not read is not a project
/// outside every repository, and no worktree ever means a step may run.
enum WorktreesForRun {
    /// The run's worktrees: its fixture's snapshot, or the project's
    /// repository.
    Found(Arc<Worktrees>),
    /// Why the run has none.
    None(String),
}

/// The worktrees a run's worktree steps use: those of its fixture's
/// snapshot when it is bound to one, else what the services found for the
/// project. Runs no git for a project it does not need one for.
fn worktrees_for(pipeline: &Pipeline, services: &Services) -> WorktreesForRun {
    let fixture = match &pipeline.fixture {
        Some(fixture) => fixture,
        None => return services.worktrees.for_run(),
    };
    // The snapshot is named by path, so this only starts git; a snapshot
    // that is not there, or not readable, is what the commands on it say.
    let git = match Git::find() {
        Ok(git) => git,
        Err(e) => return WorktreesForRun::None(format!("the fixture's snapshot: {e}")),
    };
    match git.repo(&fixture.snapshot).without_filters() {
        Ok(repo) => {
            WorktreesForRun::Found(Arc::new(Worktrees::new(repo, fixture.worktrees.clone())))
        }
        Err(e) => WorktreesForRun::None(format!("the fixture's snapshot: {e}")),
    }
}

/// What the project offers a `worktree: true` step whose run is bound to no
/// fixture: its repository, or why it has none.
///
/// The ways there are none are kept apart on purpose, each with what said it,
/// since they are different news. git running on the project's directory and
/// refusing it is news about the directory, though not about why it said no:
/// git refuses alike a directory holding no repository, a repository it cannot
/// read, and a repository whose configuration it cannot read afterwards. git
/// never getting to answer at all is news that nothing was learned about the
/// directory. Neither lets a worktree step run. `atomic` makes no worktrees of
/// a project's own checkout — a run that changes code is bound to the
/// repository its `--fixture` names — so it never hands out [`Self::Found`];
/// that is what an embedder that means it gives. What git said about the
/// project is asked where a run that changes code is refused for naming no
/// fixture (see `runs::bind_fixture`), so that the refusal says what was found
/// instead of the same words for every project.
#[derive(Clone, Debug)]
pub enum ProjectWorktrees {
    /// The project's repository, found; such a step works in a worktree of
    /// it, kept under the state directory.
    Found(Arc<Worktrees>),
    /// git ran on the project's directory and refused it, which is what git
    /// does both with a directory that holds no repository and with a
    /// repository it cannot read, and the same for a repository whose own
    /// configuration it cannot read afterwards. Which of these it was is not
    /// learned, so it is not said: what git printed is kept, since its words
    /// name the directory and are what a person needs to go on.
    Refused(String),
    /// git never answered for the project at all: no git at the paths atomic
    /// looks at, git not startable, stopped, or printing what atomic cannot
    /// read. Nothing is known about the project's repository, which is not
    /// the same as its being absent.
    Unknown(String),
    /// Nothing, whatever the project holds: a run that changes code is bound
    /// to a fixture repository of its own, and atomic makes no worktrees of
    /// a checkout it was only asked to read.
    FixtureOnly,
}

impl ProjectWorktrees {
    /// What the repository `root` is in offers a worktree step: its worktrees,
    /// kept under `trees`, or which of these it is, with what said so. Runs
    /// git, so off the async workers.
    pub fn at(root: &Path, trees: PathBuf) -> Self {
        match Worktrees::open_at(root, trees) {
            Ok(worktrees) => Self::Found(Arc::new(worktrees)),
            Err(e) => match &e {
                // git ran and said no. Anything else, including an argument
                // atomic itself refused, means git never answered, which says
                // nothing about whether the project is in a repository.
                atomic_core::worktree::WorktreeError::Git(git) if git.refused() => {
                    Self::Refused(e.to_string())
                }
                _ => Self::Unknown(e.to_string()),
            },
        }
    }

    /// [`ProjectWorktrees::at`] for the project at `state`, its worktrees kept
    /// under the state directory. What `atomic`'s own commands ask of a
    /// project, so that a refusal about worktrees can say what was found
    /// about the project rather than the same words for every one.
    pub fn find(state: &atomic_core::state::ProjectState) -> Self {
        Self::at(state.root(), state.dir().join("worktrees"))
    }

    /// Why the project offers no worktree, as a clause. What git or the file
    /// system said is kept in it, and a project git refused is never reported
    /// as one git could not be asked about, nor the other way round; why git
    /// refused is left to what it printed, since atomic did not learn it. A
    /// caller that must tell [`Self::Found`] from the rest matches on it first;
    /// the sentence for `Found` is only what its worktrees are.
    pub fn describe(&self) -> String {
        match self {
            Self::Found(_) => "the project's repository was found, and its worktrees are kept \
                 under its state directory"
                .into(),
            Self::Refused(why) => format!("git refused the project's directory: {why}"),
            Self::Unknown(why) => {
                format!("atomic cannot tell whether the project is in a git repository: {why}")
            }
            Self::FixtureOnly => "atomic makes no worktrees of a project's own checkout".into(),
        }
    }

    /// What a `worktree: true` step of a run bound to no fixture has to work
    /// in, or why it has nothing.
    fn for_run(&self) -> WorktreesForRun {
        match self {
            Self::Found(worktrees) => WorktreesForRun::Found(worktrees.clone()),
            found => WorktreesForRun::None(format!(
                "the run is bound to no fixture repository, and {}; a run that changes code is \
                 bound to its fixture when it starts, so start it again with --fixture and \
                 --fixture-rev",
                found.describe()
            )),
        }
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

/// A pinned workflow template bound to one graph epic at admission.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphPipelineRun {
    /// The frozen template and initial input.
    #[serde(flatten)]
    pub run: PipelineRun,
    /// The pinned epic whose direct work children are selected dynamically.
    pub epic: String,
    /// The operator's endpoint catalog at admission and the file whose
    /// later route edits apply only to unstarted lanes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_source: Option<LanePolicySource>,
    /// The admitted host review routing contract. An older recorded graph run
    /// lacks this field and keeps its original reviewer behavior on recovery.
    #[serde(default)]
    pub review_routing: bool,
    /// Concurrent ready work items. Older runs keep their one-lane layout.
    #[serde(default = "one_graph_lane")]
    pub lanes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct GraphLaneRun {
    pub(crate) run: PipelineRun,
    pub(crate) epic: String,
    pub(crate) claim: GraphLaneClaim,
    pub(crate) model_source: Option<LanePolicySource>,
    pub(crate) review_routing: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct GraphLaneClaim {
    pub(crate) parent: String,
    pub(crate) ordinal: usize,
    pub(crate) item: WorkItem,
    pub(crate) lease: Lease,
}

fn one_graph_lane() -> usize {
    1
}

/// The host records a formatting citation here only after checking the
/// candidate, item labels, touched controls and every finding in the panel.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct GraphAcceptance {
    #[serde(flatten)]
    pub(crate) acceptance: Acceptance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) verified_formatting_path: Option<String>,
}

/// One durable graph finalization attempt. A missing proof or revoked lease is
/// checkpointed before its wait, so replay cannot skip an unreleased park when
/// branch evidence changes while atomic is offline.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum GraphFinalize {
    Missing { branch: String },
    Fenced,
    Applied { output: Value },
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
                let reviewers: Vec<String> = r
                    .get("reviewers")
                    .and_then(Value::as_array)
                    .map(|entries| {
                        entries
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let reviewer = r
                    .get("reviewer")
                    .and_then(Value::as_str)
                    .or_else(|| reviewers.first().map(String::as_str))
                    .ok_or_else(|| fail(format!("step `{id}`: `review` names no reviewer")))?;
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
                let self_review = match r.get("self_review") {
                    None => None,
                    Some(spec) => {
                        let self_name =
                            spec.get("atom").and_then(Value::as_str).ok_or_else(|| {
                                fail(format!(
                                    "step `{id}`: `review.self_review.atom` names no atom"
                                ))
                            })?;
                        let self_atom = atoms.get(self_name).ok_or_else(|| {
                            fail(format!(
                                "step `{id}`'s self-review names `{self_name}`, which is not loaded"
                            ))
                        })?;
                        let mut self_with = BTreeMap::new();
                        let mut self_origins = BTreeMap::new();
                        for (field, source) in spec
                            .get("with")
                            .and_then(Value::as_object)
                            .into_iter()
                            .flatten()
                        {
                            let source = source.as_str().ok_or_else(|| {
                                fail(format!("step `{id}`'s self-review maps `{field}` to a value that is not a CEL expression"))
                            })?;
                            let allowed = atomic_core::workflow::review_origin(source, &kinds)
                                .map_err(|e| {
                                    fail(format!(
                                        "step `{id}`: `review.self_review.with.{field}`: {e}"
                                    ))
                                })?;
                            self_with.insert(field.clone(), source.to_owned());
                            self_origins.insert(field.clone(), allowed.to_owned());
                        }
                        for (source, text) in &self_atom.program.sources {
                            pins.insert(source.clone(), sha256(text.as_bytes()));
                        }
                        used.insert(self_name.to_owned(), self_atom.clone());
                        if with.contains_key("self_review") {
                            return Err(fail(format!(
                                "step `{id}`: `review.with.self_review` is reserved for the self-review evidence"
                            )));
                        }
                        let controls_atom = spec
                            .get("controls_atom")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        if let Some(controls_name) = &controls_atom {
                            let control = atoms.get(controls_name).ok_or_else(|| {
                                fail(format!(
                                    "step `{id}`'s controls proof names `{controls_name}`, which is not loaded"
                                ))
                            })?;
                            if control.kind != "exec" {
                                return Err(fail(format!(
                                    "step `{id}`'s controls proof `{controls_name}` is not an exec atom"
                                )));
                            }
                            if control.external().is_none() {
                                return Err(fail(format!(
                                    "step `{id}`'s controls proof `{controls_name}` needs an idempotent or read-only external effect"
                                )));
                            }
                            for (source, text) in &control.program.sources {
                                pins.insert(source.clone(), sha256(text.as_bytes()));
                            }
                            used.insert(controls_name.clone(), control.clone());
                            self_origins.insert("control_results".into(), "external".into());
                        }
                        origins.insert("self_review".into(), "external".into());
                        Some(SelfReviewStep {
                            atom: self_name.to_owned(),
                            with: self_with,
                            origins: self_origins,
                            decision_points: spec
                                .get("decision_points")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect(),
                            selected_controls: spec
                                .get("selected_controls")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect(),
                            controls_atom,
                        })
                    }
                };
                for name in std::iter::once(reviewer).chain(reviewers.iter().map(String::as_str)) {
                    let atom = atoms.get(name).ok_or_else(|| {
                        fail(format!(
                            "step `{id}`'s review names `{name}`, which is not loaded"
                        ))
                    })?;
                    for (source, text) in &atom.program.sources {
                        pins.insert(source.clone(), sha256(text.as_bytes()));
                    }
                    used.insert(name.to_owned(), atom.clone());
                }
                // Who decides the review's gate, should it open, resolves now,
                // before anything runs, as any gate's approvers do.
                gate::resolve(&format!("{id}.review"), None, deployment)
                    .map_err(|e| fail(format!("step `{id}`'s review: {e}")))?;
                Some(ReviewStep {
                    reviewer: reviewer.to_owned(),
                    reviewers,
                    with,
                    origins,
                    rounds: r.get("rounds").and_then(Value::as_u64).unwrap_or(1),
                    self_review,
                    trial_merge: r.get("trial_merge").map(|trial| {
                        let main = trial["main"]
                            .as_str()
                            .expect("workflow checked main")
                            .to_owned();
                        let checks = trial["checks"]
                            .as_array()
                            .expect("workflow checked checks")
                            .iter()
                            .map(|check| {
                                check
                                    .as_array()
                                    .expect("workflow checked command")
                                    .iter()
                                    .map(|arg| {
                                        arg.as_str().expect("workflow checked argument").to_owned()
                                    })
                                    .collect()
                            })
                            .collect();
                        TrialMergeStep { main, checks }
                    }),
                })
            }
        };
        let coding = step.get("coding").and_then(Value::as_bool) == Some(true);
        kinds.insert(id.clone(), atom.kind.clone());
        steps.push(Step::Atom(AtomStep {
            id: id.clone(),
            atom: name.to_owned(),
            with,
            worktree,
            coding,
            origins,
            criteria: step
                .get("criteria")
                .map(parse_criteria)
                .transpose()
                .map_err(|error| fail(format!("step `{id}`: {error}")))?
                .unwrap_or_default(),
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
                    atomic_core::redact::Line::new()
                        .plain("atomic: run ")
                        .shown(id)
                        .plain(
                            ": the artifact store is held by another process, such as a sweep; \
                             the run waits for it and tries again",
                        )
                        .log();
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

fn plan_routed_budgets(
    pipeline: &Pipeline,
    source: &LanePolicySource,
    settings: &BudgetSettings,
) -> std::result::Result<BudgetPlan, String> {
    source.validate_catalog()?;
    let models = source
        .endpoints
        .values()
        .map(|endpoint| endpoint.name.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let atoms = pipeline
        .atoms
        .iter()
        .filter(|(_, atom)| matches!(atom.kind.as_str(), "codex" | "lite"))
        .map(|(name, atom)| (name.as_str(), &atom.budget))
        .collect::<Vec<_>>();
    let exec = pipeline
        .atoms
        .iter()
        .filter(|(_, atom)| atom.kind == "exec")
        .map(|(name, atom)| (name.as_str(), &atom.budget))
        .collect::<Vec<_>>();
    BudgetPlan::resolve_routed(settings, &pipeline.budget, &atoms, &exec, &models).map_err(
        |(atom, error)| match atom {
            Some(atom) => format!("atom {atom}: budget: {error}"),
            None => format!("workflow {}: budget: {error}", pipeline.name),
        },
    )
}

/// Run one pipeline. Workflow code: everything that touches a model or the
/// store goes through durable steps. Every model call is metered against
/// the run's and its atom's budget (see [`crate::metering`]).
#[allow(clippy::too_many_arguments)]
async fn run_pipeline<C: WorkflowContext>(
    ctx: C,
    run: PipelineRun,
    services: Arc<Services>,
    ledger: Ledger,
    replay: Option<Arc<Replay>>,
    why: atomic_core::usage::Why,
    graph_epic: Option<String>,
    model_source: Option<LanePolicySource>,
    review_routing: bool,
    graph_lanes: usize,
    preclaimed: Option<GraphLaneClaim>,
) -> Result<Value> {
    let id = ctx.workflow_id().to_owned();
    let mut caches = None;
    let result = run_pipeline_in(
        ctx,
        run,
        services,
        ledger,
        replay,
        &why,
        &mut caches,
        graph_epic,
        model_source,
        review_routing,
        graph_lanes,
        preclaimed,
    )
    .await;
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
        atomic_core::redact::Line::new()
            .plain("atomic: run ")
            .shown(id)
            .plain(": its build caches at ")
            .redacted(&dir.display().to_string())
            .plain(" could not be removed (")
            .redacted(&e)
            .plain("); remove them by hand")
            .log();
    }
}

/// [`run_pipeline`]'s work, leaving in `run_caches` the run's build caches,
/// named before its first step, for [`run_pipeline`] to settle when the run
/// ends.
#[allow(clippy::too_many_arguments)]
async fn run_pipeline_in<C: WorkflowContext>(
    ctx: C,
    run: PipelineRun,
    services: Arc<Services>,
    ledger: Ledger,
    replay: Option<Arc<Replay>>,
    why: &atomic_core::usage::Why,
    run_caches: &mut Option<atomic_core::worktree::Caches>,
    graph_epic: Option<String>,
    model_source: Option<LanePolicySource>,
    review_routing: bool,
    graph_lanes: usize,
    preclaimed: Option<GraphLaneClaim>,
) -> Result<Value> {
    if !(1..=16).contains(&graph_lanes) || (graph_lanes > 1 && graph_epic.is_none()) {
        return Err(failed(
            "graph lanes require an epic and a count from 1 to 16".into(),
        ));
    }
    let PipelineRun {
        mut pipeline,
        input,
    } = run;
    if replay.is_none()
        && !services.model_endpoint_configured
        && (graph_epic.is_none() || model_source.is_none())
        && pipeline
            .atoms
            .values()
            .any(|atom| matches!(atom.kind.as_str(), "codex" | "lite"))
    {
        return Err(DurableError::Interrupted(
            "this run needs a project-wide model endpoint; model.lanes applies only to graph runs that recorded its policy"
                .into(),
        ));
    }
    for (name, atom) in &mut pipeline.atoms {
        normalize_legacy_context(name, atom.context.as_ref(), &mut atom.program.contracts)?;
    }
    let id = ctx.workflow_id().to_owned();
    let recovery = RecoveryKey::new(ctx.project(), ctx.workflow_id(), &services.store);
    // Before anything is recorded: which execution this is, so `runs show`
    // can tell what each one ran and what it found checkpointed. Only that
    // view depends on it, so a record that cannot be written costs the run
    // nothing but the view's accuracy; an execution that no longer owns its
    // run stops.
    match atomic_durable::history::begin(&ctx).await {
        Ok(_) => {}
        Err(e @ DurableError::Interrupted(_)) => return Err(e),
        Err(e) => atomic_core::redact::Line::new()
            .plain("atomic: run ")
            .shown(&id)
            .plain(": this execution of it could not be recorded (")
            .redacted(&e.to_string())
            .plain("); `atomic runs show` may credit its steps to an earlier one")
            .log(),
    }
    // Working from here to its end, but for waits at gates: a stopping
    // process gives it its grace, and begins none of its steps.
    let mut working = Some(begin_work(&id).await);
    reached(&recovery, 0)?;
    let store = services.store.clone();
    fits("the workflow's input", &pipeline.input, &input)?;
    check_reviews(&pipeline)?;
    if trial_uses_cargo(&pipeline) || controls_uses_cargo(&pipeline) {
        let cargo = services.trial_cargo.as_ref().ok_or_else(|| {
            failed("trial merge offline Cargo cache unavailable: set ATOMIC_TRIAL_CARGO_REGISTRY and ATOMIC_TRIAL_RUSTUP_TOOLCHAIN on the operator process".into())
        })?;
        TrialCargo::new(cargo.registry.clone(), cargo.toolchain.clone()).map_err(failed)?;
        let rustup = atomic_core::worktree::rustup_home(&|n| std::env::var(n).ok()).ok_or_else(|| {
            failed("trial merge Cargo toolchain unavailable: operator RUSTUP_HOME has no installed toolchains".into())
        })?;
        let installed = std::fs::read_dir(rustup.join("toolchains"))
            .map_err(|e| failed(format!("trial merge Cargo toolchain unavailable: {e}")))?
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                entry.path().is_dir()
                    && (name == cargo.toolchain
                        || name.starts_with(&format!("{}-", cargo.toolchain)))
            });
        if !installed {
            return Err(failed(format!(
                "trial merge Cargo toolchain unavailable: {} is not installed under {}",
                cargo.toolchain,
                rustup.display()
            )));
        }
    }
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
            let routes_match = || -> std::result::Result<(), String> {
                if let Some(source) = &model_source {
                    let admitted = source
                        .endpoints
                        .values()
                        .map(|endpoint| endpoint.name.as_str())
                        .collect::<std::collections::BTreeSet<_>>();
                    for atom in &model_atoms {
                        let options = plan.routed_models.get(*atom).ok_or_else(|| {
                            format!("atom {atom}: the pinned budget has no routed models")
                        })?;
                        if options
                            .iter()
                            .map(String::as_str)
                            .collect::<std::collections::BTreeSet<_>>()
                            != admitted
                        {
                            return Err(format!(
                                "atom {atom}: the pinned budget's models differ from the admitted graph endpoint catalog"
                            ));
                        }
                    }
                }
                Ok(())
            };
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
                .and_then(|()| routes_match())
                .map(|()| BudgetPlan {
                    held: true,
                    ..plan.clone()
                })
                .map_err(|e| format!("workflow {}: its pinned budget: {e}", pipeline.name))
        }
        None => match &model_source {
            Some(source) => plan_routed_budgets(&pipeline, source, &services.budgets),
            None => plan_budgets(&pipeline, &services.runtimes, &services.budgets),
        },
    };
    // Store the definitions the run uses before anything runs. The snapshot
    // is content-addressed, so a retried step stores the same artifact.
    let snapshot =
        serde_json::to_vec(&pipeline).map_err(|e| failed(format!("encode the pipeline: {e}")))?;
    let files = pipeline.pins.clone();
    let run_id = ctx.workflow_id().to_owned();
    let pinning = run_id.clone();
    let pin_recovery = recovery.clone();
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
                    not_reached(&pin_recovery, 0);
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
        atomic_core::redact::Line::new()
            .plain("atomic: run ")
            .shown(ctx.workflow_id())
            .plain(": under your own dollar cap, the run calls ")
            .redacted(&unvouched.join(", "))
            .plain(
                " at prices only a layer above your own configuration sets; `atomic config \
                 --explain` says more",
            )
            .log();
    }
    let budget = match &preclaimed {
        Some(claim) => RunBudget::pinned(ledger, &plan).under_parent_run(&claim.parent, &plan),
        None => RunBudget::pinned(ledger, &plan),
    };
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
    // What the run's worktree steps work in, found once and off the async
    // workers, since finding it runs git. A run that changes code needs it
    // before any step: a repository atomic cannot read is said then, in the
    // words that said it, and no step is paid for first. A run that changes
    // no code is never asked about.
    let run_worktrees = if pipeline.changes_code() {
        let (p, s) = (pipeline.clone(), services.clone());
        match tokio::task::spawn_blocking(move || worktrees_for(&p, &s)).await {
            Ok(WorktreesForRun::Found(worktrees)) => Some(worktrees),
            Ok(WorktreesForRun::None(why)) => {
                return Err(failed(format!("the run's worktrees: {why}")));
            }
            Err(e) => return Err(failed(format!("the run's worktrees: {e}"))),
        }
    } else {
        None
    };
    // The first worktree turn records its base in its existing durable
    // candidate. Replaying that candidate recovers the same admitted revision
    // without adding a checkpoint that would shift older run histories.
    let mut admitted_base: Option<String> = None;
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
    if let Some(epic) = graph_epic {
        if replay.is_some()
            || pipeline.steps.len() != 1
            || !matches!(pipeline.steps.first(), Some(Step::Atom(step)) if step.worktree)
        {
            return Err(failed("graph runs require one worktree atom step".into()));
        }
        let template = match &pipeline.steps[0] {
            Step::Atom(step) => step.clone(),
            Step::Gate(_) => unreachable!(),
        };
        if let Some(claim) = &preclaimed {
            if claim.lease.claimant != claim.parent || claim.lease.item_id != claim.item.id {
                return Err(failed(
                    "graph child claim differs from its parent or item".into(),
                ));
            }
        } else {
            let binding = epic.clone();
            let workflow = id.clone();
            ctx.transaction("epic.bind", move |mut tx| {
                let binding = binding.clone();
                let workflow = workflow.clone();
                Box::pin(async move {
                    transactional_claim::bind_epic_run(&mut tx, &binding, &workflow).await
                })
            })
            .await?;
        }
        // A graph run may be recovered after its binding checkpoint. Recheck
        // the host-owned base ref on every execution before any claim or turn;
        // a missing ref is recreated from the frozen fixture commit, while a
        // different existing ref refuses instead of changing the run's base.
        if let Some(fixture) = &pipeline.fixture {
            let worktrees = run_worktrees
                .clone()
                .ok_or_else(|| failed("graph fixture has no worktrees".into()))?;
            let (run, commit) = (id.clone(), fixture.commit.clone());
            let pinned = tokio::task::spawn_blocking(move || worktrees.pin_base(&run, &commit))
                .await
                .map_err(|e| failed(format!("pin graph fixture base: {e}")))?
                .map_err(|e| failed(format!("pin graph fixture base: {e}")))?;
            if pinned != fixture.commit {
                return Err(failed(format!(
                    "graph run {id}'s pinned Git base differs from its frozen fixture"
                )));
            }
        }
        let mut accepted = Vec::<String>::new();
        let mut admitted_base: Option<String> = None;
        let mut last_output = None;
        let mut complete = false;
        let mut closed_count = 0_usize;
        if graph_lanes > 1 {
            // Parent operations have a fixed order. Each child owns a separate
            // checkpoint sequence, so lane completion order cannot change replay.
            let mut running = FuturesUnordered::new();
            let mut outcomes = BTreeMap::<usize, Value>::new();
            let mut failure = None;
            let mut ordinal = 0_usize;
            loop {
                while running.len() < graph_lanes
                    && !complete
                    && failure.is_none()
                    && ordinal < 10_000
                {
                    keep_working(&mut working, &id).await;
                    if let Err(error) = reached(&recovery, ordinal + 1) {
                        failure = Some(error);
                        break;
                    }
                    let claim_step = format!("graph.{ordinal:05}.claim");
                    let (workflow, epic_id, claimant, step) =
                        (id.clone(), epic.clone(), id.clone(), claim_step.clone());
                    let (claim_pipeline, claim_template, claim_input) =
                        (pipeline.clone(), template.clone(), input.clone());
                    let selection: Result<ClaimNext> = ctx
                        .transaction(&claim_step, move |mut tx| {
                            let (workflow, epic_id, claimant, step) = (
                                workflow.clone(),
                                epic_id.clone(),
                                claimant.clone(),
                                step.clone(),
                            );
                            let (claim_pipeline, claim_template, claim_input) = (
                                claim_pipeline.clone(),
                                claim_template.clone(),
                                claim_input.clone(),
                            );
                            Box::pin(async move {
                                transactional_claim::claim_next_for_epic(
                                    &mut tx,
                                    &workflow,
                                    &step,
                                    &epic_id,
                                    &claimant,
                                    std::time::Duration::from_secs(900),
                                    |item| {
                                        graph_atom_input(
                                            &claim_pipeline,
                                            &claim_template,
                                            &claim_input,
                                            item,
                                            None,
                                        )
                                        .map(|_| ())
                                    },
                                )
                                .await
                            })
                        })
                        .await;
                    let selection = match selection {
                        Ok(selection) => selection,
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    };
                    let this_ordinal = ordinal;
                    ordinal += 1;
                    match selection {
                        ClaimNext::Claimed { item, lease } => {
                            let mut child_pipeline = pipeline.clone();
                            child_pipeline.budgets = Some(pinned.budgets.clone());
                            let child = GraphLaneRun {
                                run: PipelineRun {
                                    pipeline: child_pipeline,
                                    input: input.clone(),
                                },
                                epic: epic.clone(),
                                claim: GraphLaneClaim {
                                    parent: id.clone(),
                                    ordinal: this_ordinal,
                                    item: *item,
                                    lease,
                                },
                                model_source: model_source.clone(),
                                review_routing,
                            };
                            let handle = match ctx.start_child::<_, Value>(GRAPH_LANE, child).await
                            {
                                Ok(handle) => handle,
                                Err(error) => {
                                    failure = Some(error);
                                    break;
                                }
                            };
                            let child_id = handle.id().to_owned();
                            let child_step = format!("graph.{this_ordinal:05}.child");
                            if let Err(error) = ctx
                                .step(&child_step, move |_| async move { Ok(child_id) })
                                .await
                            {
                                failure = Some(error);
                                break;
                            }
                            running.push(async move { (this_ordinal, handle.result().await) });
                        }
                        ClaimNext::Pending if running.is_empty() => {
                            match atomic_durable::publication::park_graph_pending(
                                &ctx,
                                &claim_step,
                                &epic,
                                this_ordinal as u64,
                            )
                            .await
                            {
                                Ok(()) => continue,
                                Err(error) => {
                                    failure = Some(error);
                                    break;
                                }
                            }
                        }
                        ClaimNext::Pending => break,
                        ClaimNext::Complete => complete = true,
                    }
                }
                if running.is_empty() {
                    break;
                }
                // Child result polling is not a parent durable operation.
                working = None;
                if let Some((number, result)) = running.next().await {
                    match result {
                        Ok(output) => {
                            outcomes.insert(number, output);
                        }
                        Err(error) if failure.is_none() => failure = Some(error),
                        Err(_) => {}
                    }
                }
            }
            if let Some(error) = failure {
                return Err(error);
            }
            if complete {
                if outcomes.is_empty() {
                    fits(
                        "the workflow's empty graph result",
                        &pipeline.output,
                        &Value::Null,
                    )?;
                }
                let (epic_id, workflow) = (epic.clone(), id.clone());
                ctx.transaction("epic.done", move |mut tx| {
                    let (epic_id, workflow) = (epic_id.clone(), workflow.clone());
                    Box::pin(async move {
                        transactional_claim::finish_epic_run(&mut tx, &epic_id, &workflow).await
                    })
                })
                .await?;
            }
            closed_count = outcomes.len();
            for (_, output) in outcomes {
                last_output = Some(output);
            }
        } else {
            let first_ordinal = preclaimed.as_ref().map_or(0, |claim| claim.ordinal);
            let last_ordinal = if preclaimed.is_some() {
                first_ordinal.saturating_add(1)
            } else {
                10_000
            };
            for ordinal in first_ordinal..last_ordinal {
                keep_working(&mut working, &id).await;
                reached(&recovery, ordinal + 1)?;
                let step_name = format!("graph.{ordinal:05}.{}", template.id);
                let claim_step = format!("graph.{ordinal:05}.claim");
                let (workflow, epic_id, claimant, step) =
                    (id.clone(), epic.clone(), id.clone(), claim_step.clone());
                let (claim_pipeline, claim_template, claim_input, claim_base) = (
                    pipeline.clone(),
                    template.clone(),
                    input.clone(),
                    accepted.last().cloned(),
                );
                let selection: ClaimNext = if let Some(claim) = &preclaimed {
                    ClaimNext::Claimed {
                        item: Box::new(claim.item.clone()),
                        lease: claim.lease.clone(),
                    }
                } else {
                    ctx.transaction(&claim_step, move |mut tx| {
                        let (workflow, epic_id, claimant, step) = (
                            workflow.clone(),
                            epic_id.clone(),
                            claimant.clone(),
                            step.clone(),
                        );
                        let (claim_pipeline, claim_template, claim_input, claim_base) = (
                            claim_pipeline.clone(),
                            claim_template.clone(),
                            claim_input.clone(),
                            claim_base.clone(),
                        );
                        Box::pin(async move {
                            transactional_claim::claim_next_for_epic(
                                &mut tx,
                                &workflow,
                                &step,
                                &epic_id,
                                &claimant,
                                std::time::Duration::from_secs(900),
                                |item| {
                                    graph_atom_input(
                                        &claim_pipeline,
                                        &claim_template,
                                        &claim_input,
                                        item,
                                        claim_base.as_deref(),
                                    )
                                    .map(|_| ())
                                },
                            )
                            .await
                        })
                    })
                    .await?
                };
                let (item, mut lease) = match selection {
                    ClaimNext::Claimed { item, lease } => (item, lease),
                    ClaimNext::Pending => {
                        atomic_durable::publication::park_graph_pending(
                            &ctx,
                            &claim_step,
                            &epic,
                            ordinal as u64,
                        )
                        .await?;
                        continue;
                    }
                    ClaimNext::Complete => {
                        if last_output.is_none() {
                            fits(
                                "the workflow's empty graph result",
                                &pipeline.output,
                                &Value::Null,
                            )?;
                        }
                        let (epic_id, workflow) = (epic.clone(), id.clone());
                        ctx.transaction("epic.done", move |mut tx| {
                            let (epic_id, workflow) = (epic_id.clone(), workflow.clone());
                            Box::pin(async move {
                                transactional_claim::finish_epic_run(&mut tx, &epic_id, &workflow)
                                    .await
                            })
                        })
                        .await?;
                        complete = true;
                        break;
                    }
                };
                // This durable step is the boundary between policy edits and a
                // lane already under way. A recovered lane reads its choice from
                // the checkpoint; only a new lane reloads the operator's routes.
                let lane_choice: Option<LaneChoice> = if let Some(source) = &model_source {
                    let (source, kind, labels) =
                        (source.clone(), item.kind.clone(), item.labels.clone());
                    let choice: LaneChoice = ctx
                        .step(
                            &format!("graph.{ordinal:05}.model_choice"),
                            move |_| async move {
                                source
                                    .select(&kind, labels.iter().map(String::as_str))
                                    .map_err(failed)
                            },
                        )
                        .await?;
                    if model_source
                        .as_ref()
                        .and_then(|source| source.endpoints.get(&choice.endpoint))
                        != Some(&choice.target)
                    {
                        return Err(failed(format!(
                            "graph item {}: its recorded model choice differs from the admitted endpoint catalog",
                            item.id
                        )));
                    }
                    Some(choice)
                } else {
                    None
                };
                let (current_input, atom_input) = graph_atom_input(
                    &pipeline,
                    &template,
                    &input,
                    &item,
                    accepted.last().map(String::as_str),
                )?;
                let review_input = atom_input.clone();
                let run = RunScope {
                    pipeline: &pipeline,
                    services: &services,
                    budget: &budget,
                    caps: &caps,
                    exec_caps: &exec_caps,
                    models: &models,
                    routed_models: &plan.routed_models,
                    lane_choice: lane_choice.as_ref(),
                    replay: &replay,
                    pinned: &pinned,
                    worktrees: run_worktrees.as_ref(),
                    admitted_base: admitted_base.as_deref(),
                    accepted: &accepted,
                    caches: run_caches.as_ref(),
                    why,
                };
                let (candidate, told) = turn(
                    &ctx,
                    &run,
                    Turn {
                        name: step_name.clone(),
                        atom: template.atom.clone(),
                        input: atom_input,
                        origins: template.origins.clone(),
                        worktree: true,
                        coding: template.coding,
                        repair: None,
                        charge: None,
                        controls_cargo: false,
                        trial_head: None,
                        claim: Some(lease.clone()),
                    },
                )
                .await?;
                let first_base = if admitted_base.is_none() {
                    Some(
                        candidate
                            .proposal
                            .metadata
                            .get("base")
                            .and_then(Value::as_str)
                            .filter(|base| !base.is_empty())
                            .ok_or_else(|| {
                                failed(format!(
                                    "graph item {} has no host-recorded admission base",
                                    item.id
                                ))
                            })?
                            .to_owned(),
                    )
                } else {
                    None
                };
                let (candidate, acceptance, followups) = if let Some(review) = &template.review {
                    let generated = AtomStep {
                        id: step_name.clone(),
                        ..template.clone()
                    };
                    let empty_steps = json!({});
                    let input_shape = Shape::Schema(&pipeline.input);
                    let steps_shape = Shape::Fields(BTreeMap::new());
                    let values = [
                        ("input", &current_input, &input_shape),
                        ("steps", &empty_steps, &steps_shape),
                    ];
                    reviewed(
                        &ctx,
                        &run,
                        &mut working,
                        &generated,
                        &item.criteria,
                        review,
                        &values,
                        review_input.clone(),
                        (candidate, told),
                        Some(&mut lease),
                        review_routing,
                        item.labels
                            .iter()
                            .any(|label| label.eq_ignore_ascii_case("security")),
                    )
                    .await?
                } else {
                    let generated = AtomStep {
                        id: step_name.clone(),
                        ..template.clone()
                    };
                    let empty_steps = json!({});
                    let input_shape = Shape::Schema(&pipeline.input);
                    let steps_shape = Shape::Fields(BTreeMap::new());
                    let values = [
                        ("input", &current_input, &input_shape),
                        ("steps", &empty_steps, &steps_shape),
                    ];
                    let (candidate, acceptance) = unreviewed_with_criteria(
                        &ctx,
                        &run,
                        &mut working,
                        &generated,
                        &item.criteria,
                        &values,
                        review_input,
                        candidate,
                        told,
                        Some(&lease),
                    )
                    .await?;
                    (candidate, acceptance, Vec::new())
                };
                let kept = GraphAcceptance {
                    acceptance,
                    verified_formatting_path: followups.first().map(|finding| finding.file.clone()),
                };
                let receipt: GraphAcceptance = ctx
                    .step(&format!("{step_name}.accepted"), move |_| async move {
                        Ok(kept)
                    })
                    .await?;
                let acceptance = receipt.acceptance;
                let commit = candidate
                    .proposal
                    .metadata
                    .get("commit")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        failed(format!(
                            "graph item {} has no host-recorded worktree commit",
                            item.id
                        ))
                    })?
                    .to_owned();
                let mapped_result = graph_result(
                    &pipeline,
                    &template,
                    &current_input,
                    &candidate.proposal.output,
                    &candidate.proposal.metadata,
                )?;
                let precondition = lease_precondition(&lease)?;
                let publish_step = format!("{}.publish", step_name);
                // Every proof decision is a durable sibling. If a process dies
                // while parked and a human supplies proof offline, replay still
                // sees the checkpointed Missing outcome and resumes that park.
                let mut occurrence = 0_u64;
                let published = loop {
                    let (
                        candidate,
                        acceptance,
                        token,
                        sha,
                        precondition,
                        recorded_step,
                        followups,
                        epic,
                        run_id,
                    ) = (
                        Arc::new(candidate.clone()),
                        Arc::new(acceptance.clone()),
                        Arc::new(lease.clone()),
                        Arc::new(commit.clone()),
                        Arc::new(precondition.clone()),
                        Arc::new(publish_step.clone()),
                        // CONTROL_OFF: review-followup-file
                        Arc::new(followups.clone()),
                        Arc::new(epic.clone()),
                        Arc::new(id.clone()),
                    );
                    let finalized: GraphFinalize = ctx
                        .transaction(
                            &format!("{step_name}.finalize.{occurrence}"),
                            move |mut tx| {
                                let (
                                    candidate,
                                    acceptance,
                                    token,
                                    sha,
                                    precondition,
                                    recorded_step,
                                    followups,
                                    epic,
                                    run_id,
                                ) = (
                                    candidate.clone(),
                                    acceptance.clone(),
                                    token.clone(),
                                    sha.clone(),
                                    precondition.clone(),
                                    recorded_step.clone(),
                                    followups.clone(),
                                    epic.clone(),
                                    run_id.clone(),
                                );
                                Box::pin(async move {
                                    match transactional_claim::stage_commit_for_close(
                                        &mut tx, &token, &sha,
                                    )
                                    .await?
                                    {
                                        transactional_claim::CommitProof::Missing { branch } => {
                                            Ok(GraphFinalize::Missing { branch })
                                        }
                                        transactional_claim::CommitProof::Fenced => {
                                            Ok(GraphFinalize::Fenced)
                                        }
                                        transactional_claim::CommitProof::Ready => {
                                            let output =
                                                atomic_durable::publication::publish_candidate_in(
                                                    &mut tx,
                                                    &recorded_step,
                                                    &candidate,
                                                    &acceptance,
                                                    &[(*precondition).clone()],
                                                )
                                                .await?;
                                            transactional_claim::file_review_followups(
                                                &mut tx,
                                                &token,
                                                &epic,
                                                &run_id,
                                                &recorded_step,
                                                &followups,
                                            )
                                            .await?;
                                            let close = transactional_claim::close_with_commit;
                                            close(&mut tx, &token, &sha).await?;
                                            Ok(GraphFinalize::Applied { output })
                                        }
                                    }
                                })
                            },
                        )
                        .await?;
                    failpoint("graph:after-proof-stage");
                    match finalized {
                        GraphFinalize::Applied { output } => break output,
                        GraphFinalize::Missing { .. } => {
                            atomic_durable::publication::park_verified_commit(
                                &ctx,
                                &format!("{step_name}.proof.{occurrence}"),
                                &item.id,
                                &commit,
                                occurrence,
                            )
                            .await?;
                            failpoint("graph:after-proof-release");
                        }
                        GraphFinalize::Fenced => validate_before_turn(&ctx, &lease).await?,
                    }
                    occurrence = occurrence.checked_add(1).ok_or_else(|| {
                        DurableError::Engine(
                            "verified-commit park occurrence exceeds stored range".into(),
                        )
                    })?;
                };
                failpoint("graph:after-finalize-applied");
                ctx.step(&publish_step, move |_| async move { Ok(published) })
                    .await?;
                failpoint("graph:after-verified-publish");
                // The authoritative close and its checkpoint committed with
                // Applied. This sibling preserves the public step layout without
                // rechecking mutable graph state on recovery.
                ctx.step(
                    &format!("{}.close", step_name),
                    move |_| async move { Ok(()) },
                )
                .await?;
                if admitted_base.is_none() {
                    admitted_base = first_base;
                }
                accepted.push(commit);
                closed_count += 1;
                last_output = Some(mapped_result);
                failpoint(&format!("pipeline:after:{step_name}"));
                if preclaimed.is_some() {
                    complete = true;
                    break;
                }
            }
        }
        if !complete {
            return Err(failed(format!(
                "epic {epic} exceeded 10,000 work items in one run"
            )));
        }
        let result = last_output.unwrap_or(Value::Null);
        fits("the workflow's result", &pipeline.output, &result)?;
        atomic_core::redact::Line::new()
            .plain("atomic: graph epic ")
            .shown(&epic)
            .plain(" completed ")
            .shown(&closed_count.to_string())
            .plain(" work item(s) in run ")
            .shown(&id)
            .plain("; the epic remains pinned")
            .log();
        if let Some(worktrees) = run_worktrees.clone() {
            let run_id = ctx.workflow_id().to_owned();
            let caches = run_caches.clone();
            let _removed: usize = ctx
                .step("cleanup", move |_| async move {
                    if let Some(caches) = caches {
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
        return Ok(result);
    }
    let mut accepted = Vec::new();
    for (position, step) in pipeline.steps.iter().enumerate() {
        // Before its attempt is recorded: a stopping process begins nothing.
        keep_working(&mut working, &id).await;
        reached(&recovery, position + 1)?;
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
            routed_models: &plan.routed_models,
            lane_choice: None,
            replay: &replay,
            pinned: &pinned,
            worktrees: run_worktrees.as_ref(),
            admitted_base: admitted_base.as_deref(),
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
                coding: step.coding,
                repair: None,
                charge: None,
                controls_cargo: false,
                trial_head: None,
                claim: None,
            },
        )
        .await?;
        let first_worktree_base = if step.worktree && admitted_base.is_none() {
            Some(
                candidate
                    .proposal
                    .metadata
                    .get("base")
                    .and_then(Value::as_str)
                    .filter(|base| !base.is_empty())
                    .ok_or_else(|| {
                        failed(format!(
                            "step `{}`: first worktree turn has no host-recorded admission base",
                            step.id
                        ))
                    })?
                    .to_owned(),
            )
        } else {
            None
        };
        // A replay serves what review accepted when the run was recorded,
        // and reviews nothing again.
        let (candidate, acceptance) = match (&step.review, &replay) {
            (Some(review), None) => {
                let (candidate, acceptance, _) = reviewed(
                    &ctx,
                    &run,
                    &mut working,
                    step,
                    &[],
                    review,
                    &values,
                    atom_input,
                    (candidate, told),
                    None,
                    false,
                    false,
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
            (None, None) => {
                unreviewed_with_criteria(
                    &ctx,
                    &run,
                    &mut working,
                    step,
                    &[],
                    &values,
                    atom_input,
                    candidate,
                    told,
                    None,
                )
                .await?
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
        if admitted_base.is_none() {
            admitted_base = first_worktree_base;
        }
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

fn graph_result(
    pipeline: &Pipeline,
    template: &AtomStep,
    input: &Value,
    output: &Value,
    metadata: &Value,
) -> Result<Value> {
    let contract = pipeline
        .atoms
        .get(&template.atom)
        .and_then(|atom| atom.program.contracts.get("Output"))
        .ok_or_else(|| {
            failed(format!(
                "step `{}` has no atom Output contract",
                template.id
            ))
        })?;
    let metadata_schema = json!({"type": "object", "properties": {
        "input_tokens": {"type": "integer"}, "output_tokens": {"type": "integer"},
        "total_tokens": {"type": "integer"}, "turns": {"type": "integer"}
    }});
    let steps = json!({template.id.clone(): {"output": output, "metadata": metadata}});
    let steps_shape = Shape::Fields(BTreeMap::from([(
        template.id.clone(),
        Shape::fields([
            ("output", Shape::Schema(contract)),
            ("metadata", Shape::Schema(&metadata_schema)),
        ]),
    )]));
    let input_shape = Shape::Schema(&pipeline.input);
    let mapped = atomic_schema::expr::evaluate(
        &pipeline.returns,
        &[
            ("input", input, &input_shape),
            ("steps", &steps, &steps_shape),
        ],
    )
    .map_err(|error| failed(format!("graph workflow `returns`: {error}")))?;
    fits(
        "the graph item's workflow result",
        &pipeline.output,
        &mapped,
    )?;
    Ok(mapped)
}

fn graph_atom_input(
    pipeline: &Pipeline,
    template: &AtomStep,
    original: &Value,
    item: &WorkItem,
    base: Option<&str>,
) -> Result<(Value, Value)> {
    let mut current = original.clone();
    let fields = current
        .as_object_mut()
        .ok_or_else(|| failed("graph run input must be an object".into()))?;
    fields.insert("item".into(), model_graph_item(item)?);
    if let Some(base) = base {
        fields.insert("base".into(), Value::String(base.to_owned()));
    }
    fits("the graph item's workflow input", &pipeline.input, &current)?;
    let scope = json!({});
    let input_shape = Shape::Schema(&pipeline.input);
    let empty_shapes = Shape::Fields(BTreeMap::new());
    let values = [
        ("input", &current, &input_shape),
        ("steps", &scope, &empty_shapes),
    ];
    let mut mapped = Map::new();
    for (field, source) in &template.with {
        let value = atomic_schema::expr::evaluate(source, &values)
            .map_err(|e| failed(format!("step `{}`: `with.{field}`: {e}", template.id)))?;
        mapped.insert(field.clone(), value);
    }
    let atom_input = Value::Object(mapped);
    let contract = pipeline
        .atoms
        .get(&template.atom)
        .and_then(|atom| atom.program.contracts.get("Input"))
        .ok_or_else(|| failed(format!("step `{}` has no atom Input contract", template.id)))?;
    fits(
        &format!("step `{}`'s graph item input", template.id),
        contract,
        &atom_input,
    )?;
    Ok((current, atom_input))
}

fn model_graph_item(item: &WorkItem) -> Result<Value> {
    let mut value =
        serde_json::to_value(item).map_err(|e| failed(format!("encode graph item: {e}")))?;
    if let Some(object) = value.as_object_mut() {
        object.remove("criteria");
        object.remove("labels");
    }
    Ok(value)
}

fn lease_precondition(lease: &Lease) -> Result<Precondition> {
    let session = i64::try_from(lease.session).map_err(|_| {
        failed(format!(
            "graph item {} has an invalid claim session",
            lease.item_id
        ))
    })?;
    Ok(Precondition {
        what: format!("graph claim for {} remains with this run", lease.item_id),
        sql: "UPDATE atomic_graph_leases SET expires_at=expires_at WHERE item_id=? AND claimant=? AND session=? AND epoch=? AND acquisition=? AND EXISTS (SELECT 1 FROM atomic_graph_items WHERE id=? AND status='in_progress' AND claim_epoch=?)".into(),
        params: vec![json!(lease.item_id), json!(lease.claimant), json!(session), json!(lease.epoch), json!(lease.acquisition), json!(lease.item_id), json!(lease.epoch)],
    })
}

fn review_preconditions(claim: Option<&Lease>) -> Result<Vec<Precondition>> {
    claim
        .map(lease_precondition)
        .transpose()
        .map(|condition| condition.into_iter().collect())
}

/// A run-wide limit for shell commands from concurrent graph lanes. The
/// permit covers the backend call and releases even when the command fails.
struct CommandSlots {
    active: std::sync::Mutex<usize>,
    available: std::sync::Condvar,
    limit: usize,
}

static GRAPH_COMMAND_SLOTS: std::sync::LazyLock<Arc<CommandSlots>> =
    std::sync::LazyLock::new(|| Arc::new(CommandSlots::new(1)));

impl CommandSlots {
    fn new(limit: usize) -> Self {
        Self {
            active: std::sync::Mutex::new(0),
            available: std::sync::Condvar::new(),
            limit,
        }
    }

    fn enter(&self, lane: &str) -> CommandSlot<'_> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *active >= self.limit {
            atomic_core::log_line!("atomic: graph lane {lane} waiting for a build slot");
        }
        while *active >= self.limit {
            active = self
                .available
                .wait(active)
                .unwrap_or_else(|error| error.into_inner());
        }
        *active += 1;
        CommandSlot(self)
    }
}

struct CommandSlot<'a>(&'a CommandSlots);

impl Drop for CommandSlot<'_> {
    fn drop(&mut self) {
        let mut active = self
            .0
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *active -= 1;
        self.0.available.notify_one();
    }
}

struct PooledBackend {
    backend: Arc<dyn atomic_exec::ExecutionBackend>,
    slots: Arc<CommandSlots>,
    lane: String,
}

impl atomic_exec::ExecutionBackend for PooledBackend {
    fn connect(&self) -> std::result::Result<atomic_exec::Statement, atomic_exec::ExecError> {
        self.backend.connect()
    }

    fn open(
        &self,
        request: &atomic_exec::Open,
    ) -> std::result::Result<atomic_exec::Workspace, atomic_exec::ExecError> {
        self.backend.open(request)
    }

    fn run(
        &self,
        command: &atomic_exec::Command,
    ) -> std::result::Result<atomic_exec::Completed, atomic_exec::ExecError> {
        let _slot = self.slots.enter(&self.lane);
        self.backend.run(command)
    }

    fn criterion_exclusions(
        &self,
        cwd: &std::path::Path,
    ) -> std::result::Result<Vec<PathBuf>, atomic_exec::ExecError> {
        self.backend.criterion_exclusions(cwd)
    }

    fn close(
        &self,
        workspace: &atomic_exec::Workspace,
    ) -> std::result::Result<(), atomic_exec::ExecError> {
        self.backend.close(workspace)
    }

    fn exchange(
        &self,
        workspace: &atomic_exec::Workspace,
    ) -> std::result::Result<Option<Box<dyn atomic_exec::exchange::Exchange>>, atomic_exec::ExecError>
    {
        self.backend.exchange(workspace)
    }
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
    /// All model names admitted for a graph atom before route selection.
    routed_models: &'a BTreeMap<String, Vec<String>>,
    /// The host item's checkpointed choice, shared by its author and reviews.
    lane_choice: Option<&'a LaneChoice>,
    replay: &'a Option<Arc<Replay>>,
    pinned: &'a Pinned,
    /// The run's worktrees: its fixture's, or the project's.
    worktrees: Option<&'a Arc<Worktrees>>,
    /// The original reviewed revision, frozen before any worktree step ran.
    admitted_base: Option<&'a str>,
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
    /// Whether a committed patch must be supplied for self-review.
    coding: bool,
    /// For a repair round: the rejected output and the verdict asking for
    /// changes.
    repair: Option<Value>,
    /// The position of the turn whose atom allowance this one is charged to,
    /// if not its own: a repair round's is the author's first turn, a later
    /// verdict's the first verdict.
    charge: Option<i32>,
    /// This is the host's controls-exec proof, which receives offline Cargo
    /// inputs selected by the operator rather than atom-authored paths.
    controls_cargo: bool,
    /// Protected main head checked again inside a not-yet-completed review turn.
    trial_head: Option<TrialHead>,
    /// The live graph claim, bounded and renewed inside this durable turn.
    claim: Option<Lease>,
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
        routed_models,
        lane_choice,
        replay,
        pinned,
        why,
        ..
    } = *run;
    // A fixture is its own repository (see `in_worktree`).
    let bound = pipeline.fixture.is_some();
    let mut atom = pipeline
        .atoms
        .get(&t.atom)
        .cloned()
        .ok_or_else(|| failed(format!("step `{}`: no atom `{}`", t.name, t.atom)))?;
    if t.controls_cargo {
        atom = trusted_controls_atom(&atom, run.caches)?;
    }
    let mut runtimes = (*services.runtimes).clone();
    if let Some(choice) = lane_choice {
        let endpoint = choice.model_endpoint();
        runtimes.codex.endpoint = endpoint.clone();
        runtimes.lite.wire = atomic_core::lite::Wire::for_base_url(&endpoint.base_url);
        runtimes.lite.endpoint = endpoint;
        if matches!(atom.kind.as_str(), "codex" | "lite")
            && let Value::Object(frontmatter) = &mut atom.frontmatter
        {
            frontmatter.insert("model".into(), Value::String(choice.target.name.clone()));
        }
    }
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
        let selected = match lane_choice {
            Some(choice) => {
                let allowed = routed_models
                    .get(&t.atom)
                    .is_some_and(|models| models.contains(&choice.target.name));
                if !allowed {
                    return Err(failed(format!(
                        "step `{}`: model {:?} was not admitted for `{}`",
                        t.name, choice.target.name, t.atom
                    )));
                }
                Some(&choice.target.name)
            }
            None => models.get(&t.atom),
        };
        let pinned = match (&atom_caps, selected) {
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
        // Never reached for a run that got this far: a run with a worktree
        // step has its worktrees named, or failed for the reason they could
        // not be named, before its first step. Kept so that a step is never
        // run in a directory of the process's choosing.
        let worktrees = run.worktrees.cloned().ok_or_else(|| {
            failed(format!(
                "step `{}` asks for a worktree, and the run has none",
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
                atom: atom.name.clone(),
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
    let host_cargo = if t.controls_cargo && replay.is_none() {
        let selected = run.services.trial_cargo.as_ref().ok_or_else(|| {
            failed("controls offline Cargo cache unavailable: set ATOMIC_CONTROLS_CARGO_REGISTRY and ATOMIC_CONTROLS_RUSTUP_TOOLCHAIN on the operator process".into())
        })?;
        let cache = run
            .caches
            .ok_or_else(|| failed("controls offline Cargo run cache is missing".into()))?;
        let source = run
            .worktrees
            .ok_or_else(|| failed("controls proof has no bound repository".into()))?
            .repo()
            .git_dir();
        Some(prepare_controls_cargo(
            &cache.for_exec(&t.name),
            selected,
            source,
        )?)
    } else {
        None
    };
    let guard = if atom.kind == "exec" {
        let identity = atomic_core::runtime::EffectIdentity {
            run_id: run_id.clone(),
            step_id: t.name.clone(),
        };
        let (opening, who, backend) = (atom.clone(), identity.clone(), runtimes.backend.shared());
        let (giving, artifacts) = (atom_input.clone(), runtimes.codex.artifacts.clone());
        // The run's own build caches, with a Cargo target of the turn's own.
        let exec_caches = run.caches.map(|c| c.for_exec(&t.name));
        let host_cargo_for_open = host_cargo.clone();
        let place = format!("run {run_id}, step {}", t.name);
        let attempt_step = format!("{}.attempt", t.name);
        let (begun, opened) = if replay.is_some() {
            (attempt::begin(ctx, &attempt_step).await?, None)
        } else {
            attempt::begin_with(ctx, &attempt_step, move || async move {
                let mut opened = atomic_core::runtime::open_exec_workspace_with_cargo(
                    &opening,
                    &who,
                    backend,
                    exec_caches.as_ref(),
                    host_cargo_for_open.as_ref(),
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
    let claim_context = ctx.clone();
    let claim = t.claim.clone();
    let trial_head = t.trial_head.clone();
    let lane_endpoint = if matches!(atom.kind.as_str(), "codex" | "lite") {
        lane_choice.map(|choice| choice.endpoint.clone())
    } else {
        None
    };
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
        let host_cargo = host_cargo.clone();
        let (atom, atom_input, origins) = (atom.clone(), atom_input.clone(), origins.clone());
        let repair = repair.clone();
        let (id, ran, run_id) = (id.clone(), ran.clone(), run_id.clone());
        let (snapshot, tree, guard) = (snapshot.clone(), tree.clone(), guard.clone());
        let pinned_helpers = pinned_helpers.clone();
        let claim_context = claim_context.clone();
        let claim = claim.clone();
        let trial_head = trial_head.clone();
        let lane_endpoint = lane_endpoint.clone();
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
            if let Some(trial_head) = trial_head {
                let (current, checked) = tokio::task::spawn_blocking(move || {
                    trial_head
                        .repo
                        .resolve_commit(&trial_head.reference)
                        .map(|head| (head, trial_head.checked))
                })
                .await
                .map_err(|error| failed(format!("check current trial main: {error}")))?
                .map_err(|error| failed(format!("check current trial main: {error}")))?;
                if current != checked {
                    return Err(failed(format!(
                        "step `{id}`: protected main advanced after the recorded trial merge; start a new run before review"
                    )));
                }
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
                    // The step's recorded context, kept for its command's
                    // validators: the same snapshot a model turn is given, under
                    // the same step id it was resolved for. A contextual atom
                    // whose step brought none is refused before the command.
                    let step_context = snapshot.clone();
                    propose_at(&place, true, async move {
                        let compiled = compile::compile(&atom, &Registry::new())
                            .map_err(|e| AtomError::Runtime(e.to_string()))?;
                        // Opened with the attempt, or, when the attempt
                        // was recorded by an earlier execution, now.
                        let first = opened.lock().ok().and_then(|mut o| o.take());
                        let opened = match first {
                            Some(opened) => opened,
                            None => {
                                atomic_core::runtime::open_exec_workspace_with_cargo(
                                    &atom,
                                    &identity,
                                    backend,
                                    exec_caches.as_ref(),
                                    host_cargo.as_ref(),
                                )
                                .await?
                            }
                        };
                        let dispatch = step_context.as_ref().map(|record| {
                            atomic_core::runtime::DispatchContext {
                                snapshot: record,
                                step_id: &identity.step_id,
                            }
                        });
                        atomic_core::runtime::run_exec_opened_with_context_and_cargo(
                            &atom,
                            &compiled,
                            &atom_input,
                            &identity,
                            opened,
                            clock.as_ref(),
                            &artifacts,
                            &meter,
                            dispatch,
                            host_cargo.as_ref(),
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
                        coding: t.coding,
                    };
                    in_worktree(worktrees, caches, request, call, runtimes, &place, bound).await
                }
            }?;
            let mut proposal = proposal;
            let Value::Object(metadata) = &mut proposal.metadata else {
                return Err(failed("an atom step's metadata is not an object".into()));
            };
            metadata.insert("input_sha256".into(), Value::String(input_sha256));
            if let Some(endpoint) = lane_endpoint {
                metadata.insert("lane_endpoint".into(), Value::String(endpoint));
            }
            failpoint(&ran);
            Ok(proposal)
        };
        // Model calls name the run and step on their spans, and say why they
        // failed here and, if this step is what failed the run, on its span
        // too (see `mark_failed`).
        let work = async move {
            if let Some(lease) = claim {
                let limits = ClaimTurnLimits {
                    lease_for: std::time::Duration::from_secs(900),
                    turn_timeout: std::time::Duration::from_secs(600),
                    heartbeat_every: std::time::Duration::from_secs(30),
                };
                bounded_claimed_turn(&claim_context, lease, limits, |_| work).await
            } else {
                work.await
            }
        };
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

/// The self-review contract is checked by the host as well as by an atom's
/// JSON Schema. A permissive or old schema cannot turn missing evidence into
/// an approval.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelfReviewEvidence {
    items: BTreeMap<String, String>,
    decisions: Vec<DecisionEvidence>,
    controls: Vec<ControlEvidence>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionEvidence {
    point: String,
    allow_refuse: String,
    not_found: String,
    permission_denied: String,
    link_loop: String,
    not_directory: String,
    other_io: String,
    parse_error: String,
    timeout: String,
    empty_result: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ControlEvidence {
    row: String,
    result: String,
}

/// Reviewed decisions are read from the run-bound base tree, never from the
/// candidate or a workflow/model declaration. A candidate cannot alter the
/// inventory in the same change it asks reviewers to accept.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewedDecisionInventory {
    version: u8,
    decision_points: Vec<String>,
}

#[derive(Serialize)]
struct HostReviewInventory<'a> {
    decision_points: &'a [String],
    selected_controls: &'a [String],
    control_results: &'a [ControlEvidence],
}

fn validate_self_review(
    value: &Value,
    spec: &SelfReviewStep,
    host_controls: &[ControlEvidence],
    host_decisions: &[String],
) -> std::result::Result<(), String> {
    let evidence: SelfReviewEvidence = serde_json::from_value(value.clone())
        .map_err(|e| format!("expected items, decisions and controls: {e}"))?;
    if evidence.items.len() != 10 {
        return Err("items must contain exactly the ten checklist entries".into());
    }
    for number in 1..=10 {
        let key = number.to_string();
        if evidence
            .items
            .get(&key)
            .is_none_or(|text| text.trim().is_empty())
        {
            return Err(format!("checklist item {number} has no evidence"));
        }
    }
    let nonempty = |text: &str| !text.trim().is_empty();
    let has_reason = |text: &str| {
        text.strip_prefix("N/A:")
            .is_some_and(|why| !why.trim().is_empty())
    };
    if evidence.decisions.is_empty() && !has_reason(&evidence.items["9"]) {
        return Err("item 9 needs each allow/refuse point or a specific N/A reason".into());
    }
    for (index, point) in evidence.decisions.iter().enumerate() {
        let fields = [
            &point.point,
            &point.allow_refuse,
            &point.not_found,
            &point.permission_denied,
            &point.link_loop,
            &point.not_directory,
            &point.other_io,
            &point.parse_error,
            &point.timeout,
            &point.empty_result,
        ];
        if fields.iter().any(|field| !nonempty(field)) {
            return Err(format!(
                "decision point {} omits an error kind or outcome",
                index + 1
            ));
        }
    }
    for required in spec.decision_points.iter().chain(host_decisions) {
        if !evidence
            .decisions
            .iter()
            .any(|point| point.point == *required)
        {
            return Err(format!(
                "required decision point `{required}` has no evidence"
            ));
        }
    }
    if evidence.controls.is_empty() && !has_reason(&evidence.items["10"]) {
        return Err(
            "item 10 needs each selected controls row and its result or a specific N/A reason"
                .into(),
        );
    }
    let mut rows = std::collections::BTreeSet::new();
    for row in &evidence.controls {
        if !nonempty(&row.row) || !nonempty(&row.result) {
            return Err("a controls row omits its name or result".into());
        }
        if !rows.insert(&row.row) {
            return Err(format!("controls row `{}` appears twice", row.row));
        }
    }
    for required in &spec.selected_controls {
        if !rows.contains(required) {
            return Err(format!("selected controls row `{required}` has no result"));
        }
    }
    let count_mismatch = evidence.controls.len() != host_controls.len();
    if count_mismatch {
        // CONTROL_OFF: self-review-host-count
        return Err("self-review controls differ from the host's exec results".into());
    }
    for proved in host_controls {
        let reported = evidence.controls.iter().find(|row| row.row == proved.row);
        let mismatched = reported.is_none_or(|row| row.result != proved.result);
        if mismatched {
            // CONTROL_OFF: self-review-host-result
            return Err(format!(
                "self-review result for `{}` differs from the host's exec result",
                proved.row
            ));
        }
    }
    Ok(())
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
#[derive(Clone, Debug, Serialize, Deserialize)]
enum TrialMergeResult {
    Passed { main: String },
    Repair { main: String, detail: String },
}

/// A candidate check's output enters a durable checkpoint and an author
/// repair prompt. Redact the bounded diagnostic before either receives it.
fn trial_diagnostic(output: &[u8]) -> String {
    // CONTROL_OFF: trial-diagnostics-redacted
    atomic_core::redact::excerpt(&String::from_utf8_lossy(output), 4096)
}

/// The branch head whose merge and checks passed before review. A fresh
/// reviewer turn verifies it inside its own durable body.
#[derive(Clone)]
struct TrialHead {
    repo: atomic_core::git::Repo,
    reference: String,
    checked: String,
}

/// Git objects for a coding candidate live in its run-bound repository.
/// The checkout that supplied an exec proof atom may be a different project.
fn bound_control_source(run: &RunScope<'_>, step_id: &str) -> Result<String> {
    let worktrees = run.worktrees.ok_or_else(|| {
        failed(format!(
            "step {step_id}: its controls proof has no bound worktree repository"
        ))
    })?;
    Ok(worktrees.repo().git_dir().display().to_string())
}

/// A controls receipt must echo the host's frozen binding, not a value from
/// the candidate or the proof atom. This check runs before the receipt can
/// reach either reviewer.
fn host_control_bound_matches(output: &Value, reference: &str, base: &str, admitted: &str) -> bool {
    let reference_matches = output["bound"]["reference"].as_str() == Some(reference);
    let base_matches = output["bound"]["base"].as_str() == Some(base);
    let admitted_matches = output["bound"]["admitted"].as_str() == Some(admitted);
    reference_matches && base_matches && admitted_matches
}

/// A coding candidate may change source and tests, but it cannot redefine a
/// selected control while asking that control to approve the same candidate.
/// The row name comes from the pinned workflow; its definition comes from the
/// run-bound base commit, before the author received a worktree.
fn selected_control_rows_match_base(
    run: &RunScope<'_>,
    step_id: &str,
    base: &str,
    commit: &str,
    rows: &[String],
) -> Result<()> {
    let worktrees = run.worktrees.ok_or_else(|| {
        failed(format!(
            "step {step_id}: its controls proof has no bound worktree repository"
        ))
    })?;
    let repo = worktrees.repo();
    let baseline = repo.entries(base).map_err(|e| {
        failed(format!(
            "step {step_id}: cannot read selected control rows from bound base: {}",
            e
        ))
    })?;
    let candidate = repo.entries(commit).map_err(|e| {
        failed(format!(
            "step {step_id}: cannot read selected control rows from candidate: {}",
            e
        ))
    })?;
    for row in rows {
        let path = format!("scripts/controls/{row}.row");
        let before = baseline.iter().find(|entry| entry.path == path);
        let after = candidate.iter().find(|entry| entry.path == path);
        if !matches!(before, Some(entry) if entry.kind == "blob" && matches!(entry.mode.as_str(), "100644" | "100755"))
            || before != after
        {
            return Err(failed(format!(
                "step {step_id}: selected control row `{row}` differs from the bound base; repin its definition in a separate trusted change"
            )));
        }
    }
    Ok(())
}

const CODING_DECISIONS: &str = ".atomic/coding-decision-inventory.json";

/// Read the decision list a person reviewed before this coding run. Both the
/// base and candidate are host-recorded commits; the candidate may not edit
/// the list it is being judged against. A missing or malformed inventory is
/// a refusal, not a reason to accept an empty checklist.
fn reviewed_decision_points(
    run: &RunScope<'_>,
    step_id: &str,
    base: &str,
    commit: &str,
) -> Result<Vec<String>> {
    let repo = run
        .worktrees
        .ok_or_else(|| failed(format!("step {step_id}: no bound worktree repository")))?
        .repo();
    let before = repo.entries(base).map_err(|e| {
        failed(format!(
            "step {step_id}: cannot read reviewed decision inventory base: {e}"
        ))
    })?;
    let after = repo.entries(commit).map_err(|e| {
        failed(format!(
            "step {step_id}: cannot read reviewed decision inventory candidate: {e}"
        ))
    })?;
    let baseline = before.iter().find(|entry| entry.path == CODING_DECISIONS);
    let candidate = after.iter().find(|entry| entry.path == CODING_DECISIONS);
    let Some(entry) = baseline else {
        return Err(failed(format!(
            "step {step_id}: reviewed decision inventory `{CODING_DECISIONS}` is absent from the bound base"
        )));
    };
    if entry.kind != "blob"
        || !matches!(entry.mode.as_str(), "100644" | "100755")
        || candidate != Some(entry)
    {
        return Err(failed(format!(
            "step {step_id}: reviewed decision inventory `{CODING_DECISIONS}` is not a fixed regular blob from the bound base"
        )));
    }
    let source = repo.blob_by_id(&entry.id).map_err(|e| {
        failed(format!(
            "step {step_id}: cannot read reviewed decision inventory: {e}"
        ))
    })?;
    if source.len() > 64 * 1024 {
        return Err(failed(format!(
            "step {step_id}: reviewed decision inventory exceeds 64 KiB"
        )));
    }
    let inventory: ReviewedDecisionInventory = serde_json::from_slice(&source).map_err(|e| {
        failed(format!(
            "step {step_id}: reviewed decision inventory is invalid: {e}"
        ))
    })?;
    if inventory.version != 1 || inventory.decision_points.len() > 64 {
        return Err(failed(format!(
            "step {step_id}: reviewed decision inventory has an unsupported version or size"
        )));
    }
    let mut unique = std::collections::BTreeSet::new();
    for point in &inventory.decision_points {
        if point.trim() != point
            || point.is_empty()
            || point.len() > 240
            || point.chars().any(char::is_control)
            || !unique.insert(point)
        {
            return Err(failed(format!(
                "step {step_id}: reviewed decision inventory has an empty, duplicate or invalid point"
            )));
        }
    }
    Ok(inventory.decision_points)
}

fn short_control_test_name(word: &str) -> &str {
    word.rsplit("::").next().unwrap_or(word)
}

fn is_control_test_tree_path(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    parts.len() >= 4 && parts[0] == "crates" && parts[2] == "tests"
}

fn skip_rust_reference_trivia(source: &str, mut at: usize) -> Result<usize> {
    let bytes = source.as_bytes();
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if bytes.get(at..at + 2) == Some(b"//") {
            at = source[at..].find('\n').map_or(bytes.len(), |n| at + n);
        } else if bytes.get(at..at + 2) == Some(b"/*") {
            let mut depth = 1usize;
            at += 2;
            while at < bytes.len() && depth > 0 {
                if bytes.get(at..at + 2) == Some(b"/*") {
                    depth += 1;
                    at += 2;
                } else if bytes.get(at..at + 2) == Some(b"*/") {
                    depth -= 1;
                    at += 2;
                } else {
                    at += 1;
                }
            }
            if depth != 0 {
                return Err(failed(
                    "unterminated Rust comment while selecting controls".into(),
                ));
            }
        } else {
            return Ok(at);
        }
    }
}

/// Returns the end, contents and whether a normal literal used an escape.
fn rust_reference_literal(source: &str, at: usize) -> Result<Option<(usize, &str, bool)>> {
    let bytes = source.as_bytes();
    let mut quote = at;
    let mut hashes = 0usize;
    let raw = bytes.get(at) == Some(&b'r');
    if raw {
        quote += 1;
        while bytes.get(quote) == Some(&b'#') {
            hashes += 1;
            quote += 1;
        }
    }
    if bytes.get(quote) != Some(&b'"') {
        return Ok(None);
    }
    let start = quote + 1;
    let mut end = start;
    let mut escaped = false;
    while end < bytes.len() {
        if !raw && bytes[end] == b'\\' {
            escaped = true;
            end = end.saturating_add(2);
            continue;
        }
        if bytes[end] == b'"'
            && (!raw
                || bytes
                    .get(end + 1..end + 1 + hashes)
                    .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#')))
        {
            return Ok(Some((end + 1 + hashes, &source[start..end], escaped)));
        }
        end += 1;
    }
    Err(failed(
        "unterminated Rust string while selecting controls".into(),
    ))
}

/// Literal include!/include_str!/include_bytes! and #[path] arguments only.
/// Ordinary source strings and comments are deliberately skipped.
fn rust_control_references(source: &str) -> Result<Vec<&str>> {
    let bytes = source.as_bytes();
    let mut at = 0usize;
    let mut references = Vec::new();
    while at < bytes.len() {
        at = skip_rust_reference_trivia(source, at)?;
        if at >= bytes.len() {
            break;
        }
        if let Some((end, _, _)) = rust_reference_literal(source, at)? {
            at = end;
            continue;
        }
        if bytes[at] == b'\'' {
            let mut end = at + 1;
            if bytes.get(end) == Some(&b'\\') {
                end += 1;
                if bytes.get(end) == Some(&b'u') {
                    end = source[end..]
                        .find('}')
                        .map_or(end + 1, |offset| end + offset + 1);
                } else if bytes.get(end) == Some(&b'x') {
                    end += 3;
                } else {
                    end += 1;
                }
            } else if let Some(ch) = source[end..].chars().next() {
                end += ch.len_utf8();
            }
            if bytes.get(end) == Some(&b'\'') {
                at = end + 1;
                continue;
            }
        }
        let mut literal_at = None;
        if source[at..].starts_with("include")
            && (at == 0 || !bytes[at - 1].is_ascii_alphanumeric() && bytes[at - 1] != b'_')
        {
            let mut end = at + "include".len();
            if source[end..].starts_with("_str") {
                end += 4;
            } else if source[end..].starts_with("_bytes") {
                end += 6;
            }
            if !bytes
                .get(end)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                end = skip_rust_reference_trivia(source, end)?;
                if bytes.get(end) == Some(&b'!') {
                    end = skip_rust_reference_trivia(source, end + 1)?;
                    if bytes
                        .get(end)
                        .is_some_and(|byte| matches!(*byte, b'(' | b'[' | b'{'))
                    {
                        literal_at = Some(skip_rust_reference_trivia(source, end + 1)?);
                    }
                }
            }
        } else if bytes[at] == b'#' {
            let mut end = skip_rust_reference_trivia(source, at + 1)?;
            if bytes.get(end) == Some(&b'[') {
                end = skip_rust_reference_trivia(source, end + 1)?;
                if source[end..].starts_with("path") {
                    end = skip_rust_reference_trivia(source, end + 4)?;
                    if bytes.get(end) == Some(&b'=') {
                        literal_at = Some(skip_rust_reference_trivia(source, end + 1)?);
                    }
                }
            }
        }
        if let Some(literal_at) = literal_at
            && let Some((end, value, escaped)) = rust_reference_literal(source, literal_at)?
        {
            if escaped || value.contains('\\') {
                return Err(failed(
                    "escaped Rust source reference cannot be selected safely".into(),
                ));
            }
            references.push(value);
            at = end;
            continue;
        }
        at += source[at..]
            .chars()
            .next()
            .expect("valid source character")
            .len_utf8();
    }
    Ok(references)
}

/// Select rows from the reviewed base, not the candidate. This deliberately
/// includes rows whose own file, switched file, guarding test target or named
/// test text changed. Shared selector/runner changes are refused until a
/// separate trusted update, because their effects cannot be narrowed here.
fn touched_control_rows(
    run: &RunScope<'_>,
    step_id: &str,
    trusted: &str,
    step_base: &str,
    commit: &str,
) -> Result<Vec<String>> {
    let repo = run
        .worktrees
        .ok_or_else(|| failed(format!("step {step_id}: no bound worktree repository")))?
        .repo();
    let baseline = repo.entries(trusted).map_err(|e| {
        failed(format!(
            "step {step_id}: cannot read bound control rows: {e}"
        ))
    })?;
    let candidate = repo.entries(commit).map_err(|e| {
        failed(format!(
            "step {step_id}: cannot read candidate control rows: {e}"
        ))
    })?;
    let step_entries = repo.entries(step_base).map_err(|e| {
        failed(format!(
            "step {step_id}: cannot read current step base: {e}"
        ))
    })?;
    let protected_changed = repo.changed_paths(trusted, commit).map_err(|e| {
        failed(format!(
            "step {step_id}: cannot inspect trusted control definitions: {e}"
        ))
    })?;
    let changed = repo.changed_paths(step_base, commit).map_err(|e| {
        failed(format!(
            "step {step_id}: cannot select touched controls: {e}"
        ))
    })?;
    if changed.len() > 256 || protected_changed.len() > 256 {
        return Err(failed(format!(
            "step {step_id}: too many changed paths to inspect for control selection"
        )));
    }
    if let Some(path) = protected_changed
        .iter()
        .find(|path| matches!(path.as_str(), ".cargo/config" | ".cargo/config.toml"))
    {
        return Err(failed(format!(
            "step {step_id}: candidate changed Cargo config `{path}`; controls proof refuses before reviewer"
        )));
    }
    // A literal includer can name a tracked link whose resolved target has a
    // different basename. Until the host can prove that resolution from both
    // trees, refuse a repository containing links instead of omitting a row.
    if baseline
        .iter()
        .chain(candidate.iter())
        .any(|entry| entry.mode == "120000")
    {
        return Err(failed(format!(
            "step {step_id}: cannot select controls through a tracked path alias"
        )));
    }
    if protected_changed.iter().any(|path| {
        path.starts_with("scripts/controls/")
            || matches!(
                path.as_str(),
                "scripts/controls-check.sh"
                    | "scripts/controls-on.py"
                    | "scripts/controls-touched.py"
                    | "scripts/controls-exec.py"
                    | "scripts/controls-exec-test.sh"
                    | "scripts/controls-local.txt"
                    | "scripts/controls-shared.txt"
            )
    }) {
        return Err(failed(format!(
            "step {step_id}: candidate changes a control definition or shared control driver; review it in a separate trusted update"
        )));
    }
    let changed_set: std::collections::BTreeSet<&str> =
        changed.iter().map(String::as_str).collect();
    let protected_changed_set: std::collections::BTreeSet<&str> =
        protected_changed.iter().map(String::as_str).collect();
    if let Some(shared) = baseline
        .iter()
        .find(|entry| entry.path == "scripts/controls-shared.txt")
    {
        if shared.kind != "blob" || shared.mode != "100644" {
            return Err(failed(format!(
                "step {step_id}: shared control reader list is not a regular reviewed file"
            )));
        }
        let bytes = repo.blob_by_id(&shared.id).map_err(|e| {
            failed(format!(
                "step {step_id}: cannot read shared control readers: {e}"
            ))
        })?;
        let readers = std::str::from_utf8(&bytes).map_err(|_| {
            failed(format!(
                "step {step_id}: shared control readers are not UTF-8"
            ))
        })?;
        for line in readers
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            let (path, selectors) = line.trim().split_once(':').ok_or_else(|| {
                failed(format!(
                    "step {step_id}: invalid shared control reader entry"
                ))
            })?;
            let path = path.trim().trim_start_matches("./");
            if path.is_empty()
                || path.starts_with('/')
                || path.contains('\\')
                || path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
                || selectors.split_whitespace().next().is_none()
                || selectors.split_whitespace().any(|word| {
                    word != "--test"
                        && !word.split_once('=').is_some_and(|(kind, value)| {
                            matches!(kind, "feature" | "package") && !value.is_empty()
                        })
                })
                || !baseline.iter().any(|entry| entry.path == path)
            {
                return Err(failed(format!(
                    "step {step_id}: invalid or stale shared control reader entry"
                )));
            }
            if protected_changed_set.contains(path) {
                return Err(failed(format!(
                    "step {step_id}: candidate changes shared control reader `{path}`; review it in a separate trusted update"
                )));
            }
        }
    }
    let mut changed_text = String::new();
    for path in &changed {
        for tree in [&step_entries, &candidate] {
            if let Some(entry) = tree.iter().find(|entry| entry.path == *path) {
                if entry.kind != "blob" || !matches!(entry.mode.as_str(), "100644" | "100755") {
                    return Err(failed(format!(
                        "step {step_id}: cannot inspect changed control-selection source `{path}`"
                    )));
                }
                let bytes = repo
                    .blob_by_id(&entry.id)
                    .map_err(|e| failed(format!("step {step_id}: changed source `{path}`: {e}")))?;
                if bytes.len() > 2 * 1024 * 1024 {
                    return Err(failed(format!(
                        "step {step_id}: changed source `{path}` is too large to inspect for control selection"
                    )));
                }
                let text = std::str::from_utf8(&bytes).map_err(|_| {
                    failed(format!(
                        "step {step_id}: changed source `{path}` is not UTF-8 for control selection"
                    ))
                })?;
                if changed_text.len().saturating_add(text.len()) > 8 * 1024 * 1024 {
                    return Err(failed(format!(
                        "step {step_id}: changed sources exceed the control-selection input bound"
                    )));
                }
                changed_text.push_str(text);
                changed_text.push('\n');
            }
        }
    }
    // CI follows one level of literal Rust includes and treats test support
    // code/data without a unique target owner as shared. Selecting every row
    // for a possible include and every --test row for a test-tree change is a
    // conservative superset: an ambiguous owner cannot hide a touched row.
    let test_tree_change = changed.iter().any(|path| is_control_test_tree_path(path));
    let mut include_change = false;
    let mut source_bytes = 0usize;
    for tree in [&step_entries, &candidate] {
        for entry in tree.iter().filter(|entry| entry.path.ends_with(".rs")) {
            if entry.kind != "blob" || !matches!(entry.mode.as_str(), "100644" | "100755") {
                return Err(failed(format!(
                    "step {step_id}: cannot inspect Rust source references in `{}`",
                    entry.path
                )));
            }
            let bytes = repo.blob_by_id(&entry.id).map_err(|e| {
                failed(format!(
                    "step {step_id}: cannot read Rust source references in `{}`: {e}",
                    entry.path
                ))
            })?;
            source_bytes = source_bytes.saturating_add(bytes.len());
            if source_bytes > 32 * 1024 * 1024 {
                return Err(failed(format!(
                    "step {step_id}: Rust source references exceed the control-selection input bound"
                )));
            }
            let source = std::str::from_utf8(&bytes).map_err(|_| {
                failed(format!(
                    "step {step_id}: Rust source references in `{}` are not UTF-8",
                    entry.path
                ))
            })?;
            let references = rust_control_references(source).map_err(|e| {
                failed(format!(
                    "step {step_id}: cannot select controls through an escaped Rust source reference in `{}`: {e}",
                    entry.path
                ))
            })?;
            if changed.iter().any(|path| {
                let basename = path.rsplit('/').next().unwrap_or(path);
                references
                    .iter()
                    .any(|reference| reference.contains(basename))
            }) {
                include_change = true;
            }
        }
    }
    let mut selected = Vec::new();
    for entry in baseline
        .iter()
        .filter(|entry| entry.path.starts_with("scripts/controls/"))
    {
        if !entry.path.ends_with(".row") {
            return Err(failed(format!(
                "step {step_id}: non-row file in reviewed controls directory"
            )));
        }
        let name = entry
            .path
            .strip_prefix("scripts/controls/")
            .and_then(|path| path.strip_suffix(".row"))
            .ok_or_else(|| failed(format!("step {step_id}: invalid control row path")))?;
        if name.is_empty()
            || name.contains('/')
            || entry.kind != "blob"
            || !matches!(entry.mode.as_str(), "100644" | "100755")
        {
            return Err(failed(format!(
                "step {step_id}: control row `{name}` is not a regular reviewed row"
            )));
        }
        let bytes = repo.blob_by_id(&entry.id).map_err(|e| {
            failed(format!(
                "step {step_id}: cannot read control row `{name}`: {e}"
            ))
        })?;
        let line = std::str::from_utf8(&bytes)
            .map_err(|_| failed(format!("step {step_id}: control row `{name}` is not UTF-8")))?;
        if !line.ends_with('\n') || line[..line.len() - 1].contains('\n') || line.contains('\r') {
            return Err(failed(format!(
                "step {step_id}: control row `{name}` must be one newline-terminated line"
            )));
        }
        let parts: Vec<&str> = line[..line.len() - 1].split('|').collect();
        if parts.len() != 5
            || parts[0] != name
            || parts[1].is_empty()
            || parts[2].is_empty()
            || parts[4].is_empty()
        {
            return Err(failed(format!(
                "step {step_id}: control row `{name}` has an invalid reviewed definition"
            )));
        }
        let switched = parts[1];
        if switched.starts_with('/')
            || switched.contains('\\')
            || switched
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(failed(format!(
                "step {step_id}: control row `{name}` has an unsafe switched path"
            )));
        }
        let words: Vec<&str> = parts[4].split_whitespace().collect();
        let mut test_targets = Vec::new();
        let mut test_names = Vec::new();
        let mut after_separator = false;
        let mut skip_next = false;
        for (index, word) in words.iter().enumerate() {
            if skip_next {
                skip_next = false;
                continue;
            }
            if after_separator {
                if !word.starts_with('-') {
                    test_names.push(short_control_test_name(word));
                }
            } else if *word == "--" {
                after_separator = true;
            } else if *word == "--test" {
                if let Some(target) = words.get(index + 1) {
                    test_targets.push(*target);
                }
                skip_next = true;
            } else if matches!(
                *word,
                "-p" | "--package" | "--features" | "--bin" | "--example"
            ) {
                skip_next = true;
            } else if !word.starts_with('-') {
                test_names.push(short_control_test_name(word));
            }
        }
        let target_changed = test_targets.iter().any(|target| {
            changed.iter().any(|path| {
                let parts: Vec<&str> = path.split('/').collect();
                parts.len() >= 4
                    && parts[0] == "crates"
                    && parts[2] == "tests"
                    && (parts[3] == format!("{target}.rs") || parts[3] == *target)
            })
        });
        if include_change
            || test_tree_change
            || changed_set.contains(entry.path.as_str())
            || changed_set.contains(switched)
            || target_changed
            || test_names.iter().any(|test| changed_text.contains(test))
        {
            selected.push(name.to_owned());
        }
    }
    selected.sort();
    Ok(selected)
}

/// Controls build inputs come from the operator. A project may carry an empty
/// Cargo config, but may neither change the bound file nor supply settings.
fn controls_cargo_config_matches_base(
    run: &RunScope<'_>,
    step_id: &str,
    base: &str,
    commit: &str,
) -> Result<()> {
    let worktrees = run.worktrees.ok_or_else(|| {
        failed(format!(
            "step {step_id}: controls Cargo proof has no bound repository"
        ))
    })?;
    let repo = worktrees.repo();
    let before = repo
        .entries(base)
        .map_err(|e| failed(format!("step {step_id}: Cargo config base: {e}")))?;
    let after = repo
        .entries(commit)
        .map_err(|e| failed(format!("step {step_id}: Cargo config candidate: {e}")))?;
    for path in [".cargo/config", ".cargo/config.toml"] {
        let baseline = before.iter().find(|entry| entry.path == path);
        let candidate = after.iter().find(|entry| entry.path == path);
        if baseline != candidate
            || baseline.is_some_and(|entry| {
                entry.kind != "blob" || !matches!(entry.mode.as_str(), "100644" | "100755")
            })
        {
            return Err(failed(format!(
                "step {step_id}: candidate changed Cargo config `{path}`; controls proof refuses before reviewer"
            )));
        }
        let Some(entry) = baseline else { continue };
        let bytes = repo.blob_by_id(&entry.id).map_err(|e| {
            failed(format!(
                "step {step_id}: read bound Cargo config `{path}`: {e}"
            ))
        })?;
        let source = std::str::from_utf8(&bytes).map_err(|_| {
            failed(format!(
                "step {step_id}: bound Cargo config `{path}` is not UTF-8"
            ))
        })?;
        let parsed: toml::Value = toml::from_str(source).map_err(|e| {
            failed(format!(
                "step {step_id}: parse bound Cargo config `{path}`: {e}"
            ))
        })?;
        let root = parsed.as_table().ok_or_else(|| {
            failed(format!(
                "step {step_id}: bound Cargo config `{path}` is not a table"
            ))
        })?;
        if root.keys().any(|key| key != "env") {
            return Err(failed(format!(
                "step {step_id}: bound Cargo config `{path}` redirects build inputs outside the operator's offline selection"
            )));
        }
        if let Some(env) = root.get("env") {
            let env = env.as_table().ok_or_else(|| {
                failed(format!(
                    "step {step_id}: bound Cargo config `{path}` has invalid env settings"
                ))
            })?;
            if !env.is_empty() {
                return Err(failed(format!(
                    "step {step_id}: bound Cargo config `{path}` can change host build inputs"
                )));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum CriterionRecord {
    Known(CriterionOutcome),
    Unknown(String),
}

/// Check the frozen workflow and authenticated item declarations in sibling
/// steps. A lost external result is checkpointed as unknown and cannot be
/// turned into approval by releasing its park.
#[allow(clippy::too_many_arguments)]
async fn check_candidate_criteria<C: WorkflowContext>(
    ctx: &C,
    run: &RunScope<'_>,
    working: &mut Option<Running>,
    step: &AtomStep,
    item_criteria: &[Criterion],
    values: &[(&str, &Value, &Shape<'_>); 2],
    candidate: &Candidate,
    round: u64,
    claim: Option<&Lease>,
) -> Result<Option<String>> {
    let author = run
        .pipeline
        .atoms
        .get(&step.atom)
        .and_then(|atom| atom.program.contracts.get("Output"));
    let commit_schema = json!({"type":"object","properties":{
        "commit":{"type":"string"}, "diff":{"type":"string"}
    }});
    let mut fields = vec![("output", author.map_or(Shape::Untyped, Shape::Schema))];
    let mut seen = json!({"output": candidate.proposal.output});
    if step.worktree {
        fields.push(("metadata", Shape::Schema(&commit_schema)));
        seen["metadata"] = candidate.proposal.metadata.clone();
    }
    let candidate_shape = Shape::fields(fields);
    let scope = [values[0], values[1], ("candidate", &seen, &candidate_shape)];
    for (index, criterion) in step.criteria.iter().chain(item_criteria).enumerate() {
        let name = format!("{}.criteria.{round}.{index}", step.id);
        keep_working(working, ctx.workflow_id()).await;
        let record: CriterionRecord = match criterion {
            Criterion::Text { .. } | Criterion::Cel { .. } => {
                let evaluated = criteria::evaluate(
                    criterion,
                    run.services.runtimes.backend.shared().as_ref(),
                    std::env::temp_dir().as_path(),
                    &scope,
                );
                let record = match evaluated {
                    Ok(outcome) => CriterionRecord::Known(outcome),
                    Err(error) => CriterionRecord::Known(CriterionOutcome {
                        passed: false,
                        detail: error.diagnostic(),
                    }),
                };
                ctx.step(&name, move |_| async move { Ok(record) }).await?
            }
            Criterion::Test { .. } | Criterion::Http { .. } => {
                let guard = attempt::begin(ctx, &format!("{name}.attempt")).await?;
                let criterion = criterion.clone();
                let backend = run.services.runtimes.backend.shared();
                let worktrees = if step.worktree {
                    run.worktrees.cloned()
                } else {
                    None
                };
                let caches = run.caches.cloned();
                let commit = candidate
                    .proposal
                    .metadata
                    .get("commit")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let run_id = ctx.workflow_id().to_owned();
                let scratch_name = name.clone();
                ctx.step(&name, move |_| async move {
                    if guard.interrupted() {
                        return Ok(CriterionRecord::Unknown(
                            "execution may have happened before its result was recorded".into(),
                        ));
                    }
                    tokio::task::spawn_blocking(move || {
                        let cwd = if let Some(worktrees) = worktrees {
                            let commit = commit.ok_or_else(|| {
                                failed("criterion has no host-recorded candidate commit".into())
                            })?;
                            worktrees
                                .prepare(&run_id, &scratch_name, &commit)
                                .map_err(|error| failed(format!("criterion scratch: {error}")))?
                                .path
                        } else if let Some(caches) = caches {
                            let path = caches.dir().join("criteria").join(&scratch_name);
                            atomic_core::worktree::Caches::new(path.clone())
                                .create()
                                .map_err(|error| failed(format!("criterion scratch: {error}")))?;
                            path
                        } else {
                            let nonce =
                                atomic_core::credential::fresh_secret().map_err(|error| {
                                    failed(format!("criterion scratch identity: {error}"))
                                })?;
                            let path =
                                std::env::temp_dir().join(format!("atomic-criterion-{nonce}"));
                            std::fs::create_dir(&path)
                                .map_err(|error| failed(format!("criterion scratch: {error}")))?;
                            path
                        };
                        let result = criteria::evaluate(&criterion, backend.as_ref(), &cwd, &[]);
                        let record = match result {
                            Ok(outcome) => CriterionRecord::Known(outcome),
                            Err(error) if error.outcome_unknown() => {
                                CriterionRecord::Unknown(error.diagnostic())
                            }
                            Err(error) => CriterionRecord::Known(CriterionOutcome {
                                passed: false,
                                detail: error.diagnostic(),
                            }),
                        };
                        Ok::<_, DurableError>(record)
                    })
                    .await
                    .map_err(|error| failed(format!("criterion worker stopped: {error}")))?
                })
                .await?
            }
        };
        match record {
            CriterionRecord::Known(outcome) if !outcome.passed => {
                return Ok(Some(format!(
                    "criterion {} failed: {}",
                    index + 1,
                    atomic_core::redact::text(&outcome.detail)
                )));
            }
            CriterionRecord::Known(_) => {}
            CriterionRecord::Unknown(detail) => {
                let detail = atomic_core::redact::text(&detail);
                *working = None;
                atomic_durable::publication::park_criterion(
                    ctx,
                    &name,
                    &format!("{}:{index}", step.id),
                    0,
                )
                .await?;
                // Human release acknowledges the unknown result, never a
                // retry. Relinquish a graph claim before terminalizing so an
                // operator can reconcile the failed epic and start a new run.
                release_failed_criteria_claim(ctx, &step.id, claim, &detail).await?;
                return Err(failed(format!(
                    "step `{}`: criterion {} outcome unknown after runner loss; reconcile before a fresh run: {detail}",
                    step.id,
                    index + 1
                )));
            }
        }
    }
    Ok(None)
}

async fn release_failed_criteria_claim<C: WorkflowContext>(
    ctx: &C,
    step: &str,
    claim: Option<&Lease>,
    reason: &str,
) -> Result<()> {
    let Some(claim) = claim else { return Ok(()) };
    let token = claim.clone();
    let reason = reason.to_owned();
    let released: bool = ctx
        .transaction(&format!("{step}.criteria.failed"), move |mut tx| {
            let token = token.clone();
            let reason = reason.clone();
            Box::pin(async move {
                match transactional_claim::release_after_failure(&mut tx, &token, &reason).await {
                    Ok(_) => Ok(true),
                    Err(DurableError::Parked { .. }) => Ok(false),
                    Err(error) => Err(error),
                }
            })
        })
        .await?;
    if !released {
        validate_before_turn(ctx, claim).await?;
        return Err(failed(format!(
            "step `{step}`: graph claim changed before criterion failure handoff"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn unreviewed_with_criteria<C: WorkflowContext>(
    ctx: &C,
    run: &RunScope<'_>,
    working: &mut Option<Running>,
    step: &AtomStep,
    item_criteria: &[Criterion],
    values: &[(&str, &Value, &Shape<'_>); 2],
    input: Value,
    mut candidate: Candidate,
    author_turn: i32,
    claim: Option<&Lease>,
) -> Result<(Candidate, Acceptance)> {
    if step.criteria.is_empty() && item_criteria.is_empty() {
        return Ok((candidate.clone(), review_candidate(&candidate)));
    }
    let mut round = 0;
    loop {
        let criteria_failure = check_candidate_criteria(
            ctx,
            run,
            working,
            step,
            item_criteria,
            values,
            &candidate,
            round,
            claim,
        )
        .await?;
        match criteria_failure {
            None => return Ok((candidate.clone(), review_candidate(&candidate))),
            Some(detail)
                if round == 0
                    && run
                        .pipeline
                        .atoms
                        .get(&step.atom)
                        .is_some_and(|atom| atom.kind != "exec") =>
            {
                round = 1;
                (candidate, _) = turn(
                    ctx,
                    run,
                    Turn {
                        name: format!("{}.repair.{round}", step.id),
                        atom: step.atom.clone(),
                        input: input.clone(),
                        origins: step.origins.clone(),
                        worktree: step.worktree,
                        coding: step.coding,
                        repair: Some(json!({
                            "output": candidate.proposal.output,
                            "review": {"verdict":"request_changes", "source":"criteria", "findings":detail},
                        })),
                        charge: Some(author_turn),
                        controls_cargo: false,
                        trial_head: None,
                        claim: claim.cloned(),
                    },
                )
                .await?;
            }
            Some(detail) => {
                release_failed_criteria_claim(ctx, &step.id, claim, &detail).await?;
                return Err(failed(format!(
                    "step `{}`: criteria still fail after {round} author repair round(s); escalate: {detail}",
                    step.id
                )));
            }
        }
    }
}

/// Restrict formatting follow-ups to documentation paths outside sensitive areas.
fn safe_review_followup_path(path: &str) -> bool {
    if path.len() > 256
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !(path.starts_with("docs/") || path.starts_with("examples/"))
        || !(path.ends_with(".md") || path.ends_with(".markdown"))
    {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    ![
        "security",
        "permission",
        "auth",
        "credential",
        "secret",
        "sandbox",
        "gate",
        "control",
        "policy",
        "signature",
        "supply-chain",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

const TRAILING_SPACE: &str = "Remove the trailing space.";

/// A single regular Markdown blob changed, including mode and gitlink checks
/// on every other entry. Git returns both recursive listings in tree order.
fn changed_prose_blob<'a>(
    before: &'a [TreeEntry],
    after: &'a [TreeEntry],
) -> Option<(&'a TreeEntry, &'a TreeEntry)> {
    if before.len() != after.len() {
        return None;
    }
    let mut changed = before.iter().zip(after).filter(|(a, b)| a != b);
    let (a, b) = changed.next()?;
    (changed.next().is_none()
        && a.path == b.path
        && safe_review_followup_path(&a.path)
        && a.kind == "blob"
        && b.kind == "blob"
        && a.mode == "100644"
        && b.mode == "100644")
        .then_some((a, b))
}

/// Only plain, single-line ASCII prose qualifies. A second space can make a
/// Markdown hard break. Indentation, markup, embedded code and CRLF are excluded.
fn added_prose_space(before: &[u8], after: &[u8]) -> bool {
    let Some(line) = before.strip_suffix(b"\n") else {
        return false;
    };
    line.first().is_some_and(u8::is_ascii_alphabetic)
        && !line.ends_with(b" ")
        && line
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'.' | b','))
        && after.strip_suffix(b" \n") == Some(line)
}

/// An unavailable tree or blob leaves the candidate without proof. The review
/// may proceed, but a typed finding for it must then take the blocking path.
fn verified_formatting_path(repo: &Repo, base: &str, commit: &str, diff: &str) -> Option<String> {
    if diff.len() > 2048 || !diff.ends_with(" \n") {
        return None;
    }
    let before = repo.entries(base).ok()?;
    let after = repo.entries(commit).ok()?;
    let (a, b) = changed_prose_blob(&before, &after)?;
    let old = repo.blob_by_id(&a.id).ok()?;
    let new = repo.blob_by_id(&b.id).ok()?;
    if !added_prose_space(&old, &new) {
        return None;
    }
    Some(a.path.clone())
}

/// A control row may select the changed Markdown path even when it is the
/// only changed blob, so path proof does not replace this inventory check.
fn review_followup_controls_clean(touched_controls: &[String], security_item: bool) -> bool {
    touched_controls.is_empty() && !security_item
}

pub(crate) fn route_review_findings(
    reviewer: &str,
    verdict: &Value,
    said: &str,
    enabled: bool,
    controls_clean: bool,
    verified_path: Option<&str>,
) -> (String, Vec<ReviewFollowup>) {
    if !enabled || said != "approve" {
        return (said.to_owned(), Vec::new());
    }
    let Some(findings) = verdict.get("findings") else {
        return (said.to_owned(), Vec::new());
    };
    let Some(findings) = findings.as_array() else {
        return ("request_changes".into(), Vec::new());
    };
    // Legacy reviewers use strings as evidence for their own verdict.
    if findings.iter().all(Value::is_string) {
        return (said.to_owned(), Vec::new());
    }
    if findings.len() > 30 || verdict.as_object().is_none_or(|fields| fields.len() != 2) {
        return ("request_changes".into(), Vec::new());
    }
    let mut low = Vec::new();
    let mut blocking = false;
    for finding in findings {
        let Some(fields) = finding.as_object() else {
            blocking = true;
            continue;
        };
        let file = fields.get("file").and_then(Value::as_str).unwrap_or("");
        let message = fields.get("message").and_then(Value::as_str).unwrap_or("");
        let severity = fields.get("severity").and_then(Value::as_str).unwrap_or("");
        let category = fields.get("category").and_then(Value::as_str).unwrap_or("");
        if controls_clean
            && verified_path == Some(file)
            && fields.len() == 5
            && fields.get("line").and_then(Value::as_u64) == Some(1)
            && category == "formatting"
            && message == TRAILING_SPACE
            && matches!(severity, "minor" | "low" | "nit")
        {
            low.push(ReviewFollowup {
                reviewer: reviewer.to_owned(),
                file: file.to_owned(),
                line: Some(1),
                severity: severity.to_owned(),
                category: category.to_owned(),
                message: message.to_owned(),
            });
        } else {
            blocking = true;
        }
    }
    // CONTROL_OFF: review-severity-route
    if blocking {
        ("request_changes".into(), Vec::new())
    } else {
        ("approve".into(), low)
    }
}

#[cfg(test)]
mod severity_routing_tests {
    use super::{GraphPipelineRun, TRAILING_SPACE, added_prose_space, route_review_findings};
    use serde_json::json;

    #[test]
    fn touched_control_rows_block_followups() {
        assert!(super::review_followup_controls_clean(&[], false));
        assert!(!super::review_followup_controls_clean(
            &["docs-guide-row".into()],
            false
        ));
        assert!(!super::review_followup_controls_clean(&[], true));
    }

    #[test]
    fn formatting_proof_requires_one_unchanged_path_and_mode() {
        let entry = |mode: &str, kind: &str, id: &str, path: &str| super::TreeEntry {
            mode: mode.into(),
            kind: kind.into(),
            id: id.into(),
            path: path.into(),
        };
        let old = entry("100644", "blob", "old", "docs/guide.md");
        let new = entry("100644", "blob", "new", "docs/guide.md");
        let gitlink = entry("160000", "commit", "before", "vendor");
        let base = [old.clone(), gitlink.clone()];
        assert!(super::changed_prose_blob(&base, &[new.clone(), gitlink.clone()]).is_some());
        for head in [
            vec![new.clone()],
            vec![new.clone(), entry("160000", "commit", "after", "vendor")],
            vec![
                entry("100755", "blob", "new", "docs/guide.md"),
                gitlink.clone(),
            ],
            vec![
                entry("120000", "blob", "new", "docs/guide.md"),
                gitlink.clone(),
            ],
            vec![
                entry("100644", "blob", "new", "docs/renamed.md"),
                gitlink.clone(),
            ],
            vec![old, gitlink],
        ] {
            assert!(super::changed_prose_blob(&base, &head).is_none());
        }
    }

    #[test]
    fn only_a_single_added_space_on_plain_prose_is_verified() {
        let old = b"Example text.\n";
        assert!(added_prose_space(old, b"Example text. \n"));
        for (before, after) in [
            (old.as_slice(), b"Changed text. \n".as_slice()),
            (old, b"Example text.  \n"),
            (b"Example text. \n", b"Example text.  \n"),
            (b"    code\n", b"    code \n"),
            (b"Example\ntext\n", b"Example\ntext \n"),
            (b"Example *text*\n", b"Example *text* \n"),
            (b"Example\r\n", b"Example \r\n"),
        ] {
            assert!(!added_prose_space(before, after));
        }
    }

    #[test]
    fn security_and_high_findings_cannot_be_demoted_by_approve() {
        let nit = json!({"file":"docs/guide.md","line":1,"severity":"nit","category":"formatting","message":TRAILING_SPACE});
        let route = |findings, clean| {
            route_review_findings(
                "reviewer",
                &json!({"verdict":"approve","findings":findings}),
                "approve",
                true,
                clean,
                Some("docs/guide.md"),
            )
        };
        for verdict in [
            json!({"verdict":"approve","findings":{} }),
            json!({"verdict":"approve","findings":[nit],"reason":"The patch publishes names."}),
        ] {
            assert_eq!(
                route_review_findings(
                    "reviewer",
                    &verdict,
                    "approve",
                    true,
                    true,
                    Some("docs/guide.md")
                ),
                ("request_changes".into(), vec![])
            );
        }
        assert_eq!(route(vec![nit.clone(); 30], true).1.len(), 30);
        for (key, value) in [
            ("file", json!("docs/other.md")),
            ("line", json!(2)),
            ("line", json!(null)),
            ("severity", json!("major")),
            ("category", json!("grammar")),
            (
                "message",
                json!("Keep customer addresses out of this guide."),
            ),
            ("detail", json!("There is also an unresolved objection.")),
        ] {
            let mut finding = nit.clone();
            finding[key] = value;
            assert_eq!(
                route(vec![finding], true),
                ("request_changes".into(), vec![])
            );
        }
        for (findings, clean) in [
            (vec![nit.clone()], false),
            (vec![nit.clone(); 31], true),
            (vec![nit, json!("legacy evidence")], true),
        ] {
            assert_eq!(route(findings, clean), ("request_changes".into(), vec![]));
        }
    }

    #[test]
    fn legacy_string_findings_keep_the_reviewer_decision() {
        for count in [1, 30, 31] {
            for said in ["approve", "revise", "request_changes"] {
                let verdict = json!({"verdict":said, "findings":vec!["legacy evidence"; count]});
                let (routed, followups) =
                    route_review_findings("reviewer", &verdict, said, true, true, None);
                assert_eq!(routed, said);
                assert!(followups.is_empty());
            }
        }
    }

    #[test]
    fn graph_run_recorded_before_routing_recovers_with_its_old_policy() {
        let old = json!({
            "pipeline": {"name":"old-graph","steps":[],"returns":"input",
                "input":{},"output":{},"atoms":{},"pins":{}},
            "input":{},"epic":"epic"
        });
        let recorded: GraphPipelineRun =
            serde_json::from_value(old).expect("older graph run still deserializes");
        assert!(!recorded.review_routing);
        let mut current = serde_json::to_value(recorded).expect("new recording");
        current["review_routing"] = json!(true);
        let admitted: GraphPipelineRun =
            serde_json::from_value(current).expect("new graph run records routing");
        assert!(admitted.review_routing);
    }
}

/// Distinguishes independent acceptance with classified findings from a
/// plain panel approval, including in older published draft receipts.
pub(crate) const FOLLOWUP_REASON_PREFIX: &str = "accepted low findings from reviewer(s) ";

/// The two acceptance reasons written by an independently reviewed panel.
/// Delivery also verifies the published verdict of each individual reviewer.
pub(crate) fn panel_acceptance_reason(members: &str, round: u64, followups: bool) -> String {
    if followups {
        format!("{FOLLOWUP_REASON_PREFIX}{members} as follow-up work in review round {round}")
    } else {
        format!("approved by reviewer(s) {members} in review round {round}")
    }
}

#[allow(clippy::too_many_arguments)]
async fn reviewed<C: WorkflowContext>(
    ctx: &C,
    run: &RunScope<'_>,
    working: &mut Option<Running>,
    step: &AtomStep,
    item_criteria: &[Criterion],
    review: &ReviewStep,
    values: &[(&str, &Value, &Shape<'_>); 2],
    input: Value,
    (mut candidate, author_turn): (Candidate, i32),
    mut claim: Option<&mut Lease>,
    review_routing: bool,
    security_item: bool,
) -> Result<(Candidate, Acceptance, Vec<ReviewFollowup>)> {
    let author = run
        .pipeline
        .atoms
        .get(&step.atom)
        .and_then(|a| a.program.contracts.get("Output"));
    let mut seen_shape = vec![("output", author.map_or(Shape::Untyped, Shape::Schema))];
    let commit_schema = if step.coding {
        json!({"type": "object", "properties": {"commit": {"type": "string"}, "diff": {"type": "string"}}})
    } else {
        json!({"type": "object", "properties": {"commit": {"type": "string"}}})
    };
    if step.worktree {
        seen_shape.push(("metadata", Shape::Schema(&commit_schema)));
    }
    let candidate_shape = Shape::fields(seen_shape);
    let mut first_verdicts = vec![None; review.reviewers.len().max(1)];
    let mut first_self_review = None;
    let mut round = 0;
    let admitted_for_review = if step.coding {
        Some(match run.admitted_base {
            Some(admitted) => admitted.to_owned(),
            None => candidate
                .proposal
                .metadata
                .get("base")
                .and_then(Value::as_str)
                .filter(|base| !base.is_empty())
                .ok_or_else(|| {
                    failed(format!(
                        "step `{}`: first coding turn has no host-recorded admission base",
                        step.id
                    ))
                })?
                .to_owned(),
        })
    } else {
        None
    };
    let members: Vec<&str> = if review.reviewers.is_empty() {
        vec![review.reviewer.as_str()]
    } else {
        review.reviewers.iter().map(String::as_str).collect()
    };
    let (verdict, said, followups, host_requires_changes, deferral_only) = loop {
        let mut controls_clean = !step.coding;

        let mut trial_head = None;
        if let Some(trial) = &review.trial_merge {
            let worktrees = run.worktrees.cloned().ok_or_else(|| {
                failed(format!(
                    "step `{}`: trial merge has no bound repository",
                    step.id
                ))
            })?;
            let observed_repo = if let Some(fixture) = &run.pipeline.fixture {
                Git::find()
                    .and_then(|git| git.discover(&fixture.source))
                    .map_err(|error| failed(format!("trial main source: {error}")))?
            } else {
                worktrees.repo().clone()
            };
            let protected_ref = trial.main.clone();
            let metadata = candidate.proposal.metadata.as_object().ok_or_else(|| {
                failed(format!(
                    "step `{}`: trial merge has no host commit metadata",
                    step.id
                ))
            })?;
            let host = |field: &str| -> Result<String> {
                metadata
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        failed(format!(
                            "step `{}`: trial merge has no host `{field}`",
                            step.id
                        ))
                    })
            };
            let commit = atomic_core::worktree::StepCommit {
                commit: host("commit")?,
                base: host("base")?,
                reference: host("commit_ref")?,
                attempt: 0,
            };
            let backend = run.services.runtimes.backend.shared();
            let trial_cargo = run.services.trial_cargo.clone();
            let trial_denied = run.services.trial_denied.clone();
            let caches = run.caches.cloned().ok_or_else(|| {
                failed(format!("step `{}`: trial merge has no run cache", step.id))
            })?;
            let trial = trial.clone();
            let fixture_source = run
                .pipeline
                .fixture
                .as_ref()
                .map(|fixture| fixture.source.clone());
            let run_id = ctx.workflow_id().to_owned();
            let scratch_step = format!("{}.trial_merge.{round}", step.id);
            let durable_name = scratch_step.clone();
            keep_working(working, ctx.workflow_id()).await;
            let outcome: TrialMergeResult = ctx.step(&durable_name, move |_| async move {
                tokio::task::spawn_blocking(move || {
                    use atomic_core::git::TrialMerge;
                    let (scratch, main, merge, tree) = worktrees
                        .trial_merge(&run_id, &scratch_step, &commit, &trial.main, fixture_source.as_deref())
                        .map_err(|e| failed(format!("trial merge: {}", e.described())))?;
                    if let TrialMerge::Conflicts(paths) = merge {
                        return Ok(TrialMergeResult::Repair {
                            main,
                            detail: format!("merge conflicts in: {}", paths.join(", ")),
                        });
                    }
                    let tree = tree.ok_or_else(|| failed("trial merge has no merged tree".into()))?;
                    let uses_cargo = trial.checks.iter().any(|argv| {
                        argv.first().is_some_and(|program| program == "cargo" || program.ends_with("/cargo"))
                    });
                    let guard_cargo_config = uses_cargo;
                    if guard_cargo_config {
                        // Cargo config can redirect sources or force build
                        // inputs.
                        // Keep both project config spellings bound to base.
                        let before = worktrees.repo().entries(&commit.base).map_err(|e| {
                            failed(format!("trial merge Cargo config base: {e}"))
                        })?;
                        let after = worktrees.repo().entries(&commit.commit).map_err(|e| {
                            failed(format!("trial merge Cargo config candidate: {e}"))
                        })?;
                        for path in [".cargo/config", ".cargo/config.toml"] {
                            let base_config = before.iter().find(|entry| entry.path == path);
                            let candidate_config = after.iter().find(|entry| entry.path == path);
                            if base_config != candidate_config
                                || base_config.is_some_and(|entry| {
                                    entry.kind != "blob"
                                        || !matches!(entry.mode.as_str(), "100644" | "100755")
                                })
                            {
                                return Ok(TrialMergeResult::Repair {
                                    main,
                                    detail: format!(
                                        "candidate changed Cargo config `{path}`; offline Cargo inputs must remain operator-selected"
                                    ),
                                });
                            }
                        }
                    }
                    let cache = caches
                        .for_trial(&scratch)
                        .map_err(|e| failed(format!("trial merge cache: {e}")))?;
                    cache.create().map_err(|e| failed(format!("trial merge cache: {e}")))?;
                    let mut env: Vec<(String, String)> = cache.env().into_iter().collect();
                    let mut cargo_home = None;
                    let cargo = if uses_cargo {
                        let selected = trial_cargo.ok_or_else(|| failed(
                            "trial merge offline Cargo cache unavailable: no operator registry was selected".into()
                        ))?;
                        let home = cache.prepare_trial_cargo(&selected.registry).map_err(|e| {
                            failed(format!("trial merge offline Cargo cache unavailable: {e}"))
                        })?;
                        cargo_home = Some(home.clone());
                        env.extend([
                            ("CARGO_HOME".into(), home.display().to_string()),
                            ("CARGO_NET_OFFLINE".into(), "true".into()),
                            ("RUSTUP_TOOLCHAIN".into(), selected.toolchain.clone()),
                        ]);
                        Some(selected)
                    } else {
                        None
                    };
                    // Cargo and rustc are operator-selected tools. An unset
                    // CARGO_HOME uses rustup's default ~/.cargo/bin; CI may
                    // set CARGO_HOME explicitly instead. Resolve that host
                    // directory before entering the candidate sandbox;
                    // never take a tool path from the candidate's environment.
                    let path = if uses_cargo {
                        let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from);
                        let operator_home = std::env::var_os("HOME").map(PathBuf::from);
                        let bin = trial_toolchain_bin(
                            cargo_home.as_deref(),
                            operator_home.as_deref(),
                            &scratch,
                            cache.dir(),
                        )
                            .ok_or_else(|| failed("trial merge Cargo toolchain bin is missing or candidate-writable".into()))?;
                        format!("{}:/usr/local/bin:/usr/bin:/bin", bin.display())
                    } else {
                        "/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin".into()
                    };
                    env.extend([
                        ("PATH".into(), path),
                        ("HOME".into(), cache.dir().display().to_string()),
                        ("TMPDIR".into(), cache.dir().display().to_string()),
                    ]);
                    for (index, argv) in trial.checks.iter().enumerate() {
                        let cargo_check = cargo.is_some()
                            && (argv[0] == "cargo" || argv[0].ends_with("/cargo"));
                        let verify_cargo_config = cargo_check;
                        if verify_cargo_config {
                            // An earlier check may leave an untracked config
                            // in the writable scratch tree. Compare the
                            // actual files to the merged tree immediately
                            // before Cargo, then mount the directory read-only
                            // for that command.
                            let config_dir = scratch.join(".cargo");
                            match std::fs::symlink_metadata(&config_dir) {
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                    std::fs::create_dir(&config_dir).map_err(|e| {
                                        failed(format!("trial merge Cargo config directory: {e}"))
                                    })?;
                                }
                                Ok(meta) if meta.file_type().is_dir() => {}
                                _ => {
                                    return Ok(TrialMergeResult::Repair {
                                        main: main.clone(),
                                        detail: "trial merge Cargo config directory changed during checks".into(),
                                    });
                                }
                            }
                            let merged_entries = worktrees.repo().entries(&tree).map_err(|e| {
                                failed(format!("trial merge Cargo config tree: {e}"))
                            })?;
                            for path in [".cargo/config", ".cargo/config.toml"] {
                                let expected = merged_entries.iter().find(|entry| entry.path == path);
                                let actual = std::fs::symlink_metadata(scratch.join(path));
                                let same = match (expected, actual) {
                                    (None, Err(e)) if e.kind() == std::io::ErrorKind::NotFound => true,
                                    (Some(entry), Ok(meta))
                                        if entry.kind == "blob"
                                            && matches!(entry.mode.as_str(), "100644" | "100755")
                                            && meta.file_type().is_file() =>
                                    {
                                        let expected_bytes = worktrees.repo().blob_by_id(&entry.id)
                                            .map_err(|e| failed(format!("trial merge Cargo config blob: {e}")))?;
                                        std::fs::read(scratch.join(path))
                                            .is_ok_and(|bytes| bytes == expected_bytes)
                                    }
                                    _ => false,
                                };
                                if !same {
                                    return Ok(TrialMergeResult::Repair {
                                        main: main.clone(),
                                        detail: format!(
                                            "trial merge Cargo config `{path}` changed during checks"
                                        ),
                                    });
                                }
                            }
                        }
                        let check_args = argv[1..].to_vec();
                        if cargo_check
                            && check_args.iter().any(|arg| {
                                arg == "--config"
                                    || arg.starts_with("--config=")
                                    || arg.starts_with('+')
                            })
                        {
                            return Err(failed(format!(
                                "trial merge check {} cannot override operator Cargo config or toolchain",
                                index + 1
                            )));
                        }
                        let config_mount = if cargo_check {
                            Some(scratch.join(".cargo"))
                        } else {
                            None
                        };
                        let check = atomic_exec::Command {
                            program: argv[0].clone(),
                            args: check_args,
                            cwd: scratch.clone(),
                            env: env.clone(),
                            stdin: Vec::new(),
                            time_limit: std::time::Duration::from_secs(300),
                            output_limit: 64 * 1024,
                            cpu_seconds: None,
                            data_bytes: None,
                            writable: vec![scratch.clone(), cache.dir().to_path_buf()],
                            read_only: cargo
                                .iter()
                                .flat_map(|selected| {
                                    std::iter::once(selected.registry.clone())
                                        .chain(
                                            selected
                                                .registry
                                                .parent()
                                                .map(|home| home.join("git"))
                                                .filter(|git| git.is_dir()),
                                        )
                                })
                                .chain(cargo_home.iter().map(|home| home.join("registry")))
                                .chain(cargo_home.iter().cloned())
                                .chain(config_mount)
                                .chain(cargo_home.iter().flat_map(|home| {
                                    [home.join("config"), home.join("config.toml")]
                                }))
                                .chain(
                                    cargo_home
                                        .iter()
                                        .map(|home| home.join("git"))
                                        .filter(|git| git.exists()),
                                )
                                .collect(),
                            network: Vec::new(),
                            denied: trial_denied.clone(),
                            secret_env: Default::default(),
                        };
                        let done = match backend.run(&check) {
                            Ok(done) => done,
                            Err(e @ (atomic_exec::ExecError::TimedOut { .. }
                                | atomic_exec::ExecError::TooLarge { .. })) => {
                                let command = atomic_core::redact::excerpt(&format!("{argv:?}"), 1024);
                                let error = atomic_core::redact::excerpt(&e.to_string(), 4096);
                                return Ok(TrialMergeResult::Repair {
                                    main,
                                    detail: format!("check {} ({command}) failed: {error}", index + 1),
                                });
                            }
                            Err(e) => {
                                let error = atomic_core::redact::excerpt(&e.to_string(), 4096);
                                return Err(failed(format!(
                                    "trial merge check {} could not run in its scratch worktree: {error}", index + 1
                                )));
                            }
                        };
                        if !done.success() {
                            // Candidate checks may print credentials. This
                            // detail is checkpointed and sent to the author
                            // on repair, so redact before either can see it.
                            let stderr = trial_diagnostic(&done.stderr);
                            let stdout = trial_diagnostic(&done.stdout);
                            let command = atomic_core::redact::excerpt(&format!("{argv:?}"), 1024);
                            let detail = format!(
                                "check {} ({command}) exited {:?}; stderr: {stderr:?}; stdout: {stdout:?}",
                                index + 1, done.code,
                            );
                            return Ok(TrialMergeResult::Repair { main, detail });
                        }
                        let changed = worktrees
                            .trial_changed_tracked(&run_id, &scratch_step, &tree)
                            .map_err(|e| failed(format!("trial merge verify tracked files: {}", e.described())))?;
                        if !changed.is_empty() {
                            return Ok(TrialMergeResult::Repair {
                                main,
                                detail: format!("check {} rewrote tracked merged files: {}", index + 1, changed.join(", ")),
                            });
                        }
                    }
                    Ok(TrialMergeResult::Passed { main })
                }).await.map_err(|e| failed(format!("trial merge worker stopped: {e}")))?
            }).await?;
            if let TrialMergeResult::Repair {
                ref main,
                ref detail,
            } = outcome
            {
                if round >= review.rounds {
                    return Err(failed(format!(
                        "step `{}`: trial merge with main {main} needs author repair: {detail}; no reviewer turn was spent",
                        step.id
                    )));
                }
                round += 1;
                keep_working(working, ctx.workflow_id()).await;
                (candidate, _) = turn(
                    ctx, run,
                    Turn {
                        name: format!("{}.repair.{round}", step.id),
                        atom: step.atom.clone(),
                        input: input.clone(),
                        origins: step.origins.clone(),
                        worktree: step.worktree,
                        coding: step.coding,
                        repair: Some(json!({
                            "output": candidate.proposal.output,
                            "review": {"verdict": "request_changes", "source": "trial_merge", "main": main, "findings": detail},
                        })),
                        charge: Some(author_turn),
                        controls_cargo: false,
                        trial_head: None,
                        claim: claim.as_deref().cloned(),
                    },
                ).await?;
                continue;
            }
            let TrialMergeResult::Passed { main } = outcome else {
                unreachable!("repair continues the review loop")
            };
            trial_head = Some(TrialHead {
                repo: observed_repo,
                reference: protected_ref,
                checked: main,
            });
        }
        let criteria_failure = check_candidate_criteria(
            ctx,
            run,
            working,
            step,
            item_criteria,
            values,
            &candidate,
            round,
            claim.as_deref(),
        )
        .await?;
        if let Some(detail) = criteria_failure {
            let verdict = json!({
                "verdict": "request_changes",
                "source": "criteria",
                "findings": detail,
            });
            if round < review.rounds {
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
                        coding: step.coding,
                        repair: Some(json!({
                            "output": candidate.proposal.output,
                            "review": verdict,
                        })),
                        charge: Some(author_turn),
                        controls_cargo: false,
                        trial_head: None,
                        claim: claim.as_deref().cloned(),
                    },
                )
                .await?;
                continue;
            }
            release_failed_criteria_claim(ctx, &step.id, claim.as_deref(), &detail).await?;
            return Err(failed(format!(
                "step `{}`: criteria still fail after {round} author repair round(s); escalate: {detail}",
                step.id
            )));
        }
        let mut seen = json!({"output": candidate.proposal.output});
        if step.worktree {
            seen["metadata"] = if step.coding {
                json!({
                    "commit": candidate.proposal.metadata.get("commit").cloned().unwrap_or(Value::Null),
                    "diff": candidate.proposal.metadata.get("diff").cloned().unwrap_or(Value::Null)
                })
            } else {
                json!({"commit": candidate.proposal.metadata.get("commit").cloned().unwrap_or(Value::Null)})
            };
        }
        let scope = [values[0], values[1], ("candidate", &seen, &candidate_shape)];
        let mut mapped = Map::new();
        for (field, source) in &review.with {
            let value = atomic_schema::expr::evaluate(source, &scope)
                .map_err(|e| failed(format!("step `{}`: `review.with.{field}`: {e}", step.id)))?;
            mapped.insert(field.clone(), value);
        }
        if let Some(self_review) = &review.self_review {
            let mut host_controls = Vec::new();
            let metadata = candidate.proposal.metadata.as_object();
            let host_field = |field: &str| -> Result<String> {
                metadata
                    .and_then(|metadata| metadata.get(field))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        failed(format!(
                            "step `{}`: its candidate has no host-recorded `{field}`",
                            step.id
                        ))
                    })
            };
            let host_commits = if step.coding {
                let commit = host_field("commit")?;
                let base = host_field("base")?;
                let admitted = admitted_for_review.clone().ok_or_else(|| {
                    failed(format!(
                        "step `{}`: coding review has no admitted source revision",
                        step.id
                    ))
                })?;
                let repo = run
                    .worktrees
                    .ok_or_else(|| failed(format!("step `{}`: no bound repository", step.id)))?
                    .repo();
                for revision in [&base, &commit] {
                    if !repo.is_ancestor(&admitted, revision).map_err(|e| {
                        failed(format!(
                            "step `{}`: cannot verify admitted source ancestry: {e}",
                            step.id
                        ))
                    })? {
                        return Err(failed(format!(
                            "step `{}`: coding candidate is not based on the admitted reviewed revision",
                            step.id
                        )));
                    }
                }
                Some((commit, base, admitted))
            } else {
                None
            };
            let (host_decisions, touched_controls) =
                if let Some((commit, base, admitted)) = &host_commits {
                    (
                        reviewed_decision_points(run, &step.id, admitted, commit)?,
                        touched_control_rows(run, &step.id, admitted, base, commit)?,
                    )
                } else {
                    (Vec::new(), Vec::new())
                };
            // CONTROL_OFF: review-followup-touched-controls, review-followup-security-label
            controls_clean = review_followup_controls_clean(&touched_controls, security_item);
            for point in &host_decisions {
                if !self_review.decision_points.contains(point) {
                    return Err(failed(format!(
                        "step `{}`: workflow omits host-inventoried decision point `{point}`",
                        step.id
                    )));
                }
            }
            for row in &touched_controls {
                if !self_review.selected_controls.contains(row) {
                    return Err(failed(format!(
                        "step `{}`: workflow omits touched control row `{row}`",
                        step.id
                    )));
                }
            }
            if let Some(controls_name) = &self_review.controls_atom {
                let (commit, base, admitted) = host_commits.as_ref().ok_or_else(|| {
                    failed(format!(
                        "step `{}`: controls proof needs a coding candidate",
                        step.id
                    ))
                })?;
                run.pipeline.atoms.get(controls_name).ok_or_else(|| {
                    failed(format!(
                        "step `{}`: its controls atom `{controls_name}` is not loaded",
                        step.id
                    ))
                })?;
                let reference = host_field("commit_ref")?;
                let source = bound_control_source(run, &step.id)?;
                selected_control_rows_match_base(
                    run,
                    &step.id,
                    admitted,
                    commit,
                    &self_review.selected_controls,
                )?;
                controls_cargo_config_matches_base(run, &step.id, admitted, commit)?;
                for (index, row) in self_review.selected_controls.iter().enumerate() {
                    let proof_name = format!("{}.controls.{round}.{index}", step.id);
                    let mut proof_input = json!({
                        "source": source.clone(),
                        "commit": commit.clone(),
                        "bound": {"reference": reference.clone(), "base": base.clone(), "admitted": admitted.clone()},
                        "rows": [row]
                    });
                    proof_input["row_seconds"] = json!(240);
                    let mut proof_origins: BTreeMap<String, String> =
                        ["source", "commit", "bound", "rows"]
                            .into_iter()
                            .map(|field| (field.to_owned(), "external".to_owned()))
                            .collect();
                    proof_origins.insert("row_seconds".into(), "external".into());
                    keep_working(working, ctx.workflow_id()).await;
                    let (proof, _) = turn(
                        ctx,
                        run,
                        Turn {
                            name: proof_name.clone(),
                            atom: controls_name.clone(),
                            input: proof_input,
                            origins: proof_origins,
                            worktree: false,
                            coding: false,
                            repair: None,
                            charge: None,
                            controls_cargo: true,
                            trial_head: None,
                            claim: claim.as_deref().cloned(),
                        },
                    )
                    .await?;
                    let output = &proof.proposal.output;
                    let entry = output
                        .get("rows")
                        .and_then(Value::as_array)
                        .filter(|rows| rows.len() == 1)
                        .and_then(|rows| rows.first())
                        .ok_or_else(|| {
                            failed(format!(
                                "step `{}`: control `{row}` has no single host result",
                                step.id
                            ))
                        })?;
                    let verdict = entry.get("verdict").and_then(Value::as_str);
                    let detail = entry
                        .get("detail")
                        .and_then(Value::as_str)
                        .filter(|text| !text.trim().is_empty())
                        .ok_or_else(|| {
                            failed(format!(
                                "step `{}`: control `{row}` has no result detail",
                                step.id
                            ))
                        })?;
                    if output.get("ok") != Some(&Value::Bool(true))
                        || output.get("source").and_then(Value::as_str) != Some(source.as_str())
                        || output.get("commit").and_then(Value::as_str) != Some(commit.as_str())
                        || !host_control_bound_matches(output, &reference, base, admitted)
                        || entry.get("name").and_then(Value::as_str) != Some(row.as_str())
                        || verdict != Some("passed")
                    {
                        return Err(failed(format!(
                            "step `{}`: control `{row}` was not proved before review: {}",
                            step.id,
                            verdict.unwrap_or("missing verdict")
                        )));
                    }
                    publish_candidate(
                        ctx,
                        &format!("{proof_name}.publish"),
                        &proof,
                        &review_candidate(&proof),
                        review_preconditions(claim.as_deref())?,
                    )
                    .await?;
                    host_controls.push(ControlEvidence {
                        row: row.clone(),
                        result: detail.to_owned(),
                    });
                }
            }
            let mut self_input = Map::new();
            for (field, source) in &self_review.with {
                let value = atomic_schema::expr::evaluate(source, &scope).map_err(|e| {
                    failed(format!(
                        "step `{}`: `review.self_review.with.{field}`: {e}",
                        step.id
                    ))
                })?;
                self_input.insert(field.clone(), value);
            }
            if self_review.controls_atom.is_some() {
                self_input.insert(
                    "control_results".into(),
                    serde_json::to_value(&host_controls).map_err(|e| {
                        failed(format!("step `{}`: encode control results: {e}", step.id))
                    })?,
                );
            }
            let self_name = format!("{}.self_review.{round}", step.id);
            keep_working(working, ctx.workflow_id()).await;
            let (self_candidate, told) = turn(
                ctx,
                run,
                Turn {
                    name: self_name.clone(),
                    atom: self_review.atom.clone(),
                    input: Value::Object(self_input),
                    origins: self_review.origins.clone(),
                    worktree: false,
                    coding: false,
                    repair: None,
                    charge: first_self_review,
                    controls_cargo: false,
                    trial_head: trial_head.clone(),
                    claim: claim.as_deref().cloned(),
                },
            )
            .await?;
            first_self_review = first_self_review.or(Some(told));
            let evidence = self_candidate.proposal.output.clone();
            // CONTROL_OFF: self-review-evidence-required
            let checked =
                validate_self_review(&evidence, self_review, &host_controls, &host_decisions);
            checked.map_err(|e| {
                failed(format!(
                    "step `{}`: self-review evidence is incomplete: {e}",
                    step.id
                ))
            })?;
            publish_candidate(
                ctx,
                &format!("{self_name}.publish"),
                &self_candidate,
                &review_candidate(&self_candidate),
                review_preconditions(claim.as_deref())?,
            )
            .await?;
            mapped.insert("self_review".into(), evidence);
            if step.coding {
                mapped.insert(
                    "host_review".into(),
                    serde_json::to_value(HostReviewInventory {
                        decision_points: &host_decisions,
                        selected_controls: &touched_controls,
                        control_results: &host_controls,
                    })
                    .map_err(|e| {
                        failed(format!(
                            "step `{}`: encode host review inventory: {e}",
                            step.id
                        ))
                    })?,
                );
            }
        }
        // A panel is fixed by the workflow. Check the actual nested host
        // inventory against every member's full input schema before the
        // first reviewer turn; a shallow shape check cannot prove array item
        // types or nested required fields.
        let reviewer_input = Value::Object(mapped.clone());
        for reviewer in &members {
            let atom = run.pipeline.atoms.get(*reviewer).ok_or_else(|| {
                failed(format!(
                    "step `{}`: missing reviewer atom `{reviewer}`",
                    step.id
                ))
            })?;
            let schema = atom.program.contracts.get("Input").ok_or_else(|| {
                failed(format!(
                    "step `{}`: reviewer `{reviewer}` has no input schema",
                    step.id
                ))
            })?;
            jsonschema::validator_for(schema)
                .map_err(|e| {
                    failed(format!(
                        "step `{}`: reviewer `{reviewer}` input schema: {e}",
                        step.id
                    ))
                })?
                .validate(&reviewer_input)
                .map_err(|e| {
                    failed(format!(
                        "step `{}`: reviewer `{reviewer}` cannot accept host review input: {e}",
                        step.id
                    ))
                })?;
        }
        let mut decisions = Vec::new();
        let mut round_followups = Vec::new();
        let mut reviewer_origins = review.origins.clone();
        if review.self_review.is_some() {
            reviewer_origins.insert("self_review".into(), "external".into());
        }
        if step.coding && review.self_review.is_some() {
            reviewer_origins.insert("host_review".into(), "external".into());
        }
        for (index, reviewer) in members.iter().enumerate() {
            let name = if review.reviewers.is_empty() {
                format!("{}.review.{round}", step.id)
            } else {
                format!("{}.review.{round}.{index}", step.id)
            };
            keep_working(working, ctx.workflow_id()).await;
            let (verdict, told) = turn(
                ctx,
                run,
                Turn {
                    name: name.clone(),
                    atom: (*reviewer).to_owned(),
                    input: Value::Object(mapped.clone()),
                    origins: reviewer_origins.clone(),
                    worktree: false,
                    coding: false,
                    repair: None,
                    charge: first_verdicts[index],
                    controls_cargo: false,
                    trial_head: trial_head.clone(),
                    claim: claim.as_deref().cloned(),
                },
            )
            .await?;
            first_verdicts[index] = first_verdicts[index].or(Some(told));
            let verdict = publish_candidate(
                ctx,
                &format!("{name}.publish"),
                &verdict,
                &review_candidate(&verdict),
                review_preconditions(claim.as_deref())?,
            )
            .await?;
            let said = verdict
                .get("verdict")
                .and_then(Value::as_str)
                .unwrap_or("escalate")
                .to_owned();
            let (routed, mut followups) = if step.coding && claim.is_some() {
                route_review_findings(
                    reviewer,
                    &verdict,
                    &said,
                    review_routing,
                    controls_clean,
                    candidate
                        .proposal
                        .metadata
                        .get("verified_formatting_path")
                        .and_then(Value::as_str),
                )
            } else {
                (said.clone(), Vec::new())
            };
            round_followups.append(&mut followups);
            let host_routed = routed != said;
            let shown = if !host_routed {
                verdict
            } else {
                json!({"verdict": routed, "review": verdict,
                    "routing": "host severity policy"})
            };
            decisions.push(((*reviewer).to_owned(), shown, routed, host_routed));
        }
        let rejected = decisions.iter().find(|(_, _, said, _)| {
            !matches!(
                atomic_core::workflow::Verdict::of(said),
                atomic_core::workflow::Verdict::Approve | atomic_core::workflow::Verdict::Changes
            )
        });
        let changes = decisions.iter().find(|(_, _, said, _)| {
            matches!(
                atomic_core::workflow::Verdict::of(said),
                atomic_core::workflow::Verdict::Changes
            )
        });
        let said = rejected
            .or(changes)
            .map(|(_, _, said, _)| said.clone())
            .unwrap_or_else(|| "approve".into());
        let verdict = if review.reviewers.is_empty() {
            decisions[0].1.clone()
        } else {
            json!({"reviewers": decisions.iter().map(|(atom, verdict, _, _)|
                json!({"atom": atom, "verdict": verdict})).collect::<Vec<_>>()})
        };
        let host_requires_changes = changes.is_some_and(|row| row.3);
        match atomic_core::workflow::Verdict::of(&said) {
            atomic_core::workflow::Verdict::Approve => {
                // CONTROL_OFF: review-classification-required
                if !round_followups.is_empty() {
                    break (verdict, said, round_followups, false, true);
                }
                let acceptance = Acceptance {
                    candidate: candidate.id.clone(),
                    digest: candidate.digest.clone(),
                    accepted: true,
                    reason: Some(panel_acceptance_reason(&members.join(", "), round, false)),
                };
                return Ok((candidate, acceptance, Vec::new()));
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
                        coding: step.coding,
                        repair: Some(json!({
                            "output": candidate.proposal.output,
                            "review": verdict,
                        })),
                        charge: Some(author_turn),
                        controls_cargo: false,
                        trial_head: None,
                        claim: claim.as_deref().cloned(),
                    },
                )
                .await?;
            }
            _ => break (verdict, said, Vec::new(), host_requires_changes, false),
        }
    };
    // A person decides. The gate's title is atomic's own words; what the
    // reviewer and the author said is what it shows.
    let id = format!("{}.review", step.id);
    let why = match (atomic_core::workflow::Verdict::of(&said), said.as_str()) {
        (atomic_core::workflow::Verdict::Changes, _) if host_requires_changes => {
            format!(
                "reported findings that host review rules require addressing after {round} repair round(s), the most this step allows"
            )
        }
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
    let mut title = if deferral_only {
        format!(
            "Step {} of {}: reviewers approved this candidate. Classify its proposed follow-ups before publication",
            step.id, run.pipeline.name,
        )
    } else {
        format!(
            "Step {} of {}: {} {why}. Approve its output as it stands, or reject it",
            step.id,
            run.pipeline.name,
            if review.reviewers.is_empty() {
                format!("reviewer {}", review.reviewer)
            } else {
                format!("reviewers {}", review.reviewers.join(", "))
            }
        )
    };
    if !followups.is_empty() {
        show["candidate"] = json!({"id": candidate.id, "digest": candidate.digest});
        show["review_round"] = json!(round);
        show["proposed_followups"] = json!(followups);
        show["deferral_only"] = json!(deferral_only);
        title.push_str(". Approval classifies the listed findings as follow-up work. Reject if any finding needs a fix first.");
    }
    let spec = GateSpec {
        workflow: run.pipeline.name.clone(),
        title,
        show,
        evidence: Vec::new(),
        approvers,
        tier: Tier::High,
        timeout: None,
        on_timeout: gate::OnTimeout::Reject,
        question: None,
    };
    // A human may leave this gate unanswered longer than any lease horizon.
    // Release in a sibling transaction step with a handoff before opening the
    // gate. On approval we must acquire a new token; the gate decision by
    // itself never authorizes publication of the old candidate.
    let released_epoch = if let Some(token) = claim.as_deref() {
        let token = token.clone();
        let gate = id.clone();
        Some(
            ctx.transaction(&format!("{id}.claim.release"), move |mut tx| {
                let token = token.clone();
                let gate = gate.clone();
                Box::pin(async move {
                    transactional_claim::release_for_review_gate(&mut tx, &token, &gate).await
                })
            })
            .await?,
        )
    } else {
        None
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
            if let (Some(token), Some(epoch)) = (claim.as_mut(), released_epoch) {
                **token = reclaim_review_gate(
                    ctx,
                    &format!("{id}.claim.reclaim"),
                    token,
                    epoch,
                    &id,
                    std::time::Duration::from_secs(900),
                )
                .await?;
            }
            let acceptance = Acceptance {
                candidate: candidate.id.clone(),
                digest: candidate.digest.clone(),
                accepted: true,
                reason: Some(if deferral_only {
                    panel_acceptance_reason(&members.join(", "), round, true)
                } else {
                    format!("approved by {} at gate {id}", d.by)
                }),
            };
            Ok((candidate, acceptance, followups))
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
        let declared = serde_json::to_value(&step.criteria)
            .map_err(|error| failed(format!("step `{}` criteria: {error}", step.id)))?;
        parse_criteria(&declared)
            .map_err(|error| failed(format!("step `{}` criteria: {error}", step.id)))?;
        let Some(review) = &step.review else {
            if step.coding {
                return Err(failed(format!(
                    "step `{}`: a coding step needs independent review and self-review evidence",
                    step.id
                )));
            }
            continue;
        };
        if step.coding && (!step.worktree || review.self_review.is_none()) {
            return Err(failed(format!(
                "step `{}`: a coding step needs a worktree and self-review evidence before independent review",
                step.id
            )));
        }
        if step.coding && review.trial_merge.is_none() {
            // CONTROL_OFF: coding-review-needs-trial-merge
            return Err(failed(format!(
                "step `{}`: a coding review needs a trial merge and checks before reviewers run",
                step.id
            )));
        }
        if let Some(trial) = &review.trial_merge
            && (!step.coding
                || !step.worktree
                || !trial.main.starts_with("refs/heads/")
                || !atomic_core::git::is_ref_name(&trial.main)
                || trial.checks.is_empty()
                || trial.checks.iter().any(|argv| {
                    argv.is_empty()
                        || argv.len() > 32
                        || argv.iter().any(|arg| {
                            arg.is_empty() || arg.len() > 4096 || arg.chars().any(char::is_control)
                        })
                }))
        {
            return Err(failed(format!(
                "step `{}`: invalid trial merge declaration for a coding worktree step",
                step.id
            )));
        }
        if review.trial_merge.is_some() && !cfg!(target_os = "linux") {
            return Err(failed(format!(
                "step `{}`: trial-merge checks need a Linux execution sandbox",
                step.id
            )));
        }
        if step.coding
            && review.with.get("diff").map(String::as_str) != Some("candidate.metadata.diff")
        {
            return Err(failed(format!(
                "step `{}`: a coding review must bind `diff` to the host's `candidate.metadata.diff` beside self-review evidence",
                step.id
            )));
        }
        if step.coding && review.with.len() != 1 {
            return Err(failed(format!(
                "step `{}`: a coding reviewer receives the host diff and self-review evidence, without author reasoning",
                step.id
            )));
        }
        if step.coding && review.reviewer == step.atom {
            return Err(failed(format!(
                "step `{}`: a coding author cannot be its own independent reviewer",
                step.id
            )));
        }
        if !review.reviewers.is_empty()
            && (!step.coding
                || review.reviewers.len() != 2
                || review.reviewers[0] == review.reviewers[1]
                || review.reviewers[0] != review.reviewer
                || review.reviewers.iter().any(|name| name == &step.atom))
        {
            return Err(failed(format!(
                "step `{}`: a coding reviewer panel needs two distinct, independent reviewers with require-all policy",
                step.id
            )));
        }
        if step.coding
            && review
                .self_review
                .as_ref()
                .and_then(|self_review| self_review.with.get("diff"))
                .map(String::as_str)
                != Some("candidate.metadata.diff")
        {
            return Err(failed(format!(
                "step `{}`: a coding self-review must read the host's `candidate.metadata.diff`",
                step.id
            )));
        }
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
        for name in &review.reviewers {
            if !matches!(kind(name), Some("lite" | "codex")) {
                return Err(failed(format!(
                    "step `{}`: its reviewer `{name}` is not a lite or codex atom of the run",
                    step.id
                )));
            }
        }
        if let Some(self_review) = &review.self_review {
            if !matches!(kind(&self_review.atom), Some("lite" | "codex")) {
                return Err(failed(format!(
                    "step `{}`: its self-reviewer `{}` is not a lite or codex atom of the run",
                    step.id, self_review.atom
                )));
            }
            let unproved_controls =
                !self_review.selected_controls.is_empty() && self_review.controls_atom.is_none();
            if unproved_controls {
                // CONTROL_OFF: controls-proof-required
                return Err(failed(format!(
                    "step `{}`: selected controls need a `controls_atom` before review",
                    step.id
                )));
            }
            if let Some(controls_atom) = &self_review.controls_atom {
                if !cfg!(target_os = "linux") {
                    return Err(failed(format!(
                        "step `{}`: controls proof requires the Linux exec sandbox",
                        step.id
                    )));
                }
                if !step.coding || self_review.selected_controls.is_empty() {
                    return Err(failed(format!(
                        "step `{}`: `controls_atom` needs a coding worktree and selected controls",
                        step.id
                    )));
                }
                if kind(controls_atom) != Some("exec") {
                    return Err(failed(format!(
                        "step `{}`: `controls_atom` `{controls_atom}` is not an exec atom",
                        step.id
                    )));
                }
                let proof_atom = pipeline.atoms.get(controls_atom).ok_or_else(|| {
                    failed(format!(
                        "step `{}`: no controls proof atom `{controls_atom}`",
                        step.id
                    ))
                })?;
                if proof_atom.external().is_none() {
                    return Err(failed(format!(
                        "step `{}`: `controls_atom` `{controls_atom}` needs an idempotent or read-only external effect",
                        step.id
                    )));
                }
                let allows_network = proof_atom
                    .frontmatter
                    .pointer("/network/allow")
                    .and_then(Value::as_array)
                    .is_some_and(|hosts| !hosts.is_empty());
                if allows_network {
                    // CONTROL_OFF: controls-proof-no-network
                    return Err(failed(format!(
                        "step `{}`: `controls_atom` `{controls_atom}` must declare no network hosts",
                        step.id
                    )));
                }
                if self_review.with.contains_key("control_results") {
                    return Err(failed(format!(
                        "step `{}`: `control_results` comes from the host's exec proof",
                        step.id
                    )));
                }
                if self_review
                    .origins
                    .get("control_results")
                    .map(String::as_str)
                    != Some("external")
                {
                    return Err(failed(format!(
                        "step `{}`: host control results must reach self-review as external data",
                        step.id
                    )));
                }
            }
            if review.with.contains_key("self_review") {
                return Err(failed(format!(
                    "step `{}`: `review.with.self_review` is reserved for the self-review evidence",
                    step.id
                )));
            }
            if review
                .origins
                .get("self_review")
                .is_none_or(|origin| origin != "external")
            {
                return Err(failed(format!(
                    "step `{}`: self-review evidence must reach the reviewer as external data",
                    step.id
                )));
            }
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
    coding: bool,
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
    let repo = worktrees.repo().clone();
    let StepCall {
        atom,
        input,
        origins,
        snapshot,
        run_id,
        repair,
        coding,
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
        // This step's build caches and the run's shared cache directory.
        // On a full disk the step waits for room, as for the execution
        // backend's, rather than failing its run.
        let caches = caches.in_worktree(&worktrees.path(&run_id, &step));
        caches.create_waiting(&atom.name, &run_id, &step).await?;
        worktrees
            .run_step(request, |tree| async move {
                let mut here = (*runtimes).clone();
                here.codex.cwd = tree.join(subdirectory);
                here.codex.caches = Some(caches);
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
                StepError::Worktree(e) => {
                    AtomError::Runtime(format!("the step's worktree: {}", e.described()))
                }
                StepError::Waited(e) => e,
            })
    })
    .await;
    let ((output, metadata), commit) = crate::workflows::settled(finished, place, true).await?;
    failpoint("pipeline:after-commit");
    let mut metadata = serde_json::to_value(metadata)
        .map_err(|e| failed(format!("encode the atom's metadata: {e}")))?;
    commit.record(&mut metadata);
    if coding {
        let diff = repo
            .review_diff(&commit.base, &commit.commit)
            .map_err(|e| {
                failed(format!(
                    "the committed worktree diff could not be read: {e}"
                ))
            })?;
        if let Some(path) = verified_formatting_path(&repo, &commit.base, &commit.commit, &diff) {
            metadata["verified_formatting_path"] = Value::String(path);
        }
        metadata["diff"] = Value::String(diff);
    }
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
    /// An ordinary model run must never use the unconfigured endpoint that a
    /// graph-only routing policy allows the engine to carry while resuming.
    pub model_endpoint_configured: bool,
    /// Where each run's definitions are stored.
    pub store: LocalStore,
    /// Where the project runs, which decides who may decide its gates.
    pub deployment: Deployment,
    /// What the project offers a `worktree: true` step whose run is bound to
    /// no fixture, with the reason if it offers none (see
    /// [`ProjectWorktrees`]).
    pub worktrees: ProjectWorktrees,
    /// Where each run's build caches go, the state directory's `cache/`:
    /// a directory of the run's own there, which its worktree and exec
    /// steps may write and which goes when the run ends. `None` puts them
    /// beside the run's worktrees, and a run with neither gets none.
    pub caches: Option<PathBuf>,
    /// Trusted local registry and selected rustup toolchain for offline
    /// trial-merge Cargo checks. Nothing here comes from the candidate.
    pub trial_cargo: Option<TrialCargo>,
    /// Project scratch directories a trial command may not read or write.
    /// The candidate checkout and its own declared cache remain writable.
    pub trial_denied: Vec<PathBuf>,
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
    let (graph_services, graph_ledger) = (services.clone(), ledger.clone());
    let (lane_services, lane_ledger) = (services.clone(), ledger.clone());
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
            run_pipeline(
                ctx,
                run,
                services.clone(),
                ledger.clone(),
                None,
                why,
                None,
                None,
                false,
                1,
                None,
            ),
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
                None,
                None,
                false,
                1,
                None,
            ),
        );
        crate::panics::caught(id, traced)
    });
    rt.register(
        GRAPH_PIPELINE,
        move |ctx: R::Context, graph: GraphPipelineRun| {
            let GraphPipelineRun {
                run,
                epic,
                model_source,
                review_routing,
                lanes,
            } = graph;
            let span = crate::telemetry::workflow_span(&run.pipeline.name, ctx.workflow_id());
            let id = ctx.workflow_id().to_owned();
            let why = atomic_core::usage::Why::default();
            let traced = crate::telemetry::traced(
                span,
                why.clone(),
                run_pipeline(
                    ctx,
                    run,
                    graph_services.clone(),
                    graph_ledger.clone(),
                    None,
                    why,
                    Some(epic),
                    model_source,
                    review_routing,
                    lanes,
                    None,
                ),
            );
            crate::panics::caught(id, traced)
        },
    );
    rt.register(GRAPH_LANE, move |ctx: R::Context, lane: GraphLaneRun| {
        let GraphLaneRun {
            run,
            epic,
            claim,
            model_source,
            review_routing,
        } = lane;
        let span = crate::telemetry::workflow_span(&run.pipeline.name, ctx.workflow_id());
        let id = ctx.workflow_id().to_owned();
        let why = atomic_core::usage::Why::default();
        let mut scoped = (*lane_services).clone();
        let mut runtimes = (*scoped.runtimes).clone();
        let lane_name = format!("graph.{:05}", claim.ordinal);
        let backend = atomic_exec::Backend::connect(PooledBackend {
            backend: runtimes.backend.shared(),
            slots: GRAPH_COMMAND_SLOTS.clone(),
            lane: lane_name,
        });
        let child_ledger = lane_ledger.clone();
        let traced = crate::telemetry::traced(span, why.clone(), async move {
            runtimes.backend =
                backend.map_err(|error| failed(format!("connect graph command pool: {error}")))?;
            scoped.runtimes = Arc::new(runtimes);
            run_pipeline(
                ctx,
                run,
                Arc::new(scoped),
                child_ledger,
                None,
                why,
                Some(epic),
                model_source,
                review_routing,
                1,
                Some(claim),
            )
            .await
        });
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
            if let Some(self_review) = &review.self_review {
                let (self_atom, _) = atom_of(&step.id, &self_review.atom)?;
                for (field, source) in &self_review.with {
                    let read = review_origin(source, &kinds).unwrap_or("external");
                    let (origin, entry) = origin(self_review.origins.get(field), read);
                    if let Some(refused) = untrusted_binding(self_atom, source, &origin) {
                        return Err(format!(
                            "step `{}`: `review.self_review.with.{field}`: {refused}; bind it to {} instead{entry}",
                            step.id,
                            trusted_sources(
                                accepted_trust(self_atom)
                                    .map_err(|_| unknown_trust_advice(self_atom))?
                            ),
                        ));
                    }
                }
                if let Some(refused) = untrusted_binding(reviewer, "self_review", "external") {
                    return Err(format!(
                        "step `{}`: self-review evidence is external data: {refused}",
                        step.id
                    ));
                }
            }
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
    if input.get("epic").is_some() {
        return Err(failed(format!(
            "run {of} is a graph epic run; replay of dynamic graph runs is not supported"
        )));
    }
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
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn graph_command_slots_bound_concurrent_commands() {
        let slots = Arc::new(CommandSlots::new(1));
        let first = slots.enter("first");
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (ready, started) = std::sync::mpsc::channel();
        let next = {
            let slots = slots.clone();
            let entered = entered.clone();
            std::thread::spawn(move || {
                ready.send(()).expect("announce waiting command");
                let _slot = slots.enter("second");
                entered.store(true, std::sync::atomic::Ordering::SeqCst);
            })
        };
        started.recv().expect("waiting command started");
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(
            !entered.load(std::sync::atomic::Ordering::SeqCst),
            "a second command entered while the first held the only build slot"
        );
        drop(first);
        next.join().expect("waiting command");
        assert!(entered.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn trial_cargo_finds_default_rustup_shims_without_cargo_home() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("wall clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("atomic-trial-cargo-{nonce}"));
        let home = root.join("operator");
        let bin = home.join(".cargo/bin");
        let scratch = root.join("candidate");
        let cache = root.join("cache");
        for dir in [&bin, &scratch, &cache] {
            std::fs::create_dir_all(dir).expect("fixture directory");
        }
        for name in ["cargo", "rustc"] {
            std::fs::write(bin.join(name), "#!/bin/sh\nexit 0\n").expect("operator shim");
        }
        assert_eq!(
            trial_toolchain_bin(None, Some(&home), &scratch, &cache),
            Some(std::fs::canonicalize(&bin).expect("default bin"))
        );

        // An explicit candidate-writable CARGO_HOME must not win merely
        // because it contains convincing filenames.
        let untrusted = scratch.join("bin");
        std::fs::create_dir_all(&untrusted).expect("candidate bin");
        for name in ["cargo", "rustc"] {
            std::fs::write(untrusted.join(name), "#!/bin/sh\nexit 0\n").expect("candidate shim");
        }
        assert_ne!(
            trial_toolchain_bin(Some(&scratch), Some(&home), &scratch, &cache),
            Some(std::fs::canonicalize(&untrusted).expect("candidate bin"))
        );
        std::fs::remove_file(bin.join("cargo")).expect("remove operator shim");
        std::os::unix::fs::symlink(untrusted.join("cargo"), bin.join("cargo"))
            .expect("candidate shim link");
        assert_ne!(
            trial_toolchain_bin(None, Some(&home), &scratch, &cache),
            Some(std::fs::canonicalize(&bin).expect("default bin")),
            "a trusted directory may not smuggle a candidate-writable shim"
        );
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn a_host_control_proof_refuses_a_wrong_reference_receipt() {
        let valid = json!({"bound": {
            "reference": "refs/atomic/run/steps/code/1", "base": "base", "admitted": "admitted"
        }});
        assert!(host_control_bound_matches(
            &valid,
            "refs/atomic/run/steps/code/1",
            "base",
            "admitted"
        ));
        let mut forged = valid;
        forged["bound"]["reference"] = json!("refs/atomic/other/steps/code/1");
        assert!(!host_control_bound_matches(
            &forged,
            "refs/atomic/run/steps/code/1",
            "base",
            "admitted"
        ));
    }

    #[test]
    fn a_host_control_proof_refuses_a_wrong_base_receipt() {
        let valid = json!({"bound": {
            "reference": "refs/atomic/run/steps/code/1", "base": "base", "admitted": "admitted"
        }});
        assert!(host_control_bound_matches(
            &valid,
            "refs/atomic/run/steps/code/1",
            "base",
            "admitted"
        ));
        let mut forged = valid;
        forged["bound"]["base"] = json!("other-base");
        assert!(!host_control_bound_matches(
            &forged,
            "refs/atomic/run/steps/code/1",
            "base",
            "admitted"
        ));
    }

    #[test]
    fn frozen_graph_criteria_stay_out_of_model_item_input() {
        let item = WorkItem {
            id: "item-1".into(),
            title: "Title".into(),
            body: "Body".into(),
            acceptance_criteria: Some("Human prose".into()),
            labels: vec!["security".into()],
            criteria: vec![atomic_core::workflow::Criterion::Text {
                text: "Host only".into(),
            }],
            kind: "task".into(),
            priority: 2,
        };
        let visible = model_graph_item(&item).expect("model item");
        assert!(visible.get("criteria").is_none(), "{visible}");
        assert!(visible.get("labels").is_none(), "{visible}");
        assert_eq!(visible["acceptance_criteria"], "Human prose");
        assert_eq!(item.criteria.len(), 1, "host criteria remain frozen");
    }

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

    /// Why a project has no worktrees is what was found about it, and the
    /// two findings are different news: git ran on the directory and refused
    /// it, which is what git does with a directory holding no repository and
    /// with a repository it cannot read alike, so which of them it was goes
    /// unsaid; and git never got to answer at all, which proves nothing about
    /// the directory. Neither lets a worktree step go without one, and neither
    /// is reported as the other.
    #[test]
    fn a_project_with_no_worktrees_says_whether_git_refused_it_or_never_answered() {
        // The temporary directory is outside every repository, which is what
        // makes git refuse it below. Should it ever be inside one, git would
        // find that repository, `at` would return `Found`, and the `Refused`
        // match would panic: the test fails rather than passing on a
        // repository it did not expect.
        let root = std::env::temp_dir().join(format!("atomic-worktrees-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let trees = root.join("worktrees");

        let refused = ProjectWorktrees::at(&root, trees.clone());
        let ProjectWorktrees::Refused(why) = &refused else {
            panic!("git was asked about {root:?} and said no: {refused:?}");
        };
        assert!(why.contains("not a git repository"), "{why}");
        let said = refused.describe();
        assert!(
            said.contains("git refused the project's directory"),
            "{said}"
        );
        assert!(!said.contains("cannot tell"), "{said}");

        // A directory that cannot be entered: git is not asked anything, so
        // nothing is known, and the refusal says so instead of naming a
        // repository that may well be there. What comes back is the file
        // system's own `Permission denied`, not git's words: git never ran.
        std::fs::create_dir_all(root.join("blind")).expect("blind");
        let blind = root.join("blind");
        std::fs::set_permissions(&blind, std::fs::Permissions::from_mode(0o000))
            .expect("unreadable");
        let _seen = Unreadable(&blind);
        let unknown = ProjectWorktrees::at(&blind, trees.clone());
        let ProjectWorktrees::Unknown(why) = &unknown else {
            panic!("git could not be asked about {blind:?} at all: {unknown:?}");
        };
        assert!(why.contains("Permission denied"), "{why}");
        let said = unknown.describe();
        assert!(said.contains("cannot tell"), "{said}");
        assert!(!said.contains("git refused"), "{said}");

        // The reason reaches the run that needs a worktree, with the words
        // that said it, and with the way out.
        for (found, keeps) in [
            (&refused, "not a git repository"),
            (&unknown, "Permission denied"),
        ] {
            let WorktreesForRun::None(why) = found.for_run() else {
                panic!("a project with no repository offers no worktree: {found:?}");
            };
            assert!(why.contains(keeps), "{why}");
            assert!(why.contains("--fixture"), "{why}");
        }
        assert_eq!(
            ProjectWorktrees::FixtureOnly.describe(),
            "atomic makes no worktrees of a project's own checkout",
            "the reason `atomic` itself gives, which is not about the project"
        );
        drop(_seen);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Makes `0` unreadable until the test is done with it, however it ends.
    struct Unreadable<'a>(&'a Path);

    impl Drop for Unreadable<'_> {
        fn drop(&mut self) {
            // Never fails the test that is already failing: the mode is put
            // back, and a refusal to do so is left for the next test to
            // notice rather than added to this one.
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
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
            (
                Err(DurableError::Parked {
                    reason: "a cap".into(),
                    scope: None,
                }),
                false,
            ),
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

    #[test]
    fn legacy_context_projection_normalizes_only_exact_placeholder() {
        let placeholder = json!({"type":"object","additionalProperties":false});
        let mut contracts = json!({"Context": placeholder.clone()});
        normalize_legacy_context("old", None, &mut contracts).expect("old placeholder");
        assert!(contracts.get("Context").is_none());

        for schema in [
            json!({"type":"object","additionalProperties":false,"properties":{"secret":{"type":"string"}}}),
            json!({"type":"object"}),
        ] {
            let mut contracts = json!({"Context": schema.clone()});
            let error = normalize_legacy_context("damaged", None, &mut contracts)
                .expect_err("non-placeholder contract needs a snapshot");
            assert!(
                error.to_string().contains("requires its verified snapshot"),
                "{error}"
            );
            assert_eq!(
                contracts["Context"], schema,
                "refusal did not mutate the contract"
            );
        }
        let declared = json!({"type":"object"});
        let mut contracts = json!({"Context": placeholder.clone()});
        normalize_legacy_context("real", Some(&declared), &mut contracts)
            .expect("real declared context is left for snapshot validation");
        assert_eq!(contracts["Context"], placeholder);
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
