// SPDX-License-Identifier: Apache-2.0

//! Workflows written in Rust.
//!
//! Each atom runs through [`run_atom_at`]: its turn and its publication are
//! sibling durable steps, so a crash after an atom has finished never runs it
//! again, and a later atom consumes the earlier one's typed, published
//! output. Each atom's run is [`contained`] in its own task, so a panic fails
//! that atom's step instead of the process.
//!
//! Every model call an atom makes is metered against the run's usage ledger
//! (see [`crate::metering`]): admitted against the atom's and the run's caps
//! before it is dispatched, and settled once after. A call a cap has no room
//! for is not made, and the run parks.

use crate::metering::RunBudget;
use atomic_atoms::{
    ChangeRequest, DraftTask, DraftedTask, ReviewPatch, ReviewRequest, Verdict, WorkItem,
};
use atomic_core::budget::{Budget, BudgetError, BudgetSettings};
use atomic_core::runtime::{RunMetadata, Runtimes, contained, run_codex_atom, run_lite_atom};
use atomic_core::{Atom, AtomConfig, AtomError};
use atomic_durable::{
    DurableError, DurableRuntime, Proposal, Published, Result, WorkflowContext, failpoint,
    run_atom_at,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The change-request workflow: draft a task from a request, then review a
/// candidate commit against it.
pub const CHANGE_REQUEST: &str = "atomic.change-request";

/// The change-request workflow's input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRequestRun {
    /// The request, as a person wrote it.
    pub request: String,
    /// The commit the change starts from.
    pub base: String,
    /// The candidate commit to review.
    pub commit: String,
}

/// What the change-request workflow returns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangeRequestResult {
    /// The task drafted from the request.
    pub task: DraftedTask,
    /// The review of the candidate against it.
    pub verdict: Verdict,
    /// The published results of both atoms, with their run metadata.
    pub steps: Vec<Published>,
}

/// The exit code of a process that stopped because it cannot confine atoms'
/// commands: a configuration error, as `atomic runner`'s contract has it.
pub const UNCONFINABLE_EXIT: i32 = 78;

/// The exit code of a process that stopped because the execution backend
/// had no room for a step's workspace for as long as it waits: a temporary
/// failure, to try again (`atomic runs resume`) once there is room.
pub const NO_ROOM_EXIT: i32 = 75;

