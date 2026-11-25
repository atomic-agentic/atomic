// SPDX-License-Identifier: Apache-2.0
//! Durable work-graph claim before a turn. The claim and its checkpoint are
//! one transaction; a turn is a later sibling step. A bounded turn renews its
//! lease from the control plane, never from a model tool.
//! Publication and finish still fence every write with the claim token.

use atomic_durable::{DurableError, Result, WorkflowContext, failpoint};
use atomic_graph::{store::Lease, transactional_claim};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{future::Future, time::Duration};

/// Bound a claimed model turn and its control-plane lease renewal. A turn
/// deadline must precede its lease horizon, and the heartbeat must run before
/// the deadline. The lease is still fenced before publication and close.
#[derive(Clone, Copy, Debug)]
pub struct ClaimTurnLimits {
    /// Lifetime granted on each claim or renewal.
    pub lease_for: Duration,
    /// Maximum time spent in the model turn.
    pub turn_timeout: Duration,
    /// Interval between control-plane renewals while the turn runs.
    pub heartbeat_every: Duration,
}

/// Dropping a turn future must not leave its lease timer detached. The stop
/// channel also wakes the timer on normal completion; this guard covers an
/// abruptly canceled parent future while a renewal transaction is in flight.
struct AbortHeartbeat(tokio::task::JoinHandle<Result<()>>);

impl Drop for AbortHeartbeat {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ClaimTurnLimits {
    /// Reject a zero interval or a turn that could outlive its lease horizon.
    pub fn validate(self) -> Result<Self> {
        if self.heartbeat_every.is_zero()
            || self.turn_timeout.is_zero()
            || self.lease_for.is_zero()
            || self.heartbeat_every >= self.turn_timeout
            || self.turn_timeout >= self.lease_for
        {
            return Err(DurableError::Failed(
                "claim heartbeat interval must be positive and shorter than the turn timeout, which must be shorter than the lease".into(),
            ));
        }
        Ok(self)
    }

    fn from_lease(lease_for: Duration) -> Result<Self> {
        Self {
            lease_for,
            turn_timeout: lease_for / 2,
            heartbeat_every: lease_for / 4,
        }
        .validate()
    }
}

fn acquisition(workflow: &str, step: &str, item: &str) -> String {
    let mut hash = Sha256::new();
    for part in [workflow, step, item] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("atomic-claim:{:x}", hash.finalize())
}

/// Commit one item claim with its durable transaction-step checkpoint. The
/// acquisition is stable for a workflow, step, and item across recovery.
pub async fn claim_before_execution<C: WorkflowContext>(
    ctx: &C,
    step: &str,
    item_id: String,
    claimant: String,
    lease_for: Duration,
) -> Result<Option<Lease>> {
    let acquisition = acquisition(ctx.workflow_id(), step, &item_id);
    let claimed = ctx
        .transaction(step, move |mut tx| {
            let item_id = item_id.clone();
            let claimant = claimant.clone();
            let acquisition = acquisition.clone();
            Box::pin(async move {
                transactional_claim::claim(&mut tx, &item_id, &claimant, &acquisition, lease_for)
                    .await
            })
        })
        .await?;
    if let Some(lease) = &claimed {
        validate_or_park(ctx, step, lease).await?;
    }
    Ok(claimed)
}

/// Run the model turn only for the winning claim. The authoritative lease is
/// checked inside the turn body, so a replayed turn checkpoint does not start
/// a second model call and a takeover before an uncheckpointed turn parks it.
pub async fn claimed_turn<C, O, F, Fut>(
    ctx: &C,
    claim_step: &str,
    turn_step: &str,
    item_id: String,
    claimant: String,
    lease_for: Duration,
    turn: F,
) -> Result<Option<O>>
where
    C: WorkflowContext,
    O: Serialize + DeserializeOwned + Send + 'static,
    F: FnOnce(Lease) -> Fut + Send + 'static,
    Fut: Future<Output = Result<O>> + Send + 'static,
{
    claimed_turn_with_limits(
        ctx,
        claim_step,
        turn_step,
        item_id,
        claimant,
        ClaimTurnLimits::from_lease(lease_for)?,
        turn,
    )
    .await
}

/// Claim and run a bounded turn as sibling durable steps. The heartbeat uses
/// control transactions alongside the turn, without adding a nested step.
pub async fn claimed_turn_with_limits<C, O, F, Fut>(
    ctx: &C,
    claim_step: &str,
    turn_step: &str,
    item_id: String,
    claimant: String,
    limits: ClaimTurnLimits,
    turn: F,
) -> Result<Option<O>>
where
    C: WorkflowContext,
    O: Serialize + DeserializeOwned + Send + 'static,
    F: FnOnce(Lease) -> Fut + Send + 'static,
    Fut: Future<Output = Result<O>> + Send + 'static,
{
    let limits = limits.validate()?;
    let Some(lease) =
        claim_before_execution(ctx, claim_step, item_id, claimant, limits.lease_for).await?
    else {
        return Ok(None);
    };
    let control = ctx.clone();
    let output = ctx
        .step(turn_step, move |_| async move {
            bounded_claimed_turn(&control, lease, limits, turn).await
        })
        .await?;
    Ok(Some(output))
}

/// Execute the body of a claimed turn. Call this inside its durable turn step;
/// the caller owns the sibling claim and publication checkpoints.
pub async fn bounded_claimed_turn<C, O, F, Fut>(
    ctx: &C,
    lease: Lease,
    limits: ClaimTurnLimits,
    turn: F,
) -> Result<O>
where
    C: WorkflowContext,
    F: FnOnce(Lease) -> Fut + Send,
    Fut: Future<Output = Result<O>> + Send,
{
    let limits = limits.validate()?;
    let token = lease.clone();
    // Refresh the full lease horizon before the turn starts. A checkpointed
    // claim can have waited a long time before this sibling step is entered.
    match ctx
        .control_transaction(move |mut tx| {
            let token = token.clone();
            Box::pin(
                async move { transactional_claim::renew(&mut tx, &token, limits.lease_for).await },
            )
        })
        .await
    {
        Ok(()) => {}
        Err(DurableError::Parked { reason, .. }) => return Err(DurableError::Interrupted(reason)),
        Err(error) => return Err(error),
    }

    let (stop, stopped) = tokio::sync::oneshot::channel();
    let mut heartbeat = AbortHeartbeat(tokio::spawn(renew_until_stopped(
        ctx.clone(),
        lease.clone(),
        limits.lease_for,
        limits.heartbeat_every,
        stopped,
    )));
    let timed_turn = tokio::time::timeout(limits.turn_timeout, turn(lease));
    tokio::pin!(timed_turn);
    let result = tokio::select! {
        biased;
        outcome = &mut heartbeat.0 => {
            return match outcome {
                Ok(Err(DurableError::Parked { reason, .. })) => Err(DurableError::Interrupted(reason)),
                Ok(Err(error)) => Err(error),
                Ok(Ok(())) => Err(DurableError::Engine("claim heartbeat stopped before turn ended".into())),
                Err(error) => Err(DurableError::Engine(format!("claim heartbeat task failed: {error}"))),
            };
        }
        result = &mut timed_turn => result,
    };
    let _ = stop.send(());
    match (&mut heartbeat.0).await {
        Ok(Ok(())) => {}
        Ok(Err(DurableError::Parked { reason, .. })) => {
            return Err(DurableError::Interrupted(reason));
        }
        Ok(Err(error)) => return Err(error),
        Err(error) => {
            return Err(DurableError::Engine(format!(
                "claim heartbeat task failed: {error}"
            )));
        }
    }
    result.map_err(|_| DurableError::Interrupted("claimed turn exceeded its timeout".into()))?
}

async fn renew_until_stopped<C: WorkflowContext>(
    ctx: C,
    lease: Lease,
    lease_for: Duration,
    heartbeat_every: Duration,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => return Ok(()),
            _ = tokio::time::sleep(heartbeat_every) => {}
        }
        heartbeat_once(&ctx, &lease, lease_for).await?;
    }
}

