// SPDX-License-Identifier: Apache-2.0
//! Host reconstruction of a graph item's recorded coding and review evidence.
//! Delivery reads checkpoints and committed rows; it never asks a model to
//! attest its own review or to recreate evidence from a source run.

use crate::pipeline::{
    FOLLOWUP_REASON_PREFIX, GRAPH_PIPELINE, GraphAcceptance, GraphPipelineRun, LAYOUT_VERSION,
    Step, panel_acceptance_reason, route_review_findings,
};
use anyhow::{Context, Result, bail, ensure};
use atomic_durable::publication::{Acceptance, Candidate};
use atomic_durable::{DurableRuntime, Durare, Param, StepRecord, Tx};
use atomic_graph::transactional_claim::ClaimNext;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Immutable facts the delivery workflow pins before opening its human gate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverySource {
    /// Runtime project that owns the source run, if the store is project scoped.
    pub project: Option<String>,
    /// Durable workflow ID that created the accepted item.
    pub source_run: String,
    /// Pinned epic that owned the item when its run began.
    pub epic_id: String,
    /// Exact graph coding step prefix, including ordinal and template ID.
    pub base_step: String,
    /// Direct child closed by this source run.
    pub item_id: String,
    /// Host committed SHA approved for delivery.
    pub head: String,
    /// Accepted private candidate identity.
    pub candidate_id: String,
    /// Digest of the accepted candidate proposal.
    pub digest: String,
    /// Successful graph finalization attempt after any proof parks.
    pub occurrence: u64,
    /// Final independent review round.
    pub review_round: u64,
    /// Opaque session of the lease that closed this item.
    pub lease_session: u64,
    /// Epoch of the lease that closed this item.
    pub lease_epoch: i64,
    /// Acquisition identity of the lease that closed this item.
    pub lease_acquisition: String,
    /// Frozen protected branch ref used by the final trial merge.
    pub trial_main_ref: String,
    /// Protected branch head whose trial merge checks passed.
    pub trial_checked_sha: String,
}

fn step<'a>(steps: &'a [StepRecord], name: &str) -> Result<&'a StepRecord> {
    let mut found = steps.iter().filter(|step| step.name == name);
    let first = found
        .next()
        .with_context(|| format!("missing recorded step {name}"))?;
    ensure!(found.next().is_none(), "duplicate recorded step {name}");
    ensure!(first.error.is_none(), "recorded step {name} failed");
    ensure!(
        first.completed_at.is_some(),
        "recorded step {name} is unfinished"
    );
    Ok(first)
}

fn output<'a>(steps: &'a [StepRecord], name: &str) -> Result<&'a Value> {
    step(steps, name)?
        .output
        .as_ref()
        .with_context(|| format!("recorded step {name} has no output"))
}

fn turn(steps: &[StepRecord], name: &str, source_run: &str) -> Result<Candidate> {
    let value = output(steps, name)?;
    candidate_from_turn(value, name, source_run)
}

fn candidate_from_turn(value: &Value, name: &str, source_run: &str) -> Result<Candidate> {
    let candidate: Candidate = serde_json::from_value(
        value
            .get("Candidate")
            .context("turn did not record a Candidate")?
            .clone(),
    )
    .with_context(|| format!("invalid candidate at {name}"))?;
    ensure!(
        candidate.id.starts_with(&format!("{source_run}:")),
        "candidate belongs to another run"
    );
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&candidate.proposal)?)
    );
    ensure!(
        candidate.digest == digest,
        "candidate proposal digest differs from checkpoint"
    );
    Ok(candidate)
}

fn sha(value: &str) -> bool {
    atomic_core::git::is_sha(value)
}

fn same_state_scope(recorded: Option<&str>, frozen: Option<&str>) -> bool {
    fn state(scope: &str) -> Option<&str> {
        scope
            .rsplit_once('/')
            .map(|(_, id)| id)
            .filter(|id| !id.is_empty())
    }
    match (recorded, frozen) {
        (None, None) => true,
        (Some(current), Some(original)) => {
            state(current).is_some() && state(current) == state(original)
        }
        _ => false,
    }
}

