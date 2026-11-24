// SPDX-License-Identifier: Apache-2.0

//! Human-gated host delivery of an accepted coding change. A durable intent
//! precedes each outbound call; an unresolved intent is inspected and parked,
//! never blindly sent again after restart.

use crate::delivery_remote::DeliveryRemote;
use anyhow::{Context, bail};
use atomic_app::delivery_evidence::{self, DeliverySource};
use atomic_core::state::ProjectState;
use atomic_durable::delivery_effect::{self, Admission, DeliveryPlan, DeliverySlot};
use atomic_durable::gate::{self, Deployment, GateFilter, GateSpec, OnTimeout, Principal, Tier};
use atomic_durable::publication::park_delivery;
use atomic_durable::{
    Database, DurableError, DurableRuntime, Durare, Result, RunStatus, WorkflowContext, failpoint,
};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
enum DeliveryDecision {
    Applied { receipt: Value },
    Unknown,
}

/// Input frozen when the host starts a delivery run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DeliveryRun {
    pub plan: DeliveryPlan,
    pub operator: String,
    pub source: DeliverySource,
}

fn failed(message: impl Into<String>) -> DurableError {
    DurableError::Failed(message.into())
}

fn slot_name(slot: DeliverySlot) -> &'static str {
    match slot {
        DeliverySlot::Push => "push",
        DeliverySlot::PullRequest => "pull_request",
        DeliverySlot::Enqueue => "enqueue",
    }
}

// The manifest name may change while the private project state ID remains
// fixed. Durare adopts runs into that renamed scope; delivery keeps its
// original approved plan and provider keys across the rename.
fn same_project_state(left: &str, right: &str) -> bool {
    let Some((_, left_id)) = left.rsplit_once('/') else {
        return false;
    };
    let Some((_, right_id)) = right.rsplit_once('/') else {
        return false;
    };
    !left_id.is_empty() && left_id == right_id
}

/// Operator entry points for a reviewed coding item's gated delivery.
#[derive(Debug, Subcommand)]
pub(crate) enum DeliveryCommand {
    /// Open a human gate for one committed, independently reviewed graph item.
    Start {
        /// Source graph run ID.
        source_run: String,
        /// Exact graph step, for example graph.00000.implement.
        #[arg(long)]
        step: String,
        /// Stable delivery run ID; generated when omitted.
        #[arg(long)]
        run_id: Option<String>,
    },
    /// Continue a delivery run after its gate or remote outcome park is released.
    Resume { run_id: String },
}

fn operator_name() -> String {
    std::env::var("USER")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "operator".into())
}

async fn open_runtime(state: &ProjectState, resume: Option<&str>) -> anyhow::Result<Durare> {
    state.create()?;
    let database = crate::engine::database(state)?;
    if matches!(database, Database::Postgres { .. }) {
        bail!("v0.2 graph delivery requires the local SQLite project store");
    }
    atomic_durable::store::open_sqlite(&state.database()).await?;
    let mut rt = Durare::open(
        &database,
        None,
        atomic_app::pipeline::LAYOUT_VERSION,
        &crate::engine::store_project(state)?,
    )
    .await?;
    register_delivery(&mut rt, state.clone());
    if let Some(run) = resume {
        let record = rt
            .own_run_record(run)
            .await?
            .with_context(|| format!("no delivery run {run} in this project"))?;
        if record.workflow != delivery_effect::WORKFLOW {
            bail!("run {run} is not a delivery workflow");
        }
        if record.version != rt.version() {
            bail!(
                "delivery run {run} uses layout {}; this binary uses {}",
                record.version,
                rt.version()
            );
        }
        if let RunStatus::Failed(reason) = &record.status {
            bail!("delivery run {run} already failed: {reason}");
        }
        if let RunStatus::Other(status) = &record.status
            && status != "MAX_RECOVERY_ATTEMPTS_EXCEEDED"
        {
            bail!("delivery run {run} is {status} and cannot be resumed");
        }
        rt.take_resumed_runs(Some(run)).await?;
        if !matches!(record.status, RunStatus::Succeeded) {
            rt.resume(run).await?;
        }
    }
    rt.launch().await?;
    if resume.is_some() {
        gate::redeliver(&rt).await?;
        atomic_durable::release::redeliver(&rt).await?;
    }
    Ok(rt)
}

