// SPDX-License-Identifier: Apache-2.0

//! Gates: a person's sign-off between steps, durable across crashes.
//!
//! A gate is two things:
//!
//! - **A row in the gate store**, `atomic_gates` in atomic's database: the run
//!   and step it belongs to, its title, what it shows, the digests of its
//!   evidence, a proposal digest binding all three, its approvers, risk tier
//!   and deadline, and, once there is one, its resolution and who made it.
//!   The store needs no work graph.
//! - **A durable receive** in the workflow, which the decision completes.
//!
//! [`gate`] is the workflow side, three kinds of sibling operation and never
//! one inside another: a transaction step opens the row (once), a receive
//! waits for a wake-up, and a transaction step reads the resolution back
//! from the store. The receive is only a wake-up. What passes the gate is
//! the store row, read by this run's own gate id and checked against this
//! run and the proposal digest the workflow computes from what it shows, so
//! a recorded receive (forged, imported or copied from another run) cannot
//! approve anything. A completed decision checkpoint is revalidated against
//! the store outside checkpoint replay before the workflow can proceed.
//!
//! [`decide`] is the person's side: an idempotent command with a client-made
//! command id, committed in one short transaction before it is
//! acknowledged. It carries a [`HumanCredential`], checked against the
//! verifier the person enrolled (see [`credential`]): who decided is who
//! proved it, never a name the command gives. The first durable resolution wins; a replayed command gets
//! its original result back, and any other command on a resolved gate gets a
//! [`DecideError::Conflict`] carrying the resolution that won. Only after the
//! commit does it send the wake-up, keyed by the gate id so a repeat sends
//! nothing new; [`redeliver`] sends again for resolved gates whose runs are
//! still waiting, for a decider that stopped between the two.
//!
//! A deadline is a wall-clock time fixed when the gate opens. It runs on
//! while atomic is offline: a gate whose deadline has passed resolves as
//! expired the next time anything looks at it, before the workflow goes on,
//! and a decision that arrives after it gets the expiry as its conflict.

// A conflict carries the winning resolution by value, so callers can match
// on it; the error is large for it.
#![allow(clippy::result_large_err)]

pub mod approvers;
pub mod credential;

pub use approvers::{ApproverError, Approvers, Deployment, Principal, resolve};
pub use credential::{
    EnrollError, Enrollment, HumanCredential, enroll, enrolled, replace_enrollment,
};

use crate::failpoint;
use crate::runtime::{DurableError, DurableRuntime, Param, Result, Row, Tx, WorkflowContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A gate's risk tier. On a high-tier gate only a person can approve.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// A reviewer's verdict may approve.
    Low,
    /// Approval needs a person.
    #[default]
    High,
}

impl Tier {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            other => Err(corrupt(format!("unknown tier {other:?}"))),
        }
    }
}

/// What a gate's deadline does when it passes undecided.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnTimeout {
    /// The gate expires, which authorizes nothing: the run does not pass it.
    #[default]
    Reject,
    /// A question gate resolves with its question's default answer
    /// ([`Question::default`]).
    Default,
}

impl OnTimeout {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Default => "default",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "reject" => Ok(Self::Reject),
            "default" => Ok(Self::Default),
            other => Err(corrupt(format!("unknown on_timeout {other:?}"))),
        }
    }
}

/// What a question gate asks for instead of approval: a typed answer.
///
/// The answer is the decision. It must fit `answer`, a JSON Schema (for a
/// list of choices, an `enum`); with `remember`, it is also kept as a dated
/// entry in the feedback log ([`feedback`]). What happens when nobody
/// answers in time is the workflow's to say (an approval gate's
/// [`OnTimeout`] still applies to the store: the gate expires).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// The JSON Schema an answer must fit.
    pub answer: Value,
    /// Whether the answer goes into the feedback log.
    #[serde(default)]
    pub remember: bool,
    /// The answer silence selects: what the gate resolves with when its
    /// deadline passes, with [`OnTimeout::Default`]. `Some(null)` is a null
    /// default, not none.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub default: Option<Value>,
}

/// A field that is present, even as `null`, is `Some`.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

impl Question {
    /// `answer` if it fits, or why not.
    pub fn check(&self, answer: &Value) -> std::result::Result<(), String> {
        let validator = jsonschema::validator_for(&self.answer)
            .map_err(|e| format!("the question's answer schema is invalid: {e}"))?;
        let errors: Vec<String> = validator
            .iter_errors(answer)
            .map(|e| e.to_string())
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// The allowed choices, when the answer is one of a list.
    pub fn choices(&self) -> Option<&[Value]> {
        self.answer
            .get("enum")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
    }
}

/// A person's verdict on a gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// The run may pass the gate.
    Approve,
    /// It may not.
    Reject,
}

impl Verdict {
    fn state(self) -> &'static str {
        match self {
            Self::Approve => "approved",
            Self::Reject => "rejected",
        }
    }
}