fn ordered(before: &StepRecord, after: &StepRecord) -> Result<()> {
    ensure!(
        before.step_id < after.step_id,
        "review evidence is out of order"
    );
    Ok(())
}

fn round_for(
    base: &str,
    steps: &[StepRecord],
    accepted: &Acceptance,
    source_run: &str,
) -> Result<(u64, Candidate, String)> {
    let mut candidates = Vec::new();
    for recorded in steps {
        let round = if recorded.name == format!("{base}.turn") {
            Some(0)
        } else {
            recorded
                .name
                .strip_prefix(&format!("{base}.repair."))
                .and_then(|tail| tail.strip_suffix(".turn"))
                .and_then(|number| number.parse::<u64>().ok())
                .filter(|number| *number > 0)
        };
        if let Some(round) = round {
            let candidate = turn(steps, &recorded.name, source_run)?;
            candidates.push((round, candidate, recorded.name.clone()));
        }
    }
    candidates.sort_by_key(|(round, _, _)| *round);
    ensure!(!candidates.is_empty(), "source has no coding candidate");
    ensure!(candidates[0].0 == 0, "source has no first coding turn");
    for pair in candidates.windows(2) {
        ensure!(
            pair[1].0 == pair[0].0 + 1,
            "source has missing or duplicate repair round"
        );
    }
    let (round, candidate, name) = candidates.pop().expect("nonempty");
    ensure!(
        candidate.id == accepted.candidate && candidate.digest == accepted.digest,
        "accepted candidate does not match final coding turn"
    );
    Ok((round, candidate, name))
}

async fn published(tx: &mut Tx<'_, '_>, candidate: &Candidate, output: &Value) -> Result<()> {
    let row = tx
        .query_opt(
            "SELECT digest,output FROM atomic_publications WHERE publication_key=?",
            &[Param::Text(candidate.id.clone())],
        )
        .await?
        .context("candidate has no committed publication")?;
    ensure!(
        row.text("digest")? == candidate.digest,
        "published candidate digest changed"
    );
    let recorded: Value = serde_json::from_str(&row.text("output")?)?;
    ensure!(
        recorded == *output && candidate.proposal.output == *output,
        "published output differs from its candidate"
    );
    Ok(())
}

