// SPDX-License-Identifier: Apache-2.0

//! Intent-before-send ledger for a host's push, pull request and queue operations.
//! Only a newly inserted intent grants a dispatch. Subsequent callers must read
//! the remote and record a matching outcome, or leave the intent unresolved.
//! Gate authorization is read from the store on every call, including replay.

use crate::runtime::TxFuture;
use crate::{DurableError, Param, Result, Tx, WorkflowContext, gate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Name of the host delivery workflow recorded in the gate store.
pub const WORKFLOW: &str = "atomic.delivery";
/// Unique decision step in the delivery workflow.
pub const GATE_STEP: &str = "delivery.authorize";
/// Title of the delivery authorization presented to the operator.
pub const GATE_TITLE: &str = "Authorize branch push, pull request and merge queue";

/// Fully resolved immutable host requests. Each request includes its target
/// and desired revision. The host constructs this after validating source
/// review and check evidence; this ledger authorizes exactly the displayed data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryPlan {
    /// Project display and state identity frozen when the gate opened.
    pub project: String,
    /// Source run that completed review and checks.
    pub source_run: String,
    /// Stable source step occurrence (distinct from the delivery workflow ID).
    pub occurrence: String,
    /// Protected branch the candidate was trial-merged against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial_ref: Option<String>,
    /// Exact protected head observed by the final successful trial merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial_sha: Option<String>,
    /// Exact branch update including the target repository and ref.
    pub push: Value,
    /// Exact pull request create/update including head and base.
    pub pull_request: Value,
    /// Exact queue admission including expected head revision.
    pub enqueue: Value,
}

impl DeliveryPlan {
    /// Value to present to the human on the authorization gate.
    pub fn gate_show(&self) -> Value {
        json!(self)
    }

    fn check(&self) -> Result<()> {
        if [&self.project, &self.source_run, &self.occurrence]
            .iter()
            .any(|part| part.is_empty())
            || [&self.push, &self.pull_request, &self.enqueue]
                .iter()
                .any(|request| request.as_object().is_none_or(|object| object.is_empty()))
        {
            return Err(refuse(
                "delivery plan needs a project, run, occurrence and three requests",
            ));
        }
        if self.trial_ref.is_some() != self.trial_sha.is_some() {
            return Err(refuse(
                "delivery trial ref and SHA must be supplied together",
            ));
        }
        if let (Some(reference), Some(sha)) = (&self.trial_ref, &self.trial_sha)
            && (!reference.starts_with("refs/heads/")
                || reference.len() <= "refs/heads/".len()
                || !(sha.len() == 40 || sha.len() == 64)
                || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(refuse("delivery trial ref or SHA is invalid"));
        }
        Ok(())
    }

    fn project_id(&self) -> &str {
        stable_project_id(&self.project)
    }

    fn key(&self, slot: DeliverySlot) -> String {
        let identity = json!([
            "atomic.delivery.v2",
            self.project_id(),
            self.source_run,
            self.occurrence,
            slot
        ]);
        format!("{:x}", Sha256::digest(identity.to_string().as_bytes()))
    }

    fn legacy_key(&self, slot: DeliverySlot) -> String {
        let identity = json!([
            "atomic.delivery.v1",
            self.project,
            self.source_run,
            self.occurrence,
            slot
        ]);
        format!("{:x}", Sha256::digest(identity.to_string().as_bytes()))
    }
}

fn stable_project_id(project: &str) -> &str {
    project.rsplit_once('/').map_or(project, |(_, id)| id)
}

/// The three sequential operation slots of one delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliverySlot {
    /// Push the branch at its approved revision.
    Push,
    /// Create or update the pull request.
    PullRequest,
    /// Enqueue the exact pull request head.
    Enqueue,
}

impl DeliverySlot {
    fn previous(self) -> Option<Self> {
        match self {
            Self::Push => None,
            Self::PullRequest => Some(Self::Push),
            Self::Enqueue => Some(Self::PullRequest),
        }
    }
}

/// Result of a fresh control transaction. Never checkpoint a dispatch permit:
/// a replayed permit would authorize another send.
#[derive(Debug, PartialEq)]
pub enum Admission {
    /// First caller, after its intent has committed. It may dispatch once.
    Dispatch {
        /// Stable provider key for this operation.
        key: String,
    },
    /// A previous caller may have transmitted. Only read the remote.
    Reconcile {
        /// Original provider key.
        key: String,
    },
    /// Exact successful response or positively reconciled receipt already recorded.
    Applied {
        /// Immutable host receipt.
        receipt: Value,
    },
}