/// A recorded decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// Approve or reject.
    pub verdict: Verdict,
    /// Who decided.
    pub by: Principal,
    /// What they said, if anything.
    pub comment: Option<String>,
    /// A question gate's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Value>,
    /// The command that recorded it.
    pub command: String,
    /// When, in milliseconds since the Unix epoch.
    pub at: i64,
}

/// How a gate was resolved: the first durable resolution, which never
/// changes afterwards.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resolution {
    /// A person decided.
    Decided(Decision),
    /// The deadline passed first.
    Expired {
        /// When the expiry was recorded, in milliseconds since the Unix epoch.
        at: i64,
        /// What the deadline did.
        on_timeout: OnTimeout,
    },
    /// The deadline passed on a question gate that names a default, so
    /// silence selected it.
    Defaulted {
        /// When, in milliseconds since the Unix epoch.
        at: i64,
        /// The question's default answer.
        answer: Value,
    },
}

impl Resolution {
    /// Whether it lets the run pass the gate.
    pub fn approved(&self) -> bool {
        matches!(
            self,
            Self::Decided(Decision {
                verdict: Verdict::Approve,
                ..
            })
        )
    }
}

/// What a workflow asks for when it opens a gate. Built by workflow code
/// from the workflow's definition; it cannot be deserialized, so a model's
/// output cannot become one, and its [`Approvers`] come only from
/// [`resolve`].
#[derive(Clone, Debug, PartialEq)]
pub struct GateSpec {
    /// The workflow's name.
    pub workflow: String,
    /// What the person is asked.
    pub title: String,
    /// What they are shown.
    pub show: Value,
    /// Content digests (`sha256:<hex>`) of the artifacts the gate shows as
    /// evidence.
    pub evidence: Vec<String>,
    /// Who may decide.
    pub approvers: Approvers,
    /// The risk tier.
    pub tier: Tier,
    /// How long the gate waits from when it opens, or `None` for no deadline.
    pub timeout: Option<Duration>,
    /// What the deadline does.
    pub on_timeout: OnTimeout,
    /// For a question gate, the answer it asks for.
    pub question: Option<Question>,
}

/// A gate as the store records it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    /// Its id, `<run>:<step>`.
    pub id: String,
    /// The run (workflow id) it belongs to.
    pub run: String,
    /// The workflow's name.
    pub workflow: String,
    /// The gate's step name in the workflow.
    pub step: String,
    /// What the person is asked.
    pub title: String,
    /// What they are shown.
    pub show: Value,
    /// Evidence digests.
    pub evidence: Vec<String>,
    /// The proposal digest: SHA-256 over the title, what is shown and the
    /// evidence digests. A decision names the digest it was made on.
    pub proposal: String,
    /// Who may decide.
    pub approvers: Vec<Principal>,
    /// The risk tier.
    pub tier: Tier,
    /// When it opened, in milliseconds since the Unix epoch.
    pub opened_at: i64,
    /// Its deadline, in milliseconds since the Unix epoch.
    pub deadline: Option<i64>,
    /// What the deadline does.
    pub on_timeout: OnTimeout,
    /// For a question gate, the answer it asks for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<Question>,
    /// How it was resolved, once it has been.
    pub resolution: Option<Resolution>,
}

/// A person's decision on a gate, sent as an idempotent command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionCommand {
    /// A command id the client made, unique per decision it means to send.
    /// Sending the same command again returns the same result; reusing the
    /// id for a different command is refused.
    pub command_id: String,
    /// The gate.
    pub gate: String,
    /// Approve or reject.
    pub verdict: Verdict,
    /// A comment, passed on with the decision.
    pub comment: Option<String>,
    /// The proposal digest of what the person was shown. A decision on
    /// anything else is refused.
    pub proposal: String,
    /// The answer to a question gate, which an approval of one needs; an
    /// approval gate takes none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Value>,
}

/// Why a decision was not recorded.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum DecideError {
    /// There is no such gate.
    #[error("no gate {0}")]
    NoSuchGate(String),
    /// The principal has no credential enrolled, so nothing it sends is a
    /// decision.
    #[error("{0} has no operator credential enrolled")]
    NotEnrolled(Principal),
    /// The answer does not fit the question, or the gate asks none.
    #[error("gate {gate}: {why}")]
    BadAnswer {
        /// The gate.
        gate: String,
        /// What is wrong with it.
        why: String,
    },
    /// The credential is not the one the principal enrolled.
    #[error("the credential presented for {0} is not the one it enrolled")]
    BadCredential(Principal),
    /// The person is not one of the gate's approvers.
    #[error("{who} is not an approver of gate {gate}")]
    NotAnApprover {
        /// The gate.
        gate: String,
        /// Who tried.
        who: Principal,
    },
    /// The command id was already used for a different command.
    #[error("command {0} was already used for a different decision")]
    CommandReused(String),
    /// The decision was made on a proposal the gate does not show.
    #[error("gate {gate} shows proposal {current}, not {shown}; look at it again")]
    ProposalChanged {
        /// The gate.
        gate: String,
        /// The proposal digest the command named.
        shown: String,
        /// The gate's.
        current: String,
    },
    /// The gate was already resolved, and not by this command.
    #[error("gate {gate} was already resolved")]
    Conflict {
        /// The gate.
        gate: String,
        /// The resolution that won.
        winning: Resolution,
    },
    /// The store failed; nothing was recorded.
    #[error(transparent)]
    Store(#[from] DurableError),
}

