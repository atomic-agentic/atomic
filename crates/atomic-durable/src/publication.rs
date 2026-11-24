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
#[derive(Serialize)]
enum Turn {
    Candidate(Candidate),
    Parked {
        position: i32,
        reason: String,
        #[serde(default)]
        scope: Option<String>,
    },
}

/// A turn checkpoint as stored. A store written before turns could park
/// holds the bare candidate; it decodes as [`Turn::Candidate`], so a run
/// checkpointed then resumes now.
impl<'de> Deserialize<'de> for Turn {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        enum Tagged {
            Candidate(Candidate),
            Parked {
                position: i32,
                reason: String,
                #[serde(default)]
                scope: Option<String>,
            },
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            Tagged(Tagged),
            Bare(Candidate),
        }
        Ok(match Stored::deserialize(d)? {
            Stored::Tagged(Tagged::Candidate(c)) | Stored::Bare(c) => Turn::Candidate(c),
            Stored::Tagged(Tagged::Parked {
                position,
                reason,
                scope,
            }) => Turn::Parked {
                position,
                reason,
                scope,
            },
        })
    }
}

impl crate::runtime::Durare {
    /// Each atom of run `run` whose result was published, by the name its
    /// operations share (`draft` for `draft.turn` and `draft.publish`): the
    /// candidate its last turn checkpointed before its publication. Each is
    /// decoded as the turn recorded it, tagged or bare, and refused if it
    /// does not match its digest. A turn that parked, or whose candidate was
    /// never published, has no entry.
    pub async fn recorded_candidates(
        &self,
        run: &str,
    ) -> Result<std::collections::BTreeMap<String, Candidate>> {
        let mut steps = DurableRuntime::steps(self, run).await?;
        steps.sort_by_key(|s| s.step_id);
        let mut turns = std::collections::BTreeMap::new();
        let mut published = std::collections::BTreeMap::new();
        for step in steps {
            if let Some(name) = step.name.strip_suffix(".turn") {
                // A repair round's turn (`draft.repair.1.turn`) replaces the
                // candidate of the atom it repairs, which `draft.publish`
                // publishes.
                let name = repaired(name).unwrap_or(name);
                let turn = match &step.output {
                    Some(output) if step.error.is_none() => {
                        serde_json::from_value::<Turn>(output.clone()).map_err(|e| {
                            DurableError::Failed(format!(
                                "run {run}: {}: not a turn checkpoint: {e}",
                                step.name
                            ))
                        })?
                    }
                    _ => {
                        turns.remove(name);
                        continue;
                    }
                };
                match turn {
                    Turn::Candidate(candidate) => {
                        if digest(&candidate.proposal)? != candidate.digest {
                            return Err(DurableError::Failed(format!(
                                "run {run}: {}: candidate {} does not match its digest",
                                step.name, candidate.id
                            )));
                        }
                        turns.insert(name.to_owned(), candidate);
                    }
                    Turn::Parked { .. } => {
                        turns.remove(name);
                    }
                }
            } else if let Some(name) = step.name.strip_suffix(".publish")
                && step.error.is_none()
                && let Some(candidate) = turns.remove(name)
            {
                if step.output.as_ref() != Some(&candidate.proposal.output) {
                    return Err(DurableError::Failed(format!(
                        "run {run}: {}: the published output is not candidate {}'s",
                        step.name, candidate.id
                    )));
                }
                published.insert(name.to_owned(), candidate);
            }
        }
        Ok(published)
    }
}

/// The atom a repair round's operations name repairs: `draft` for
/// `draft.repair.2`.
fn repaired(name: &str) -> Option<&str> {
    let (base, round) = name.rsplit_once(".repair.")?;
    (!round.is_empty() && round.bytes().all(|b| b.is_ascii_digit())).then_some(base)
}

/// A parked run's row in `atomic_parks`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Park {
    /// `<run>:<position of the parked turn>`: each park has its own.
    pub id: String,
    /// The run.
    pub run: String,
    /// The operation that parked, such as `draft.park`.
    pub step: String,
    /// Why, for a person.
    pub reason: String,
    /// The allowance that had no room, when the refusal named one.
    #[serde(default)]
    pub scope: Option<String>,
    /// The project the run parked for, as its runtime was opened with; `None`
    /// for one opened for no project.
    #[serde(default)]
    pub project: Option<String>,
    /// When, in milliseconds since the Unix epoch.
    pub parked_at: i64,
}

impl Park {
    /// The topic the parked run waits on: a message on it releases the park.
    pub(crate) fn topic(&self) -> String {
        topic(&self.id)
    }