/// Reconstruct a reviewed, published graph item without running the source.
pub async fn approved_graph_item(
    rt: &Durare,
    source_run: &str,
    base_step: &str,
) -> Result<DeliverySource> {
    let record = rt
        .own_run_record(source_run)
        .await?
        .context("source run is absent from this project")?;
    ensure!(
        record.version == LAYOUT_VERSION,
        "source run predates current graph evidence layout"
    );
    ensure!(
        record.workflow == GRAPH_PIPELINE,
        "source is not a graph workflow"
    );
    let frozen: GraphPipelineRun = serde_json::from_value(record.input)
        .context("source graph input is not a frozen graph pipeline")?;
    ensure!(
        frozen.run.pipeline.steps.len() == 1,
        "source graph template changed shape"
    );
    let Step::Atom(template) = &frozen.run.pipeline.steps[0] else {
        bail!("source graph template is not an atom");
    };
    ensure!(
        template.worktree && template.coding,
        "source is not a coding graph item"
    );
    let prefix = base_step
        .strip_prefix("graph.")
        .context("invalid graph step name")?;
    let (number, step_id) = prefix.split_once('.').context("invalid graph step name")?;
    ensure!(
        number.len() == 5 && number.bytes().all(|b| b.is_ascii_digit()) && step_id == template.id,
        "graph step is not the frozen coding template"
    );
    let review = template
        .review
        .as_ref()
        .context("coding item has no review policy")?;
    ensure!(
        review.reviewers.len() == 2 && review.reviewers[0] != review.reviewers[1],
        "delivery requires two fixed independent reviewers"
    );
    let self_review = review
        .self_review
        .as_ref()
        .context("source has no self-review policy")?;
    ensure!(
        review.trial_merge.is_some(),
        "source has no trial merge policy"
    );
    ensure!(
        self_review.controls_atom.is_some() || self_review.selected_controls.is_empty(),
        "source selected controls have no proof atom"
    );
    let steps = rt.steps(source_run).await?;
    step(&steps, "epic.bind")?;
    let claim_name = format!("graph.{number}.claim");
    let claimed: ClaimNext = serde_json::from_value(output(&steps, &claim_name)?.clone())?;
    let ClaimNext::Claimed { item, lease } = claimed else {
        bail!("source graph step did not claim an item");
    };
    ensure!(
        lease.claimant == source_run && lease.item_id == item.id,
        "source claim has wrong run or item"
    );
    let accepted_name = format!("{base_step}.accepted");
    let receipt: GraphAcceptance = serde_json::from_value(output(&steps, &accepted_name)?.clone())?;
    let accepted = &receipt.acceptance;
    ensure!(accepted.accepted, "source candidate was not accepted");
    ensure!(
        !steps
            .iter()
            .any(|s| s.name == format!("{base_step}.review.gate")
                || s.name == format!("{base_step}.review")),
        "human-overridden review cannot authorize delivery"
    );
    let (round, candidate, turn_name) = round_for(base_step, &steps, accepted, source_run)?;
    ensure!(
        round <= review.rounds,
        "accepted repair exceeds frozen policy"
    );
    let members = review.reviewers.join(", ");
    let reason = accepted.reason.as_deref();
    let routing_enabled = frozen.review_routing;
    let followup_reason = reason == Some(panel_acceptance_reason(&members, round, true).as_str());
    ensure!(
        reason == Some(panel_acceptance_reason(&members, round, false).as_str())
            || (routing_enabled && followup_reason),
        "source acceptance was human-overridden or mismatched"
    );
    let head = candidate
        .proposal
        .metadata
        .get("commit")
        .and_then(Value::as_str)
        .context("accepted candidate lacks host commit")?
        .to_owned();
    ensure!(sha(&head), "accepted host commit is not a full SHA");
    let turn_step = step(&steps, &turn_name)?;
    let trial_name = format!("{base_step}.trial_merge.{round}");
    let trial = output(&steps, &trial_name)?;
    let passed_main = trial
        .get("Passed")
        .and_then(|v| v.get("main"))
        .and_then(Value::as_str)
        .context("final trial merge did not pass")?;
    ensure!(sha(passed_main), "trial merge has no host main SHA");
    let trial_checked_sha = passed_main.to_owned();
    let trial_main_ref = review
        .trial_merge
        .as_ref()
        .expect("checked above")
        .main
        .clone();
    ordered(turn_step, step(&steps, &trial_name)?)?;
    let mut previous = step(&steps, &trial_name)?;
    for (index, row) in self_review.selected_controls.iter().enumerate() {
        let name = format!("{base_step}.controls.{round}.{index}");
        let proof = turn(&steps, &format!("{name}.turn"), source_run)?;
        let proof_output = output(&steps, &format!("{name}.publish"))?;
        let entry = proof_output
            .get("rows")
            .and_then(Value::as_array)
            .filter(|rows| rows.len() == 1)
            .and_then(|rows| rows.first())
            .context("selected control has no single proof row")?;
        ensure!(
            proof_output.get("ok") == Some(&Value::Bool(true))
                && proof_output.get("commit").and_then(Value::as_str) == Some(head.as_str())
                && entry.get("name").and_then(Value::as_str) == Some(row.as_str())
                && entry.get("verdict").and_then(Value::as_str) == Some("passed"),
            "selected control proof did not pass for accepted commit"
        );
        ordered(previous, step(&steps, &format!("{name}.turn"))?)?;
        ordered(
            step(&steps, &format!("{name}.turn"))?,
            step(&steps, &format!("{name}.publish"))?,
        )?;
        previous = step(&steps, &format!("{name}.publish"))?;
        rt.control_transaction({
            let proof = proof.clone();
            let output = proof_output.clone();
            move |mut tx| {
                let proof = proof.clone();
                let output = output.clone();
                Box::pin(async move {
                    published(&mut tx, &proof, &output)
                        .await
                        .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))
                })
            }
        })
        .await?;
    }
    let self_name = format!("{base_step}.self_review.{round}");
    let self_candidate = turn(&steps, &format!("{self_name}.turn"), source_run)?;
    let self_output = output(&steps, &format!("{self_name}.publish"))?;
    ensure!(
        self_output
            .get("items")
            .and_then(Value::as_object)
            .is_some(),
        "self-review has no checklist evidence"
    );
    ordered(previous, step(&steps, &format!("{self_name}.turn"))?)?;
    ordered(
        step(&steps, &format!("{self_name}.turn"))?,
        step(&steps, &format!("{self_name}.publish"))?,
    )?;
    previous = step(&steps, &format!("{self_name}.publish"))?;
    let mut has_followups = false;
    let verified_path = receipt.verified_formatting_path.as_deref().filter(|path| {
        candidate
            .proposal
            .metadata
            .get("verified_formatting_path")
            .and_then(Value::as_str)
            == Some(*path)
            && !item
                .labels
                .iter()
                .any(|label| label.eq_ignore_ascii_case("security"))
    });
    for index in 0..2 {
        let name = format!("{base_step}.review.{round}.{index}");
        let reviewer = turn(&steps, &format!("{name}.turn"), source_run)?;
        let verdict = output(&steps, &format!("{name}.publish"))?;
        has_followups |= verified_reviewer_findings(verdict, routing_enabled, verified_path)?;
        ordered(previous, step(&steps, &format!("{name}.turn"))?)?;
        ordered(
            step(&steps, &format!("{name}.turn"))?,
            step(&steps, &format!("{name}.publish"))?,
        )?;
        previous = step(&steps, &format!("{name}.publish"))?;
        rt.control_transaction({
            let reviewer = reviewer.clone();
            let output = verdict.clone();
            move |mut tx| {
                let reviewer = reviewer.clone();
                let output = output.clone();
                Box::pin(async move {
                    published(&mut tx, &reviewer, &output)
                        .await
                        .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))
                })
            }
        })
        .await?;
    }
    ensure!(
        has_followups == followup_reason,
        "source panel reason differs from its verified findings"
    );
    ordered(previous, step(&steps, &accepted_name)?)?;
    // The finalizer may have parked for branch proof before it applied. Freeze
    // the exact successful occurrence for the delivery plan.
    let applied: Vec<_> = steps
        .iter()
        .filter(|s| {
            s.name.starts_with(&format!("{base_step}.finalize."))
                && s.output
                    .as_ref()
                    .and_then(|v| v.get("status"))
                    .and_then(Value::as_str)
                    == Some("applied")
        })
        .collect();
    ensure!(
        applied.len() == 1,
        "source has no unique applied graph finalization"
    );
    let occurrence = applied[0]
        .name
        .strip_prefix(&format!("{base_step}.finalize."))
        .context("bad finalization name")?
        .parse::<u64>()?;
    ensure!(
        applied[0].output.as_ref().and_then(|v| v.get("output"))
            == Some(output(&steps, &format!("{base_step}.publish"))?),
        "applied finalization and public result differ"
    );
    ordered(step(&steps, &accepted_name)?, applied[0])?;
    ordered(applied[0], step(&steps, &format!("{base_step}.publish"))?)?;
    ordered(
        step(&steps, &format!("{base_step}.publish"))?,
        step(&steps, &format!("{base_step}.close"))?,
    )?;
    let source = DeliverySource {
        project: rt.project().map(str::to_owned),
        source_run: source_run.to_owned(),
        epic_id: frozen.epic,
        base_step: base_step.to_owned(),
        item_id: item.id,
        head,
        candidate_id: candidate.id.clone(),
        digest: candidate.digest.clone(),
        occurrence,
        review_round: round,
        lease_session: lease.session,
        lease_epoch: lease.epoch,
        lease_acquisition: lease.acquisition,
        trial_main_ref,
        trial_checked_sha,
    };
    rt.control_transaction({
        let source = source.clone();
        let self_candidate = self_candidate.clone();
        let self_output = self_output.clone();
        let candidate = candidate.clone();
        let public_output = output(&steps, &format!("{base_step}.publish"))?.clone();
        move |mut tx| {
            let source = source.clone();
            let self_candidate = self_candidate.clone();
            let self_output = self_output.clone();
            let candidate = candidate.clone();
            let public_output = public_output.clone();
            Box::pin(async move {
                published(&mut tx, &self_candidate, &self_output)
                    .await
                    .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))?;
                published(&mut tx, &candidate, &public_output)
                    .await
                    .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))?;
                revalidate_delivery_source_in(&mut tx, &source)
                    .await
                    .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))
            })
        }
    })
    .await?;
    Ok(source)
}