/// Which gates [`gates`] lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GateFilter {
    /// Only gates that are still open.
    pub open_only: bool,
    /// Only the gates of this run.
    pub run: Option<String>,
}

// ---------------------------------------------------------------------------
// Ids, digests and time.

/// The id of gate `step` in run `run`.
pub fn gate_id(run: &str, step: &str) -> String {
    format!("{run}:{step}")
}

/// The topic a gate's wake-up is sent on.
fn topic(gate: &str) -> String {
    format!("atomic.gate:{gate}")
}

/// The proposal digest of a gate showing `show` with `evidence` under
/// `title`: SHA-256 over their canonical JSON, keys sorted.
pub fn proposal_digest(title: &str, show: &Value, evidence: &[String]) -> String {
    let mut bytes = Vec::new();
    canonical(
        &json!({"title": title, "show": show, "evidence": evidence}),
        &mut bytes,
    );
    format!("sha256:{:x}", Sha256::digest(&bytes))
}

/// `value` as JSON with object keys in sorted order, whatever order the map
/// keeps them in.
fn canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend(Value::String(k.clone()).to_string().into_bytes());
                out.push(b':');
                canonical(&map[k], out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                canonical(v, out);
            }
            out.push(b']');
        }
        other => out.extend(other.to_string().into_bytes()),
    }
}

fn is_digest(s: &str) -> bool {
    s.strip_prefix("sha256:")
        // Lowercase only, as the artifact store names content, so a pin always
        // matches what it pins.
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Milliseconds since the Unix epoch, now.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// How long a wait with no deadline waits for: long enough to be forever.
const NO_DEADLINE: Duration = Duration::from_secs(10 * 365 * 24 * 3600);

fn corrupt(what: impl std::fmt::Display) -> DurableError {
    DurableError::Engine(format!("the gate store: {what}"))
}

fn encode(v: &impl Serialize) -> Result<String> {
    serde_json::to_string(v).map_err(corrupt)
}

fn decode<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(corrupt)
}

// ---------------------------------------------------------------------------
// Rows.

const COLUMNS: &str = "g.gate_id AS gate_id, g.run AS run, g.workflow AS workflow, \
                       g.step AS step, g.title AS title, g.show AS show, \
                       g.evidence AS evidence, g.proposal AS proposal, \
                       g.approvers AS approvers, g.tier AS tier, g.opened_at AS opened_at, \
                       g.deadline AS deadline, g.on_timeout AS on_timeout, g.state AS state, \
                       g.decided_by AS decided_by, g.decided_at AS decided_at, \
                       g.comment AS comment, g.command_id AS command_id, \
                       q.question AS question, q.answer AS answer";

/// The gates and their questions, if they ask one.
const GATES: &str = "atomic_gates g LEFT JOIN atomic_gate_questions q ON q.gate_id = g.gate_id";

fn gate_from_row(row: &Row) -> Result<Gate> {
    let state = row.text("state")?;
    let on_timeout = OnTimeout::parse(&row.text("on_timeout")?)?;
    let resolution = match state.as_str() {
        "open" => None,
        "expired" => Some(Resolution::Expired {
            at: row.int("decided_at")?,
            on_timeout,
        }),
        "defaulted" => Some(Resolution::Defaulted {
            at: row.int("decided_at")?,
            answer: decode(
                &row.text_opt("answer")?
                    .ok_or_else(|| corrupt("a defaulted gate with no answer"))?,
            )?,
        }),
        "approved" | "rejected" => Some(Resolution::Decided(Decision {
            verdict: if state == "approved" {
                Verdict::Approve
            } else {
                Verdict::Reject
            },
            by: Principal::stored(row.text("decided_by")?),
            comment: row.text_opt("comment")?,
            answer: row.text_opt("answer")?.map(|a| decode(&a)).transpose()?,
            command: row.text("command_id")?,
            at: row.int("decided_at")?,
        })),
        other => return Err(corrupt(format!("unknown gate state {other:?}"))),
    };
    Ok(Gate {
        id: row.text("gate_id")?,
        run: row.text("run")?,
        workflow: row.text("workflow")?,
        step: row.text("step")?,
        title: row.text("title")?,
        show: decode(&row.text("show")?)?,
        evidence: decode(&row.text("evidence")?)?,
        proposal: row.text("proposal")?,
        approvers: decode(&row.text("approvers")?)?,
        tier: Tier::parse(&row.text("tier")?)?,
        opened_at: row.int("opened_at")?,
        deadline: row.int_opt("deadline")?,
        on_timeout,
        question: row.text_opt("question")?.map(|q| decode(&q)).transpose()?,
        resolution,
    })
}

/// The gates with the project of each one's run, as `rp`, for
/// [`FOR_PROJECT`].
const SCOPED_GATES: &str = "atomic_gates g LEFT JOIN atomic_gate_questions q ON q.gate_id = g.gate_id \
     LEFT JOIN atomic_run_projects rp ON rp.run_id = g.run";

