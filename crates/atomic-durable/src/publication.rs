// SPDX-License-Identifier: Apache-2.0

//! Candidates and publication: how an atom's result becomes durable.
//!
//! An atom invocation is sibling durable operations, never one:
//!
//! 1. [`execute_candidate`] runs the author (the model turn) outside any
//!    database transaction and checkpoints the result as a *private candidate*:
//!    its output, its run metadata, the writes it proposes and a digest over
//!    them. A crash after this checkpoint recovers the candidate without
//!    calling the author again.
//! 2. [`review_candidate`] decides whether the candidate is accepted. Until
//!    reviewers exist it accepts every candidate, since a candidate exists
//!    only once the atom's output has passed its schema and its own checks.
//! 3. [`publish_candidate`] opens one short transaction that checks the
//!    acceptance names this candidate and its digest, checks the caller's
//!    preconditions (run and claim epochs, say), applies every proposed write,
//!    records the publication under a unique key and commits the operation's
//!    own checkpoint, all together or not at all.
//!
//! [`run_atom`] composes the three. Nothing a candidate proposes is visible
//! to a later step until it is published.
//!
//! A failed precondition means this execution no longer owns the run. It is
//! not an outcome: the transaction rolls back, nothing is recorded, and the
//! execution is interrupted, so a stale owner can never publish.
//!
//! A precondition is a *fence*: a conditional no-op write on the ownership
//! row, not a read. The write locks the row until the publication commits, so
//! a takeover that changes the epoch waits for the publication, and a
//! publication that meets a takeover in progress waits for it and then finds
//! the new epoch. A plain read would not: under PostgreSQL's READ COMMITTED,
//! another execution could change the epoch and commit between the read and
//! this commit. (On SQLite every write transaction already runs alone.)

use crate::runtime::{DurableError, DurableRuntime, Param, Result, StepInfo, Tx, WorkflowContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// One proposed write: a SQL statement with `?` placeholders and its values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Write {
    /// The statement.
    pub sql: String,
    /// Its values, as JSON scalars.
    pub params: Vec<Value>,
}

/// A condition the publication transaction checks before it writes anything,
/// and holds until it commits.
///
/// `sql` is a statement with `?` placeholders that changes nothing and
/// affects exactly one row when the condition holds, such as
///
/// ```sql
/// UPDATE run_epochs SET epoch = epoch WHERE run = ? AND epoch = ?
/// ```
///
/// Affecting the row is what locks it, so the statement must be an `UPDATE`:
/// anything else fails the condition, as does any row count but one. (A
/// query would not do even when it returns one row: PostgreSQL reports the
/// rows a `SELECT` returns as its count, and the read locks nothing.)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Precondition {
    /// What the condition means, for the refusal message.
    pub what: String,
    /// The fence statement.
    pub sql: String,
    /// Its values, as JSON scalars.
    pub params: Vec<Value>,
}

/// What the author produced: the output it would publish, what the run
/// recorded about itself, and the writes it proposes to make with it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    /// The output.
    pub output: Value,
    /// The run's metadata: thread, model, tokens, prompt hash.
    pub metadata: Value,
    /// The writes to make when the candidate is published.
    pub writes: Vec<Write>,
}

/// A private candidate, as checkpointed by [`execute_candidate`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// Unique per candidate: the workflow and the position of the operation
    /// that produced it, so each replacement candidate has its own identity.
    pub id: String,
    /// What it proposes.
    pub proposal: Proposal,
    /// SHA-256 over the canonical JSON of `proposal`.
    pub digest: String,
}

/// The decision that lets a candidate be published.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acceptance {
    /// The candidate it is about.
    pub candidate: String,
    /// The digest it was made on.
    pub digest: String,
    /// Whether the candidate may be published.
    pub accepted: bool,
    /// Why, when it may not.
    pub reason: Option<String>,
}

/// A published result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Published {
    /// The candidate that was published.
    pub candidate: String,
    /// The output.
    pub output: Value,
    /// The run's metadata.
    pub metadata: Value,
}

