// SPDX-License-Identifier: Apache-2.0

//! Releasing a parked run: a person's action, with a human credential.
//!
//! A run that reaches a budget cap parks (see [`crate::publication`]) and
//! stays parked: resuming never raises a cap or releases a park. A person
//! does both at once with [`release_park`]: optionally raise an allowance's
//! caps, mark the park released and journal the action, in one transaction,
//! checked against the credential they enrolled (see
//! [`crate::gate::credential`]); then wake the run, which runs the parked
//! turn again as a new step. The command is idempotent: sending it again
//! returns what it did the first time and wakes the run again, harmlessly.
//!
//! Nothing else raises a cap or releases a park: the unauthenticated ledger
//! and wake-up operations are private to this crate. An atom's commands
//! cannot reach the store or the credential (see the sandbox's guard).

use crate::gate::Principal;
use crate::gate::credential::{self, HumanCredential, Verified};
use crate::publication::Park;
use crate::runtime::{DurableError, DurableRuntime, Param, Tx};
use crate::usage::{Allowance, Caps};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

/// Raise one allowance's caps, each only where it is larger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Raise {
    /// The allowance, such as `run:<run id>` or `atom:<run id>:<position>`.
    pub scope: String,
    /// The caps it should have at least.
    pub caps: Caps,
}

/// A person's release of a park, sent as an idempotent command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseCommand {
    /// A command id the client made, unique per release it means to send.
    pub command_id: String,
    /// The park, as [`crate::parks`] lists it.
    pub park: String,
    /// A cap to raise first, if any. A release with no more room than
    /// before parks the run again.
    pub raise: Option<Raise>,
    /// What the person said, if anything.
    pub comment: Option<String>,
}

/// A recorded release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Released {
    /// The park.
    pub park: Park,
    /// Who released it.
    pub by: Principal,
    /// When, in milliseconds since the Unix epoch.
    pub at: i64,
    /// The allowance as the raise left it, if there was one.
    pub raised: Option<Allowance>,
    /// The command that recorded it.
    pub command: String,
    /// What the person said.
    pub comment: Option<String>,
}

/// Why a release was not recorded.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// The principal has no credential enrolled.
    #[error("{0} has no operator credential enrolled")]
    NotEnrolled(Principal),
    /// The credential is not the one the principal enrolled.
    #[error("the credential presented for {0} is not the one it enrolled")]
    BadCredential(Principal),
    /// There is no such park.
    #[error("no park {0}")]
    NoSuchPark(String),
    /// There is no such allowance to raise.
    #[error("no allowance {0} to raise")]
    NoSuchAllowance(String),
    /// The park was already released, by another command.
    #[error("park {park} was already released by {by}")]
    AlreadyReleased {
        /// The park.
        park: String,
        /// Who released it.
        by: Principal,
    },
    /// The command id was already used for a different command.
    #[error("command {0} was already used for a different action")]
    CommandReused(String),
    /// The store failed; nothing was recorded.
    #[error(transparent)]
    Store(#[from] DurableError),
}

