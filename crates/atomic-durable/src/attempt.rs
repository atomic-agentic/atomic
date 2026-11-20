// SPDX-License-Identifier: Apache-2.0

//! Recognizing recovery of a step whose effects may already have happened.
//!
//! A durable step runs at least once. If its body runs but its checkpoint
//! cannot commit, recovery can run the body again, even in the same process.
//! For effects atomic cannot observe, that retry must be recognized before
//! anything is dispatched again.
//!
//! [`begin`] creates a fresh identity on each call and records it as a
//! sibling step before the guarded step. First execution reads back the
//! identity it just wrote. Recovery reads the old identity, which differs
//! from this call's identity; [`Attempt::interrupted`] then reports that the
//! guarded body may already have run.
//!
//! A failure between the record and the guarded body also counts as an
//! interrupted attempt, conservatively. Check the guard only inside the
//! guarded body: a completed step replays its checkpoint without entering
//! that body. Branching on the guard in workflow code would change durable
//! step positions on replay.

use crate::runtime::{DurableError, Result, WorkflowContext};
use serde::{Deserialize, Serialize};

/// A guard for this execution of a step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    interrupted: bool,
}

impl Attempt {
    /// Whether an earlier execution may have run the guarded step without
    /// saving its result. Check only inside the guarded step's body.
    pub fn interrupted(&self) -> bool {
        self.interrupted
    }
}

/// The identity written before dispatch. Keep the original checkpoint field
/// name so records written by the previous process-based guard still replay.
#[derive(Serialize, Deserialize)]
struct Started {
    process: String,
}

/// Record durable step `name` before the guarded step. Each invocation gets
/// a new identity, including automatic recovery within the same process.
pub async fn begin<C: WorkflowContext>(ctx: &C, name: &str) -> Result<Attempt> {
    let (attempt, _) = begin_with(ctx, name, || async { Ok(()) }).await?;
    Ok(attempt)
}

/// [`begin`], running `prepare` in the recording step's own body first, so
/// the attempt is recorded only once `prepare` has succeeded: an attempt
/// whose preparation failed, or never ran, is never taken for one whose
/// guarded step may have run. Returns what `prepare` made when it ran in
/// this execution, and `None` when the step was recorded by an earlier one
/// and replays without running its body.
pub async fn begin_with<C, T, F, Fut>(
    ctx: &C,
    name: &str,
    prepare: F,
) -> Result<(Attempt, Option<T>)>
where
    C: WorkflowContext,
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T>> + Send + 'static,
{
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| DurableError::Engine(format!("create an attempt identity: {e}")))?;
    let here: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let token = here.clone();
    let made = std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot = made.clone();
    let started: Started = ctx
        .step(name, move |_| async move {
            let value = prepare().await?;
            if let Ok(mut slot) = slot.lock() {
                *slot = Some(value);
            }
            Ok(Started { process: token })
        })
        .await?;
    let value = made.lock().ok().and_then(|mut slot| slot.take());
    Ok((
        Attempt {
            interrupted: started.process != here,
        },
        value,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DurableError, DurableRuntime, Durare, RunStatus};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn unique() -> String {
        format!(
            "attempt_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        )
    }

    async fn same_process_recovery<R: DurableRuntime>(mut rt: R) {
        let calls = Arc::new(AtomicUsize::new(0));
        let effects = calls.clone();
        rt.register("guarded", move |ctx: R::Context, (): ()| {
            let effects = effects.clone();
            async move {
                let attempt = begin(&ctx, "send.attempt").await?;
                ctx.step("send", move |_| async move {
                    if attempt.interrupted() {
                        return Err(DurableError::Failed("outcome_unknown".into()));
                    }
                    let previous = effects.fetch_add(1, Ordering::SeqCst);
                    if previous == 0 {
                        Err(DurableError::Interrupted("lost the checkpoint".into()))
                    } else {
                        Ok(())
                    }
                })
                .await
            }
        });
        rt.launch().await.expect("launch");
        let id = unique();
        rt.start("guarded", &id, ()).await.expect("start");
        let mut terminal = None;
        for _ in 0..500 {
            let status = rt.status(&id).await.expect("status");
            if !matches!(status, Some(RunStatus::Pending)) {
                terminal = status;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the recovered body must park before repeating its effect"
        );
        assert!(
            matches!(terminal, Some(RunStatus::Failed(ref error)) if error.contains("outcome_unknown")),
            "{terminal:?}"
        );
    }

    #[tokio::test]
    async fn sqlite_recovery_in_the_same_process_does_not_repeat_an_effect() {
        let path = std::env::temp_dir().join(format!("{}.db", unique()));
        same_process_recovery(Durare::sqlite(&path).await.expect("sqlite")).await;
    }

    #[cfg(feature = "postgres-tests")]
    #[tokio::test]
    async fn postgres_recovery_in_the_same_process_does_not_repeat_an_effect() {
        let url = crate::testing::database().await;
        same_process_recovery(Durare::postgres(&url, &unique()).await.expect("postgres")).await;
    }
}