/// Start or resume the delivery workflow without selecting a model endpoint.
pub(crate) async fn command(
    command: DeliveryCommand,
    state: &ProjectState,
    as_json: bool,
) -> anyhow::Result<()> {
    state.create()?;
    state.lock_execution("atomic delivery")?.hold_until_exit();
    match command {
        DeliveryCommand::Start {
            source_run,
            step,
            run_id,
        } => {
            let remote = DeliveryRemote::open(state)?;
            let rt = open_runtime(state, None).await?;
            let source = delivery_evidence::approved_graph_item(&rt, &source_run, &step).await?;
            let project = crate::engine::store_project(state)?;
            if !source
                .project
                .as_deref()
                .is_some_and(|source_project| same_project_state(source_project, &project))
            {
                bail!("reviewed graph item belongs to another project");
            }
            if remote.base_ref() != source.trial_main_ref {
                bail!(
                    "delivery base {} differs from the trial-merged base {}",
                    remote.base_ref(),
                    source.trial_main_ref
                );
            }
            let plan = remote.plan(
                &project,
                &source.source_run,
                &format!("{}:{}", source.base_step, source.occurrence),
                &source.head,
                Some(&source.trial_main_ref),
                Some(&source.trial_checked_sha),
            )?;
            let id = match run_id {
                Some(id) if !id.is_empty() && id.len() <= 128 => id,
                Some(_) => bail!("delivery run ID must have 1..128 bytes"),
                None => format!("delivery-{}", atomic_core::credential::fresh_secret()?),
            };
            if rt.own_run_record(&id).await?.is_some() {
                bail!("delivery run ID {id} already exists in this project");
            }
            rt.start(
                delivery_effect::WORKFLOW,
                &id,
                DeliveryRun {
                    plan: plan.clone(),
                    operator: operator_name(),
                    source,
                },
            )
            .await?;
            for _ in 0..200 {
                let open = gate::gates(
                    &rt,
                    GateFilter {
                        open_only: true,
                        run: Some(id.clone()),
                    },
                )
                .await?;
                if let Some(gate) = open.into_iter().next() {
                    if gate.show != plan.gate_show() || gate.workflow != delivery_effect::WORKFLOW {
                        bail!("delivery gate differs from the requested plan");
                    }
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string(
                                &json!({"run":id,"gate":gate.id,"proposal":gate.proposal,"plan":gate.show})
                            )?
                        );
                    } else {
                        println!(
                            "delivery run {id} awaits human approval at gate {}",
                            gate.id
                        );
                    }
                    return Ok(());
                }
                if let Some(record) = rt.own_run_record(&id).await?
                    && let RunStatus::Failed(reason) = record.status
                {
                    bail!("delivery run {id} failed before opening its gate: {reason}");
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            bail!("delivery run {id} did not open its human gate")
        }
        DeliveryCommand::Resume { run_id } => {
            let rt = open_runtime(state, Some(&run_id)).await?;
            let record = rt
                .own_run_record(&run_id)
                .await?
                .with_context(|| format!("no delivery run {run_id} in this project"))?;
            if record.workflow != delivery_effect::WORKFLOW {
                bail!("run {run_id} is not a delivery workflow");
            }
            for _ in 0..4800 {
                let record = rt
                    .own_run_record(&run_id)
                    .await?
                    .with_context(|| format!("delivery run {run_id} disappeared"))?;
                match record.status {
                    RunStatus::Succeeded => {
                        let output: Value = rt.result(&run_id).await?;
                        if as_json {
                            println!("{}", serde_json::to_string(&output)?);
                        } else {
                            println!("delivery run {run_id} completed");
                        }
                        return Ok(());
                    }
                    RunStatus::Failed(reason) => bail!("delivery run {run_id} failed: {reason}"),
                    RunStatus::Pending | RunStatus::Other(_) => {}
                }
                if let Some(park) = atomic_durable::release::waiting(&rt)
                    .await?
                    .into_iter()
                    .find(|park| park.run == run_id)
                {
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string(
                                &json!({"run":run_id,"park":park.id,"status":"outcome_unknown"})
                            )?
                        );
                    } else {
                        println!(
                            "delivery run {run_id} awaits reconciliation at park {}",
                            park.id
                        );
                    }
                    return Ok(());
                }
                let open = gate::gates(
                    &rt,
                    GateFilter {
                        open_only: true,
                        run: Some(run_id.clone()),
                    },
                )
                .await?;
                if let Some(gate) = open.into_iter().next() {
                    if as_json {
                        println!(
                            "{}",
                            serde_json::to_string(
                                &json!({"run":run_id,"gate":gate.id,"status":"awaiting_approval"})
                            )?
                        );
                    } else {
                        println!(
                            "delivery run {run_id} awaits human approval at gate {}",
                            gate.id
                        );
                    }
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            bail!("delivery run {run_id} is still working; resume it later")
        }
    }
}

/// Register on every runtime that can recover a delivery run, including serve.
pub(crate) fn register_delivery<R: DurableRuntime>(
    rt: &mut R,
    state: atomic_core::state::ProjectState,
) {
    rt.register(
        delivery_effect::WORKFLOW,
        move |ctx: R::Context, run: DeliveryRun| {
            let state = state.clone();
            async move {
                let remote = DeliveryRemote::open(&state).map_err(|error| {
                    failed(format!("protected delivery configuration: {error}"))
                })?;
                execute(ctx, run, remote).await
            }
        },
    );
}