fn corrupt(what: impl std::fmt::Display) -> DurableError {
    DurableError::Engine(format!("the park store: {what}"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The digest that tells a replay of `cmd` by `by` from a reused id.
fn request_digest(by: &Principal, cmd: &ReleaseCommand) -> String {
    let request = json!({"by": by, "command": cmd});
    format!(
        "sha256:{:x}",
        Sha256::digest(request.to_string().as_bytes())
    )
}

async fn read_park(tx: &mut Tx<'_, '_>, id: &str) -> crate::runtime::Result<Option<Park>> {
    let row = tx
        .query_opt(
            "SELECT park_id, run, step, reason, parked_at FROM atomic_parks WHERE park_id = ?",
            &[Param::Text(id.to_owned())],
        )
        .await?;
    row.map(|r| {
        Ok(Park {
            id: r.text("park_id")?,
            run: r.text("run")?,
            step: r.text("step")?,
            reason: r.text("reason")?,
            parked_at: r.int("parked_at")?,
        })
    })
    .transpose()
}

/// Release a park as the person `credential` proves, raising a cap first if
/// the command asks, and wake its run.
///
/// In one transaction: the credential is checked against the one its
/// principal enrolled, the cap is raised, the park is marked released and
/// the action is journaled, with who did it and when. Only after the commit
/// is the run woken, keyed by the park, so a repeat wakes it once.
pub async fn release_park<R: DurableRuntime>(
    rt: &R,
    credential: &HumanCredential,
    cmd: ReleaseCommand,
) -> Result<Released, ReleaseError> {
    let by = credential.principal().clone();
    let request = request_digest(&by, &cmd);
    let credential = credential.clone();
    let recorded = rt
        .control_transaction(move |mut tx| {
            let (cmd, by, request, credential) =
                (cmd.clone(), by.clone(), request.clone(), credential.clone());
            Box::pin(async move {
                match credential::verify(&mut tx, &credential).await? {
                    Verified::Yes => {}
                    Verified::NotEnrolled => return Ok(Err(ReleaseError::NotEnrolled(by))),
                    Verified::Mismatch => return Ok(Err(ReleaseError::BadCredential(by))),
                }
                // A replay returns what the command did.
                if let Some(row) = tx
                    .query_opt(
                        "SELECT request, detail FROM atomic_admin_actions WHERE command_id = ?",
                        &[Param::Text(cmd.command_id.clone())],
                    )
                    .await?
                {
                    if row.text("request")? != request {
                        return Ok(Err(ReleaseError::CommandReused(cmd.command_id.clone())));
                    }
                    let released: Released =
                        serde_json::from_str(&row.text("detail")?).map_err(corrupt)?;
                    return Ok(Ok(released));
                }
                let Some(park) = read_park(&mut tx, &cmd.park).await? else {
                    return Ok(Err(ReleaseError::NoSuchPark(cmd.park.clone())));
                };
                if let Some(row) = tx
                    .query_opt(
                        "SELECT released_by FROM atomic_park_releases WHERE park_id = ?",
                        &[Param::Text(park.id.clone())],
                    )
                    .await?
                {
                    return Ok(Err(ReleaseError::AlreadyReleased {
                        park: park.id.clone(),
                        by: Principal::stored(row.text("released_by")?),
                    }));
                }
                let raised = match &cmd.raise {
                    None => None,
                    Some(raise) => {
                        match crate::usage::raise_in(&mut tx, &raise.scope, raise.caps).await? {
                            Some(a) => Some(a),
                            None => {
                                return Ok(Err(ReleaseError::NoSuchAllowance(raise.scope.clone())));
                            }
                        }
                    }
                };
                let now = now_ms();
                let released = Released {
                    park: park.clone(),
                    by: by.clone(),
                    at: now,
                    raised,
                    command: cmd.command_id.clone(),
                    comment: cmd.comment.clone(),
                };
                tx.execute(
                    "INSERT INTO atomic_park_releases (park_id, released_by, released_at, \
                     command_id) VALUES (?, ?, ?, ?)",
                    &[
                        Param::Text(park.id.clone()),
                        Param::Text(by.as_str().to_owned()),
                        Param::Int(now),
                        Param::Text(cmd.command_id.clone()),
                    ],
                )
                .await?;
                tx.execute(
                    "INSERT INTO atomic_admin_actions (command_id, action, target, request, \
                     principal, detail, recorded_at) VALUES (?, 'release', ?, ?, ?, ?, ?)",
                    &[
                        Param::Text(cmd.command_id.clone()),
                        Param::Text(park.id.clone()),
                        Param::Text(request.clone()),
                        Param::Text(by.as_str().to_owned()),
                        Param::Text(serde_json::to_string(&released).map_err(corrupt)?),
                        Param::Int(now),
                    ],
                )
                .await?;
                Ok(Ok(released))
            })
        })
        .await??;
    // Recorded; the wake-up is not part of it. A failed wake is delivered by
    // sending the same command again.
    crate::publication::release(rt, &recorded.park).await?;
    Ok(recorded)
}

/// Every release recorded, oldest first.
pub async fn releases<R: DurableRuntime>(rt: &R) -> crate::runtime::Result<Vec<Released>> {
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.query_all(
                "SELECT detail FROM atomic_admin_actions WHERE action = 'release' \
                 ORDER BY recorded_at, command_id",
                &[],
            )
            .await?
            .iter()
            .map(|r| serde_json::from_str(&r.text("detail")?).map_err(corrupt))
            .collect()
        })
    })
    .await
}

/// Every park not yet released, oldest first.
pub async fn waiting<R: DurableRuntime>(rt: &R) -> crate::runtime::Result<Vec<Park>> {
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.query_all(
                "SELECT park_id, run, step, reason, parked_at FROM atomic_parks p \
                 WHERE NOT EXISTS (SELECT 1 FROM atomic_park_releases r \
                 WHERE r.park_id = p.park_id) ORDER BY parked_at, park_id",
                &[],
            )
            .await?
            .iter()
            .map(|r| {
                Ok(Park {
                    id: r.text("park_id")?,
                    run: r.text("run")?,
                    step: r.text("step")?,
                    reason: r.text("reason")?,
                    parked_at: r.int("parked_at")?,
                })
            })
            .collect()
        })
    })
    .await
}

/// Whether park `id` is released.
pub async fn is_released<R: DurableRuntime>(rt: &R, id: &str) -> crate::runtime::Result<bool> {
    let id = id.to_owned();
    rt.control_transaction(move |mut tx| {
        let id = id.clone();
        Box::pin(async move {
            Ok(tx
                .query_opt(
                    "SELECT park_id FROM atomic_park_releases WHERE park_id = ?",
                    &[Param::Text(id)],
                )
                .await?
                .is_some())
        })
    })
    .await
}

#[cfg(test)]
mod tests;