async fn heartbeat_once<C: WorkflowContext>(
    ctx: &C,
    lease: &Lease,
    lease_for: Duration,
) -> Result<()> {
    let token = lease.clone();
    ctx.control_transaction(move |mut tx| {
        let token = token.clone();
        Box::pin(async move { transactional_claim::renew(&mut tx, &token, lease_for).await })
    })
    .await
}

/// Reconcile a checkpointed token with the current lease before an external
/// atom turn that manages its own durable steps.
pub async fn validate_before_turn<C: WorkflowContext>(ctx: &C, lease: &Lease) -> Result<()> {
    validate_or_park(ctx, "claim", lease).await
}

/// Resume after a human review gate by acquiring a fresh token in a durable
/// step. A competing claim changes the release epoch, so the reviewed
/// candidate stays private and the run parks for an operator.
pub async fn reclaim_review_gate<C: WorkflowContext>(
    ctx: &C,
    step: &str,
    old: &Lease,
    released_epoch: i64,
    gate: &str,
    lease_for: Duration,
) -> Result<Lease> {
    let mut occurrence = 0_u64;
    loop {
        let token = old.clone();
        let gate = gate.to_owned();
        let attempt = format!("{step}.{occurrence}");
        let reclaimed: Option<Lease> = ctx
            .transaction(&attempt, move |mut tx| {
                let old = token.clone();
                let gate = gate.clone();
                Box::pin(async move {
                    match transactional_claim::reclaim_after_review_gate(
                        &mut tx,
                        &old,
                        released_epoch,
                        &gate,
                        lease_for,
                    )
                    .await
                    {
                        Ok(lease) => Ok(Some(lease)),
                        Err(DurableError::Parked { .. }) => Ok(None),
                        Err(error) => Err(error),
                    }
                })
            })
            .await?;
        match reclaimed {
            Some(lease) => {
                failpoint("graph:after-review-reclaim");
                return Ok(lease);
            }
            None => {
                atomic_durable::publication::park_claim(
                    ctx,
                    &attempt,
                    &old.acquisition,
                    &old.item_id,
                    occurrence,
                )
                .await?;
                occurrence = occurrence.checked_add(1).ok_or_else(|| {
                    DurableError::Engine(
                        "review reclaim park occurrence exceeds stored range".into(),
                    )
                })?;
            }
        }
    }
}

async fn validate_or_park<C: WorkflowContext>(ctx: &C, step: &str, lease: &Lease) -> Result<()> {
    let mut occurrence = 0_u64;
    loop {
        let token = lease.clone();
        match ctx
            .control_transaction(move |mut tx| {
                let token = token.clone();
                Box::pin(async move { transactional_claim::validate(&mut tx, &token).await })
            })
            .await
        {
            Ok(()) => return Ok(()),
            Err(DurableError::Parked { .. }) => {
                atomic_durable::publication::park_claim(
                    ctx,
                    step,
                    &lease.acquisition,
                    &lease.item_id,
                    occurrence,
                )
                .await?;
                occurrence = occurrence.checked_add(1).ok_or_else(|| {
                    DurableError::Engine("claim park occurrence exceeds stored range".into())
                })?;
            }
            Err(error) => return Err(error),
        }
    }
}