/// The condition on a [`SCOPED_GATES`] row that its run is of the project
/// its one parameter, [`project_param`], names: on a store several projects
/// share, no project lists, reads or decides another's gates.
const FOR_PROJECT: &str = "COALESCE(rp.project, '') = ?";

/// The parameter [`FOR_PROJECT`] takes for runtime `project`: a gate of a
/// run for no project is seen only by a runtime for none.
fn project_param(project: Option<&str>) -> Param {
    Param::Text(project.unwrap_or_default().to_owned())
}

/// Gate `id`, if its run is of the project `project` names.
async fn read_own_gate(tx: &mut Tx<'_, '_>, id: &str, project: &Param) -> Result<Option<Gate>> {
    let sql = format!("SELECT {COLUMNS} FROM {SCOPED_GATES} WHERE g.gate_id = ? AND {FOR_PROJECT}");
    match tx
        .query_opt(&sql, &[Param::Text(id.to_owned()), project.clone()])
        .await?
    {
        Some(row) => Ok(Some(gate_from_row(&row)?)),
        None => Ok(None),
    }
}

async fn read_gate(tx: &mut Tx<'_, '_>, id: &str) -> Result<Option<Gate>> {
    let sql = format!("SELECT {COLUMNS} FROM {GATES} WHERE g.gate_id = ?");
    match tx.query_opt(&sql, &[Param::Text(id.to_owned())]).await? {
        Some(row) => Ok(Some(gate_from_row(&row)?)),
        None => Ok(None),
    }
}

// The same identity/proposal fence applies inside the durable decision step
// and after replay. Never infer authorization from a copied step output.
async fn checked_gate(tx: &mut Tx<'_, '_>, id: &str, run: &str, proposal: &str) -> Result<Gate> {
    let gate = read_gate(tx, id).await?.ok_or_else(|| {
        DurableError::Failed(format!(
            "gate {id} is not in the gate store, so nothing approved it"
        ))
    })?;
    if gate.run != run {
        return Err(DurableError::Failed(format!(
            "gate {id} in the gate store belongs to run {}, not {run}",
            gate.run
        )));
    }
    if gate.proposal != proposal {
        return Err(DurableError::Failed(format!(
            "gate {id} was opened on proposal {}, but this run now shows {proposal}: changed content needs a new gate",
            gate.proposal
        )));
    }
    Ok(gate)
}

/// Record that `gate` expired at `now`, if it is still open. Returns the
/// gate as the store now has it.
async fn expire(tx: &mut Tx<'_, '_>, gate: &Gate, now: i64) -> Result<Gate> {
    let default = match (gate.on_timeout, &gate.question) {
        (OnTimeout::Default, Some(q)) => q.default.clone(),
        _ => None,
    };
    let state = if default.is_some() {
        "defaulted"
    } else {
        "expired"
    };
    let won = tx
        .execute(
            "UPDATE atomic_gates SET state = ?, decided_at = ? WHERE gate_id = ? AND state = 'open'",
            &[
                Param::Text(state.to_owned()),
                Param::Int(now),
                Param::Text(gate.id.clone()),
            ],
        )
        .await?;
    if won == 1
        && let (Some(answer), Some(q)) = (&default, &gate.question)
    {
        let answer = encode(answer)?;
        tx.execute(
            "UPDATE atomic_gate_questions SET answer = ? WHERE gate_id = ?",
            &[Param::Text(answer.clone()), Param::Text(gate.id.clone())],
        )
        .await?;
        if q.remember {
            remember(tx, gate, &answer, None, now).await?;
        }
    }
    read_gate(tx, &gate.id)
        .await?
        .ok_or_else(|| corrupt(format!("gate {} vanished", gate.id)))
}

/// Keep `answer` to `gate` in the feedback log, as given by `by`, or by
/// silence when `None`.
async fn remember(
    tx: &mut Tx<'_, '_>,
    gate: &Gate,
    answer: &str,
    by: Option<&Principal>,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO atomic_feedback (gate_id, workflow, step, ask, answer, decided_by, \
         recorded_at) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT (gate_id) DO NOTHING",
        &[
            Param::Text(gate.id.clone()),
            Param::Text(gate.workflow.clone()),
            Param::Text(gate.step.clone()),
            Param::Text(gate.title.clone()),
            Param::Text(answer.to_owned()),
            Param::Text(by.map(|b| b.as_str().to_owned()).unwrap_or_default()),
            Param::Int(now),
        ],
    )
    .await?;
    Ok(())
}

fn past_deadline(gate: &Gate, now: i64) -> bool {
    gate.resolution.is_none() && gate.deadline.is_some_and(|d| now >= d)
}

// ---------------------------------------------------------------------------
// The workflow side.

/// What the check after a wake-up found.
#[derive(Serialize, Deserialize)]
enum Check {
    /// Still open and not past its deadline: wait again.
    Open { deadline: Option<i64> },
    /// Resolved.
    Resolved(Resolution),
}