fn refuse(why: &str) -> DurableError {
    DurableError::Failed(why.to_owned())
}

fn text(value: &str) -> Param {
    Param::Text(value.to_owned())
}

struct Binding {
    gate_id: String,
    principal: String,
    command: String,
    plan: String,
}

struct EffectRow {
    key: String,
    plan: String,
    receipt: Option<String>,
}

async fn bind_alias(tx: &mut Tx<'_, '_>, stable_key: &str, provider_key: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO atomic_delivery_effect_aliases (stable_key,provider_key) VALUES (?,?) ON CONFLICT(stable_key) DO NOTHING",
        &[text(stable_key), text(provider_key)],
    )
    .await?;
    let row = tx
        .query_one(
            "SELECT provider_key FROM atomic_delivery_effect_aliases WHERE stable_key=?",
            &[text(stable_key)],
        )
        .await?;
    if row.text("provider_key")? != provider_key {
        return Err(refuse(
            "delivery has multiple provider intents for one stable slot",
        ));
    }
    Ok(())
}

/// A v19 store may hold provider keys already transmitted by an older binary.
/// Build their indexed aliases once, in the first ledger transaction after
/// migration. Every later lookup is by primary key, even in a large store.
async fn ensure_aliases(tx: &mut Tx<'_, '_>) -> Result<()> {
    if tx
        .query_opt(
            "SELECT complete FROM atomic_delivery_effect_alias_backfill WHERE id=1",
            &[],
        )
        .await?
        .is_some()
    {
        return Ok(());
    }
    for row in tx
        .query_all("SELECT provider_key,plan FROM atomic_delivery_effects", &[])
        .await?
    {
        let provider_key = row.text("provider_key")?;
        let stored: DeliveryPlan = serde_json::from_str(&row.text("plan")?)
            .map_err(|_| refuse("invalid stored delivery proposal during key migration"))?;
        let slots: Vec<DeliverySlot> = [
            DeliverySlot::Push,
            DeliverySlot::PullRequest,
            DeliverySlot::Enqueue,
        ]
        .into_iter()
        .filter(|slot| {
            provider_key == stored.key(*slot) || provider_key == stored.legacy_key(*slot)
        })
        .collect();
        let [slot] = slots.as_slice() else {
            return Err(refuse(
                "stored delivery provider key does not match its plan",
            ));
        };
        bind_alias(tx, &stored.key(*slot), &provider_key).await?;
    }
    tx.execute(
        "INSERT INTO atomic_delivery_effect_alias_backfill (id,complete) VALUES (1,1) ON CONFLICT(id) DO NOTHING",
        &[],
    )
    .await?;
    Ok(())
}

/// Resolve both current stable keys and legacy keys already sent to a remote.
/// A manifest rename may change the display project but never the state ID
/// or the exact source/request. Reusing the original provider key is required
/// for pending reconciliation; an applied old row must not dispatch again.
async fn effect_row(
    tx: &mut Tx<'_, '_>,
    plan: &DeliveryPlan,
    slot: DeliverySlot,
) -> Result<Option<EffectRow>> {
    ensure_aliases(tx).await?;
    let Some(alias) = tx
        .query_opt(
            "SELECT provider_key FROM atomic_delivery_effect_aliases WHERE stable_key=?",
            &[text(&plan.key(slot))],
        )
        .await?
    else {
        return Ok(None);
    };
    let key = alias.text("provider_key")?;
    let row = tx
        .query_one(
            "SELECT plan,receipt FROM atomic_delivery_effects WHERE provider_key=?",
            &[text(&key)],
        )
        .await?;
    let stored_text = row.text("plan")?;
    let stored: DeliveryPlan = serde_json::from_str(&stored_text)
        .map_err(|_| refuse("invalid stored delivery proposal"))?;
    if key != stored.key(slot) && key != stored.legacy_key(slot) {
        return Err(refuse(
            "stored delivery provider key does not match its plan",
        ));
    }
    if stored.project_id() != plan.project_id()
        || stored.source_run != plan.source_run
        || stored.occurrence != plan.occurrence
        || stored.push != plan.push
        || stored.pull_request != plan.pull_request
        || stored.enqueue != plan.enqueue
    {
        return Err(refuse(
            "existing delivery slot has a different bound request",
        ));
    }
    Ok(Some(EffectRow {
        key,
        plan: stored_text,
        receipt: row.text_opt("receipt")?,
    }))
}