fn digest(proposal: &Proposal) -> Result<String> {
    let bytes = serde_json::to_vec(proposal)
        .map_err(|e| DurableError::Failed(format!("encode proposal: {e}")))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// What an atom's turn records: the candidate, or that the run parked before
/// the turn could finish.
#[derive(Serialize, Deserialize)]
enum Turn {
    Candidate(Candidate),
    Parked { position: i32, reason: String },
}

/// A parked run's row in `atomic_parks`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Park {
    /// `<run>:<position of the parked turn>`.
    pub id: String,
    /// The run.
    pub run: String,
    /// The operation that parked, such as `draft.park`.
    pub step: String,
    /// Why, for a person.
    pub reason: String,
    /// When, in milliseconds since the Unix epoch.
    pub parked_at: i64,
}

/// How long a park waits for: long enough to be forever.
const PARKED: std::time::Duration = std::time::Duration::from_secs(10 * 365 * 24 * 3600);

/// Run `author` as a durable step and checkpoint what it proposes as a private
/// candidate. The author runs at most once per committed checkpoint; inside the
/// at-least-once window (a crash after it returns, before the checkpoint
/// commits) it can run again.
///
/// An author that returns [`DurableError::Parked`], because a budget cap left
/// no room for its next model call, parks the run: the turn records that it
/// parked, `<name>.park` records the park in `atomic_parks`, and the run then
/// waits durably and never goes on to later steps. A recovered run replays
/// both and waits again: resuming does not unpark it.
pub async fn execute_candidate<C, F, Fut>(ctx: &C, name: &str, author: F) -> Result<Candidate>
where
    C: WorkflowContext,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Proposal>> + Send + 'static,
{
    execute_candidate_at(ctx, name, move |_| author()).await
}

/// As [`execute_candidate`], with the author told which step it runs as, so
/// it can key what it does outside the step, such as its model calls'
/// ledger requests, by the step's stable position.
pub async fn execute_candidate_at<C, F, Fut>(ctx: &C, name: &str, author: F) -> Result<Candidate>
where
    C: WorkflowContext,
    F: FnOnce(StepInfo) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Proposal>> + Send + 'static,
{
    let turn: Turn = ctx
        .step(name, move |step| async move {
            let (id, position) = (
                format!("{}:{}", step.workflow_id, step.step_id),
                step.step_id,
            );
            match author(step).await {
                Ok(proposal) => {
                    let digest = digest(&proposal)?;
                    Ok(Turn::Candidate(Candidate {
                        id,
                        proposal,
                        digest,
                    }))
                }
                Err(DurableError::Parked(reason)) => Ok(Turn::Parked { position, reason }),
                Err(e) => Err(e),
            }
        })
        .await?;
    match turn {
        Turn::Candidate(candidate) => Ok(candidate),
        Turn::Parked { position, reason } => park(ctx, name, position, reason).await,
    }
}

/// Record the park, then wait. Nothing in this version raises a cap, so the
/// wait never ends; a wake-up on the park's topic is ignored.
async fn park<C: WorkflowContext>(
    ctx: &C,
    turn: &str,
    position: i32,
    reason: String,
) -> Result<Candidate> {
    let run = ctx.workflow_id().to_owned();
    let id = format!("{run}:{position}");
    let step = format!("{}.park", turn.strip_suffix(".turn").unwrap_or(turn));
    let row = Arc::new(Park {
        id: id.clone(),
        run,
        step: step.clone(),
        reason,
        parked_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0),
    });
    ctx.transaction(&step, move |mut tx| {
        let row = row.clone();
        Box::pin(async move {
            tx.execute(
                "INSERT INTO atomic_parks (park_id, run, step, reason, parked_at)                  VALUES (?, ?, ?, ?, ?) ON CONFLICT (park_id) DO NOTHING",
                &[
                    Param::Text(row.id.clone()),
                    Param::Text(row.run.clone()),
                    Param::Text(row.step.clone()),
                    Param::Text(row.reason.clone()),
                    Param::Int(row.parked_at),
                ],
            )
            .await?;
            Ok(())
        })
    })
    .await?;
    crate::failpoint("usage:after-park");
    loop {
        let _: Option<Value> = ctx.recv(&format!("atomic.park:{id}"), PARKED).await?;
    }
}