/// Open gate `step` in this run with `spec`, wait for it to be resolved, and
/// return the resolution. Only [`Resolution::approved`] lets the run pass;
/// a rejection, an expiry or a failure stops it.
///
/// Its operations, all siblings: `<step>.gate` opens the store row,
/// `<step>.wait` waits for the wake-up, `<step>.decision` reads the
/// resolution from the store; the last two repeat if a wake-up finds the gate
/// still open. A step name must be unique among the run's gates.
pub async fn gate<C: WorkflowContext>(ctx: &C, step: &str, spec: GateSpec) -> Result<Resolution> {
    if let Some(bad) = spec.evidence.iter().find(|e| !is_digest(e)) {
        return Err(DurableError::Failed(format!(
            "gate {step}: evidence {bad:?} is not a content digest (sha256:<hex>)"
        )));
    }
    let defaults = spec.question.as_ref().is_some_and(|q| q.default.is_some());
    if (spec.on_timeout == OnTimeout::Default) != defaults || (defaults && spec.timeout.is_none()) {
        return Err(DurableError::Failed(format!(
            "gate {step}: `on_timeout: default` goes with a question that names a default, and \
             a deadline for it"
        )));
    }
    let run = ctx.workflow_id().to_owned();
    let id = gate_id(&run, step);
    let proposal = match &spec.question {
        None => proposal_digest(&spec.title, &spec.show, &spec.evidence),
        // The question is part of what the person decides on.
        Some(q) => proposal_digest(
            &spec.title,
            &json!({"show": spec.show, "question": q}),
            &spec.evidence,
        ),
    };

    let (open_id, open_run, open_step, open_proposal) =
        (id.clone(), run.clone(), step.to_owned(), proposal.clone());
    let opened: Gate = ctx
        .transaction(&format!("{step}.gate"), move |mut tx| {
            let (id, run, step, proposal, spec) = (
                open_id.clone(),
                open_run.clone(),
                open_step.clone(),
                open_proposal.clone(),
                spec.clone(),
            );
            Box::pin(async move {
                if read_gate(&mut tx, &id).await?.is_some() {
                    return Err(DurableError::Failed(format!(
                        "gate {id} is already in the gate store: a gate's step name must be \
                         unique within its run"
                    )));
                }
                let now = now_ms();
                let deadline = spec
                    .timeout
                    .map(|t| now.saturating_add(i64::try_from(t.as_millis()).unwrap_or(i64::MAX)));
                // A NULL binds as text on PostgreSQL, hence the cast.
                tx.execute(
                    "INSERT INTO atomic_gates (gate_id, run, workflow, step, title, show, \
                     evidence, proposal, approvers, tier, opened_at, deadline, on_timeout, state) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CAST(? AS BIGINT), ?, 'open')",
                    &[
                        Param::Text(id.clone()),
                        Param::Text(run),
                        Param::Text(spec.workflow.clone()),
                        Param::Text(step),
                        Param::Text(spec.title.clone()),
                        Param::Text(encode(&spec.show)?),
                        Param::Text(encode(&spec.evidence)?),
                        Param::Text(proposal),
                        Param::Text(encode(&spec.approvers)?),
                        Param::Text(spec.tier.as_str().to_owned()),
                        Param::Int(now),
                        deadline.map_or(Param::Null, Param::Int),
                        Param::Text(spec.on_timeout.as_str().to_owned()),
                    ],
                )
                .await?;
                if let Some(q) = &spec.question {
                    tx.execute(
                        "INSERT INTO atomic_gate_questions (gate_id, question) VALUES (?, ?)",
                        &[Param::Text(id.clone()), Param::Text(encode(q)?)],
                    )
                    .await?;
                }
                read_gate(&mut tx, &id)
                    .await?
                    .ok_or_else(|| corrupt(format!("gate {id} was not written")))
            })
        })
        .await?;
    failpoint("gate:after-open");

    let mut deadline = opened.deadline;
    loop {
        let wait = match deadline {
            Some(d) => Duration::from_millis(u64::try_from(d - now_ms()).unwrap_or(0)),
            None => NO_DEADLINE,
        };
        // Only a wake-up: what it carries is never read.
        let _: Option<Value> = ctx.recv(&topic(&id), wait).await?;
        failpoint("gate:after-wait");

        let (check_id, check_run, check_proposal) = (id.clone(), run.clone(), proposal.clone());
        let check: Check = ctx
            .transaction(&format!("{step}.decision"), move |mut tx| {
                let (id, run, proposal) =
                    (check_id.clone(), check_run.clone(), check_proposal.clone());
                Box::pin(async move {
                    let gate = checked_gate(&mut tx, &id, &run, &proposal).await?;
                    let now = now_ms();
                    let gate = if past_deadline(&gate, now) {
                        expire(&mut tx, &gate, now).await?
                    } else {
                        gate
                    };
                    Ok(match gate.resolution {
                        Some(r) => Check::Resolved(r),
                        None => Check::Open {
                            deadline: gate.deadline,
                        },
                    })
                })
            })
            .await?;
        match check {
            Check::Resolved(_) => {
                // A transaction checkpoint can be copied or forged just like a
                // receive checkpoint. Recheck the authoritative row outside
                // every replayed operation, and return its actual resolution.
                let (id, run, proposal) = (id.clone(), run.clone(), proposal.clone());
                return ctx
                    .control_transaction(move |mut tx| {
                        let (id, run, proposal) = (id.clone(), run.clone(), proposal.clone());
                        Box::pin(async move {
                            checked_gate(&mut tx, &id, &run, &proposal)
                                .await?
                                .resolution
                                .ok_or_else(|| {
                                    DurableError::Failed(format!(
                                        "gate {id} is not resolved in the gate store"
                                    ))
                                })
                        })
                    })
                    .await;
            }
            Check::Open { deadline: d } => deadline = d,
        }
    }
}

