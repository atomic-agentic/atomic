// SPDX-License-Identifier: Apache-2.0

//! A run's executions: each time workflow code ran for it, from its start or
//! from its checkpoints after a crash, a recovery or a resume.
//!
//! The engine records each operation's checkpoint and when it started, and
//! nothing of the executions that produced them. [`begin`] adds that: the
//! workflow calls it each time it starts, as a control record, not a
//! checkpoint, so every execution writes one, replays included. Comparing an
//! operation's start with the executions' tells which execution recorded it:
//! an execution after it found it already checkpointed.

use crate::runtime::{DurableRuntime, Param, Result, StepRecord, WorkflowContext};
use std::time::{SystemTime, UNIX_EPOCH};

/// One execution of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Execution {
    /// Its number, from 1.
    pub attempt: i64,
    /// When it began, in milliseconds since the Unix epoch.
    pub started_at: i64,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Record that an execution of this workflow begins, before it records
/// anything, and return its number. Call it first in the workflow's body.
pub async fn begin<C: WorkflowContext>(ctx: &C) -> Result<i64> {
    let run = ctx.workflow_id().to_owned();
    let started_at = now_ms();
    ctx.control_transaction(move |mut tx| {
        let run = run.clone();
        Box::pin(async move {
            let n = tx
                .query_one(
                    "SELECT COALESCE(MAX(attempt), 0) AS n FROM atomic_executions WHERE run = ?",
                    &[Param::Text(run.clone())],
                )
                .await?
                .int("n")?
                + 1;
            tx.execute(
                "INSERT INTO atomic_executions (run, attempt, started_at) \
                 VALUES (?, CAST(? AS BIGINT), CAST(? AS BIGINT))",
                &[Param::Text(run), Param::Int(n), Param::Int(started_at)],
            )
            .await?;
            Ok(n)
        })
    })
    .await
}

/// Run `run`'s executions, first to last.
pub async fn executions<R: DurableRuntime>(rt: &R, run: &str) -> Result<Vec<Execution>> {
    let run = run.to_owned();
    rt.control_transaction(move |mut tx| {
        let run = run.clone();
        Box::pin(async move {
            tx.query_all(
                "SELECT attempt, started_at FROM atomic_executions WHERE run = ? \
                 ORDER BY attempt",
                &[Param::Text(run)],
            )
            .await?
            .iter()
            .map(|r| {
                Ok(Execution {
                    attempt: r.int("attempt")?,
                    started_at: r.int("started_at")?,
                })
            })
            .collect()
        })
    })
    .await
}

/// The execution that recorded `step`: the last to begin before it started,
/// or `None` when its start is not recorded or comes before every execution.
pub fn recorded_by(step: &StepRecord, executions: &[Execution]) -> Option<i64> {
    let started = step.started_at?;
    executions
        .iter()
        .filter(|e| e.started_at <= started)
        .map(|e| e.attempt)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(started_at: Option<i64>) -> StepRecord {
        StepRecord {
            step_id: 0,
            name: "s".into(),
            output: None,
            error: None,
            started_at,
            completed_at: started_at,
        }
    }

    #[test]
    fn a_step_belongs_to_the_last_execution_that_began_before_it() {
        let executions = [
            Execution {
                attempt: 1,
                started_at: 100,
            },
            Execution {
                attempt: 2,
                started_at: 200,
            },
        ];
        assert_eq!(recorded_by(&step(Some(100)), &executions), Some(1));
        assert_eq!(recorded_by(&step(Some(199)), &executions), Some(1));
        assert_eq!(recorded_by(&step(Some(200)), &executions), Some(2));
        assert_eq!(recorded_by(&step(Some(99)), &executions), None);
        assert_eq!(recorded_by(&step(None), &executions), None);
    }
}
