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

use crate::runtime::{
    DurableError, DurableRuntime, Param, Result, StepRecord, Tx, WorkflowContext,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// How many times [`begin`] tries to record an execution before it gives up.
const TRIES: u32 = 5;

/// Record that an execution of this workflow begins, before it records
/// anything, and return its number. Call it first in the workflow's body.
///
/// The number is taken and recorded in one statement, and tried again when
/// it fails, as it can when another execution of the same run took the same
/// number at the same moment or the database was busy. An execution that no
/// longer owns its run is told so at once.
pub async fn begin<C: WorkflowContext>(ctx: &C) -> Result<i64> {
    let run = ctx.workflow_id().to_owned();
    let started_at = now_ms();
    retrying(|| {
        let run = run.clone();
        ctx.control_transaction(move |mut tx| {
            let run = run.clone();
            Box::pin(async move { insert(&mut tx, &run, started_at).await })
        })
    })
    .await
}

/// Run `once` until it succeeds, it is interrupted, or [`TRIES`] attempts
/// failed, waiting a little longer after each failure.
async fn retrying<F, Fut>(mut once: F) -> Result<i64>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<i64>>,
{
    let mut tried = 0;
    loop {
        match once().await {
            Ok(n) => return Ok(n),
            Err(e @ DurableError::Interrupted(_)) => return Err(e),
            Err(e) => {
                tried += 1;
                if tried >= TRIES {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_millis(10 << tried)).await;
            }
        }
    }
}

/// Record the next execution of `run` in one statement, so two executions
/// cannot both read the same last number and then both write the next: one
/// of them fails on the key instead, and tries again.
async fn insert(tx: &mut Tx<'_, '_>, run: &str, started_at: i64) -> Result<i64> {
    tx.query_one(
        "INSERT INTO atomic_executions (run, attempt, started_at) \
         SELECT CAST(? AS TEXT), COALESCE(MAX(attempt), 0) + 1, CAST(? AS BIGINT) \
         FROM atomic_executions WHERE run = ? \
         RETURNING attempt",
        &[
            Param::Text(run.to_owned()),
            Param::Int(started_at),
            Param::Text(run.to_owned()),
        ],
    )
    .await?
    .int("attempt")
}

/// Run `run`'s executions, first to last. A read: it takes no write lock, so
/// it answers while a run holds one.
pub async fn executions<R: DurableRuntime>(rt: &R, run: &str) -> Result<Vec<Execution>> {
    let run = run.to_owned();
    rt.read_transaction(move |mut tx| {
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
    use crate::runtime::Durare;

    fn step(started_at: Option<i64>) -> StepRecord {
        StepRecord {
            step_id: 0,
            name: "s".into(),
            child_workflow_id: None,
            output: None,
            error: None,
            started_at,
            completed_at: started_at,
        }
    }

    fn unique() -> String {
        format!(
            "history_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        )
    }

    /// [`begin`]'s record, from outside a workflow, holding its transaction
    /// open a moment after the insert so that executions starting together
    /// all read the same last number.
    async fn begin_slowly(rt: &Durare, run: &str) -> Result<i64> {
        let run = run.to_owned();
        retrying(|| {
            let run = run.clone();
            rt.control_transaction(move |mut tx| {
                let run = run.clone();
                Box::pin(async move {
                    let n = insert(&mut tx, &run, now_ms()).await?;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok(n)
                })
            })
        })
        .await
    }

    /// Executions of one run that begin together each get a number of their
    /// own: those that lose the race try again instead of failing the run.
    async fn executions_beginning_together_are_numbered_apart(rt: Durare) {
        let run = unique();
        let rt = std::sync::Arc::new(rt);
        let together: Vec<_> = (0..4)
            .map(|_| {
                let (rt, run) = (rt.clone(), run.clone());
                tokio::spawn(async move { begin_slowly(&rt, &run).await })
            })
            .collect();
        let mut numbers = Vec::new();
        for one in together {
            numbers.push(
                one.await
                    .expect("join")
                    .expect("each execution is recorded"),
            );
        }
        numbers.sort_unstable();
        assert_eq!(numbers, [1, 2, 3, 4]);
        let recorded: Vec<i64> = executions(rt.as_ref(), &run)
            .await
            .expect("executions")
            .iter()
            .map(|e| e.attempt)
            .collect();
        assert_eq!(recorded, [1, 2, 3, 4]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sqlite_executions_beginning_together_are_numbered_apart() {
        let path = std::env::temp_dir().join(format!("{}.db", unique()));
        executions_beginning_together_are_numbered_apart(
            Durare::sqlite(&path).await.expect("sqlite"),
        )
        .await;
    }

    #[cfg(feature = "postgres-tests")]
    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_executions_beginning_together_are_numbered_apart() {
        let schema = crate::testing::schema("history").await;
        executions_beginning_together_are_numbered_apart(
            Durare::postgres(schema.url(), schema.name())
                .await
                .expect("postgres"),
        )
        .await;
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