/// The same exact finding contract as graph review. A verified receipt also
/// attests that the item and its touched controls allowed a follow-up.
fn verified_reviewer_findings(
    verdict: &Value,
    routing_enabled: bool,
    verified_path: Option<&str>,
) -> Result<bool> {
    let said = verdict.get("verdict").and_then(Value::as_str);
    ensure!(
        said == Some("approve"),
        "independent reviewer did not approve"
    );
    let (routed, findings) = route_review_findings(
        "delivery",
        verdict,
        "approve",
        routing_enabled,
        true,
        verified_path,
    );
    ensure!(
        routed == "approve",
        "source review finding lacks host-verified formatting evidence"
    );
    Ok(!findings.is_empty())
}

async fn checkpoint_in(tx: &mut Tx<'_, '_>, run: &str, name: &str) -> Result<Value> {
    let row = tx
        .query_one(
            "SELECT output,error FROM operation_outputs WHERE workflow_uuid=? AND function_name=?",
            &[Param::Text(run.to_owned()), Param::Text(name.to_owned())],
        )
        .await?;
    ensure!(
        row.text_opt("error")?.is_none(),
        "source checkpoint {name} failed"
    );
    Ok(serde_json::from_slice(&decode_base64(
        &row.text("output")?,
    )?)?)
}