// ---------------------------------------------------------------------------
// The person's side.

/// What a command's journal entry records.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Recorded {
    Decided(Decision),
    Conflict(Resolution),
}

impl Recorded {
    fn into_result(self, gate: &str) -> std::result::Result<Decision, DecideError> {
        match self {
            Self::Decided(d) => Ok(d),
            Self::Conflict(winning) => Err(DecideError::Conflict {
                gate: gate.to_owned(),
                winning,
            }),
        }
    }
}

/// The digest that identifies what a command asks for, so a reused id with
/// a different request is told apart from a replay.
fn request_digest(by: &Principal, cmd: &DecisionCommand) -> String {
    let mut bytes = Vec::new();
    let mut request = json!({
        "by": by,
        "gate": cmd.gate,
        "verdict": cmd.verdict,
        "comment": cmd.comment,
        "proposal": cmd.proposal,
    });
    // Only when there is one: a command journaled before question gates
    // keeps its digest, so its replay is still recognised.
    if let Some(answer) = &cmd.answer {
        request["answer"] = answer.clone();
    }
    canonical(&request, &mut bytes);
    format!("sha256:{:x}", Sha256::digest(&bytes))
}

async fn journaled(
    tx: &mut Tx<'_, '_>,
    command: &str,
    request: &str,
) -> Result<Option<std::result::Result<Recorded, DecideError>>> {
    let Some(row) = tx
        .query_opt(
            "SELECT request, result FROM atomic_gate_commands WHERE command_id = ?",
            &[Param::Text(command.to_owned())],
        )
        .await?
    else {
        return Ok(None);
    };
    if row.text("request")? != request {
        return Ok(Some(Err(DecideError::CommandReused(command.to_owned()))));
    }
    Ok(Some(Ok(decode(&row.text("result")?)?)))
}

