// SPDX-License-Identifier: Apache-2.0

//! The candidate and publication adapter.
//!
//! An atom invocation is two sibling durable operations, never one:
//!
//! 1. [`execute_candidate`] runs the author (the model turn) outside any
//!    database transaction and checkpoints the result as a *private candidate*:
//!    its output, the writes it proposes and a digest over both. A crash after
//!    this checkpoint recovers the candidate without calling the author again.
//! 2. [`publish_candidate`] opens one short transaction that checks the
//!    caller's preconditions (run and claim epochs, say), re-checks the digest,
//!    applies every proposed write, records the publication under a unique key
//!    and commits the operation's own checkpoint, all together or not at all.
//!
//! Review and repair rounds run between the two as further sibling operations;
//! nothing here depends on them.
//!
//! A failed precondition means this execution no longer owns the run. It is
//! not an outcome: the transaction rolls back, nothing is recorded, and the
//! execution is interrupted, so a stale owner can never publish.

// The adapter is used by the workflow runtime, which lands in a later
// milestone; until then only its tests call it.
#![allow(dead_code)]

use durare::{DurableContext, Error, Param, Result, StepCtx, Tx};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// One proposed write: a SQL statement with `?` placeholders and its values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Write {
    pub(crate) sql: String,
    pub(crate) params: Vec<Value>,
}

/// A condition the publication transaction checks before it writes anything.
///
/// `sql` is a query with `?` placeholders that returns one row whose first
/// column, named `ok`, is 1 when the condition holds. No row, or any other
/// value, fails the condition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Precondition {
    pub(crate) what: String,
    pub(crate) sql: String,
    pub(crate) params: Vec<Value>,
}

/// What the author produced: the output it would publish and the writes it
/// proposes to make with it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Proposal {
    pub(crate) output: Value,
    pub(crate) writes: Vec<Write>,
}

/// A private candidate, as checkpointed by [`execute_candidate`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Candidate {
    /// Unique per candidate: the workflow and the position of the operation
    /// that produced it, so each replacement candidate has its own identity.
    pub(crate) id: String,
    pub(crate) proposal: Proposal,
    /// SHA-256 over the canonical JSON of `proposal`.
    pub(crate) digest: String,
}

pub(crate) fn digest(proposal: &Proposal) -> Result<String> {
    let bytes =
        serde_json::to_vec(proposal).map_err(|e| Error::app(format!("encode proposal: {e}")))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// Run `author` as a durable step and checkpoint what it proposes as a private
/// candidate. The author runs at most once per committed checkpoint; inside the
/// at-least-once window (a crash after it returns, before the checkpoint
/// commits) it can run again.
pub(crate) async fn execute_candidate<F, Fut>(
    ctx: &DurableContext,
    name: &str,
    author: F,
) -> Result<Candidate>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Proposal>> + Send + 'static,
{
    ctx.step(name, move |step: StepCtx| async move {
        let proposal = author().await?;
        let digest = digest(&proposal)?;
        Ok(Candidate {
            id: format!("{}:{}", step.workflow_id(), step.step_id()),
            proposal,
            digest,
        })
    })
    .await
}

/// Publish `candidate` in one transaction step: check `preconditions`, verify
/// the digest, apply every write, record the publication and commit this
/// operation's checkpoint together. Returns the published output.
///
/// The publication key is the candidate's identity, unique in
/// `atomic_publications`: publishing the same candidate again returns the
/// output already published instead of writing twice.
pub(crate) async fn publish_candidate(
    ctx: &DurableContext,
    name: &str,
    candidate: &Candidate,
    preconditions: Vec<Precondition>,
) -> Result<Value> {
    let candidate = Arc::new(candidate.clone());
    let preconditions = Arc::new(preconditions);
    ctx.transaction(name, move |tx| {
        let candidate = candidate.clone();
        let preconditions = preconditions.clone();
        Box::pin(async move { publish_in(tx, &candidate, &preconditions).await })
    })
    .await
}

async fn publish_in(
    tx: &mut Tx<'_>,
    candidate: &Candidate,
    preconditions: &[Precondition],
) -> Result<Value> {
    for p in preconditions {
        // Not an outcome of this operation either way: if the condition fails,
        // or cannot even be evaluated, this execution has not shown that it
        // may publish. Interrupt it and record nothing.
        let refuse = |why: String| {
            Error::RecoveryRequired(Arc::new(Error::app(format!(
                "publication of {} refused: {}: {why}",
                candidate.id, p.what
            ))))
        };
        let row = tx
            .query_opt(&p.sql, &params_of(&p.params)?)
            .await
            .map_err(|e| refuse(format!("could not evaluate the precondition: {e}")))?;
        let holds = match row {
            Some(row) => is_one(&row),
            None => false,
        };
        if !holds {
            return Err(refuse("precondition failed".into()));
        }
    }

    if digest(&candidate.proposal)? != candidate.digest {
        return Err(Error::app(format!(
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
        let stored_digest: String = row.try_get("digest")?;
        if stored_digest != candidate.digest {
            return Err(Error::app(format!(
                "publication key {} already holds a different candidate",
                candidate.id
            )));
        }
        let output: String = row.try_get("output")?;
        return serde_json::from_str(&output)
            .map_err(|e| Error::app(format!("decode publication: {e}")));
    }

    for w in &candidate.proposal.writes {
        tx.execute(&w.sql, &params_of(&w.params)?).await?;
    }
    let output = serde_json::to_string(&candidate.proposal.output)
        .map_err(|e| Error::app(format!("encode output: {e}")))?;
    tx.execute(
        "INSERT INTO atomic_publications (publication_key, digest, output) VALUES (?, ?, ?)",
        &[
            Param::Text(candidate.id.clone()),
            Param::Text(candidate.digest.clone()),
            Param::Text(output),
        ],
    )
    .await?;
    Ok(candidate.proposal.output.clone())
}

/// Whether the row's `ok` column is 1 or true, whatever integer or boolean
/// type the database returns it as.
fn is_one(row: &durare::Row) -> bool {
    row.try_get::<i64>("ok")
        .map(|v| v == 1)
        .or_else(|_| row.try_get::<i32>("ok").map(|v| v == 1))
        .or_else(|_| row.try_get::<bool>("ok"))
        .unwrap_or(false)
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
                .ok_or_else(|| Error::app(format!("unsupported number {n}"))),
            other => Err(Error::app(format!("unsupported parameter {other}"))),
        })
        .collect()
}

#[cfg(test)]
mod tests;