    /// The allowance that refused the parked call, as the refusal recorded
    /// it.
    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }
}

fn topic(park: &str) -> String {
    format!("atomic.park:{park}")
}

/// How long a park waits before it waits again: long enough to be forever.
const PARKED: std::time::Duration = std::time::Duration::from_secs(10 * 365 * 24 * 3600);

/// Run `author` as a durable step and checkpoint what it proposes as a private
/// candidate. The author runs at most once per committed checkpoint; inside the
/// at-least-once window (a crash after it returns, before the checkpoint
/// commits) it can run again.
///
/// An author that returns [`DurableError::Parked`], because a budget cap left
/// no room for its next model call, parks the run: the turn records that it
/// parked, `<name>.park` records the park in `atomic_parks`, and the run waits
/// durably on the park's [topic](Park::topic), going on to no later step. A
/// recovered run replays all three and waits again: resuming does not unpark
/// it. When the park is [released](release), the author runs again as a new
/// turn, and its calls are admitted afresh against the caps as they stand
/// then: if they still leave no room, it parks again.
pub async fn execute_candidate<C, F, Fut>(ctx: &C, name: &str, author: F) -> Result<Candidate>
where
    C: WorkflowContext,
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Proposal>> + Send + 'static,
{
    execute_candidate_at(ctx, name, move |_| author()).await
}

/// As [`execute_candidate`], with the author told which step it runs as, so
/// it can key what it does outside the step, such as its model calls'
/// ledger requests, by the step's stable position.
///
/// The position it is told is its first turn's, on every turn: a turn run
/// again after a park makes new attempts at the same ledger requests,
/// against the same atom allowance, rather than new ones.
pub async fn execute_candidate_at<C, F, Fut>(ctx: &C, name: &str, author: F) -> Result<Candidate>
where
    C: WorkflowContext,
    F: Fn(StepInfo) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Proposal>> + Send + 'static,
{
    Ok(execute_candidate_told(ctx, name, author).await?.0)
}

/// As [`execute_candidate_at`], with the position the author was told: its
/// first turn's, however many times a park ran it again. It is the same on
/// a replay of the turn, which runs no author.
pub async fn execute_candidate_told<C, F, Fut>(
    ctx: &C,
    name: &str,
    author: F,
) -> Result<(Candidate, i32)>
where
    C: WorkflowContext,
    F: Fn(StepInfo) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Proposal>> + Send + 'static,
{
    let author = Arc::new(author);
    let mut first: Option<i32> = None;
    loop {
        let author = author.clone();
        let turn: Turn = ctx
            .step(name, move |step| async move {
                let position = step.step_id;
                let id = format!("{}:{position}", step.workflow_id);
                let told = StepInfo {
                    workflow_id: step.workflow_id,
                    step_id: first.unwrap_or(position),
                };
                match author(told).await {
                    Ok(proposal) => {
                        let digest = digest(&proposal)?;
                        Ok(Turn::Candidate(Candidate {
                            id,
                            proposal,
                            digest,
                        }))
                    }
                    Err(DurableError::Parked { reason, scope }) => Ok(Turn::Parked {
                        position,
                        reason,
                        scope,
                    }),
                    Err(e) => Err(e),
                }
            })
            .await?;
        match turn {
            Turn::Candidate(candidate) => {
                let told = match first {
                    Some(first) => first,
                    None => candidate
                        .id
                        .rsplit_once(':')
                        .and_then(|(_, p)| p.parse().ok())
                        .ok_or_else(|| {
                            DurableError::Failed(format!(
                                "candidate {} does not name its turn's position",
                                candidate.id
                            ))
                        })?,
                };
                return Ok((candidate, told));
            }
            Turn::Parked {
                position,
                reason,
                scope,
            } => {
                first.get_or_insert(position);
                park(ctx, name, position, reason, scope).await?;
            }
        }
    }
}

/// Record the park, then wait on its topic until it is released.
async fn park<C: WorkflowContext>(
    ctx: &C,
    turn: &str,
    position: i32,
    reason: String,
    scope: Option<String>,
) -> Result<()> {
    let run = ctx.workflow_id().to_owned();
    let id = format!("{run}:{position}");
    park_named(ctx, turn, id, reason, scope).await
}