/// Record the decision `cmd` of the person `credential` proves, durably,
/// and wake the gate's run.
///
/// The credential is checked against the gate store in the same
/// transaction: a principal with none enrolled, or a credential that does
/// not match, records nothing. Nothing in the command names who decides.
/// Returns the recorded decision, the same one for a replayed command, or
/// why it was not recorded.
pub async fn decide<R: DurableRuntime>(
    rt: &R,
    credential: &HumanCredential,
    cmd: DecisionCommand,
) -> std::result::Result<Decision, DecideError> {
    let by = credential.principal().clone();
    let request = request_digest(&by, &cmd);
    let gate_id = cmd.gate.clone();
    let credential = credential.clone();
    let project = project_param(rt.project());
    let (recorded, run) = rt
        .control_transaction(move |mut tx| {
            let (cmd, by, request, credential, project) = (
                cmd.clone(),
                by.clone(),
                request.clone(),
                credential.clone(),
                project.clone(),
            );
            Box::pin(async move {
                match credential::verify(&mut tx, &credential).await? {
                    credential::Verified::Yes => {}
                    credential::Verified::NotEnrolled => {
                        return Ok((Err(DecideError::NotEnrolled(by)), None));
                    }
                    credential::Verified::Mismatch => {
                        return Ok((Err(DecideError::BadCredential(by)), None));
                    }
                }
                // Another project's gate is, to this one, no gate at all:
                // checked before anything is read back of a decision on it.
                let Some(gate) = read_own_gate(&mut tx, &cmd.gate, &project).await? else {
                    return Ok((Err(DecideError::NoSuchGate(cmd.gate.clone())), None));
                };
                if let Some(found) = journaled(&mut tx, &cmd.command_id, &request).await? {
                    // A replay: wake the run again, in case the first time
                    // stopped before it did.
                    return Ok((found, Some(gate.run)));
                }
                if !gate.approvers.contains(&by) {
                    return Ok((
                        Err(DecideError::NotAnApprover {
                            gate: gate.id.clone(),
                            who: by.clone(),
                        }),
                        None,
                    ));
                }
                // A competing command may win its journal key after we have
                // updated this gate. Keep those writes reversible until the
                // command identity has also been admitted.
                tx.execute("SAVEPOINT atomic_gate_decision", &[]).await?;
                let now = now_ms();
                let gate = if past_deadline(&gate, now) {
                    expire(&mut tx, &gate, now).await?
                } else {
                    gate
                };
                let recorded = match gate.resolution {
                    Some(winning) => Recorded::Conflict(winning),
                    None if gate.proposal != cmd.proposal => {
                        return Ok((
                            Err(DecideError::ProposalChanged {
                                gate: gate.id.clone(),
                                shown: cmd.proposal.clone(),
                                current: gate.proposal.clone(),
                            }),
                            None,
                        ));
                    }
                    None => {
                        if let Err(why) = answer_fits(&gate, &cmd) {
                            return Ok((
                                Err(DecideError::BadAnswer {
                                    gate: gate.id.clone(),
                                    why,
                                }),
                                None,
                            ));
                        }
                        let answer = match &cmd.answer {
                            Some(a) if cmd.verdict == Verdict::Approve => Some(encode(a)?),
                            _ => None,
                        };
                        let won = tx
                            .execute(
                                "UPDATE atomic_gates SET state = ?, decided_by = ?, \
                                 decided_at = ?, comment = ?, command_id = ? \
                                 WHERE gate_id = ? AND state = 'open'",
                                &[
                                    Param::Text(cmd.verdict.state().to_owned()),
                                    Param::Text(by.as_str().to_owned()),
                                    Param::Int(now),
                                    cmd.comment.clone().map_or(Param::Null, Param::Text),
                                    Param::Text(cmd.command_id.clone()),
                                    Param::Text(gate.id.clone()),
                                ],
                            )
                            .await?;
                        if won == 1
                            && let Some(answer) = &answer
                        {
                            tx.execute(
                                "UPDATE atomic_gate_questions SET answer = ? WHERE gate_id = ?",
                                &[Param::Text(answer.clone()), Param::Text(gate.id.clone())],
                            )
                            .await?;
                        }
                        if won == 1
                            && let (Some(q), Some(answer)) = (&gate.question, &answer)
                            && q.remember
                        {
                            // In the decision's own transaction: remembered
                            // exactly when it is decided.
                            remember(&mut tx, &gate, answer, Some(&by), now).await?;
                        }
                        let now_stored = read_gate(&mut tx, &gate.id)
                            .await?
                            .ok_or_else(|| corrupt(format!("gate {} vanished", gate.id)))?;
                        match now_stored.resolution {
                            Some(Resolution::Decided(d)) if won == 1 => Recorded::Decided(d),
                            Some(winning) => Recorded::Conflict(winning),
                            None => return Err(corrupt(format!("gate {} stayed open", gate.id))),
                        }
                    }
                };
                let inserted = tx
                    .execute(
                        "INSERT INTO atomic_gate_commands \
                         (command_id, gate_id, request, result, recorded_at) \
                         VALUES (?, ?, ?, ?, ?) ON CONFLICT (command_id) DO NOTHING",
                        &[
                            Param::Text(cmd.command_id.clone()),
                            Param::Text(gate.id.clone()),
                            Param::Text(request.clone()),
                            Param::Text(encode(&recorded)?),
                            Param::Int(now),
                        ],
                    )
                    .await?;
                if inserted == 0 {
                    // Undo this transaction's gate changes before returning
                    // the winner (or refusing a different request on its key).
                    tx.execute("ROLLBACK TO SAVEPOINT atomic_gate_decision", &[])
                        .await?;
                    let found = journaled(&mut tx, &cmd.command_id, &request)
                        .await?
                        .ok_or_else(|| corrupt("a conflicting command journal entry vanished"))?;
                    return Ok((found, Some(gate.run)));
                }
                Ok((Ok(recorded), Some(gate.run)))
            })
        })
        .await?;
    let recorded = recorded?;
    if let Some(run) = run {
        // The decision is durable; the wake-up is not part of it. If it
        // fails, `redeliver`, or the waiting run's own deadline, catches up.
        let _ = wake(rt, &run, &gate_id).await;
    }
    recorded.into_result(&gate_id)
}

/// Whether `cmd` answers `gate` as it must: an approval of a question gate
/// carries an answer that fits it, and nothing else carries one.
fn answer_fits(gate: &Gate, cmd: &DecisionCommand) -> std::result::Result<(), String> {
    match (&gate.question, &cmd.answer, cmd.verdict) {
        (None, None, _) | (Some(_), None, Verdict::Reject) => Ok(()),
        (None, Some(_), _) => Err("it asks no question, so it takes no answer".into()),
        (Some(_), Some(_), Verdict::Reject) => {
            Err("a rejection gives no answer; reject it without one".into())
        }
        (Some(_), None, Verdict::Approve) => Err("it asks a question: answer it".into()),
        (Some(q), Some(a), Verdict::Approve) => q
            .check(a)
            .map_err(|why| format!("{a} does not answer it: {why}")),
    }
}

/// One remembered answer in the feedback log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback {
    /// The question gate.
    pub gate: String,
    /// Its workflow.
    pub workflow: String,
    /// Its step.
    pub step: String,
    /// What it asked.
    pub ask: String,
    /// The answer.
    pub answer: Value,
    /// Who answered, or `None` when silence selected the default.
    pub by: Option<Principal>,
    /// When, in milliseconds since the Unix epoch.
    pub at: i64,
}