async fn recheck_followups_in(
    tx: &mut Tx<'_, '_>,
    source: &DeliverySource,
    receipt: &GraphAcceptance,
) -> Result<()> {
    let path = receipt
        .verified_formatting_path
        .as_deref()
        .context("delivery follow-ups lack a host-verified formatting receipt")?;
    let base = &source.base_step;
    let round = source.review_round;
    let author = if round == 0 {
        format!("{base}.turn")
    } else {
        format!("{base}.repair.{round}.turn")
    };
    let recorded = checkpoint_in(tx, &source.source_run, &author).await?;
    let candidate = candidate_from_turn(&recorded, &author, &source.source_run)?;
    ensure!(
        candidate.id == source.candidate_id
            && candidate.digest == source.digest
            && candidate
                .proposal
                .metadata
                .get("verified_formatting_path")
                .and_then(Value::as_str)
                == Some(path),
        "delivery formatting proof differs from its accepted candidate"
    );
    let mut has_followups = false;
    for index in 0..2 {
        let name = format!("{base}.review.{round}.{index}.publish");
        let verdict = checkpoint_in(tx, &source.source_run, &name).await?;
        has_followups |= verified_reviewer_findings(&verdict, true, Some(path))?;
    }
    ensure!(
        has_followups,
        "delivery follow-up receipt has no verified finding"
    );
    Ok(())
}