/// Set by `atomic serve`: a run whose step cannot confine its commands is
/// set aside instead of stopping the process (see [`settled`]).
static SERVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Make this process `atomic serve`: from now on a step that cannot confine
/// its commands leaves its run unfinished and the process serving.
pub fn serve_past_unconfinable_steps() {
    SERVING.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Where a step's result goes once its atom has run, as [`atom_failure`] has it,
/// except for a step whose commands cannot be confined on this machine:
/// nothing ran, and nothing will here until the machine is fixed, so nothing
/// is recorded. `atomic run` and `atomic runs resume` stop at once with
/// [`UNCONFINABLE_EXIT`]. `atomic serve` keeps serving its other runs and
/// leaves this one unfinished in this process, with no outcome and no
/// recovery spent on it, for the next process that recovers it; `counted`
/// says the run is counted in [`crate::pipeline::steps_running`], which then
/// stops counting it, so a stop does not wait for it.
pub(crate) async fn settled<T>(
    result: std::result::Result<T, AtomError>,
    place: &str,
    counted: bool,
) -> Result<T> {
    match result {
        // This process is stopping: the step gives up its wait for a
        // workspace, records nothing, and is not one a stop waits for.
        Err(AtomError::Stopping) => {
            if counted {
                crate::pipeline::set_aside();
            }
            std::future::pending().await
        }
        // Only `atomic run` and `atomic runs resume` give up: under `atomic
        // serve` a step past its limit is set aside and keeps trying
        // (`atomic_core::runtime::keep_trying_past_the_limit`).
        Err(AtomError::NoRoom(why)) => {
            atomic_core::log_line!("atomic: {why}");
            atomic_core::log_line!(
                "atomic: {place} stays unfinished, with nothing recorded; continue it with \
                 `atomic runs resume` once the execution backend has room"
            );
            std::process::exit(NO_ROOM_EXIT);
        }
        Err(AtomError::Unconfinable(why)) if SERVING.load(std::sync::atomic::Ordering::SeqCst) => {
            atomic_core::log_line!("atomic serve: {place}: {why}");
            atomic_core::log_line!(
                "atomic serve: {place} stays unfinished, with nothing recorded, until atomic \
                 serve is started where the sandbox can start; other runs go on"
            );
            if counted {
                crate::pipeline::set_aside();
            }
            std::future::pending().await
        }
        result => result.map_err(atom_failure),
    }
}

/// Turn an atom's failure into its step's failure, recorded as the outcome;
/// a cap that left no room for its next call parks the run instead. Every
/// workflow converts an atom's error through this one function, so a budget
/// refusal parks the run wherever the atom ran. (An execution backend with
/// no room makes the run wait; see `atomic_core::runtime`.) A step whose
/// commands cannot be confined goes through [`settled`] first, which keeps
/// `atomic serve` from reaching the arm here that stops the process.
pub fn atom_failure(e: AtomError) -> DurableError {
    match e {
        AtomError::Budget(why) => DurableError::Parked(why),
        // Nothing ran, and nothing will on this machine until it is fixed:
        // the process stops, recording nothing, and the run waits to be
        // resumed where atoms' commands can be confined.
        AtomError::Unconfinable(why) => {
            atomic_core::log_line!("atomic: {why}");
            atomic_core::log_line!(
                "atomic: stopping without recording an outcome; resume the run where the sandbox \
                 can start"
            );
            std::process::exit(UNCONFINABLE_EXIT);
        }
        // As above: a run that waited too long for room stops, recording
        // nothing ([`settled`] goes first and says which run).
        AtomError::NoRoom(why) => {
            atomic_core::log_line!("atomic: {why}");
            atomic_core::log_line!(
                "atomic: stopping without recording an outcome; continue the run with `atomic \
                 runs resume` once the execution backend has room"
            );
            std::process::exit(NO_ROOM_EXIT);
        }
        // Like a crash before the checkpoint: recovery runs the step again,
        // through its attempt guard.
        e @ (AtomError::OutcomeUnknown(_) | AtomError::Stopping) => {
            DurableError::Interrupted(e.to_string())
        }
        e => DurableError::Failed(e.to_string()),
    }
}

/// The run settings `atom` asks for over `defaults`.
fn configured(atom: &impl Atom, defaults: &AtomConfig) -> AtomConfig {
    let mut config = defaults.clone();
    atom.configure(&mut config);
    config
}

/// The candidate an atom's run proposes: its output and its run metadata,
/// with no writes yet. The run happens in its own task.
pub async fn propose<O, F>(run: F) -> Result<Proposal>
where
    O: Serialize + Send + 'static,
    F: Future<Output = std::result::Result<(O, RunMetadata), AtomError>> + Send + 'static,
{
    propose_at("a step", false, run).await
}

/// [`propose`] for the step at `place`, whose run [`settled`] says is
/// `counted` or not.
pub(crate) async fn propose_at<O, F>(place: &str, counted: bool, run: F) -> Result<Proposal>
where
    O: Serialize + Send + 'static,
    F: Future<Output = std::result::Result<(O, RunMetadata), AtomError>> + Send + 'static,
{
    let (out, metadata) = settled(contained(run).await, place, counted).await?;
    let encode = |v: serde_json::Result<serde_json::Value>| {
        v.map_err(|e| DurableError::Failed(format!("encode the atom's result: {e}")))
    };
    Ok(Proposal {
        output: encode(serde_json::to_value(out))?,
        metadata: encode(serde_json::to_value(metadata))?,
        writes: Vec::new(),
    })
}

fn decode<T: DeserializeOwned>(published: &Published) -> Result<T> {
    serde_json::from_value(published.output.clone()).map_err(|e| {
        DurableError::Failed(format!(
            "the output of {} does not decode: {e}",
            published.candidate
        ))
    })
}

/// Register the change-request workflow on `rt`, running its atoms on
/// `runtimes` and metering their calls under `budgets`. Fails, before
/// anything is registered, when a budget cannot be resolved, such as a
/// dollar cap on a model the price table does not price.
pub fn register_change_request<R: DurableRuntime>(
    rt: &mut R,
    runtimes: Arc<Runtimes>,
    budgets: Arc<BudgetSettings>,
) -> std::result::Result<(), BudgetError> {
    let draft_config = configured(&DraftTask, &runtimes.lite.defaults);
    let review_config = configured(&ReviewPatch, &runtimes.codex.defaults);
    let draft_model = draft_config
        .model
        .clone()
        .unwrap_or_else(|| runtimes.lite.endpoint.model.clone());
    let review_model = review_config
        .model
        .clone()
        .unwrap_or_else(|| runtimes.codex.endpoint.model.clone());
    let budget = RunBudget::new(
        rt.ledger(),
        budgets,
        &Budget::default(),
        &[&draft_model, &review_model],
    )?;
    let draft_caps = budget.atom(&draft_config.budget, &draft_model)?;
    let review_caps = budget.atom(&review_config.budget, &review_model)?;
    rt.register(
        CHANGE_REQUEST,
        move |ctx: R::Context, input: ChangeRequestRun| {
            let runtimes = runtimes.clone();
            let (budget, draft_caps, review_caps) =
                (budget.clone(), draft_caps.clone(), review_caps.clone());
            let run = ctx.workflow_id().to_owned();
            let span = crate::telemetry::workflow_span(CHANGE_REQUEST, &run);
            let work = async move {
                let (lite, backend) = (runtimes.lite.clone(), runtimes.backend.clone());
                let request = ChangeRequest {
                    request: input.request.clone(),
                };
                let meter = budget.clone();
                let draft = run_atom_at(
                    &ctx,
                    "draft",
                    move |step| {
                        let (mut lite, backend) = (lite.clone(), backend.clone());
                        lite.meter = meter.meter(&step, &draft_caps);
                        let request = request.clone();
                        let span =
                            crate::telemetry::step_span("draft-task", "draft", &step.workflow_id);
                        let step_id = step.workflow_id.clone();
                        crate::telemetry::traced(
                            span,
                            atomic_core::usage::for_step(step_id, "draft", async move {
                                propose(async move {
                                    run_lite_atom(&DraftTask, &request, &lite, &backend).await
                                })
                                .await
                            }),
                        )
                    },
                    Vec::new(),
                )
                .await?;
                failpoint("change-request:after-draft");
                let task: DraftedTask = decode(&draft)?;

                let (codex, backend) = (runtimes.codex.clone(), runtimes.backend.clone());
                let review_request = ReviewRequest {
                    item: WorkItem {
                        id: format!("{}:draft", ctx.workflow_id()),
                        title: task.title.clone(),
                        priority: task.priority,
                        criteria: task.criteria.clone(),
                    },
                    commit: input.commit.clone(),
                    base: input.base.clone(),
                };
                let review = run_atom_at(
                    &ctx,
                    "review",
                    move |step| {
                        let (mut codex, backend) = (codex.clone(), backend.clone());
                        codex.meter = budget.meter(&step, &review_caps);
                        let review_request = review_request.clone();
                        let span = crate::telemetry::step_span(
                            "review-patch",
                            "review",
                            &step.workflow_id,
                        );
                        let step_id = step.workflow_id.clone();
                        crate::telemetry::traced(
                            span,
                            atomic_core::usage::for_step(step_id, "review", async move {
                                propose(async move {
                                    run_codex_atom(&ReviewPatch, &review_request, &codex, &backend)
                                        .await
                                })
                                .await
                            }),
                        )
                    },
                    Vec::new(),
                )
                .await?;
                let verdict: Verdict = decode(&review)?;
                Ok(ChangeRequestResult {
                    task,
                    verdict,
                    steps: vec![draft, review],
                })
            };
            crate::telemetry::traced(span, work)
        },
    );
    Ok(())
}
