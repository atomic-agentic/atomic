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
use crate::publication::{FOR_PROJECT, PARKS, Park, park_from, project_param};
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
    /// The park was already released, by another command; its run's
    /// wake-up was sent again, in case it was lost.
    #[error(
        "park {} was already released by {by}; {}",
        park.id,
        match wake_failed {
            None => "its run was woken again, in case its wake-up was lost".to_owned(),
            Some(why) => format!(
                "waking its run again failed ({why}); atomic serve or atomic runs resume {} \
                 delivers it",
                park.run
            ),
        }
    )]
    AlreadyReleased {
        /// The park.
        park: Box<Park>,
        /// Who released it.
        by: Principal,
        /// Why waking its run again failed, if it did.
        wake_failed: Option<String>,
    },
    /// The park's run has finished: there is nothing to release.
    #[error("park {park}'s run {run} has finished; there is nothing to release")]
    RunFinished {
        /// The park.
        park: String,
        /// Its run.
        run: String,
    },
    /// The allowance to raise is not in the parked run's own chain.
    #[error(
        "{scope} is not an allowance of run {run}: only the run's own allowance, its atoms' \
         and those above it may be raised to release it"
    )]
    NotTheRunsAllowance {
        /// The allowance asked for.
        scope: String,
        /// The parked run.
        run: String,
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

async fn read_park(
    tx: &mut Tx<'_, '_>,
    id: &str,
    project: &Param,
) -> crate::runtime::Result<Option<Park>> {
    tx.query_opt(
        &format!("{PARKS} WHERE p.park_id = ? AND {FOR_PROJECT}"),
        &[Param::Text(id.to_owned()), project.clone()],
    )
    .await?
    .as_ref()
    .map(park_from)
    .transpose()
}

/// Park `id` of `rt`'s project, released or not.
pub async fn park<R: DurableRuntime>(rt: &R, id: &str) -> crate::runtime::Result<Option<Park>> {
    let (id, project) = (id.to_owned(), project_param(rt.project()));
    rt.control_transaction(move |mut tx| {
        let (id, project) = (id.clone(), project.clone());
        Box::pin(async move { read_park(&mut tx, &id, &project).await })
    })
    .await
}

/// Whether `scope` is in the allowance chain of run `run`: the run's own
/// allowance, one of its atoms', or an allowance above the run's, such as
/// its workflow's. Another run's allowance is not.
async fn in_chain(tx: &mut Tx<'_, '_>, run: &str, scope: &str) -> crate::runtime::Result<bool> {
    let run_scope = format!("run:{run}");
    let mut at = Some(scope.to_owned());
    // Up from `scope`: through the run's own allowance, or to it.
    let mut seen = 0;
    while let Some(s) = at {
        if s == run_scope {
            return Ok(true);
        }
        seen += 1;
        if seen > 64 {
            return Err(corrupt(format!(
                "the allowance chain above {scope} does not end"
            )));
        }
        at = tx
            .query_opt(
                "SELECT parent FROM atomic_usage_allowances WHERE scope = ?",
                &[Param::Text(s)],
            )
            .await?
            .map(|r| r.text_opt("parent"))
            .transpose()?
            .flatten();
    }
    // Above the run's: an ancestor of its allowance.
    let mut at = tx
        .query_opt(
            "SELECT parent FROM atomic_usage_allowances WHERE scope = ?",
            &[Param::Text(run_scope)],
        )
        .await?
        .map(|r| r.text_opt("parent"))
        .transpose()?
        .flatten();
    let mut seen = 0;
    while let Some(s) = at {
        if s == scope {
            return Ok(true);
        }
        seen += 1;
        if seen > 64 {
            return Err(corrupt(format!(
                "the allowance chain above run {run} does not end"
            )));
        }
        at = tx
            .query_opt(
                "SELECT parent FROM atomic_usage_allowances WHERE scope = ?",
                &[Param::Text(s)],
            )
            .await?
            .map(|r| r.text_opt("parent"))
            .transpose()?
            .flatten();
    }
    Ok(false)
}

/// Whether run `run` has finished, by the engine's record of it.
async fn finished(tx: &mut Tx<'_, '_>, run: &str) -> crate::runtime::Result<bool> {
    Ok(tx
        .query_opt(
            "SELECT status FROM workflow_status WHERE workflow_uuid = ?",
            &[Param::Text(run.to_owned())],
        )
        .await?
        .map(|r| r.text("status"))
        .transpose()?
        .is_some_and(|s| matches!(s.as_str(), "SUCCESS" | "ERROR" | "CANCELLED")))
}

/// A release as [`release_park`] returns it: what was recorded, and whether
/// the run was woken.
#[derive(Debug)]
pub struct Release {
    /// The release, as recorded.
    pub released: Released,
    /// Why waking the run failed, if it did: the release stands, and
    /// [`redeliver`] (which every resume and server start runs) wakes it.
    pub wake_failed: Option<String>,
    /// Whether the raise asked for left every cap as it was.
    pub raised_nothing: bool,
}

/// Release a park as the person `credential` proves, raising a cap first if
/// the command asks, and wake its run.
///
/// In one transaction: the credential is checked against the one its
/// principal enrolled, the cap is raised (only one in the parked run's own
/// allowance chain), the park is marked released and the action is
/// journaled, with who did it and when. Only after the commit is the run
/// woken, keyed by the park, so a repeat wakes it once. A park already
/// released is woken again, and the refusal says who released it.
pub async fn release_park<R: DurableRuntime>(
    rt: &R,
    credential: &HumanCredential,
    cmd: ReleaseCommand,
) -> Result<Release, ReleaseError> {
    let by = credential.principal().clone();
    let request = request_digest(&by, &cmd);
    let credential = credential.clone();
    // Only a park of the runtime's own project: on a store several projects
    // share, another's is no park of this one's.
    let project = project_param(rt.project());
    let recorded = rt
        .control_transaction(move |mut tx| {
            let (cmd, by, request, credential, project) = (
                cmd.clone(),
                by.clone(),
                request.clone(),
                credential.clone(),
                project.clone(),
            );
            Box::pin(async move {
                match credential::verify(&mut tx, &credential).await? {
                    Verified::Yes => {}
                    Verified::NotEnrolled => return Ok(Err(ReleaseError::NotEnrolled(by))),
                    Verified::Mismatch => return Ok(Err(ReleaseError::BadCredential(by))),
                }
                // A replay returns what the command did.
                if let Some(found) = journaled(&mut tx, &cmd.command_id, &request).await? {
                    return Ok(found.map(|r| (r, false)));
                }
                #[cfg(test)]
                tests::after_replay_check(&cmd.park).await;
                let Some(park) = read_park(&mut tx, &cmd.park, &project).await? else {
                    return Ok(Err(ReleaseError::NoSuchPark(cmd.park.clone())));
                };
                // A finished run has nothing to wake, released or not.
                if finished(&mut tx, &park.run).await? {
                    return Ok(Err(ReleaseError::RunFinished {
                        park: park.id.clone(),
                        run: park.run.clone(),
                    }));
                }
                if let Some(row) = tx
                    .query_opt(
                        "SELECT released_by FROM atomic_park_releases WHERE park_id = ?",
                        &[Param::Text(park.id.clone())],
                    )
                    .await?
                {
                    // Released by this same command, committed since the
                    // replay check above: at PostgreSQL's read committed
                    // isolation, which this transaction runs at, each
                    // statement reads what has committed by its start, so
                    // the release and its journal entry, committed
                    // together, are both seen now. A replay, then, not a
                    // release someone else made.
                    let replay = journaled(&mut tx, &cmd.command_id, &request).await?;
                    if let Some(found) = replay {
                        return Ok(found.map(|r| (r, false)));
                    }
                    return Ok(Err(ReleaseError::AlreadyReleased {
                        park: Box::new(park.clone()),
                        by: Principal::stored(row.text("released_by")?),
                        wake_failed: None,
                    }));
                }
                if let Some(raise) = &cmd.raise
                    && !in_chain(&mut tx, &park.run, &raise.scope).await?
                {
                    return Ok(Err(ReleaseError::NotTheRunsAllowance {
                        scope: raise.scope.clone(),
                        run: park.run.clone(),
                    }));
                }
                // A competing release may win the park or the command's key
                // after this one raised the cap: keep its writes reversible
                // until both are admitted.
                tx.execute("SAVEPOINT atomic_park_release", &[]).await?;
                #[cfg(test)]
                let pausing = park.id.clone();
                let (raised, unchanged) = match &cmd.raise {
                    None => (None, false),
                    Some(raise) => {
                        let before = crate::usage::read_allowance_in(&mut tx, &raise.scope).await?;
                        match crate::usage::raise_in(&mut tx, &raise.scope, raise.caps).await? {
                            Some(a) => {
                                let same = before.as_ref().is_some_and(|b| b.caps == a.caps);
                                (Some(a), same)
                            }
                            None => {
                                return Ok(Err(ReleaseError::NoSuchAllowance(raise.scope.clone())));
                            }
                        }
                    }
                };
                #[cfg(test)]
                tests::before_marking(&pausing).await;
                let now = now_ms();
                let released = Released {
                    park: park.clone(),
                    by: by.clone(),
                    at: now,
                    raised,
                    command: cmd.command_id.clone(),
                    comment: cmd.comment.clone(),
                };
                let marked = tx
                    .execute(
                        "INSERT INTO atomic_park_releases (park_id, released_by, released_at, \
                         command_id) VALUES (?, ?, ?, ?) ON CONFLICT (park_id) DO NOTHING",
                        &[
                            Param::Text(park.id.clone()),
                            Param::Text(by.as_str().to_owned()),
                            Param::Int(now),
                            Param::Text(cmd.command_id.clone()),
                        ],
                    )
                    .await?;
                let journaled_now = if marked == 1 {
                    tx.execute(
                        "INSERT INTO atomic_admin_actions (command_id, action, target, request, \
                         principal, detail, recorded_at) VALUES (?, 'release', ?, ?, ?, ?, ?) \
                         ON CONFLICT (command_id) DO NOTHING",
                        &[
                            Param::Text(cmd.command_id.clone()),
                            Param::Text(park.id.clone()),
                            Param::Text(request.clone()),
                            Param::Text(by.as_str().to_owned()),
                            Param::Text(serde_json::to_string(&released).map_err(corrupt)?),
                            Param::Int(now),
                        ],
                    )
                    .await?
                } else {
                    0
                };
                if marked == 1 && journaled_now == 1 {
                    return Ok(Ok((released, unchanged)));
                }
                // Another release won: undo this one's raise, then answer as
                // a replay of the winner's command, or as the release it is.
                tx.execute("ROLLBACK TO SAVEPOINT atomic_park_release", &[])
                    .await?;
                if let Some(found) = journaled(&mut tx, &cmd.command_id, &request).await? {
                    return Ok(found.map(|r| (r, false)));
                }
                let row = tx
                    .query_opt(
                        "SELECT released_by FROM atomic_park_releases WHERE park_id = ?",
                        &[Param::Text(park.id.clone())],
                    )
                    .await?
                    .ok_or_else(|| corrupt(format!("the release of park {} vanished", park.id)))?;
                Ok(Err(ReleaseError::AlreadyReleased {
                    park: Box::new(park.clone()),
                    by: Principal::stored(row.text("released_by")?),
                    wake_failed: None,
                }))
            })
        })
        .await?;
    match recorded {
        Ok((released, raised_nothing)) => {
            // Recorded; the wake-up is not part of it. If it fails, the
            // release stands and `redeliver` wakes the run.
            let wake_failed = wake(rt, &released.park).await.err().map(|e| e.to_string());
            Ok(Release {
                released,
                wake_failed,
                raised_nothing,
            })
        }
        Err(ReleaseError::AlreadyReleased { park, by, .. }) => {
            // Its wake may have been lost: send it again, harmlessly, and
            // say whether that worked.
            let wake_failed = wake(rt, &park).await.err().map(|e| e.to_string());
            Err(ReleaseError::AlreadyReleased {
                park,
                by,
                wake_failed,
            })
        }
        Err(e) => Err(e),
    }
}

/// The command `id`'s recorded result, if it was recorded: the release it
/// made, or a refusal when the id was used for another request.
async fn journaled(
    tx: &mut Tx<'_, '_>,
    id: &str,
    request: &str,
) -> crate::runtime::Result<Option<Result<Released, ReleaseError>>> {
    let Some(row) = tx
        .query_opt(
            "SELECT request, detail FROM atomic_admin_actions WHERE command_id = ?",
            &[Param::Text(id.to_owned())],
        )
        .await?
    else {
        return Ok(None);
    };
    if row.text("request")? != request {
        return Ok(Some(Err(ReleaseError::CommandReused(id.to_owned()))));
    }
    let released: Released = serde_json::from_str(&row.text("detail")?).map_err(corrupt)?;
    Ok(Some(Ok(released)))
}

async fn wake<R: DurableRuntime>(rt: &R, park: &Park) -> crate::runtime::Result<()> {
    #[cfg(test)]
    tests::woken(&park.id);
    crate::failpoint("release:before-wake");
    crate::publication::release(rt, park).await
}

/// Wake every released park whose run is still waiting: a release whose
/// wake-up was lost, as when the releaser stopped after recording it, is
/// delivered here. Run it when a runtime starts or resumes runs, beside
/// [`crate::gate::redeliver`]. Returns how many it woke; waking a run that
/// already woke changes nothing.
pub async fn redeliver<R: DurableRuntime>(rt: &R) -> crate::runtime::Result<usize> {
    crate::failpoint("release:redeliver");
    // Only the runtime's own project's: another's wakes its runs itself.
    let project = project_param(rt.project());
    let waiting: Vec<Park> = rt
        .control_transaction(move |mut tx| {
            let project = project.clone();
            Box::pin(async move {
                tx.query_all(
                    &format!(
                        "{PARKS} JOIN atomic_park_releases r ON r.park_id = p.park_id \
                         JOIN workflow_status w ON w.workflow_uuid = p.run \
                         WHERE w.status IN ('PENDING', 'ENQUEUED') AND {FOR_PROJECT} \
                         ORDER BY p.parked_at, p.park_id"
                    ),
                    &[project],
                )
                .await?
                .iter()
                .map(park_from)
                .collect()
            })
        })
        .await?;
    for park in &waiting {
        wake(rt, park).await?;
    }
    Ok(waiting.len())
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

/// Every park of `rt`'s project not yet released, oldest first.
pub async fn waiting<R: DurableRuntime>(rt: &R) -> crate::runtime::Result<Vec<Park>> {
    let project = project_param(rt.project());
    rt.control_transaction(move |mut tx| {
        let project = project.clone();
        Box::pin(async move {
            tx.query_all(
                &format!(
                    "{PARKS} WHERE {FOR_PROJECT} AND NOT EXISTS (SELECT 1 FROM \
                     atomic_park_releases r WHERE r.park_id = p.park_id) \
                     ORDER BY p.parked_at, p.park_id"
                ),
                &[project],
            )
            .await?
            .iter()
            .map(park_from)
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