/// Recheck the frozen source immediately before a remote effect is dispatched.
/// The caller holds a durable transaction and its outbound slot in that same
/// transaction; stale accepted content or a reopened item refuses the slot.
pub async fn revalidate_delivery_source_in(
    tx: &mut Tx<'_, '_>,
    source: &DeliverySource,
) -> Result<()> {
    let row = tx.query_opt(
        "SELECT w.name,w.status,p.project FROM workflow_status w LEFT JOIN atomic_run_projects p ON p.run_id=w.workflow_uuid WHERE w.workflow_uuid=?",
        &[Param::Text(source.source_run.clone())],
    ).await?.context("source run disappeared")?;
    ensure!(
        row.text("name")? == GRAPH_PIPELINE,
        "source is no longer a graph workflow"
    );
    ensure!(
        same_state_scope(
            row.text_opt("project")?.as_deref(),
            source.project.as_deref()
        ),
        "source moved to a different project state"
    );
    let accepted = tx
        .query_opt(
            "SELECT output,error FROM operation_outputs WHERE workflow_uuid=? AND function_name=?",
            &[
                Param::Text(source.source_run.clone()),
                Param::Text(format!("{}.accepted", source.base_step)),
            ],
        )
        .await?
        .context("source acceptance disappeared")?;
    ensure!(
        accepted.text_opt("error")?.is_none(),
        "source acceptance failed"
    );
    let bytes = decode_base64(&accepted.text("output")?)?;
    let stored: GraphAcceptance = serde_json::from_slice(&bytes)?;
    let receipt = &stored.acceptance;
    ensure!(
        receipt.accepted
            && receipt.candidate == source.candidate_id
            && receipt.digest == source.digest,
        "source acceptance changed"
    );
    let followup_receipt = receipt
        .reason
        .as_deref()
        .is_some_and(|reason| reason.starts_with(FOLLOWUP_REASON_PREFIX));
    ensure!(
        stored.verified_formatting_path.is_none() || followup_receipt,
        "source formatting receipt has a different acceptance reason"
    );
    if followup_receipt {
        recheck_followups_in(tx, source, &stored).await?;
    }
    let publication = tx
        .query_opt(
            "SELECT digest,output FROM atomic_publications WHERE publication_key=?",
            &[Param::Text(source.candidate_id.clone())],
        )
        .await?
        .context("source candidate is not published")?;
    ensure!(
        publication.text("digest")? == source.digest,
        "source publication changed"
    );
    let finalized = tx
        .query_opt(
            "SELECT output,error FROM operation_outputs WHERE workflow_uuid=? AND function_name=?",
            &[
                Param::Text(source.source_run.clone()),
                Param::Text(format!(
                    "{}.finalize.{}",
                    source.base_step, source.occurrence
                )),
            ],
        )
        .await?
        .context("source finalization disappeared")?;
    ensure!(
        finalized.text_opt("error")?.is_none(),
        "source finalization failed"
    );
    let result: Value = serde_json::from_slice(&decode_base64(&finalized.text("output")?)?)?;
    let published_output: Value = serde_json::from_str(&publication.text("output")?)?;
    ensure!(
        result.get("status").and_then(Value::as_str) == Some("applied")
            && result.get("output") == Some(&published_output),
        "source finalization no longer matches its publication"
    );
    let trial = tx
        .query_opt(
            "SELECT output,error FROM operation_outputs WHERE workflow_uuid=? AND function_name=?",
            &[
                Param::Text(source.source_run.clone()),
                Param::Text(format!(
                    "{}.trial_merge.{}",
                    source.base_step, source.review_round
                )),
            ],
        )
        .await?
        .context("source passing trial disappeared")?;
    ensure!(trial.text_opt("error")?.is_none(), "source trial failed");
    let trial_result: Value = serde_json::from_slice(&decode_base64(&trial.text("output")?)?)?;
    ensure!(
        trial_result
            .get("Passed")
            .and_then(|v| v.get("main"))
            .and_then(Value::as_str)
            == Some(source.trial_checked_sha.as_str()),
        "source checked main changed"
    );
    let item = tx.query_opt(
        "SELECT status,terminal_outcome,last_finish_claimant,last_finish_session,last_finish_epoch,last_finish_acquisition,parent FROM atomic_graph_items WHERE id=?",
        &[Param::Text(source.item_id.clone())],
    ).await?.context("source graph item disappeared")?;
    ensure!(
        item.text_opt("parent")?.as_deref() == Some(source.epic_id.as_str()),
        "source graph item changed epic"
    );
    ensure!(
        item.text("status")? == "closed"
            && item.text_opt("terminal_outcome")?.as_deref() == Some("success")
            && item.text_opt("last_finish_claimant")?.as_deref()
                == Some(source.source_run.as_str())
            && item.int_opt("last_finish_session")?
                == Some(
                    i64::try_from(source.lease_session)
                        .context("source lease session exceeds stored range")?
                )
            && item.int_opt("last_finish_epoch")? == Some(source.lease_epoch)
            && item.text_opt("last_finish_acquisition")?.as_deref()
                == Some(source.lease_acquisition.as_str()),
        "source graph item is no longer closed by its run"
    );
    let link = tx
        .query_opt(
            "SELECT sha FROM atomic_graph_commit_links WHERE item_id=? AND sha=?",
            &[
                Param::Text(source.item_id.clone()),
                Param::Text(source.head.clone()),
            ],
        )
        .await?;
    ensure!(
        link.is_some(),
        "accepted commit is no longer linked to graph item"
    );
    Ok(())
}