/// Park a graph claim after its checkpointed acquisition is no longer the
/// authoritative lease. `occurrence` is the number of prior releases of this
/// claim's park in the replayed workflow. Each wait has a distinct releasable
/// id, while replay of the same wait keeps its id. Releasing only wakes the
/// workflow; callers must check the same acquisition again.
pub async fn park_claim<C: WorkflowContext>(
    ctx: &C,
    step: &str,
    acquisition: &str,
    item_id: &str,
    occurrence: u64,
) -> Result<()> {
    let id = format!("{}:claim:{acquisition}:{occurrence}", ctx.workflow_id());
    park_named(
        ctx,
        step,
        id,
        format!("claim for {item_id} was taken over; revalidate before executing"),
        Some(item_id.to_owned()),
    )
    .await
}

/// Park a graph work loop when work children remain but none is ready. A
/// person releases the park after resolving a blocker or due condition; the
/// next durable claim step then selects against the refreshed graph.
pub async fn park_graph_pending<C: WorkflowContext>(
    ctx: &C,
    step: &str,
    epic: &str,
    occurrence: u64,
) -> Result<()> {
    let id = format!("{}:graph-pending:{epic}:{occurrence}", ctx.workflow_id());
    park_named(
        ctx,
        step,
        id,
        format!("graph epic {epic} has unfinished work children, but none is ready; resolve blockers or due dates, then release this park"),
        Some(epic.to_owned()),
    ).await
}

/// Wait for host branch evidence for a staged graph commit. A later check
/// uses a new occurrence, so releasing an earlier park cannot silently
/// authorize a commit that still lacks proof.
pub async fn park_verified_commit<C: WorkflowContext>(
    ctx: &C,
    step: &str,
    item_id: &str,
    sha: &str,
    occurrence: u64,
) -> Result<()> {
    let id = format!(
        "{}:verified-commit:{item_id}:{sha}:{occurrence}",
        ctx.workflow_id()
    );
    park_named(
        ctx,
        step,
        id,
        format!(
            "graph item {item_id} awaits host branch proof for commit {sha}; verify the branch, then release this park"
        ),
        Some(item_id.to_owned()),
    )
    .await
}

/// Park a criterion whose external execution may have happened without a
/// trustworthy result. Releasing this park acknowledges that the operator
/// has seen the unknown outcome; the caller then releases any graph claim and
/// terminalizes this run without dispatching the criterion again.
pub async fn park_criterion<C: WorkflowContext>(
    ctx: &C,
    step: &str,
    criterion: &str,
    occurrence: u64,
) -> Result<()> {
    let id = format!("{}:criterion:{criterion}:{occurrence}", ctx.workflow_id());
    park_named(
        ctx,
        step,
        id,
        format!(
            "criterion {criterion} has an unknown external outcome; reconcile it and start a fresh run"
        ),
        None,
    )
    .await
}

/// Wait for an operator after a remote delivery's effect may have happened
/// without a recorded receipt. Releasing the park only lets the workflow
/// reconcile/read the remote state; it never authorizes dispatch again.
pub async fn park_delivery<C: WorkflowContext>(
    ctx: &C,
    step: &str,
    slot: &str,
    occurrence: u64,
) -> Result<()> {
    let id = format!("{}:delivery:{slot}:{occurrence}", ctx.workflow_id());
    park_named(
        ctx,
        step,
        id,
        format!(
            "remote delivery {slot} has an unknown effect outcome; reconcile the remote receipt before release"
        ),
        None,
    )
    .await
}

async fn park_named<C: WorkflowContext>(
    ctx: &C,
    turn: &str,
    id: String,
    reason: String,
    scope: Option<String>,
) -> Result<()> {
    let run = ctx.workflow_id().to_owned();
    let step = format!("{}.park", turn.strip_suffix(".turn").unwrap_or(turn));
    let row = Arc::new(Park {
        id: id.clone(),
        run,
        step: step.clone(),
        reason,
        scope,
        project: ctx.project().map(str::to_owned),
        parked_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0),
    });
    ctx.transaction(&step, move |mut tx| {
        let row = row.clone();
        Box::pin(async move {
            tx.execute(
                "INSERT INTO atomic_parks (park_id, run, step, reason, parked_at) \
                 VALUES (?, ?, ?, ?, ?) ON CONFLICT (park_id) DO NOTHING",
                &[
                    Param::Text(row.id.clone()),
                    Param::Text(row.run.clone()),
                    Param::Text(row.step.clone()),
                    Param::Text(row.reason.clone()),
                    Param::Int(row.parked_at),
                ],
            )
            .await?;
            if let Some(scope) = &row.scope {
                tx.execute(
                    "INSERT INTO atomic_park_scopes (park_id, scope) VALUES (?, ?) \
                     ON CONFLICT (park_id) DO NOTHING",
                    &[Param::Text(row.id.clone()), Param::Text(scope.clone())],
                )
                .await?;
            }
            if let Some(project) = &row.project {
                tx.execute(
                    "INSERT INTO atomic_park_projects (park_id, project) VALUES (?, ?) \
                     ON CONFLICT (park_id) DO NOTHING",
                    &[Param::Text(row.id.clone()), Param::Text(project.clone())],
                )
                .await?;
            }
            Ok(())
        })
    })
    .await?;
    crate::failpoint("usage:after-park");
    loop {
        let woken: Option<Value> = ctx.recv(&topic(&id), PARKED).await?;
        if woken.is_some() {
            return Ok(());
        }
    }
}

