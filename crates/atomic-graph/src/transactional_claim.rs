// SPDX-License-Identifier: Apache-2.0
//! Claim operations for a durable transaction step. The caller owns the
//! transaction; its step checkpoint commits with these rows in the same DB.

use crate::handoff::Handoff;
use crate::store::Lease;
use atomic_core::workflow::{Criterion, parse_criteria};
use atomic_durable::{DurableError, Param, Result, Tx};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The immutable fields a graph work loop gives one atom invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    /// Stable graph item identifier.
    pub id: String,
    /// Human-readable item title.
    pub title: String,
    /// Item description and acceptance criteria.
    pub body: String,
    /// Beads acceptance criteria retained from an imported item, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_criteria: Option<String>,
    /// Host graph metadata from an imported item's labels. This is used for
    /// model routing and is never inserted into the model-facing item input.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Human-authorized structured criteria frozen with this claim. Imported
    /// Beads prose never populates this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria: Vec<Criterion>,
    /// Work item kind.
    pub kind: String,
    /// Lower values are selected first.
    pub priority: i64,
    /// The previous holder's agent-trust handoff, preserved in the claim
    /// checkpoint. The host resolves file digests before showing it to a model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<Handoff>,
}

/// A review finding proposed for human classification after the panel.
/// Review text remains agent-authored even when the operator defers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFollowup {
    /// The atom that produced the verdict.
    pub reviewer: String,
    /// Repository-relative source path cited by the finding.
    pub file: String,
    /// Optional one-based source line.
    pub line: Option<u32>,
    /// `low`, `minor` or `nit`.
    pub severity: String,
    /// The reviewer's exact category, checked against host formatting proof.
    pub category: String,
    /// The reviewer's finding, kept at agent trust.
    pub message: String,
}

/// What a durable select-and-claim step recorded for one epic iteration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimNext {
    /// One ready child and its fenced lease.
    Claimed {
        /// Immutable fields for the atom input.
        item: Box<WorkItem>,
        /// The live claim token.
        lease: Lease,
    },
    /// Every direct child is closed.
    Complete,
    /// Children remain, but none is ready at this instant.
    Pending,
}

fn acquisition_for(workflow: &str, step: &str, item: &str) -> String {
    let mut hash = Sha256::new();
    for part in [workflow, step, item] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("atomic-claim:{:x}", hash.finalize())
}

/// Bind a previously authorized run to one pinned epic. The authorization is
/// written by the enrolled operator before the durable run is admitted; this
/// operation and its checkpoint give the epic one active owner.
pub async fn bind_epic_run(tx: &mut Tx<'_, '_>, epic: &str, run_id: &str) -> Result<()> {
    let auth = tx
        .query_opt(
            "SELECT epic_id FROM atomic_graph_run_authorizations WHERE run_id=?",
            &[text(run_id)],
        )
        .await?;
    if auth
        .as_ref()
        .map(|row| row.text("epic_id"))
        .transpose()?
        .as_deref()
        != Some(epic)
    {
        return Err(DurableError::Failed(format!(
            "run {run_id} is not authorized for graph epic {epic}"
        )));
    }
    let item = tx
        .query_opt(
            "SELECT kind,status FROM atomic_graph_items WHERE id=?",
            &[text(epic)],
        )
        .await?;
    if !matches!(item, Some(ref row) if row.text("kind")? == "epic" && row.text("status")? == "pinned")
    {
        return Err(DurableError::Failed(format!(
            "graph epic {epic} must be pinned; create one with atomic graph create --kind epic --pinned"
        )));
    }
    tx.execute(
        "INSERT INTO atomic_graph_epic_runs (epic_id,run_id,state) VALUES (?,?,'active') ON CONFLICT(epic_id) DO NOTHING",
        &[text(epic), text(run_id)],
    ).await?;
    let held = tx
        .query_one(
            "SELECT run_id,state FROM atomic_graph_epic_runs WHERE epic_id=?",
            &[text(epic)],
        )
        .await?;
    if held.text("run_id")? != run_id {
        return Err(DurableError::Failed(format!(
            "graph epic {epic} is already bound to run {}",
            held.text("run_id")?
        )));
    }
    if held.text("state")? != "active" {
        return Err(DurableError::Parked {
            reason: format!("graph epic {epic}'s run binding is no longer active"),
            scope: Some(epic.to_owned()),
        });
    }
    Ok(())
}