fn decode_base64(input: &str) -> Result<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    ensure!(input.len().is_multiple_of(4), "invalid checkpoint encoding");
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for chunk in input.as_bytes().chunks_exact(4) {
        let mut values = [0u32; 4];
        let mut padding = 0;
        for (i, byte) in chunk.iter().enumerate() {
            if *byte == b'=' {
                ensure!(i >= 2, "invalid checkpoint padding");
                padding += 1;
            } else {
                ensure!(padding == 0, "invalid checkpoint padding");
                values[i] = ALPHABET
                    .iter()
                    .position(|c| c == byte)
                    .context("invalid checkpoint base64")? as u32;
            }
        }
        ensure!(padding <= 2, "invalid checkpoint padding");
        let n = (values[0] << 18) | (values[1] << 12) | (values[2] << 6) | values[3];
        out.push((n >> 16) as u8);
        if padding < 2 {
            out.push((n >> 8) as u8);
        }
        if padding == 0 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_durable::publication::Proposal;
    use serde_json::json;

    fn candidate(id: &str, commit: &str) -> Candidate {
        let proposal = Proposal {
            output: json!({"ok": true}),
            metadata: json!({"commit": commit}),
            writes: Vec::new(),
        };
        Candidate {
            id: id.into(),
            digest: format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&proposal).expect("serialize test proposal"))
            ),
            proposal,
        }
    }

    fn recorded(name: &str, position: i32, candidate: &Candidate) -> StepRecord {
        StepRecord {
            step_id: position,
            name: name.into(),
            output: Some(json!({"Candidate": candidate})),
            error: None,
            started_at: Some(position as i64),
            completed_at: Some(position as i64),
        }
    }

    #[test]
    fn repaired_candidate_must_be_the_final_accepted_turn() {
        let first = candidate("run:4", &"a".repeat(40));
        let repaired = candidate("run:8", &"b".repeat(40));
        let steps = vec![
            recorded("graph.00000.fix.turn", 4, &first),
            recorded("graph.00000.fix.repair.1.turn", 8, &repaired),
        ];
        let stale = Acceptance {
            candidate: first.id.clone(),
            digest: first.digest.clone(),
            accepted: true,
            reason: None,
        };
        assert!(round_for("graph.00000.fix", &steps, &stale, "run").is_err());
        let current = Acceptance {
            candidate: repaired.id.clone(),
            digest: repaired.digest.clone(),
            accepted: true,
            reason: None,
        };
        let (round, found, _) = round_for("graph.00000.fix", &steps, &current, "run")
            .expect("locate repaired test round");
        assert_eq!(round, 1);
        assert_eq!(found.id, repaired.id);
    }

    #[test]
    fn checkpoint_decoder_rejects_bad_padding() {
        assert_eq!(
            decode_base64("eyJvayI6dHJ1ZX0=").expect("decode valid test payload"),
            br#"{"ok":true}"#
        );
        assert!(decode_base64("=yJvayI6dHJ1ZX0=").is_err());
        assert!(decode_base64("eyJvayI6dHJ1ZX0").is_err());
    }
}
