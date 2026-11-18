// SPDX-License-Identifier: Apache-2.0

//! Telling a step's run after a crash from its first run.
//!
//! A durable step runs at least once: if the process stops after the step's
//! body began and before its checkpoint committed, recovery runs the body
//! again, and nothing tells the body so. For an effect atomic cannot observe,
//! such as a command that calls a remote system, that difference decides
//! whether running it again is safe.
//!
//! [`begin`] records, as a step of its own just before the step it guards,
//! which process is about to run that step. The record commits before the
//! guarded body starts, so it survives the crash it guards against. Inside
//! the guarded body, [`Attempt::interrupted`] says whether the record came
//! from another process: one that began this attempt and stopped before its
//! checkpoint, so whether its effects happened is unknown.
//!
//! Two limits follow from how it works:
//!
//! - A process that stops between the record and the start of the guarded
//!   body also counts as an interrupted attempt, although nothing ran. The
//!   error is on the safe side.
//! - The answer means something only inside the body of the step it guards.
//!   On a replay, the record is an old process's, but a guarded step that
//!   finished replays its checkpoint and its body never runs. Workflow code
//!   must not branch on it, which would change the run's sequence of steps
//!   on replay.

use crate::runtime::{Result, WorkflowContext};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// Who began a guarded step's latest attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    process: String,
}

impl Attempt {
    /// Whether another process began this attempt and stopped before the
    /// guarded step's checkpoint. Call it only inside the guarded step's
    /// body.
    pub fn interrupted(&self) -> bool {
        self.process != process()
    }
}

/// Record, as durable step `name`, that this process is about to run the
/// step after it.
pub async fn begin<C: WorkflowContext>(ctx: &C, name: &str) -> Result<Attempt> {
    let here = process().to_owned();
    ctx.step(name, move |_| async move { Ok(Attempt { process: here }) })
        .await
}

/// This process's identity: random, fixed for its lifetime, and never
/// reused by another process.
fn process() -> &'static str {
    static PROCESS: OnceLock<String> = OnceLock::new();
    PROCESS.get_or_init(|| {
        let mut bytes = [0u8; 16];
        if getrandom::fill(&mut bytes).is_err() {
            // No system randomness: the process id and the start time still
            // tell one process from the next.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            bytes[..4].copy_from_slice(&std::process::id().to_le_bytes());
            bytes[4..].copy_from_slice(&nanos.to_le_bytes()[..12]);
        }
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    })
}