/// Wake `park`'s run: it runs the parked turn again, admitting its calls
/// afresh. Waking does not raise a cap; a wake with no more room than before
/// parks the run again. Waking the same park twice delivers one wake-up.
///
/// Not public: a person releases a park through
/// [`crate::release::release_park`], with a human credential, which records
/// the release and then wakes the run.
pub(crate) async fn release<R: DurableRuntime>(rt: &R, park: &Park) -> Result<()> {
    rt.send(
        &park.run,
        &park.topic(),
        serde_json::json!({"released": park.id}),
        &format!("release:{}", park.id),
    )
    .await
}

/// Every park with the allowance that refused it, if it named one, and the
/// project it is for: what [`park_from`] reads, as `p` for further
/// conditions.
pub(crate) const PARKS: &str = "SELECT p.park_id AS park_id, p.run AS run, p.step AS step, \
     p.reason AS reason, s.scope AS scope, pp.project AS project, p.parked_at AS parked_at \
     FROM atomic_parks p LEFT JOIN atomic_park_scopes s ON s.park_id = p.park_id \
     LEFT JOIN atomic_park_projects pp ON pp.park_id = p.park_id";

/// The condition on a [`PARKS`] row that it is for the project its one
/// parameter, [`project_param`], names: on a store several projects share,
/// no project lists or acts on another's parks.
pub(crate) const FOR_PROJECT: &str = "COALESCE(pp.project, '') = ?";

/// The parameter [`FOR_PROJECT`] takes for runtime `project`: a park for no
/// project is seen only by a runtime for none.
pub(crate) fn project_param(project: Option<&str>) -> Param {
    Param::Text(project.unwrap_or_default().to_owned())
}

/// A park from a row of [`PARKS`].
pub(crate) fn park_from(r: &crate::runtime::Row) -> Result<Park> {
    Ok(Park {
        id: r.text("park_id")?,
        run: r.text("run")?,
        step: r.text("step")?,
        reason: r.text("reason")?,
        scope: r.text_opt("scope")?,
        project: r.text_opt("project")?,
        parked_at: r.int("parked_at")?,
    })
}

/// The parks recorded for run `run` of `rt`'s project, oldest first.
pub async fn parks<R: DurableRuntime>(rt: &R, run: &str) -> Result<Vec<Park>> {
    let (run, project) = (run.to_owned(), project_param(rt.project()));
    rt.control_transaction(move |mut tx| {
        let (run, project) = (run.clone(), project.clone());
        Box::pin(async move {
            let rows = tx
                .query_all(
                    &format!(
                        "{PARKS} WHERE p.run = ? AND {FOR_PROJECT} ORDER BY p.parked_at, p.park_id"
                    ),
                    &[Param::Text(run), project],
                )
                .await?;
            rows.iter().map(park_from).collect()
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
    let candidate = Arc::new(candidate.clone());
    let acceptance = Arc::new(acceptance.clone());
    let preconditions = Arc::new(preconditions);
    let step = Arc::new(name.to_owned());
    ctx.transaction(name, move |mut tx| {
        let candidate = candidate.clone();
        let acceptance = acceptance.clone();
        let preconditions = preconditions.clone();
        let step = step.clone();
        Box::pin(async move {
            publish_candidate_in(&mut tx, &step, &candidate, &acceptance, &preconditions).await
        })
    })
    .await
}

/// Publish inside the caller's durable transaction, so a graph workflow can
/// close its fenced item in the same commit as the candidate's writes and
/// publication record. The caller uses `step` as the transaction checkpoint
/// name. Any later error rolls back all of these writes together.
pub async fn publish_candidate_in(
    tx: &mut Tx<'_, '_>,
    step: &str,
    candidate: &Candidate,
    acceptance: &Acceptance,
    preconditions: &[Precondition],
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
    publish_in(tx, candidate, step, preconditions).await
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
    F: Fn() -> Fut + Send + Sync + 'static,
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
    F: Fn(StepInfo) -> Fut + Send + Sync + 'static,
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
    tx: &mut Tx<'_, '_>,
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