/// The feedback log, oldest first.
pub async fn feedback<R: DurableRuntime>(rt: &R) -> Result<Vec<Feedback>> {
    rt.control_transaction(|mut tx| {
        Box::pin(async move {
            tx.query_all(
                "SELECT gate_id, workflow, step, ask, answer, decided_by, recorded_at \
                 FROM atomic_feedback ORDER BY recorded_at, gate_id",
                &[],
            )
            .await?
            .iter()
            .map(|row| {
                Ok(Feedback {
                    gate: row.text("gate_id")?,
                    workflow: row.text("workflow")?,
                    step: row.text("step")?,
                    ask: row.text("ask")?,
                    answer: decode(&row.text("answer")?)?,
                    by: Some(row.text("decided_by")?)
                        .filter(|b| !b.is_empty())
                        .map(Principal::stored),
                    at: row.int("recorded_at")?,
                })
            })
            .collect()
        })
    })
    .await
}

async fn wake<R: DurableRuntime>(rt: &R, run: &str, gate: &str) -> Result<()> {
    failpoint("gate:before-wake");
    rt.send(run, &topic(gate), json!({ "gate": gate }), gate)
        .await
}

/// Every gate the filter selects, oldest first.
pub async fn gates<R: DurableRuntime>(rt: &R, filter: GateFilter) -> Result<Vec<Gate>> {
    let project = project_param(rt.project());
    rt.read_transaction(move |mut tx| {
        let (filter, project) = (filter.clone(), project.clone());
        Box::pin(async move {
            // Only the runtime's project's gates.
            let mut sql = format!("SELECT {COLUMNS} FROM {SCOPED_GATES} WHERE {FOR_PROJECT}");
            let mut ps = vec![project];
            if filter.open_only {
                sql.push_str(" AND g.state = 'open'");
            }
            if let Some(run) = filter.run {
                sql.push_str(" AND g.run = ?");
                ps.push(Param::Text(run));
            }
            sql.push_str(" ORDER BY g.opened_at, g.gate_id");
            tx.query_all(&sql, &ps)
                .await?
                .iter()
                .map(gate_from_row)
                .collect()
        })
    })
    .await
}

/// One gate, if it exists and its run is of the runtime's project.
pub async fn find<R: DurableRuntime>(rt: &R, id: &str) -> Result<Option<Gate>> {
    let (id, project) = (id.to_owned(), project_param(rt.project()));
    rt.control_transaction(move |mut tx| {
        let (id, project) = (id.clone(), project.clone());
        Box::pin(async move { read_own_gate(&mut tx, &id, &project).await })
    })
    .await
}

/// Send the wake-up again for every resolved gate whose run is still
/// unfinished. Run it when a runtime starts or resumes runs: a decider that
/// stopped after committing its decision but before waking the run leaves
/// that run waiting until this, or its own deadline. Returns how many it
/// sent; a repeat for a run that already woke changes nothing.
pub async fn redeliver<R: DurableRuntime>(rt: &R) -> Result<usize> {
    // Only resolved gates of unfinished runs, chosen in one query: a
    // server asks every few seconds, and finished runs' gates pile up.
    // Only the runtime's project's runs.
    let project = project_param(rt.project());
    let waiting: Vec<(String, String)> = rt
        .control_transaction(move |mut tx| {
            let project = project.clone();
            Box::pin(async move {
                let rows = tx
                    .query_all(
                        &format!(
                            "SELECT g.gate_id AS gate_id, g.run AS run FROM atomic_gates g \
                             JOIN workflow_status w ON w.workflow_uuid = g.run \
                             LEFT JOIN atomic_run_projects rp ON rp.run_id = g.run \
                             WHERE g.state <> 'open' AND w.status IN ('PENDING', 'ENQUEUED') \
                             AND {FOR_PROJECT}"
                        ),
                        &[project],
                    )
                    .await?;
                rows.iter()
                    .map(|r| Ok((r.text("gate_id")?, r.text("run")?)))
                    .collect()
            })
        })
        .await?;
    for (gate, run) in &waiting {
        wake(rt, run, gate).await?;
    }
    Ok(waiting.len())
}

/// Every evidence digest a gate in the store refers to. Retention keeps
/// this content for as long as the gate's decision and its run's recovery
/// may need it.
pub async fn pinned_evidence<R: DurableRuntime>(rt: &R) -> Result<Vec<String>> {
    // Every project's gates: a store's content is kept for all of them.
    let mut all: Vec<String> = rt
        .read_transaction(|mut tx| {
            Box::pin(async move {
                let rows = tx
                    .query_all(&format!("SELECT {COLUMNS} FROM {GATES}"), &[])
                    .await?;
                rows.iter()
                    .map(gate_from_row)
                    .collect::<Result<Vec<Gate>>>()
            })
        })
        .await?
        .into_iter()
        .flat_map(|g| g.evidence)
        .collect();
    all.sort();
    all.dedup();
    Ok(all)
}

#[cfg(test)]
mod tests;