async fn authorize(
    tx: &mut Tx<'_, '_>,
    plan: &DeliveryPlan,
    run: &str,
    project: Option<&str>,
) -> Result<Binding> {
    plan.check()?;
    if project.is_some_and(|id| stable_project_id(id) != plan.project_id()) {
        return Err(refuse("delivery plan belongs to another project"));
    }
    let gate_id = gate::gate_id(run, GATE_STEP);
    let row = tx.query_opt(
        "SELECT g.show,g.decided_by,g.command_id FROM atomic_gates g LEFT JOIN atomic_run_projects p ON p.run_id=g.run WHERE g.gate_id=? AND g.run=? AND g.workflow=? AND g.step=? AND g.title=? AND g.tier='high' AND g.state='approved' AND g.decided_by IS NOT NULL AND g.command_id IS NOT NULL AND COALESCE(p.project,'')=?",
        &[text(&gate_id), text(run), text(WORKFLOW), text(GATE_STEP), text(GATE_TITLE), text(project.unwrap_or_default())],
    ).await?.ok_or_else(|| refuse("delivery requires this run's human-approved operator gate"))?;
    let show: Value = serde_json::from_str(&row.text("show")?)
        .map_err(|_| refuse("invalid stored delivery proposal"))?;
    if show != plan.gate_show() {
        return Err(refuse(
            "delivery plan differs from the human-approved proposal",
        ));
    }
    Ok(Binding {
        gate_id,
        principal: row.text("decided_by")?,
        command: row.text("command_id")?,
        plan: show.to_string(),
    })
}

#[cfg(test)]
async fn admit<C: WorkflowContext>(
    ctx: &C,
    plan: &DeliveryPlan,
    slot: DeliverySlot,
) -> Result<Admission> {
    admit_checked(ctx, plan, slot, |_| Box::pin(async { Ok(()) })).await
}

/// Revalidate the source in the intent's own transaction before granting a
/// first dispatch. A rejected source rolls back the tentative intent. Pending
/// or applied operations can still be reconciled when the source has changed.
pub async fn admit_checked<C, F>(
    ctx: &C,
    plan: &DeliveryPlan,
    slot: DeliverySlot,
    check: F,
) -> Result<Admission>
where
    C: WorkflowContext,
    F: for<'a, 't, 'c> Fn(&'a mut Tx<'t, 'c>) -> TxFuture<'a, ()> + Send + Sync + 'static,
{
    let check = std::sync::Arc::new(check);
    let (run, project, plan) = (
        ctx.workflow_id().to_owned(),
        ctx.project().map(str::to_owned),
        plan.clone(),
    );
    ctx.control_transaction(move |mut tx| {
        let (run, project, plan, check) =
            (run.clone(), project.clone(), plan.clone(), check.clone());
        Box::pin(async move {
            let binding = authorize(&mut tx, &plan, &run, project.as_deref()).await?;
            let admission = admit_in(&mut tx, &plan, slot, &binding).await?;
            if matches!(admission, Admission::Dispatch { .. }) {
                check(&mut tx).await?;
            }
            Ok(admission)
        })
    })
    .await
}

async fn admit_in(
    tx: &mut Tx<'_, '_>,
    plan: &DeliveryPlan,
    slot: DeliverySlot,
    binding: &Binding,
) -> Result<Admission> {
    if let Some(previous) = slot.previous()
        && effect_row(tx, plan, previous)
            .await?
            .is_none_or(|effect| effect.receipt.is_none())
    {
        return Err(refuse("delivery predecessor has no matching receipt"));
    }
    if let Some(existing) = effect_row(tx, plan, slot).await? {
        return match existing.receipt {
            Some(value) => Ok(Admission::Applied {
                receipt: serde_json::from_str(&value)
                    .map_err(|_| refuse("invalid stored delivery receipt"))?,
            }),
            None => Ok(Admission::Reconcile { key: existing.key }),
        };
    }
    let key = plan.key(slot);
    let inserted = tx.execute(
        "INSERT INTO atomic_delivery_effects (provider_key,plan,gate_id,principal,command_id) VALUES (?,?,?,?,?) ON CONFLICT(provider_key) DO NOTHING",
        &[text(&key), text(&binding.plan), text(&binding.gate_id), text(&binding.principal), text(&binding.command)],
    ).await?;
    if inserted == 1 {
        bind_alias(tx, &key, &key).await?;
        return Ok(Admission::Dispatch { key });
    }
    bind_alias(tx, &key, &key).await?;
    match effect_row(tx, plan, slot).await? {
        Some(existing) if existing.key == key => match existing.receipt {
            Some(value) => Ok(Admission::Applied {
                receipt: serde_json::from_str(&value)
                    .map_err(|_| refuse("invalid stored delivery receipt"))?,
            }),
            None => Ok(Admission::Reconcile { key }),
        },
        _ => Err(refuse("delivery provider intent conflict")),
    }
}

