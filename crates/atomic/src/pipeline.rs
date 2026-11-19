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

use crate::metering::RunBudget;
use crate::workflows::propose;
use atomic_core::artifact::{ArtifactStore, LocalStore, Provenance};
use atomic_core::astro::StepClock;
use atomic_core::atom::Atom;
use atomic_core::budget::{Budget, BudgetSettings, Resolved};
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
/// the new one: a step added, removed, renamed or moved, or a checkpoint's
/// shape changed.
pub const LAYOUT_VERSION: &str = "atomic.pipeline.1";

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
    /// The SHA-256 of each file the run uses, by path: the workflow file and
    /// every source of every atom, as they were read when the run was
    /// prepared.
    pub pins: BTreeMap<String, String>,
}

/// The media type a pipeline snapshot is stored under.
pub const SNAPSHOT_MEDIA_TYPE: &str = "application/vnd.atomic.pipeline+json";

/// What a run's first step records: where its definitions were stored, and
/// the hash of every file they came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        pins,
    })
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
    if errors.is_empty() {
        Ok(())
    } else {
        Err(failed(format!(
            "{what} does not fit its contract: {}",
            errors.join("; ")
        )))
    }
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

/// The run's caps and each model atom's, resolved before anything runs: a
/// budget that cannot be resolved, such as a dollar cap on an unpriced model,
/// fails the run here.
fn budgets(
    pipeline: &Pipeline,
    runtimes: &Runtimes,
    ledger: Ledger,
    settings: Arc<BudgetSettings>,
) -> Result<(RunBudget, BTreeMap<String, Resolved>)> {
    let models: BTreeMap<&str, String> = pipeline
        .atoms
        .iter()
        .filter_map(|(name, atom)| Some((name.as_str(), model_of(atom, runtimes)?)))
        .collect();
    let all: Vec<&str> = models.values().map(String::as_str).collect();
    let budget = RunBudget::new(ledger, settings, &pipeline.budget, &all)
        .map_err(|e| failed(format!("workflow {}: budget: {e}", pipeline.name)))?;
    let mut caps = BTreeMap::new();
    for (name, model) in &models {
        let atom = &pipeline.atoms[*name];
        let resolved = budget
            .atom(&atom.budget, model)
            .map_err(|e| failed(format!("atom {name}: budget: {e}")))?;
        caps.insert((*name).to_owned(), resolved);
    }
    Ok((budget, caps))
}

/// Run one pipeline. Workflow code: everything that touches a model or the
/// store goes through durable steps. Every model call is metered against
/// the run's and its atom's budget (see [`crate::metering`]).
async fn run_pipeline<C: WorkflowContext>(
    ctx: C,
    run: PipelineRun,
    services: Arc<Services>,
    ledger: Ledger,
) -> Result<Value> {
    let PipelineRun { pipeline, input } = run;
    let store = services.store.clone();
    fits("the workflow's input", &pipeline.input, &input)?;
    let (budget, caps) = budgets(
        &pipeline,
        &services.runtimes,
        ledger,
        services.budgets.clone(),
    )?;
    // Store the definitions the run uses before anything runs. The snapshot
    // is content-addressed, so a retried step stores the same artifact.
    let snapshot =
        serde_json::to_vec(&pipeline).map_err(|e| failed(format!("encode the pipeline: {e}")))?;
    let files = pipeline.pins.clone();
    let run_id = ctx.workflow_id().to_owned();
    let helper_runtime = atomic_core::astro::Program::current_helper_runtime();
    let pinned: Pinned = ctx
        .step("pin", move |_| async move {
            let manifest = tokio::task::spawn_blocking(move || {
                store.put(
                    &mut snapshot.as_slice(),
                    Provenance {
                        media_type: SNAPSHOT_MEDIA_TYPE.into(),
                        workflow: Some(run_id),
                        step: Some("pin".into()),
                        origin: Some("run_pipeline".into()),
                        labels: Default::default(),
                    },
                )
            })
            .await
            .map_err(|e| failed(format!("store the pipeline: {e}")))?
            .map_err(|e| failed(format!("store the pipeline: {e}")))?;
            Ok(Pinned {
                snapshot: manifest.artifact.to_string(),
                files,
                helper_runtime: Some(helper_runtime),
            })
        })
        .await?;
    let mut steps = Map::new();
    let mut used_worktrees = false;
    let mut accepted = Vec::new();
    for step in &pipeline.steps {
        let scope = json!(steps);
        let values = [("input", &input), ("steps", &scope)];
        let step = match step {
            Step::Atom(step) => step,
            Step::Gate(g) => {
                let decision =
                    run_gate(&ctx, &pipeline.name, g, &values, &services.deployment).await?;
                failpoint(&format!("pipeline:after:{}", g.id));
                steps.insert(g.id.clone(), json!({ "decision": decision }));
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
        let (budget, atom_caps) = (budget.clone(), caps.get(&step.atom).cloned());
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
        let guard = if atom.kind == "exec" {
            let begun = attempt::begin(&ctx, &format!("{}.attempt", step.id)).await?;
            Some((begun, atom.external().is_some()))
        } else {
            None
        };
        let origins = step.origins.clone();
        let pinned_helpers = pinned.helper_runtime.clone();
        let (id, ran) = (step.id.clone(), format!("pipeline:ran:{}", step.id));
        let published = run_atom_at(
            &ctx,
            &step.id,
            move |position| {
                // An exec atom calls no model and has no caps to meter.
                if let Some(atom_caps) = &atom_caps {
                    let meter = budget.meter(&position, atom_caps);
                    runtimes.codex.meter = meter.clone();
                    runtimes.lite.meter = meter;
                }
                async move {
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
                if let Some((begun, declared)) = guard
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
                let proposal = match tree {
                    None => {
                        propose(async move {
                            run_in(&atom, &atom_input, &origins, &run_id, &id, snapshot.as_ref(), &runtimes)
                                .await
                        })
                            .await
                    }
                    Some((worktrees, request)) => {
                        let runtimes = Arc::new(runtimes);
                        let call = StepCall {
                            atom,
                            input: atom_input,
                            origins,
                            snapshot,
                            run_id,
                        };
                        in_worktree(worktrees, request, call, runtimes).await
                    }
                }?;
                failpoint(&ran);
                Ok(proposal)
                }
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
        steps.insert(
            step.id.clone(),
            json!({"output": published.output, "metadata": published.metadata}),
        );
    }
    let scope = json!(steps);
    let result =
        atomic_schema::expr::evaluate(&pipeline.returns, &[("input", &input), ("steps", &scope)])
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
    Ok(result)
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
) -> Result<Proposal> {
    let StepCall {
        atom,
        input,
        origins,
        snapshot,
        run_id,
    } = call;
    let step = request.step.clone();
    let ((output, metadata), commit) = contained(async move {
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
    .await
    .map_err(|e| failed(e.to_string()))?;
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
    values: &[(&str, &Value)],
    deployment: &Deployment,
) -> Result<Value> {
    let at = |e: String| failed(format!("gate `{}`: {e}", step.id));
    let title = match atomic_schema::expr::evaluate(&step.title, values)
        .map_err(|e| at(format!("`title`: {e}")))?
    {
        Value::String(title) => title,
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
    rt.register(PIPELINE, move |ctx: R::Context, run: PipelineRun| {
        run_pipeline(ctx, run, services.clone(), ledger.clone())
    });
}