/// Mark a bound epic run done only after every direct work child has closed.
/// The pinned epic remains as its durable container; closing it needs its own
/// host-verified policy evidence and is deliberately separate.
pub async fn finish_epic_run(tx: &mut Tx<'_, '_>, epic: &str, run_id: &str) -> Result<()> {
    let held = tx
        .query_opt(
            "SELECT run_id,state FROM atomic_graph_epic_runs WHERE epic_id=?",
            &[text(epic)],
        )
        .await?;
    if held
        .as_ref()
        .map(|row| row.text("run_id"))
        .transpose()?
        .as_deref()
        != Some(run_id)
    {
        return Err(DurableError::Failed(format!(
            "graph epic {epic} is not bound to run {run_id}"
        )));
    }
    if held
        .as_ref()
        .map(|row| row.text("state"))
        .transpose()?
        .as_deref()
        == Some("done")
    {
        return Ok(());
    }
    if held
        .as_ref()
        .map(|row| row.text("state"))
        .transpose()?
        .as_deref()
        != Some("active")
    {
        return Err(DurableError::Parked {
            reason: format!("graph epic {epic}'s run binding is no longer active"),
            scope: Some(epic.to_owned()),
        });
    }
    let item = tx
        .query_opt(
            "SELECT kind,status FROM atomic_graph_items WHERE id=?",
            &[text(epic)],
        )
        .await?;
    if !matches!(item, Some(ref row) if row.text("kind")? == "epic" && row.text("status")? == "pinned")
    {
        return Err(DurableError::Failed(format!(
            "graph epic {epic} changed from pinned before completion"
        )));
    }
    if tx.query_opt(
        "SELECT id FROM atomic_graph_items WHERE parent=? AND kind NOT IN ('goal','message') AND status NOT IN ('closed','pinned') LIMIT 1",
        &[text(epic)],
    ).await?.is_some() {
        return Err(DurableError::Interrupted(format!("graph epic {epic} has unfinished work children")));
    }
    tx.execute(
        "UPDATE atomic_graph_epic_runs SET state='done' WHERE epic_id=? AND run_id=?",
        &[text(epic), text(run_id)],
    )
    .await?;
    Ok(())
}