/// Read an authoritative applied receipt without minting a dispatch permit.
/// A workflow checkpoint cannot stand in for a missing intent or receipt.
pub async fn applied_receipt<C: WorkflowContext>(
    ctx: &C,
    plan: &DeliveryPlan,
    slot: DeliverySlot,
) -> Result<Value> {
    let (run, project, plan) = (
        ctx.workflow_id().to_owned(),
        ctx.project().map(str::to_owned),
        plan.clone(),
    );
    ctx.control_transaction(move |mut tx| {
        let (run, project, plan) = (run.clone(), project.clone(), plan.clone());
        Box::pin(async move {
            authorize(&mut tx, &plan, &run, project.as_deref()).await?;
            let effect = effect_row(&mut tx, &plan, slot)
                .await?
                .ok_or_else(|| refuse("delivery has no matching applied receipt"))?;
            let receipt = effect
                .receipt
                .ok_or_else(|| refuse("delivery has no matching applied receipt"))?;
            serde_json::from_str(&receipt).map_err(|_| refuse("invalid stored delivery receipt"))
        })
    })
    .await
}

/// Record success only after the host receives a successful response or
/// positively reconciles the exact intent. A different later receipt is refused.
pub async fn record_applied<C: WorkflowContext>(
    ctx: &C,
    plan: &DeliveryPlan,
    slot: DeliverySlot,
    receipt: &Value,
) -> Result<()> {
    let (run, project, plan, receipt) = (
        ctx.workflow_id().to_owned(),
        ctx.project().map(str::to_owned),
        plan.clone(),
        receipt.clone(),
    );
    ctx.control_transaction(move |mut tx| {
        let (run, project, plan, receipt) = (run.clone(), project.clone(), plan.clone(), receipt.clone());
        Box::pin(async move {
            authorize(&mut tx, &plan, &run, project.as_deref()).await?;
            if receipt.as_object().is_none_or(|object| object.is_empty()) {
                return Err(refuse("delivery requires a nonempty receipt"));
            }
            let receipt = receipt.to_string();
            let effect = effect_row(&mut tx, &plan, slot)
                .await?
                .ok_or_else(|| refuse("delivery receipt has no matching intent"))?;
            let changed = tx.execute(
                "UPDATE atomic_delivery_effects SET receipt=? WHERE provider_key=? AND plan=? AND (receipt IS NULL OR receipt=?)",
                &[text(&receipt), text(&effect.key), text(&effect.plan), text(&receipt)],
            ).await?;
            if changed != 1 {
                return Err(refuse("delivery receipt has no matching intent or conflicts with its recorded outcome"));
            }
            Ok(())
        })
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::{
        DecisionCommand, Deployment, GateSpec, HumanCredential, OnTimeout, Principal, Tier, Verdict,
    };
    use crate::publication::{park_delivery, parks};
    use crate::release::{ReleaseCommand, release_park};
    use crate::{Database, DurableRuntime, Durare, DurareContext};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn plan() -> DeliveryPlan {
        DeliveryPlan {
            project: "local".into(),
            source_run: "source-run".into(),
            occurrence: "fix-1".into(),
            trial_ref: None,
            trial_sha: None,
            push: json!({"repo":"owner/repo","ref":"refs/heads/lane","sha":"one"}),
            pull_request: json!({"repo":"owner/repo","base":"main","head":"lane","sha":"one"}),
            enqueue: json!({"repo":"owner/repo","head":"lane","sha":"one"}),
        }
    }

    async fn exercise(ctx: DurareContext, approved: DeliveryPlan) -> Result<Vec<bool>> {
        let mut results = vec![admit(&ctx, &approved, DeliverySlot::Push).await.is_err()];
        let gate = GateSpec {
            workflow: WORKFLOW.into(),
            title: GATE_TITLE.into(),
            show: approved.gate_show(),
            evidence: vec![],
            approvers: gate::resolve(
                GATE_STEP,
                None,
                &Deployment::Local {
                    operator: Principal::local("owner"),
                },
            )
            .map_err(|err| refuse(&err.to_string()))?,
            tier: Tier::High,
            timeout: None,
            on_timeout: OnTimeout::Reject,
            question: None,
        };
        gate::gate(&ctx, GATE_STEP, gate).await?;
        results.push(
            applied_receipt(&ctx, &approved, DeliverySlot::Push)
                .await
                .is_err(),
        );
        results.push(
            admit_checked(&ctx, &approved, DeliverySlot::Push, |_| {
                Box::pin(async { Err(refuse("source was reopened")) })
            })
            .await
            .is_err(),
        );
        let mut different = approved.clone();
        different.enqueue["sha"] = json!("different");
        results.push(admit(&ctx, &different, DeliverySlot::Push).await.is_err());
        results.push(admit(&ctx, &approved, DeliverySlot::Enqueue).await.is_err());
        let receipt = json!({"observed":"one"});
        results.push(
            record_applied(&ctx, &approved, DeliverySlot::PullRequest, &receipt)
                .await
                .is_err(),
        );
        let (a, b) = tokio::join!(
            admit(&ctx, &approved, DeliverySlot::Push),
            admit(&ctx, &approved, DeliverySlot::Push)
        );
        let (a, b) = (a?, b?);
        let key = approved.key(DeliverySlot::Push);
        results.push(matches!((a,b),
            (Admission::Dispatch {key: first}, Admission::Reconcile {key: second})
            | (Admission::Reconcile {key: second}, Admission::Dispatch {key: first})
            if first == key && second == key));
        results.push(
            admit(&ctx, &approved, DeliverySlot::PullRequest)
                .await
                .is_err(),
        );
        record_applied(&ctx, &approved, DeliverySlot::Push, &receipt).await?;
        record_applied(&ctx, &approved, DeliverySlot::Push, &receipt).await?;
        results.push(
            record_applied(
                &ctx,
                &approved,
                DeliverySlot::Push,
                &json!({"observed":"other"}),
            )
            .await
            .is_err(),
        );
        results.push(
            admit(&ctx, &approved, DeliverySlot::Push).await?
                == Admission::Applied {
                    receipt: receipt.clone(),
                },
        );
        results.push(applied_receipt(&ctx, &approved, DeliverySlot::Push).await? == receipt);
        for slot in [DeliverySlot::PullRequest, DeliverySlot::Enqueue] {
            results.push(matches!(
                admit(&ctx, &approved, slot).await?,
                Admission::Dispatch { .. }
            ));
            record_applied(&ctx, &approved, slot, &receipt).await?;
            results.push(
                admit(&ctx, &approved, slot).await?
                    == Admission::Applied {
                        receipt: receipt.clone(),
                    },
            );
        }
        Ok(results)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delivery_effect_binds_human_approval_and_dispatches_each_ordered_slot_once() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("current time follows Unix epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("atomic-delivery-{}-{nonce}.db", std::process::id()));
        let mut rt = Durare::sqlite(&path)
            .await
            .expect("open delivery test store");
        rt.register(WORKFLOW, exercise);
        rt.launch().await.expect("launch delivery test runtime");
        let credential = HumanCredential::new(Principal::local("owner"), "operator secret");
        gate::enroll(&rt, &credential)
            .await
            .expect("enroll delivery test operator");
        rt.start(WORKFLOW, "delivery-run", plan())
            .await
            .expect("start delivery test run");
        let id = gate::gate_id("delivery-run", GATE_STEP);
        let opened = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(gate) = gate::find(&rt, &id).await.expect("find delivery gate") {
                    break gate;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .expect("delivery gate opens");
        gate::decide(
            &rt,
            &credential,
            DecisionCommand {
                command_id: "deliver-once".into(),
                gate: id,
                verdict: Verdict::Approve,
                comment: None,
                proposal: opened.proposal,
                answer: None,
            },
        )
        .await
        .expect("approve delivery gate");
        let observed: Vec<bool> = rt.result("delivery-run").await.expect("delivery completes");
        assert_eq!(observed, vec![true; 15]);
        rt.shutdown(Duration::from_secs(5))
            .await
            .expect("stop delivery test runtime");
        drop(rt);
        std::fs::remove_file(path).expect("remove delivery test store");
    }

    async fn rename_probe(ctx: DurareContext, input: Value) -> Result<Value> {
        let plan: DeliveryPlan = serde_json::from_value(input["plan"].clone())
            .map_err(|_| refuse("invalid rename test plan"))?;
        let seed = input["seed"].as_bool() == Some(true);
        let spec = GateSpec {
            workflow: WORKFLOW.into(),
            title: GATE_TITLE.into(),
            show: plan.gate_show(),
            evidence: vec![],
            approvers: gate::resolve(
                GATE_STEP,
                None,
                &Deployment::Local {
                    operator: Principal::local("owner"),
                },
            )
            .map_err(|error| refuse(&error.to_string()))?,
            tier: Tier::High,
            timeout: None,
            on_timeout: OnTimeout::Reject,
            question: None,
        };
        gate::gate(&ctx, GATE_STEP, spec).await?;
        if seed {
            let old_key = plan.legacy_key(DeliverySlot::Push);
            let old_plan = serde_json::to_string(&plan)
                .map_err(|_| refuse("encode legacy rename test plan"))?;
            ctx.control_transaction(move |mut tx| {
                let (old_key, old_plan) = (old_key.clone(), old_plan.clone());
                Box::pin(async move {
                    tx.execute(
                        "INSERT INTO atomic_delivery_effects (provider_key,plan,gate_id,principal,command_id) VALUES (?,?,?,?,?) ON CONFLICT(provider_key) DO NOTHING",
                        &[text(&old_key), text(&old_plan), text("legacy-gate"), text("owner"), text("legacy-command")],
                    ).await?;
                    Ok(())
                })
            }).await?;
            park_delivery(&ctx, "rename.pending", "push", 0).await?;
        }
        let admitted = admit_checked(&ctx, &plan, DeliverySlot::Push, |_| {
            Box::pin(async { Ok(()) })
        })
        .await?;
        match admitted {
            Admission::Reconcile { key } if seed => {
                let receipt = json!({"observed":"old-key"});
                record_applied(&ctx, &plan, DeliverySlot::Push, &receipt).await?;
                Ok(json!({"mode":"reconcile","key":key,"receipt":receipt}))
            }
            Admission::Applied { receipt } if !seed => {
                let key = effect_row_in_context(&ctx, &plan).await?;
                Ok(json!({"mode":"applied","key":key,"receipt":receipt}))
            }
            _ => Err(refuse(
                "manifest rename dispatched an already admitted effect",
            )),
        }
    }

    async fn effect_row_in_context(ctx: &DurareContext, plan: &DeliveryPlan) -> Result<String> {
        let plan = plan.clone();
        ctx.control_transaction(move |mut tx| {
            let plan = plan.clone();
            Box::pin(async move {
                effect_row(&mut tx, &plan, DeliverySlot::Push)
                    .await?
                    .map(|row| row.key)
                    .ok_or_else(|| refuse("legacy key vanished after manifest rename"))
            })
        })
        .await
    }

    async fn approve_rename_gate(
        rt: &Durare,
        credential: &HumanCredential,
        run: &str,
        command: &str,
    ) {
        let id = gate::gate_id(run, GATE_STEP);
        let opened = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(opened) = gate::find(rt, &id).await.expect("find delivery gate") {
                    break opened;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .expect("delivery gate opened");
        gate::decide(
            rt,
            credential,
            DecisionCommand {
                command_id: command.into(),
                gate: id,
                verdict: Verdict::Approve,
                comment: None,
                proposal: opened.proposal,
                answer: None,
            },
        )
        .await
        .expect("approve delivery gate");
    }

    #[test]
    fn rename_old_child() {
        let Ok(database) = std::env::var("ATOMIC_DELIVERY_RENAME_CHILD_DB") else {
            return;
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("child Tokio runtime");
        runtime.block_on(async {
            let path = std::path::PathBuf::from(&database);
            let mut original = Durare::open(
                &Database::Sqlite(path),
                None,
                "delivery-rename-test",
                "original/stable-state-id",
            )
            .await
            .expect("original project runtime");
            original.register(WORKFLOW, rename_probe);
            original.launch().await.expect("original launch");
            let credential = HumanCredential::new(Principal::local("owner"), "rename-test-secret");
            gate::enroll(&original, &credential)
                .await
                .expect("enroll original operator");
            let mut old_plan = plan();
            old_plan.project = "original/stable-state-id".into();
            original
                .start(
                    WORKFLOW,
                    "rename-pending",
                    json!({"plan":old_plan,"seed":true}),
                )
                .await
                .expect("start pending legacy operation");
            approve_rename_gate(
                &original,
                &credential,
                "rename-pending",
                "approve-old-delivery",
            )
            .await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if !parks(&original, "rename-pending")
                        .await
                        .expect("parks")
                        .is_empty()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
            })
            .await
            .expect("legacy run parked");
            std::fs::write(format!("{database}.ready"), b"parked").expect("signal parked child");
            std::future::pending::<()>().await;
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn legacy_pending_key_recovers_after_project_rename_and_success_is_reused() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("current time follows Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "atomic-delivery-rename-{}-{nonce}.db",
            std::process::id()
        ));
        crate::store::open_sqlite(&path).await.expect("v20 store");
        let database = Database::Sqlite(path.clone());
        let credential = HumanCredential::new(Principal::local("owner"), "rename-test-secret");
        let mut old_plan = plan();
        old_plan.project = "original/stable-state-id".into();
        let legacy_key = old_plan.legacy_key(DeliverySlot::Push);
        let ready = format!("{}.ready", path.display());
        let log =
            std::fs::File::create(path.with_extension("child.log")).expect("child process log");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "delivery_effect::tests::rename_old_child",
                "--nocapture",
            ])
            .env("ATOMIC_DELIVERY_RENAME_CHILD_DB", &path)
            .stdout(log.try_clone().expect("child stdout"))
            .stderr(log)
            .spawn()
            .expect("start old delivery process");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if std::path::Path::new(&ready).exists() {
                    break;
                }
                assert!(
                    child.try_wait().expect("child status").is_none(),
                    "old process exited before parking"
                );
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .expect("old process parked");
        child.kill().expect("SIGKILL old delivery process");
        child.wait().expect("reap old delivery process");

        let mut renamed = Durare::open(
            &database,
            None,
            "delivery-rename-test",
            "renamed/stable-state-id",
        )
        .await
        .expect("renamed project adopts pending run");
        renamed.register(WORKFLOW, rename_probe);
        renamed.launch().await.expect("renamed launch");
        renamed.recover().await.expect("recover old run");
        let park = parks(&renamed, "rename-pending")
            .await
            .expect("adopted park")
            .into_iter()
            .next()
            .expect("old run was parked");
        gate::enroll(&renamed, &credential)
            .await
            .expect("enroll operator");
        release_park(
            &renamed,
            &credential,
            ReleaseCommand {
                command_id: "release-renamed-pending".into(),
                park: park.id,
                raise: None,
                comment: Some("remote outcome reconciled".into()),
            },
        )
        .await
        .expect("release pending old key");
        let resumed: Value = renamed
            .result("rename-pending")
            .await
            .expect("old run resumes");
        assert_eq!(resumed["mode"], "reconcile");
        assert_eq!(resumed["key"], legacy_key);
        assert_eq!(resumed["receipt"], json!({"observed":"old-key"}));

        let mut new_plan = plan();
        new_plan.project = "renamed/stable-state-id".into();
        new_plan.trial_ref = Some("refs/heads/main".into());
        new_plan.trial_sha = Some("a".repeat(40));
        renamed
            .start(
                WORKFLOW,
                "rename-after-success",
                json!({"plan":new_plan,"seed":false}),
            )
            .await
            .expect("new delivery attempt");
        approve_rename_gate(
            &renamed,
            &credential,
            "rename-after-success",
            "approve-renamed-delivery",
        )
        .await;
        let reused: Value = renamed
            .result("rename-after-success")
            .await
            .expect("reuses old success");
        assert_eq!(reused["mode"], "applied");
        assert_eq!(
            reused["key"], legacy_key,
            "original provider key remains authoritative"
        );
        assert_eq!(reused["receipt"], json!({"observed":"old-key"}));
        renamed
            .shutdown(Duration::from_secs(5))
            .await
            .expect("shutdown renamed");
        let _ = std::fs::remove_file(path);
    }

    struct CountDrop(Arc<AtomicUsize>);

    impl Drop for CountDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_process_shutdown_fences_old_delivery_before_rename_recovery() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("current time follows Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "atomic-delivery-shutdown-{}-{nonce}.db",
            std::process::id()
        ));
        crate::store::open_sqlite(&path).await.expect("v20 store");
        let database = Database::Sqlite(path.clone());
        let credential = HumanCredential::new(Principal::local("owner"), "rename-test-secret");
        let mut old_plan = plan();
        old_plan.project = "original/stable-state-id".into();
        let legacy_key = old_plan.legacy_key(DeliverySlot::Push);
        let stopped = Arc::new(AtomicUsize::new(0));
        let mut original = Durare::open(
            &database,
            None,
            "delivery-rename-test",
            "original/stable-state-id",
        )
        .await
        .expect("original project runtime");
        let count = stopped.clone();
        original.register(WORKFLOW, move |ctx, input| {
            let count = count.clone();
            async move {
                let _on_drop = CountDrop(count);
                rename_probe(ctx, input).await
            }
        });
        original.launch().await.expect("original launch");
        gate::enroll(&original, &credential)
            .await
            .expect("enroll original operator");
        original
            .start(
                WORKFLOW,
                "shutdown-pending",
                json!({"plan":old_plan,"seed":true}),
            )
            .await
            .expect("start pending legacy operation");
        approve_rename_gate(
            &original,
            &credential,
            "shutdown-pending",
            "approve-shutdown-delivery",
        )
        .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !parks(&original, "shutdown-pending")
                    .await
                    .expect("original park")
                    .is_empty()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .expect("old execution parked");
        assert_eq!(stopped.load(Ordering::SeqCst), 0);
        original
            .shutdown(Duration::from_millis(20))
            .await
            .expect("shutdown fences parked old execution");
        assert_eq!(
            stopped.load(Ordering::SeqCst),
            1,
            "old workflow future must be gone before a new executor adopts it"
        );

        let mut unrelated = Durare::open(
            &database,
            Some("unrelated-executor"),
            "delivery-rename-test",
            "other/separate-state-id",
        )
        .await
        .expect("unrelated project runtime");
        unrelated.register(WORKFLOW, rename_probe);
        unrelated.launch().await.expect("unrelated launch");
        let mut other_plan = plan();
        other_plan.project = "other/separate-state-id".into();
        other_plan.source_run = "other-source".into();
        unrelated
            .start(
                WORKFLOW,
                "other-pending",
                json!({"plan":other_plan,"seed":true}),
            )
            .await
            .expect("start unrelated pending operation");
        approve_rename_gate(
            &unrelated,
            &credential,
            "other-pending",
            "approve-other-delivery",
        )
        .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !parks(&unrelated, "other-pending")
                    .await
                    .expect("unrelated park")
                    .is_empty()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .expect("unrelated execution parked");
        unrelated
            .shutdown(Duration::from_millis(20))
            .await
            .expect("shutdown unrelated runtime");

        let mut renamed = Durare::open(
            &database,
            None,
            "delivery-rename-test",
            "renamed/stable-state-id",
        )
        .await
        .expect("renamed project adopts old run");
        renamed.register(WORKFLOW, rename_probe);
        renamed.launch().await.expect("renamed launch");
        assert_eq!(renamed.recover().await.expect("recover own run"), 1);
        assert!(
            parks(&renamed, "other-pending")
                .await
                .expect("inspect unrelated park")
                .is_empty()
        );
        let park = parks(&renamed, "shutdown-pending")
            .await
            .expect("adopted park")
            .into_iter()
            .next()
            .expect("old run was parked");
        release_park(
            &renamed,
            &credential,
            ReleaseCommand {
                command_id: "release-shutdown-pending".into(),
                park: park.id,
                raise: None,
                comment: Some("remote outcome reconciled".into()),
            },
        )
        .await
        .expect("release approved pending delivery");
        let resumed: Value = renamed
            .result("shutdown-pending")
            .await
            .expect("new executor reconciles old run");
        assert_eq!(resumed["mode"], "reconcile");
        assert_eq!(resumed["key"], legacy_key);
        assert_eq!(resumed["receipt"], json!({"observed":"old-key"}));
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        assert_eq!(renamed.recover().await.expect("no duplicate recovery"), 0);
        assert_eq!(
            parks(&unrelated, "other-pending")
                .await
                .expect("unrelated project keeps its park")
                .len(),
            1,
            "renamed recovery must not consume another project's pending delivery"
        );
        renamed
            .shutdown(Duration::from_secs(5))
            .await
            .expect("shutdown renamed runtime");
        let _ = std::fs::remove_file(path);
    }
}
