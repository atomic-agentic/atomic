// SPDX-License-Identifier: Apache-2.0

//! Workflows written in Rust.
//!
//! Each atom runs through [`run_atom`]: its turn and its publication are
//! sibling durable steps, so a crash after an atom has finished never runs it
//! again, and a later atom consumes the earlier one's typed, published
//! output. Each atom's run is [`contained`] in its own task, so a panic fails
//! that atom's step instead of the process.

use atomic_atoms::{
    ChangeRequest, DraftTask, DraftedTask, ReviewPatch, ReviewRequest, Verdict, WorkItem,
};
use atomic_core::AtomError;
use atomic_core::runtime::{RunMetadata, Runtimes, contained, run_codex_atom, run_lite_atom};
use atomic_durable::{
    DurableError, DurableRuntime, Proposal, Published, Result, WorkflowContext, failpoint, run_atom,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The change-request workflow: draft a task from a request, then review a
/// candidate commit against it.
pub const CHANGE_REQUEST: &str = "atomic.change-request";

/// The change-request workflow's input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRequestRun {
    /// The request, as a person wrote it.
    pub request: String,
    /// The commit the change starts from.
    pub base: String,
    /// The candidate commit to review.
    pub commit: String,
}

/// What the change-request workflow returns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangeRequestResult {
    /// The task drafted from the request.
    pub task: DraftedTask,
    /// The review of the candidate against it.
    pub verdict: Verdict,
    /// The published results of both atoms, with their run metadata.
    pub steps: Vec<Published>,
}

/// Turn an atom's failure into its step's failure, recorded as the outcome.
fn failed(e: AtomError) -> DurableError {
    DurableError::Failed(e.to_string())
}

/// The candidate an atom's run proposes: its output and its run metadata,
/// with no writes yet. The run happens in its own task.
pub async fn propose<O, F>(run: F) -> Result<Proposal>
where
    O: Serialize + Send + 'static,
    F: Future<Output = std::result::Result<(O, RunMetadata), AtomError>> + Send + 'static,
{
    let (out, metadata) = contained(run).await.map_err(failed)?;
    let encode = |v: serde_json::Result<serde_json::Value>| {
        v.map_err(|e| DurableError::Failed(format!("encode the atom's result: {e}")))
    };
    Ok(Proposal {
        output: encode(serde_json::to_value(out))?,
        metadata: encode(serde_json::to_value(metadata))?,
        writes: Vec::new(),
    })
}

fn decode<T: DeserializeOwned>(published: &Published) -> Result<T> {
    serde_json::from_value(published.output.clone()).map_err(|e| {
        DurableError::Failed(format!(
            "the output of {} does not decode: {e}",
            published.candidate
        ))
    })
}

/// Register the change-request workflow on `rt`, running its atoms on
/// `runtimes`.
pub fn register_change_request<R: DurableRuntime>(rt: &mut R, runtimes: Arc<Runtimes>) {
    rt.register(
        CHANGE_REQUEST,
        move |ctx: R::Context, input: ChangeRequestRun| {
            let runtimes = runtimes.clone();
            async move {
                let lite = runtimes.lite.clone();
                let request = ChangeRequest {
                    request: input.request.clone(),
                };
                let draft = run_atom(
                    &ctx,
                    "draft",
                    move || {
                        propose(async move { run_lite_atom(&DraftTask, &request, &lite).await })
                    },
                    Vec::new(),
                )
                .await?;
                failpoint("change-request:after-draft");
                let task: DraftedTask = decode(&draft)?;

                let codex = runtimes.codex.clone();
                let review_request = ReviewRequest {
                    item: WorkItem {
                        id: format!("{}:draft", ctx.workflow_id()),
                        title: task.title.clone(),
                        priority: task.priority,
                        criteria: task.criteria.clone(),
                    },
                    commit: input.commit.clone(),
                    base: input.base.clone(),
                };
                let review = run_atom(
                    &ctx,
                    "review",
                    move || {
                        propose(async move {
                            run_codex_atom(&ReviewPatch, &review_request, &codex).await
                        })
                    },
                    Vec::new(),
                )
                .await?;
                let verdict: Verdict = decode(&review)?;
                Ok(ChangeRequestResult {
                    task,
                    verdict,
                    steps: vec![draft, review],
                })
            }
        },
    );
}