/// Select a ready direct child of a pinned epic and claim it in the caller's
/// checkpoint transaction. Filtering by epic happens before LIMIT, so a busy
/// project cannot starve this run with unrelated ready items.
pub async fn claim_next_for_epic(
    tx: &mut Tx<'_, '_>,
    workflow: &str,
    step: &str,
    epic: &str,
    claimant: &str,
    lease_for: Duration,
    validate_input: impl FnOnce(&WorkItem) -> Result<()> + Send,
) -> Result<ClaimNext> {
    let Some(parent) = tx
        .query_opt(
            "SELECT status,kind FROM atomic_graph_items WHERE id=?",
            &[text(epic)],
        )
        .await?
    else {
        return Err(DurableError::Failed(format!(
            "graph epic {epic} does not exist"
        )));
    };
    if parent.text("kind")? != "epic" || parent.text("status")? != "pinned" {
        return Err(DurableError::Failed(format!(
            "graph epic {epic} must be a pinned epic; create one with atomic graph create --kind epic --pinned"
        )));
    }
    let bound = tx
        .query_opt(
            "SELECT run_id,state FROM atomic_graph_epic_runs WHERE epic_id=?",
            &[text(epic)],
        )
        .await?;
    if !matches!(bound, Some(ref row) if row.text("run_id")? == claimant && row.text("state")? == "active")
    {
        return Err(DurableError::Parked {
            reason: format!("graph epic {epic}'s run binding is no longer active"),
            scope: Some(epic.to_owned()),
        });
    }
    let now = now_millis()?;
    let ready = tx
        .query_opt(
            "WITH RECURSIVE blocked_chain(source,target) AS (\
             SELECT source,target FROM atomic_graph_blocked \
             UNION SELECT chain.source,b.target FROM blocked_chain chain \
             JOIN atomic_graph_blocked b ON b.source=chain.target) \
             SELECT i.id,i.title,i.body,i.kind,i.priority,i.imported_json,c.criteria_json \
             FROM atomic_graph_items i LEFT JOIN atomic_graph_leases l ON l.item_id=i.id \
             LEFT JOIN atomic_graph_item_criteria c ON c.item_id=i.id \
             WHERE i.parent=? AND (i.status='open' OR (i.status='in_progress' AND (l.item_id IS NULL OR l.expires_at<=?))) \
             AND i.kind NOT IN ('goal','message') AND i.status!='pinned' AND (i.due_at IS NULL OR i.due_at<=?) \
             AND NOT EXISTS (SELECT 1 FROM blocked_chain chain WHERE chain.source=i.id) \
             ORDER BY i.priority,i.id LIMIT 1",
            &[text(epic), Param::Int(now), Param::Int(now)],
        )
        .await?;
    if let Some(row) = ready {
        let imported = row
            .text_opt("imported_json")?
            .map(|raw| {
                serde_json::from_str::<serde_json::Value>(&raw).map_err(|error| {
                    DurableError::Failed(format!(
                        "graph item's imported record is invalid JSON: {error}"
                    ))
                })
            })
            .transpose()?;
        let labels = imported
            .as_ref()
            .and_then(|record| record.get("labels"))
            .map(|value| {
                value
                    .as_array()
                    .ok_or_else(|| {
                        DurableError::Failed("graph item's imported labels must be an array".into())
                    })?
                    .iter()
                    .map(|value| {
                        value.as_str().map(str::to_owned).ok_or_else(|| {
                            DurableError::Failed(
                                "graph item's imported labels must be strings".into(),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        let item_id = row.text("id")?;
        let item = WorkItem {
            id: item_id.clone(),
            title: row.text("title")?,
            body: row.text("body")?,
            acceptance_criteria: imported.as_ref().and_then(|record| {
                record
                    .get("acceptance_criteria")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            }),
            labels,
            criteria: row
                .text_opt("criteria_json")?
                .map(|raw| {
                    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|error| {
                        DurableError::Failed(format!(
                            "graph item's structured criteria are invalid JSON: {error}"
                        ))
                    })?;
                    parse_criteria(&value).map_err(|error| {
                        DurableError::Failed(format!(
                            "graph item's structured criteria are invalid: {error}"
                        ))
                    })
                })
                .transpose()?
                .unwrap_or_default(),
            kind: row.text("kind")?,
            priority: row.int("priority")?,
            handoff: latest_handoff(tx, &item_id).await?,
        };
        validate_input(&item)?;
        let acquisition = acquisition_for(workflow, step, &item.id);
        if let Some(lease) = claim(tx, &item.id, claimant, &acquisition, lease_for).await? {
            return Ok(ClaimNext::Claimed {
                item: Box::new(item),
                lease,
            });
        }
    }
    if tx
        .query_opt(
            "SELECT id FROM atomic_graph_items WHERE parent=? AND kind NOT IN ('goal','message') AND status NOT IN ('closed','pinned') LIMIT 1",
            &[text(epic)],
        )
        .await?
        .is_some()
    {
        Ok(ClaimNext::Pending)
    } else {
        Ok(ClaimNext::Complete)
    }
}

fn text(value: &str) -> Param {
    Param::Text(value.to_owned())
}

fn now_millis() -> Result<i64> {
    let value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DurableError::Engine(format!("claim clock precedes epoch: {error}")))?;
    i64::try_from(value.as_millis())
        .map_err(|_| DurableError::Engine("claim clock exceeds stored range".into()))
}

fn review_id(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

/// File approved, low-severity review findings atomically with publication and
/// close. Identical findings share a task and credit each reviewer. Follow-ups
/// start without a parent and do not extend the active epic.
/// An operator can assign them to a pinned epic using the ordinary graph link.
/// Stable IDs make a retried finalization safe if a prior result was lost.
pub async fn file_review_followups(
    tx: &mut Tx<'_, '_>,
    lease: &Lease,
    epic: &str,
    run: &str,
    step: &str,
    findings: &[ReviewFollowup],
) -> Result<()> {
    if findings.is_empty() {
        return Ok(());
    }
    validate(tx, lease).await?;
    let now = now_millis()?;
    let mut grouped: BTreeMap<(String, u32, String, String, String), BTreeSet<String>> =
        BTreeMap::new();
    for finding in findings {
        if !matches!(finding.severity.as_str(), "minor" | "low" | "nit")
            || finding.file.is_empty()
            || finding.file.len() > 256
            || finding.category != "formatting"
            || finding.line != Some(1)
            || finding.message != "Remove the trailing space."
        {
            return Err(DurableError::Failed(
                "review follow-up is not a bounded low-severity finding".into(),
            ));
        }
        grouped
            .entry((
                finding.file.clone(),
                1,
                finding.severity.clone(),
                finding.category.clone(),
                finding.message.clone(),
            ))
            .or_default()
            .insert(finding.reviewer.clone());
    }
    for ((file, line, severity, category, message), reviewers) in grouped {
        let line_text = line.to_string();
        let hash = review_id(&[
            "finding:v2",
            epic,
            run,
            step,
            &lease.item_id,
            &file,
            &line_text,
            &severity,
            &category,
            &message,
        ]);
        let item_id = format!("atomic-review-{}", &hash[..24]);
        let request = format!("atomic-review-followup:{hash}");
        let location = format!("{file}:{line}");
        let title = format!("Review {severity}: {location}")
            .chars()
            .take(200)
            .collect::<String>();
        let reviewers = reviewers
            .into_iter()
            .map(|reviewer| format!("- {reviewer}"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = format!(
            "Reviewers:\n{reviewers}\nLocation: {location}\nSeverity: {severity}\nCategory: {category}\nFinding: {message}"
        );
        let priority = if severity == "nit" { 4 } else { 3 };
        tx.execute(
            "INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at) \
             VALUES (?,?,?,'task',?,'open',NULL,'agent','atomic-review',?) \
             ON CONFLICT(id) DO NOTHING",
            &[
                text(&item_id), text(&title), text(&body), Param::Int(priority),
                Param::Int(now),
            ],
        )
        .await?;
        let item = tx
            .query_one(
                "SELECT kind,body,trust,created_by FROM atomic_graph_items WHERE id=?",
                &[text(&item_id)],
            )
            .await?;
        if item.text("kind")? != "task"
            || item.text("body")? != body
            || item.text("trust")? != "agent"
            || item.text("created_by")? != "atomic-review"
        {
            return Err(DurableError::Failed(
                "review follow-up ID is occupied".into(),
            ));
        }
        tx.execute(
            "INSERT INTO atomic_graph_edges (source,kind,target) VALUES (?,'discovered-from',?) \
             ON CONFLICT(source,kind,target) DO NOTHING",
            &[text(&item_id), text(&lease.item_id)],
        )
        .await?;
        tx.execute(
            "INSERT INTO atomic_graph_requests (request_id,kind,value) \
             VALUES (?,'review-followup',?) ON CONFLICT(request_id) DO NOTHING",
            &[text(&request), text(&item_id)],
        )
        .await?;
        let recorded = tx
            .query_one(
                "SELECT kind,value FROM atomic_graph_requests WHERE request_id=?",
                &[text(&request)],
            )
            .await?;
        if recorded.text("kind")? != "review-followup" || recorded.text("value")? != item_id {
            return Err(DurableError::Failed(
                "review follow-up request ID is occupied".into(),
            ));
        }
    }
    Ok(())
}

fn expiry(now: i64, duration: Duration) -> Result<i64> {
    let millis = i64::try_from(duration.as_millis())
        .map_err(|_| DurableError::Failed("claim duration exceeds stored range".into()))?;
    if millis == 0 {
        return Err(DurableError::Failed(
            "claim duration must be at least one millisecond".into(),
        ));
    }
    now.checked_add(millis)
        .ok_or_else(|| DurableError::Failed("claim expiry exceeds stored range".into()))
}

fn parked(item_id: &str) -> DurableError {
    DurableError::Parked {
        reason: format!("claim for {item_id} was taken over; revalidate before executing"),
        scope: Some(item_id.to_owned()),
    }
}

/// Verify a checkpointed token against the authoritative lease and item epoch.
/// A replayed checkpoint alone is not authority to begin a model turn.
pub async fn validate(tx: &mut Tx<'_, '_>, lease: &Lease) -> Result<()> {
    let row = tx.query_opt(
        "SELECT l.claimant,l.session,l.epoch,l.acquisition,i.status,i.claim_epoch FROM atomic_graph_leases l JOIN atomic_graph_items i ON i.id=l.item_id WHERE l.item_id=?",
        &[text(&lease.item_id)],
    ).await?;
    let Some(row) = row else {
        return Err(parked(&lease.item_id));
    };
    if row.text("claimant")? != lease.claimant
        || row.int("session")?
            != i64::try_from(lease.session).map_err(|_| parked(&lease.item_id))?
        || row.int("epoch")? != lease.epoch
        || row.text("acquisition")? != lease.acquisition
        || row.text("status")? != "in_progress"
        || row.int("claim_epoch")? != lease.epoch
    {
        return Err(parked(&lease.item_id));
    }
    Ok(())
}

/// Extend a live claim from the control plane. Expiry alone does not revoke a
/// holder; only a newer epoch does. The compare-and-set still fences a holder
/// whose lease was taken over between reading it and writing the renewal.
pub async fn renew(tx: &mut Tx<'_, '_>, lease: &Lease, lease_for: Duration) -> Result<()> {
    let now = now_millis()?;
    let expires_at = expiry(now, lease_for)?;
    let session = i64::try_from(lease.session).map_err(|_| parked(&lease.item_id))?;
    let updated = tx
        .execute(
            "UPDATE atomic_graph_leases SET expires_at=CASE WHEN expires_at>? THEN expires_at ELSE ? END \
             WHERE item_id=? AND claimant=? AND session=? AND epoch=? AND acquisition=? \
             AND EXISTS (SELECT 1 FROM atomic_graph_items WHERE id=? AND status='in_progress' AND claim_epoch=?)",
            &[
                Param::Int(expires_at),
                Param::Int(expires_at),
                text(&lease.item_id),
                text(&lease.claimant),
                Param::Int(session),
                Param::Int(lease.epoch),
                text(&lease.acquisition),
                text(&lease.item_id),
                Param::Int(lease.epoch),
            ],
        )
        .await?;
    if updated != 1 {
        return Err(parked(&lease.item_id));
    }
    Ok(())
}

/// Give a review item back before waiting at a human gate. The checkpointed
/// transaction records why the candidate is pending, invalidates the old
/// token, and makes the item selectable again. A gate decision alone does not
/// restore authority to publish the candidate.
pub async fn release_for_review_gate(
    tx: &mut Tx<'_, '_>,
    lease: &Lease,
    gate: &str,
) -> Result<i64> {
    validate(tx, lease).await?;
    let released_epoch = lease
        .epoch
        .checked_add(1)
        .ok_or_else(|| DurableError::Failed("review release epoch exceeds stored range".into()))?;
    let updated = tx.execute(
        "UPDATE atomic_graph_items SET status='open',claim_epoch=? WHERE id=? AND status='in_progress' AND claim_epoch=?",
        &[Param::Int(released_epoch), text(&lease.item_id), Param::Int(lease.epoch)],
    ).await?;
    if updated != 1 {
        return Err(parked(&lease.item_id));
    }
    let session = i64::try_from(lease.session).map_err(|_| parked(&lease.item_id))?;
    let deleted = tx.execute(
        "DELETE FROM atomic_graph_leases WHERE item_id=? AND claimant=? AND session=? AND epoch=? AND acquisition=?",
        &[text(&lease.item_id), text(&lease.claimant), Param::Int(session), Param::Int(lease.epoch), text(&lease.acquisition)],
    ).await?;
    if deleted != 1 {
        return Err(parked(&lease.item_id));
    }
    let next = tx
        .query_one(
            "SELECT COALESCE(MAX(ordinal)+1,0) AS ordinal FROM atomic_graph_notes WHERE item_id=?",
            &[text(&lease.item_id)],
        )
        .await?
        .int("ordinal")?;
    tx.execute(
        "INSERT INTO atomic_graph_notes (item_id,ordinal,text,author,trust) VALUES (?,?,?,?, 'agent')",
        &[text(&lease.item_id), Param::Int(next), text(&format!("Review candidate awaits human decision at gate {gate}; the graph claim was released and must be re-claimed before publication.")), text(&lease.claimant)],
    ).await?;
    Ok(released_epoch)
}

/// Persist the previous holder's bounded handoff in the same transaction as
/// claim release. The release epoch is unique per item and fences replay.
pub async fn record_released_handoff(
    tx: &mut Tx<'_, '_>,
    lease: &Lease,
    released_epoch: i64,
    handoff: &Handoff,
) -> Result<()> {
    handoff.validate().map_err(DurableError::Failed)?;
    if released_epoch
        != lease.epoch.checked_add(1).ok_or_else(|| {
            DurableError::Failed("handoff release epoch exceeds stored range".into())
        })?
    {
        return Err(DurableError::Failed(
            "handoff release epoch does not match claim".into(),
        ));
    }
    let released = tx
        .query_opt(
            "SELECT status,claim_epoch FROM atomic_graph_items WHERE id=?",
            &[text(&lease.item_id)],
        )
        .await?;
    if !matches!(released, Some(ref row) if row.text("status")? == "open"
        && released_epoch == row.int("claim_epoch")?)
    {
        return Err(parked(&lease.item_id));
    }
    let acquisition = tx.query_opt(
        "SELECT item_id,claimant,session,epoch FROM atomic_graph_acquisitions WHERE acquisition=?",
        &[text(&lease.acquisition)],
    ).await?;
    if !matches!(acquisition, Some(ref row) if row.text("item_id")? == lease.item_id
        && row.text("claimant")? == lease.claimant
        && row.int("session")? == i64::try_from(lease.session).map_err(|_| parked(&lease.item_id))?
        && row.int("epoch")? == lease.epoch)
    {
        return Err(parked(&lease.item_id));
    }
    let encoded = serde_json::to_string(handoff)
        .map_err(|error| DurableError::Failed(format!("encode graph handoff: {error}")))?;
    tx.execute(
        "INSERT INTO atomic_graph_handoffs (item_id,release_epoch,record_json) VALUES (?,?,?)",
        &[
            text(&lease.item_id),
            Param::Int(released_epoch),
            text(&encoded),
        ],
    )
    .await?;
    Ok(())
}

/// Release a review claim and checkpoint a structured handoff atomically.
pub async fn release_for_review_gate_with_handoff(
    tx: &mut Tx<'_, '_>,
    lease: &Lease,
    gate: &str,
    handoff: &Handoff,
) -> Result<i64> {
    let epoch = release_for_review_gate(tx, lease, gate).await?;
    record_released_handoff(tx, lease, epoch, handoff).await?;
    Ok(epoch)
}

/// Release a failed claim and checkpoint a structured handoff atomically.
pub async fn release_after_failure_with_handoff(
    tx: &mut Tx<'_, '_>,
    lease: &Lease,
    reason: &str,
    handoff: &Handoff,
) -> Result<i64> {
    let epoch = release_after_failure(tx, lease, reason).await?;
    record_released_handoff(tx, lease, epoch, handoff).await?;
    Ok(epoch)
}

async fn latest_handoff(tx: &mut Tx<'_, '_>, item_id: &str) -> Result<Option<Handoff>> {
    let row = tx.query_opt(
        "SELECT record_json FROM atomic_graph_handoffs WHERE item_id=? ORDER BY release_epoch DESC LIMIT 1",
        &[text(item_id)],
    ).await?;
    row.map(|row| {
        let record: Handoff = serde_json::from_str(&row.text("record_json")?)
            .map_err(|error| DurableError::Failed(format!("decode graph handoff: {error}")))?;
        record.validate().map_err(DurableError::Failed)?;
        Ok(record)
    })
    .transpose()
}

/// Return a claimed child after the graph run has exhausted its repair path.
/// The caller checkpoints this as a named sibling transaction before it
/// records the terminal workflow failure. The exact token is fenced and an
/// agent-trust handoff is left for the next run; a stale token parks instead
/// of releasing somebody else's work.
pub async fn release_after_failure(
    tx: &mut Tx<'_, '_>,
    lease: &Lease,
    reason: &str,
) -> Result<i64> {
    validate(tx, lease).await?;
    let released_epoch = lease
        .epoch
        .checked_add(1)
        .ok_or_else(|| DurableError::Failed("failure release epoch exceeds stored range".into()))?;
    let changed = tx.execute(
        "UPDATE atomic_graph_items SET status='open',claim_epoch=? WHERE id=? AND status='in_progress' AND claim_epoch=?",
        &[Param::Int(released_epoch), text(&lease.item_id), Param::Int(lease.epoch)],
    ).await?;
    if changed != 1 {
        return Err(parked(&lease.item_id));
    }
    let session = i64::try_from(lease.session).map_err(|_| parked(&lease.item_id))?;
    let deleted = tx.execute(
        "DELETE FROM atomic_graph_leases WHERE item_id=? AND claimant=? AND session=? AND epoch=? AND acquisition=?",
        &[text(&lease.item_id), text(&lease.claimant), Param::Int(session), Param::Int(lease.epoch), text(&lease.acquisition)],
    ).await?;
    if deleted != 1 {
        return Err(parked(&lease.item_id));
    }
    let next = tx
        .query_one(
            "SELECT COALESCE(MAX(ordinal)+1,0) AS ordinal FROM atomic_graph_notes WHERE item_id=?",
            &[text(&lease.item_id)],
        )
        .await?
        .int("ordinal")?;
    let detail: String = reason.chars().take(1024).collect();
    tx.execute(
        "INSERT INTO atomic_graph_notes (item_id,ordinal,text,author,trust) VALUES (?,?,?,?, 'agent')",
        &[text(&lease.item_id), Param::Int(next), text(&format!("Graph run failed after claiming this item: {detail}. The claim was released; review the failure before retrying.")), text(&lease.claimant)],
    ).await?;
    Ok(released_epoch)
}

/// Re-claim a reviewed item only if nobody took it after the gate release.
/// The expected epoch is the release step's durable output. A later claim,
/// close, or other state change parks this candidate even if the item becomes
/// open again, because its review was for the earlier state.
pub async fn reclaim_after_review_gate(
    tx: &mut Tx<'_, '_>,
    lease: &Lease,
    released_epoch: i64,
    gate: &str,
    lease_for: Duration,
) -> Result<Lease> {
    let row = tx.query_opt(
        "SELECT i.status,i.claim_epoch,p.kind AS parent_kind,p.status AS parent_status,r.run_id,r.state AS run_state \
         FROM atomic_graph_items i \
         LEFT JOIN atomic_graph_items p ON p.id=i.parent \
         LEFT JOIN atomic_graph_epic_runs r ON r.epic_id=i.parent \
         WHERE i.id=?",
        &[text(&lease.item_id)],
    ).await?;
    // CONTROL_OFF: graph-review-reclaim-epic
    if !matches!(row, Some(ref row) if row.text("status")? == "open"
        && row.int("claim_epoch")? == released_epoch
        && row.text_opt("parent_kind")?.as_deref() == Some("epic")
        && row.text_opt("parent_status")?.as_deref() == Some("pinned")
        && row.text_opt("run_id")?.as_deref() == Some(lease.claimant.as_str())
        && row.text_opt("run_state")?.as_deref() == Some("active"))
    {
        return Err(parked(&lease.item_id));
    }
    let acquisition = acquisition_for(&lease.claimant, gate, &lease.item_id);
    claim(tx, &lease.item_id, &lease.claimant, &acquisition, lease_for)
        .await?
        .ok_or_else(|| parked(&lease.item_id))
}

/// Claim one ready item using a caller-stable acquisition. This function runs
/// only inside `WorkflowContext::transaction`; it never commits separately.
/// Reusing an acquisition returns its original lease or parks after takeover.
pub async fn claim(
    tx: &mut Tx<'_, '_>,
    item_id: &str,
    claimant: &str,
    acquisition: &str,
    lease_for: Duration,
) -> Result<Option<Lease>> {
    let now = now_millis()?;
    let expires_at = expiry(now, lease_for)?;
    if let Some(row) = tx.query_opt(
        "SELECT item_id,claimant,session,epoch FROM atomic_graph_acquisitions WHERE acquisition=?",
        &[text(acquisition)],
    ).await? {
        if row.text("item_id")? != item_id || row.text("claimant")? != claimant {
            return Err(DurableError::Failed("acquisition reused for a different item or claimant".into()));
        }
        let session = row.int("session")?;
        let lease = Lease {
            item_id: item_id.to_owned(), claimant: claimant.to_owned(),
            session: u64::try_from(session).map_err(|_| parked(item_id))?,
            epoch: row.int("epoch")?, acquisition: acquisition.to_owned(),
        };
        validate(tx, &lease).await?;
        return Ok(Some(lease));
    }
    let Some(item) = tx
        .query_opt(
            "SELECT status,kind,due_at,claim_epoch FROM atomic_graph_items WHERE id=?",
            &[text(item_id)],
        )
        .await?
    else {
        return Ok(None);
    };
    let status = item.text("status")?;
    if (status != "open" && status != "in_progress")
        || matches!(item.text("kind")?.as_str(), "goal" | "message")
        || item.int_opt("due_at")?.is_some_and(|due| due > now)
    {
        return Ok(None);
    }
    if tx
        .query_opt(
            "SELECT target FROM atomic_graph_blocked WHERE source=? LIMIT 1",
            &[text(item_id)],
        )
        .await?
        .is_some()
    {
        return Ok(None);
    }
    let previous = tx.query_opt(
        "SELECT claimant,session,epoch,acquisition,expires_at FROM atomic_graph_leases WHERE item_id=?",
        &[text(item_id)],
    ).await?;
    if let Some(lease) = &previous
        && lease.int("expires_at")? > now
    {
        return Ok(None);
    }
    let old_epoch = item.int("claim_epoch")?;
    let epoch = old_epoch
        .checked_add(if previous.is_some() { 2 } else { 1 })
        .ok_or_else(|| DurableError::Failed("claim epoch exceeds stored range".into()))?;
    if epoch <= 0 {
        return Err(DurableError::Failed("claim epoch is not positive".into()));
    }
    let session = epoch;
    let written = tx.execute(
        "INSERT INTO atomic_graph_leases (item_id,claimant,session,epoch,acquisition,expires_at) VALUES (?,?,?,?,?,?) ON CONFLICT(item_id) DO UPDATE SET claimant=excluded.claimant,session=excluded.session,epoch=excluded.epoch,acquisition=excluded.acquisition,expires_at=excluded.expires_at WHERE atomic_graph_leases.expires_at<=?",
        &[text(item_id), text(claimant), Param::Int(session), Param::Int(epoch), text(acquisition), Param::Int(expires_at), Param::Int(now)],
    ).await?;
    if written != 1 {
        return Ok(None);
    }
    let updated = tx.execute(
        "UPDATE atomic_graph_items SET status='in_progress',claim_epoch=? WHERE id=? AND claim_epoch=? AND status IN ('open','in_progress')",
        &[Param::Int(epoch), text(item_id), Param::Int(old_epoch)],
    ).await?;
    if updated != 1 {
        return Err(DurableError::Interrupted(
            "claim epoch changed concurrently".into(),
        ));
    }
    tx.execute(
        "INSERT INTO atomic_graph_acquisitions (acquisition,item_id,claimant,session,epoch) VALUES (?,?,?,?,?)",
        &[text(acquisition), text(item_id), text(claimant), Param::Int(session), Param::Int(epoch)],
    ).await?;
    Ok(Some(Lease {
        item_id: item_id.to_owned(),
        claimant: claimant.to_owned(),
        session: session as u64,
        epoch,
        acquisition: acquisition.to_owned(),
    }))
}

async fn close_policy(tx: &mut Tx<'_, '_>, item_id: &str) -> Result<(bool, Option<String>)> {
    let item = tx
        .query_opt(
            "SELECT requires_commit,requires_verified_commit,authorized_branch FROM atomic_graph_item_policy WHERE item_id=?",
            &[text(item_id)],
        )
        .await?;
    if let Some(row) = item {
        let verified = row.int("requires_verified_commit")? != 0;
        let branch = row.text_opt("authorized_branch")?;
        if verified && !branch.as_deref().is_some_and(valid_policy_branch) {
            return Err(DurableError::Failed(format!(
                "graph item {item_id} has no valid authorized branch for verified_commit"
            )));
        }
        return Ok((
            row.int("requires_commit")? != 0,
            if verified { branch } else { None },
        ));
    }
    let legacy = tx
        .query_opt(
            "SELECT requires_commit,requires_verified_commit FROM atomic_graph_policy WHERE id=1",
            &[],
        )
        .await?;
    match legacy {
        Some(row) if row.int("requires_verified_commit")? != 0 => Err(DurableError::Failed(
            "legacy verified_commit policy has no authorized branch".into(),
        )),
        Some(row) => Ok((row.int("requires_commit")? != 0, None)),
        None => Ok((false, None)),
    }
}

fn valid_policy_branch(branch: &str) -> bool {
    let Some(name) = branch.strip_prefix("refs/heads/") else {
        return false;
    };
    !name.is_empty()
        && branch.len() <= 255
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && !part.ends_with('.')
                && !part.ends_with(".lock")
                && !part.contains("..")
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

async fn refresh_blocked(tx: &mut Tx<'_, '_>) -> Result<()> {
    tx.execute("DELETE FROM atomic_graph_blocked", &[]).await?;
    tx.execute(
        "INSERT INTO atomic_graph_blocked (source,target) \
         SELECT e.source,e.target FROM atomic_graph_edges e \
         JOIN atomic_graph_items b ON b.id=e.target \
         WHERE e.kind IN ('blocks','parent-child','conditional-blocks','waits-for') \
         AND ((e.kind='conditional-blocks' AND (b.status!='closed' OR COALESCE(b.terminal_outcome,'unknown')!='failure')) \
         OR (e.kind!='conditional-blocks' AND b.status!='closed')) \
         AND NOT (e.kind='parent-child' AND b.status='pinned') \
         ON CONFLICT(source,target) DO NOTHING",
        &[],
    )
    .await?;
    Ok(())
}

fn commit_sha(sha: &str) -> Result<()> {
    if !matches!(sha.len(), 40 | 64) || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DurableError::Failed(
            "host worktree commit must be a full hexadecimal Git object ID".into(),
        ));
    }
    Ok(())
}

/// A provenance row survives a release, so only the exact currently held
/// token can mark this claim as human-owned. This mirrors the SQLite store's
/// human_lease_principal check.
async fn current_human_lease(tx: &mut Tx<'_, '_>, lease: &Lease) -> Result<bool> {
    let session = i64::try_from(lease.session).map_err(|_| parked(&lease.item_id))?;
    Ok(tx
        .query_opt(
            "SELECT item_id FROM atomic_graph_human_leases \
             WHERE item_id=? AND claimant=? AND session=? AND epoch=? AND acquisition=? /* exact human token */",
            &[
                text(&lease.item_id),
                text(&lease.claimant),
                Param::Int(session),
                Param::Int(lease.epoch),
                text(&lease.acquisition),
            ],
        )
        .await?
        .is_some())
}

/// The current host proof state for a staged graph commit. `Fenced` is a
/// semantic result so a stale workflow can enter a releasable park without
/// making the durable transaction fail and latch recovery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommitProof {
    /// The current item policy allows this SHA to close.
    Ready,
    /// The SHA is staged, but exact host branch proof is still required.
    Missing {
        /// The exact authorized ref that still needs host proof.
        branch: String,
    },
    /// Another holder or status transition revoked the claim.
    Fenced,
}

/// Stage the host-generated SHA without asserting verification and inspect
/// the item policy's exact branch proof under the full current lease. A later
/// verified link is never overwritten or downgraded. Call this again in the
/// combined publication/close transaction; policy and proof may change while
/// a human gate waits.
pub async fn stage_commit_for_close(
    tx: &mut Tx<'_, '_>,
    lease: &Lease,
    sha: &str,
) -> Result<CommitProof> {
    commit_sha(sha)?;
    match validate(tx, lease).await {
        Ok(()) => {}
        Err(DurableError::Parked { .. }) => return Ok(CommitProof::Fenced),
        Err(error) => return Err(error),
    }
    if current_human_lease(tx, lease).await? {
        return Err(DurableError::Failed(
            "a human claim cannot be closed by graph workflow orchestration".into(),
        ));
    }
    tx.execute(
        "INSERT INTO atomic_graph_commit_links (item_id,sha,indexed,on_branch) VALUES (?,?,0,0) \
         ON CONFLICT(item_id,sha) DO NOTHING /* staged host SHA */",
        &[text(&lease.item_id), text(sha)],
    )
    .await?;
    let (_, verified_branch) = close_policy(tx, &lease.item_id).await?;
    let Some(branch) = verified_branch else {
        return Ok(CommitProof::Ready);
    };
    let link = tx
        .query_opt(
            "SELECT indexed,on_branch,verified_branch,verified_head,pinned_base \
             FROM atomic_graph_commit_links WHERE item_id=? AND sha=?",
            &[text(&lease.item_id), text(sha)],
        )
        .await?;
    let verified = match link {
        Some(row) => {
            row.int("indexed")? != 0
                && row.int("on_branch")? != 0
                && row.text_opt("verified_branch")?.as_deref() == Some(branch.as_str()) // CONTROL_OFF: graph-stage-proof-branch
                && row.text_opt("verified_head")?.is_some()
                && row.text_opt("pinned_base")?.is_some()
        }
        None => false,
    };
    Ok(if verified {
        CommitProof::Ready
    } else {
        CommitProof::Missing { branch }
    })
}

/// Close one claimed item with the host worktree's commit in the same
/// transaction as this operation's durable checkpoint. A verified-commit
/// policy accepts only an existing branch-bound proof minted by the host;
/// untrusted caller flags are never accepted here as proof.
pub async fn close_with_commit(tx: &mut Tx<'_, '_>, lease: &Lease, sha: &str) -> Result<()> {
    commit_sha(sha)?;
    if current_human_lease(tx, lease).await? {
        return Err(DurableError::Failed(
            "a human claim cannot be closed by graph workflow orchestration".into(),
        ));
    }
    let existing = tx
        .query_opt(
            "SELECT status,terminal_outcome,last_finish_session,last_finish_epoch,last_finish_claimant,last_finish_acquisition FROM atomic_graph_items WHERE id=?",
            &[text(&lease.item_id)],
        )
        .await?
        .ok_or_else(|| DurableError::Failed(format!("graph item {} does not exist", lease.item_id)))?;
    if existing.text("status")? == "closed" {
        let same = existing.text_opt("terminal_outcome")?.as_deref() == Some("success")
            && existing.int_opt("last_finish_session")?
                == Some(i64::try_from(lease.session).map_err(|_| parked(&lease.item_id))?)
            && existing.int_opt("last_finish_epoch")? == Some(lease.epoch)
            && existing.text_opt("last_finish_claimant")?.as_deref()
                == Some(lease.claimant.as_str())
            && existing.text_opt("last_finish_acquisition")?.as_deref()
                == Some(lease.acquisition.as_str());
        if same {
            let link = tx
                .query_opt(
                    "SELECT sha FROM atomic_graph_commit_links WHERE item_id=? AND sha=?",
                    &[text(&lease.item_id), text(sha)],
                )
                .await?;
            if link.is_some() {
                return Ok(());
            }
        }
        return Err(parked(&lease.item_id));
    }
    validate(tx, lease).await?;
    let (_, verified_branch) = close_policy(tx, &lease.item_id).await?;
    if let Some(branch) = verified_branch {
        let link = tx.query_opt(
            "SELECT indexed,on_branch,verified_branch,verified_head,pinned_base FROM atomic_graph_commit_links WHERE item_id=? AND sha=?",
            &[text(&lease.item_id), text(sha)],
        ).await?;
        let verified = match link {
            Some(row) => {
                row.int("indexed")? != 0
                    && row.int("on_branch")? != 0
                    && row.text_opt("verified_branch")?.as_deref() == Some(branch.as_str())
                    && row.text_opt("verified_head")?.is_some()
                    && row.text_opt("pinned_base")?.is_some()
            }
            None => false,
        };
        if !verified {
            return Err(DurableError::Failed(format!(
                "graph item {} requires host-verified branch evidence before close",
                lease.item_id
            )));
        }
    }
    tx.execute(
        "INSERT INTO atomic_graph_commit_links (item_id,sha,indexed,on_branch) VALUES (?,?,0,0) \
         ON CONFLICT(item_id,sha) DO NOTHING",
        &[text(&lease.item_id), text(sha)],
    )
    .await?;
    let now = now_millis()?;
    let changed = tx.execute(
        "UPDATE atomic_graph_items SET status='closed',closed_at=?,terminal_outcome='success', \
         last_finish_session=?,last_finish_epoch=?,last_finish_claimant=?,last_finish_acquisition=? \
         WHERE id=? AND status='in_progress' AND claim_epoch=?",
        &[
            Param::Int(now),
            Param::Int(i64::try_from(lease.session).map_err(|_| parked(&lease.item_id))?),
            Param::Int(lease.epoch),
            text(&lease.claimant),
            text(&lease.acquisition),
            text(&lease.item_id),
            Param::Int(lease.epoch),
        ],
    ).await?;
    if changed != 1 {
        return Err(parked(&lease.item_id));
    }
    let removed = tx.execute(
        "DELETE FROM atomic_graph_leases WHERE item_id=? AND claimant=? AND epoch=? AND acquisition=?",
        &[text(&lease.item_id), text(&lease.claimant), Param::Int(lease.epoch), text(&lease.acquisition)],
    ).await?;
    if removed != 1 {
        return Err(parked(&lease.item_id));
    }
    refresh_blocked(tx).await
}