async fn execute<C: WorkflowContext>(
    ctx: C,
    run: DeliveryRun,
    remote: DeliveryRemote,
) -> Result<Value> {
    let DeliveryRun {
        plan,
        operator,
        source,
    } = run;
    if operator.trim().is_empty() {
        return Err(failed("delivery has no enrolled operator name"));
    }
    if !source.project.as_deref().zip(ctx.project()).is_some_and(
        |(source_project, current_project)| same_project_state(source_project, current_project),
    ) || !ctx
        .project()
        .is_some_and(|current_project| same_project_state(&plan.project, current_project))
        || plan.source_run != source.source_run
        || plan.occurrence != format!("{}:{}", source.base_step, source.occurrence)
        || plan.push["head"].as_str() != Some(source.head.as_str())
    {
        return Err(failed(
            "delivery plan differs from its frozen reviewed source",
        ));
    }
    let first_source = source.clone();
    ctx.transaction("delivery.source", move |mut tx| {
        let first_source = first_source.clone();
        Box::pin(async move {
            delivery_evidence::revalidate_delivery_source_in(&mut tx, &first_source)
                .await
                .map_err(|error| failed(format!("delivery source: {error}")))
        })
    })
    .await?;
    let approvers = gate::resolve(
        delivery_effect::GATE_STEP,
        None,
        &Deployment::Local {
            operator: Principal::local(&operator),
        },
    )
    .map_err(|error| failed(format!("delivery gate approvers: {error}")))?;
    let gate = GateSpec {
        workflow: delivery_effect::WORKFLOW.into(),
        title: delivery_effect::GATE_TITLE.into(),
        show: plan.gate_show(),
        evidence: Vec::new(),
        approvers,
        tier: Tier::High,
        timeout: None,
        on_timeout: OnTimeout::Reject,
        question: None,
    };
    let decision = gate::gate(&ctx, delivery_effect::GATE_STEP, gate).await?;
    if !decision.approved() {
        return Err(failed("delivery was not approved by the enrolled operator"));
    }
    failpoint("delivery:after-gate");

    let mut pull_request = None::<Value>;
    let mut completed = Vec::new();
    for slot in [
        DeliverySlot::Push,
        DeliverySlot::PullRequest,
        DeliverySlot::Enqueue,
    ] {
        let name = slot_name(slot);
        let mut occurrence = 0_u64;
        let receipt = loop {
            let current_source = source.clone();
            let admission = delivery_effect::admit_checked(&ctx, &plan, slot, move |tx| {
                let current_source = current_source.clone();
                Box::pin(async move {
                    delivery_evidence::revalidate_delivery_source_in(tx, &current_source)
                        .await
                        .map_err(|error| failed(format!("delivery source: {error}")))
                })
            })
            .await?;
            let observed = match admission {
                Admission::Applied { receipt } => Some(receipt),
                Admission::Dispatch { key } => {
                    failpoint("delivery:after-intent");
                    match remote.apply(&plan, slot, &key, pull_request.as_ref()).await {
                        Ok(receipt) => Some(receipt),
                        Err(_) => remote
                            .inspect(&plan, slot, &key, pull_request.as_ref())
                            .await
                            .ok()
                            .flatten(),
                    }
                }
                Admission::Reconcile { key } => {
                    failpoint("delivery:after-intent");
                    remote
                        .inspect(&plan, slot, &key, pull_request.as_ref())
                        .await
                        .ok()
                        .flatten()
                }
            };
            failpoint("delivery:after-dispatch");
            let decision = if let Some(receipt) = observed {
                delivery_effect::record_applied(&ctx, &plan, slot, &receipt).await?;
                failpoint("delivery:after-record");
                DeliveryDecision::Applied { receipt }
            } else {
                DeliveryDecision::Unknown
            };
            // A checkpointed Unknown remains Unknown after remote state
            // changes offline. Replay must revisit its original human park
            // before moving to a later occurrence.
            let decision: DeliveryDecision = ctx
                .step(
                    &format!("delivery.{name}.decision.{occurrence}"),
                    move |_| async move { Ok(decision) },
                )
                .await?;
            failpoint("delivery:after-decision");
            match decision {
                DeliveryDecision::Applied { receipt } => {
                    let authoritative = delivery_effect::applied_receipt(&ctx, &plan, slot).await?;
                    if authoritative != receipt {
                        return Err(failed(format!(
                            "delivery {name} checkpoint differs from its applied ledger receipt"
                        )));
                    }
                    break receipt;
                }
                DeliveryDecision::Unknown => {
                    park_delivery(
                        &ctx,
                        &format!("delivery.{name}.park.{occurrence}"),
                        name,
                        occurrence,
                    )
                    .await?;
                    failpoint("delivery:after-park-release");
                }
            }
            occurrence = occurrence
                .checked_add(1)
                .ok_or_else(|| failed("delivery park occurrence exceeds stored range"))?;
        };
        let authoritative = receipt.clone();
        let projected: Value = ctx
            .step(&format!("delivery.{name}.done"), move |_| async move {
                Ok(receipt)
            })
            .await?;
        if projected != authoritative {
            return Err(failed(format!(
                "delivery {name} projection differs from its authoritative receipt"
            )));
        }
        if slot == DeliverySlot::PullRequest {
            pull_request = Some(authoritative);
        }
        completed.push(json!({"slot":name,"receipt":projected}));
    }
    Ok(json!({"source_run":plan.source_run,"occurrence":plan.occurrence,"effects":completed}))
}