/// The parks recorded for run `run`, oldest first.
pub async fn parks<R: DurableRuntime>(rt: &R, run: &str) -> Result<Vec<Park>> {
    let run = run.to_owned();
    rt.control_transaction(move |mut tx| {
        let run = run.clone();
        Box::pin(async move {
            let rows = tx
                .query_all(
                    "SELECT park_id, run, step, reason, parked_at FROM atomic_parks                      WHERE run = ? ORDER BY parked_at, park_id",
                    &[Param::Text(run)],
                )
                .await?;
            rows.iter()
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

/// Decide whether `candidate` may be published. Reviewers attach here as
/// sibling steps; until they exist every candidate is accepted, because a
/// candidate exists only once the atom's output passed its schema and its
/// own checks.
pub fn review_candidate(candidate: &Candidate) -> Acceptance {
    Acceptance {
        candidate: candidate.id.clone(),
        digest: candidate.digest.clone(),
        accepted: true,
        reason: None,
    }
}

/// Publish `candidate` in one transaction step: check that `acceptance`
/// accepts this candidate and digest, check `preconditions`, verify the
/// digest, apply every write, record the publication and commit this
/// operation's checkpoint together. Returns the published output.
///
/// The publication key is the candidate's identity, unique in
/// `atomic_publications`: publishing the same candidate again returns the
/// output already published instead of writing twice.
pub async fn publish_candidate<C: WorkflowContext>(
    ctx: &C,
    name: &str,
    candidate: &Candidate,
    acceptance: &Acceptance,
    preconditions: Vec<Precondition>,
) -> Result<Value> {
    if acceptance.candidate != candidate.id
        || acceptance.digest != candidate.digest
        || !acceptance.accepted
    {
        return Err(DurableError::Failed(format!(
            "candidate {} was not accepted{}",
            candidate.id,
            acceptance
                .reason
                .as_deref()
                .map(|r| format!(": {r}"))
                .unwrap_or_default()
        )));
    }
    let candidate = Arc::new(candidate.clone());
    let preconditions = Arc::new(preconditions);
    let step = Arc::new(name.to_owned());
    ctx.transaction(name, move |tx| {
        let candidate = candidate.clone();
        let preconditions = preconditions.clone();
        let step = step.clone();
        Box::pin(async move { publish_in(tx, &candidate, &step, &preconditions).await })
    })
    .await
}

/// Run one atom as sibling durable operations: `{name}.turn` executes the
/// author and checkpoints its candidate, review decides on it, and
/// `{name}.publish` publishes it behind `preconditions`.
pub async fn run_atom<C, F, Fut>(
    ctx: &C,
    name: &str,
    author: F,
    preconditions: Vec<Precondition>,
) -> Result<Published>
where
    C: WorkflowContext,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Proposal>> + Send + 'static,
{
    run_atom_at(ctx, name, move |_| author(), preconditions).await
}

/// As [`run_atom`], with the author told which step its turn runs as (see
/// [`execute_candidate_at`]).
pub async fn run_atom_at<C, F, Fut>(
    ctx: &C,
    name: &str,
    author: F,
    preconditions: Vec<Precondition>,
) -> Result<Published>
where
    C: WorkflowContext,
    F: FnOnce(StepInfo) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Proposal>> + Send + 'static,
{
    let candidate = execute_candidate_at(ctx, &format!("{name}.turn"), author).await?;
    let acceptance = review_candidate(&candidate);
    let output = publish_candidate(
        ctx,
        &format!("{name}.publish"),
        &candidate,
        &acceptance,
        preconditions,
    )
    .await?;
    Ok(Published {
        candidate: candidate.id,
        output,
        metadata: candidate.proposal.metadata,
    })
}

async fn publish_in(
    mut tx: Tx<'_, '_>,
    candidate: &Candidate,
    step: &str,
    preconditions: &[Precondition],
) -> Result<Value> {
    for p in preconditions {
        // Not an outcome of this operation either way: if the condition fails,
        // or cannot even be evaluated, this execution has not shown that it
        // may publish. Interrupt it and record nothing.
        let refuse = |why: String| {
            DurableError::Interrupted(format!(
                "publication of {} refused: {}: {why}",
                candidate.id, p.what
            ))
        };
        let is_update = p
            .sql
            .trim_start()
            .get(..7)
            .is_some_and(|head| head.eq_ignore_ascii_case("update "));
        if !is_update {
            return Err(refuse(
                "a fence must be an UPDATE that locks the row".into(),
            ));
        }
        let rows = tx
            .execute(&p.sql, &params_of(&p.params)?)
            .await
            .map_err(|e| refuse(format!("could not evaluate the precondition: {e}")))?;
        if rows != 1 {
            return Err(refuse(format!(
                "precondition failed ({rows} rows matched, not 1)"
            )));
        }
    }
    #[cfg(test)]
    tests::after_fences(&candidate.id).await;

    if digest(&candidate.proposal)? != candidate.digest {
        return Err(DurableError::Failed(format!(
            "candidate {} does not match its digest",
            candidate.id
        )));
    }

    tx.execute(
        "CREATE TABLE IF NOT EXISTS atomic_publications (
             publication_key TEXT PRIMARY KEY,
             digest TEXT NOT NULL,
             output TEXT NOT NULL
         )",
        &[],
    )
    .await?;
    let existing = tx
        .query_opt(
            "SELECT digest, output FROM atomic_publications WHERE publication_key = ?",
            &[Param::Text(candidate.id.clone())],
        )
        .await?;
    if let Some(row) = existing {
        if row.text("digest")? != candidate.digest {
            return Err(DurableError::Failed(format!(
                "publication key {} already holds a different candidate",
                candidate.id
            )));
        }
        return serde_json::from_str(&row.text("output")?)
            .map_err(|e| DurableError::Failed(format!("decode publication: {e}")));
    }

    for w in &candidate.proposal.writes {
        tx.execute(&w.sql, &params_of(&w.params)?).await?;
    }
    let output = serde_json::to_string(&candidate.proposal.output)
        .map_err(|e| DurableError::Failed(format!("encode output: {e}")))?;
    tx.execute(
        "INSERT INTO atomic_publications (publication_key, digest, output) VALUES (?, ?, ?)",
        &[
            Param::Text(candidate.id.clone()),
            Param::Text(candidate.digest.clone()),
            Param::Text(output),
        ],
    )
    .await?;
    // The thread the candidate's turn ran on, indexed both ways, in the same
    // transaction as the publication.
    if let Some(thread) = candidate
        .proposal
        .metadata
        .get("thread_id")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        let run = candidate
            .id
            .rsplit_once(':')
            .map_or(candidate.id.as_str(), |(run, _)| run);
        tx.execute(crate::runtime::THREADS_TABLE, &[]).await?;
        tx.execute(
            "INSERT INTO atomic_threads (candidate, run, step, thread_id) VALUES (?, ?, ?, ?)",
            &[
                Param::Text(candidate.id.clone()),
                Param::Text(run.to_owned()),
                Param::Text(step.to_owned()),
                Param::Text(thread.to_owned()),
            ],
        )
        .await?;
    }
    Ok(candidate.proposal.output.clone())
}

fn params_of(values: &[Value]) -> Result<Vec<Param>> {
    values
        .iter()
        .map(|v| match v {
            Value::Null => Ok(Param::Null),
            Value::Bool(b) => Ok(Param::Bool(*b)),
            Value::String(s) => Ok(Param::Text(s.clone())),
            Value::Number(n) => n
                .as_i64()
                .map(Param::Int)
                .or_else(|| n.as_f64().map(Param::Float))
                .ok_or_else(|| DurableError::Failed(format!("unsupported number {n}"))),
            other => Err(DurableError::Failed(format!(
                "unsupported parameter {other}"
            ))),
        })
        .collect()
}

#[cfg(test)]
mod tests;
