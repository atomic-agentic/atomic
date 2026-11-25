// SPDX-License-Identifier: Apache-2.0

//! The work-graph store API that atomic's M5 conformance suite expects.
//!
//! This is the production contract shared by SQLite and future backends.
//! The conformance suite imports these types directly and runs every scenario
//! against a persistent SQLite store.
//!
//! It is deliberately narrow. Readiness, claiming, leasing and closing are
//! store operations; claiming, heartbeats and releases stay orchestration
//! operations that a model never gets as tools
//! ([`docs/design/work-graph.md`](../../../docs/design/work-graph.md) § Claims
//! and leases). Nothing here takes SQL, a connection or a backend's own types.
//!
//! Time is not a parameter of any operation: a store reads its recorded clock,
//! and the suite hands that clock to the store it builds. `docs/development.md`
//! is the authority on which clock a step's store sees; the suite's clock is
//! the harness `Clock` the store was constructed with, so lease expiry is a
//! deterministic assertion and not a sleep.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::time::Duration;
use std::time::SystemTime;

use atomic_core::workflow::Criterion;

/// Who a caller claims is acting on the graph. This value grants no human
/// authority on its own: the SQLite store derives the real principal and
/// human trust from an enrolled credential verified inside the write
/// transaction. A plain store refuses a forged `human` actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// Who the store records as the author of what this actor writes.
    pub principal: String,
    /// Whether the credential behind this call belongs to a person.
    pub human: bool,
}

impl Actor {
    /// Request a person's action. The store still requires a current enrolled
    /// credential; this constructor alone grants nothing.
    pub fn human(principal: &str) -> Self {
        Self {
            principal: principal.to_string(),
            human: true,
        }
    }

    /// A model, an atom or an import: may write, may not set policy, and its
    /// text never enters a prompt on its own.
    pub fn agent(principal: &str) -> Self {
        Self {
            principal: principal.to_string(),
            human: false,
        }
    }
}

/// Where stored text came from, highest first.
///
/// Only `Human` and `Approved` text is placed in a prompt without being asked
/// for, and nothing written by an agent or brought in by an import can promote
/// itself to a higher level (§ Who wrote it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trust {
    /// Written by a person.
    Human,
    /// Written by an agent and approved by a person.
    Approved,
    /// Written by an agent.
    Agent,
    /// Brought in from outside: an import, a message from another machine.
    External,
}

impl Trust {
    /// The name the store keeps.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Approved => "approved",
            Self::Agent => "agent",
            Self::External => "external",
        }
    }
}

impl fmt::Display for Trust {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where an item is. The set is beads'; see
/// [`docs/design/work-graph.md`](../../../docs/design/work-graph.md) § Items
/// and edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Ready to be taken, once nothing blocking it is open.
    Open,
    /// Claimed, or being worked on.
    InProgress,
    /// A person has held it back.
    Blocked,
    /// Pushed aside without an edge holding it.
    Deferred,
    /// Finished.
    Closed,
    /// Never ready: a goal, a pinned thread root.
    Pinned,
}

impl Status {
    /// The name the store keeps.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Deferred => "deferred",
            Self::Closed => "closed",
            Self::Pinned => "pinned",
        }
    }

    /// The status a name means, or `None` for a name no status has. An import
    /// meets names it did not pick — beads' `pinned` is a flag and not a
    /// status — and an unknown one refuses the line rather than becoming a
    /// default status.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "open" => Some(Self::Open),
            "in_progress" => Some(Self::InProgress),
            "blocked" => Some(Self::Blocked),
            "deferred" => Some(Self::Deferred),
            "closed" => Some(Self::Closed),
            "pinned" => Some(Self::Pinned),
            _ => None,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An edge between two items. Four kinds decide readiness; the rest are links
/// that never block (§ Items and edges).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// The target holds `from` back until it is closed.
    Blocks,
    /// `from` is a child of the target; a child's epic holds it.
    ParentChild,
    /// Holds `from` back only while the target is not closed with a failure.
    ConditionalBlocks,
    /// Open until the children created at run time are all closed.
    WaitFor,
    /// A link.
    Related,
    /// `from` was discovered while working the target.
    DiscoveredFrom,
    /// A link that keeps review comments next to the work.
    RelatesTo,
    /// A link.
    Duplicates,
    /// A link.
    Supersedes,
    /// A message thread: `from` answers the target.
    RepliesTo,
}

impl EdgeKind {
    /// Whether this kind can hold an item back from `ready`.
    pub fn blocks_readiness(self) -> bool {
        matches!(
            self,
            Self::Blocks | Self::ParentChild | Self::ConditionalBlocks | Self::WaitFor
        )
    }

    /// The name the store keeps, and beads' import maps onto it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocks => "blocks",
            Self::ParentChild => "parent-child",
            Self::ConditionalBlocks => "conditional-blocks",
            Self::WaitFor => "waits-for",
            Self::Related => "related",
            Self::DiscoveredFrom => "discovered-from",
            Self::RelatesTo => "relates-to",
            Self::Duplicates => "duplicates",
            Self::Supersedes => "supersedes",
            Self::RepliesTo => "replies-to",
        }
    }
}

impl fmt::Display for EdgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a caller asks to store. `create_item` turns this into an [`Item`],
/// stamping `created_at` from the store's clock and `trust` from the actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// The item's id, as the caller wants it: short hash, with a suffix for a
    /// child (`at-a1b2`, `at-a1b2.1`).
    pub id: String,
    /// One-line summary.
    pub title: String,
    /// The body: design notes, acceptance criteria.
    pub body: String,
    /// `task`, `epic`, `bug`, `message`, `goal`, …
    pub kind: String,
    /// Lower sorts first in `claim_next`.
    pub priority: i32,
    /// The status to start in. Direct `create_item` refuses `Closed`; import
    /// maps historical closed records separately with external trust.
    pub status: Status,
    /// `parent-child` to this item, if any.
    pub parent: Option<String>,
    /// Ready no earlier than this, even with every dependency closed.
    pub due_at: Option<SystemTime>,
    /// Notes to store with the item, oldest first.
    ///
    /// Their author and trust come from whoever created the item, not from the
    /// text: an import that arrives with per-comment authors keeps those counts
    /// in [`Report::kept`] and still writes the notes as `external`.
    pub notes: Vec<String>,
}

impl Draft {
    /// A plain open task of normal priority, which is what most of the lease
    /// and close-policy scenarios start from.
    pub fn new(id: &str, title: &str) -> Self {
        Self {
            id: id.to_string(),
            title: title.to_string(),
            body: String::new(),
            kind: "task".to_string(),
            priority: 2,
            status: Status::Open,
            parent: None,
            due_at: None,
            notes: Vec::new(),
        }
    }
}

/// A stored item, as the store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The item's id.
    pub id: String,
    /// One-line summary.
    pub title: String,
    /// The body.
    pub body: String,
    /// The kind.
    pub kind: String,
    /// Priority for `claim_next`.
    pub priority: i32,
    /// Where it is.
    pub status: Status,
    /// The parent, if it has one.
    pub parent: Option<String>,
    /// Where its text came from.
    pub trust: Trust,
    /// Who created it.
    pub created_by: String,
    /// When it was created, on the store's clock.
    pub created_at: SystemTime,
    /// When it closed, on the store's clock, if it has.
    pub closed_at: Option<SystemTime>,
    /// Not ready before this.
    pub due_at: Option<SystemTime>,
}

/// A note on an item, with where it came from.
///
/// Every piece of stored text says where it came from, because a brief may only
/// carry a person's text unasked for (§ Who wrote it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// The text, as stored.
    pub text: String,
    /// The principal that wrote it.
    pub author: String,
    /// Where it came from: `human` and `approved` may enter a brief unasked
    /// for, `agent` and `external` may not.
    pub trust: Trust,
}

/// Who holds an item now, if anybody does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// The claimant the lease was taken for.
    pub claimant: String,
    /// The epoch the store will honour writes at, which is what a stale lease
    /// is compared against.
    pub epoch: i64,
}

/// An item with what the store knows around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Show {
    /// The item.
    pub item: Item,
    /// Its notes, oldest first.
    pub notes: Vec<Note>,
    /// The ids of the items whose edges hold this one back, sorted. Only edges
    /// that block readiness appear here.
    pub blocked_by: Vec<String>,
    /// Who holds it, or `None` when nothing does — which is how a check sees a
    /// release, a sweep and a takeover without reading a lease it may no longer
    /// have.
    pub holder: Option<Holder>,
}

/// What a successful claim hands out: the token every later write is checked
/// against (§ Claims and leases). Orchestration commits this with the
/// transaction-step checkpoint and gives the atom an opaque reference to it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Lease {
    /// The item held.
    pub item_id: String,
    /// The claimant this claim was made for. Retrying a claim for the same
    /// claimant is a retry, not a new holder.
    pub claimant: String,
    /// The store's own handle for this holding, opaque to callers.
    pub session: u64,
    /// The claim epoch. Each claim that takes an item raises it, so a holder
    /// whose lease lapsed cannot write after someone else has taken over.
    pub epoch: i64,
    /// The acquisition id: stable across a crash, so recovery gets back the
    /// same claim instead of making a new one.
    pub acquisition: String,
}

/// What a holder does at the end of its turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// Done, with evidence. The tenant's close policy decides what evidence
    /// this needs (§ M5 close policies).
    Close,
    /// The held work reached a terminal failure. This releases compensation
    /// waiting on a `conditional-blocks` edge, without claiming success.
    Fail,
    /// Given back unfinished, with a handoff note (M6). The item goes back to
    /// the queue.
    Release,
}

impl fmt::Display for Finish {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Close => f.write_str("close"),
            Self::Fail => f.write_str("fail"),
            Self::Release => f.write_str("release"),
        }
    }
}

/// Something a close policy can require. The set is closed on purpose: a
/// policy that names anything else must be refused even by a person, so that
/// an unenforceable requirement cannot be written and then silently ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// A commit linked to the item itself, as atomic's worktree step reported
    /// it. Not the session's commit, and not a commit the atom only claims to
    /// have made.
    Commit,
    /// A commit atomic has seen in its own index that a branch reaches.
    VerifiedCommit,
}

/// Which stored rule determines an item's effective close requirements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosePolicySource {
    /// A human explicitly set a rule for this item.
    Item,
    /// The earlier database-wide rule applies because this item has no rule.
    LegacyGlobal,
}

/// The close requirements currently applied to one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemClosePolicy {
    /// Evidence a successful close must supply.
    pub requires: Vec<Evidence>,
    /// Full branch ref for verified-commit evidence, when required.
    pub authorized_branch: Option<String>,
    /// Whether the item rule or legacy global rule supplies the requirements.
    pub source: ClosePolicySource,
    /// Credential-derived principal that set an item rule; absent for legacy.
    pub set_by: Option<String>,
}

/// Human-authored executable criteria for one graph item. These are stored
/// separately from imported Beads acceptance prose, which is never executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemCriteria {
    /// Bounded structured criteria, in the order the operator set them.
    pub criteria: Vec<Criterion>,
    /// Credential-derived principal that set the criteria.
    pub set_by: String,
}

impl Evidence {
    /// The name the store keeps.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::VerifiedCommit => "verified_commit",
        }
    }
}

impl fmt::Display for Evidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A memory: a short note kept outside the items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    /// What it is keyed by.
    pub key: String,
    /// The text.
    pub value: String,
    /// Where it came from. Only `Human` and `Approved` memories reach a prompt
    /// unasked for.
    pub trust: Trust,
}

/// Why an import left an item it already had exactly as it was.
///
/// A reason the importer answers with, rather than prose a test would have to
/// string-match: the two cases mean different things to whoever ran the import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftAlone {
    /// The store already had this id from an earlier import, and the text in
    /// this file differs from it.
    ImportedBeforeFromDifferentText,
    /// atomic made this item itself. An import never overwrites it.
    MadeInAtomic,
}

impl fmt::Display for LeftAlone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ImportedBeforeFromDifferentText => {
                f.write_str("imported before from different text")
            }
            Self::MadeInAtomic => {
                f.write_str("atomic has an item with this id that was not imported")
            }
        }
    }
}

/// What one import did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Lines mapped, that is items in the file.
    pub items: usize,
    /// Ids the import wrote that were not there before.
    pub added: Vec<String>,
    /// Ids already there with exactly this text, so left alone.
    pub unchanged: Vec<String>,
    /// Ids already there with different text, each with why it was left as
    /// is. The import never overwrites.
    pub left_as_is: Vec<(String, LeftAlone)>,
    /// Edges that decide readiness, added.
    pub dependencies_added: usize,
    /// Parents set.
    pub parents_set: usize,
    /// Notes added.
    pub notes_added: usize,
    /// Items that came in already claimed, which the import leaves claimable.
    pub in_progress: usize,
    /// Items written with trust `external`, whatever the file asked for.
    pub external: usize,
    /// Fields with no column of their own, kept with the item, counted by
    /// name: `labels`, `relations`, `external_ref`, …
    pub kept: BTreeMap<String, usize>,
}

/// Why an operation did not happen.
///
/// Every variant names what it refused, so a test can assert the reason and
/// not just the failure: an error that a missing value was substituted for is
/// as much a defect as the refusal that never came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// A database or persisted-data failure.
    Backend(String),
    /// SQLite could not prove that a transaction committed; the caller must reconcile before retrying.
    OutcomeUnknown {
        /// The transaction phase whose outcome needs reconciliation.
        operation: &'static str,
    },
    /// An edge would break the graph's parent or endpoint rules.
    InvalidEdge {
        /// Why the edge violates graph consistency.
        detail: String,
    },
    /// A request id was previously used for another operation or target.
    RequestConflict {
        /// The id that cannot be reused for this write.
        request_id: String,
    },
    /// A lease duration cannot be represented safely by the stored clock.
    InvalidDuration,
    /// Direct creation cannot make a terminal item without a fenced close.
    /// Historical closed items enter through import with external trust.
    CannotCreateClosed {
        /// The draft item that was refused before any row or request ID write.
        item_id: String,
    },
    /// This backend has no implementation for `operation`. This is what a
    /// store that does not exist yet returns, and what the conformance suite
    /// expects before `atomic-graph` grows its SQLite backend.
    MissingImplementation {
        /// The operation refused, by its [`GraphStore`] method name.
        operation: &'static str,
    },
    /// The lease no longer matches what the store holds: the epoch moved, the
    /// session is not the holder's, or the item is no longer `in_progress`.
    /// Nothing was written.
    Fenced {
        /// The item the write named.
        item_id: String,
        /// The epoch the store holds for that item now, which is not the one
        /// the caller presented.
        epoch: i64,
    },
    /// The item is not in the store, so a lease naming it names nothing.
    MissingItem {
        /// The item the lease named.
        item_id: String,
    },
    /// The action needs a human credential and the actor has none.
    NeedsHumanCredential {
        /// The action refused.
        action: String,
    },
    /// The requested graph run cannot be admitted to the epic.
    EpicAdmission {
        /// The refusal and the action needed to make the epic runnable.
        detail: String,
    },
    /// An operator requested a status transition the graph does not allow.
    InvalidStatusTransition {
        /// The refused item and status change.
        detail: String,
    },
    /// A person proposed a close policy whose branch or evidence combination
    /// cannot be checked. Nothing was written.
    InvalidClosePolicy {
        /// What made the policy invalid.
        detail: String,
    },
    /// An operator's executable criteria were not valid under the runtime
    /// criterion contract; no row was changed.
    InvalidCriteria {
        /// The violated bound or shape.
        detail: String,
    },
    /// A goal definition or goal-state operation was refused before writing.
    InvalidGoal {
        /// Why the goal operation is invalid.
        detail: String,
    },
    /// The close policy refuses to close the item yet.
    NeedsEvidence {
        /// The item that stayed held.
        item_id: String,
        /// What is still missing.
        required: String,
    },
    /// The whole file is refused and nothing was written, because a line could
    /// not be mapped or a reference pointed at nothing. Each problem in turn.
    ImportRefused(Vec<String>),
    /// A dry run: the report says what the import would do and nothing was
    /// written. Boxed because a report is much larger than a refusal, and every
    /// store answer carries this error.
    ImportDryRun(Box<Report>),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(message) => write!(f, "graph backend: {message}"),
            Self::OutcomeUnknown { operation } => write!(f, "graph outcome unknown: {operation}"),
            Self::InvalidEdge { detail } => write!(f, "invalid graph edge: {detail}"),
            Self::RequestConflict { request_id } => write!(
                f,
                "request id {request_id} was already used for a different write"
            ),
            Self::InvalidDuration => {
                f.write_str("lease duration is zero or exceeds the stored clock range")
            }
            Self::CannotCreateClosed { item_id } => {
                write!(
                    f,
                    "direct creation of closed item {item_id} requires a later close"
                )
            }
            Self::MissingImplementation { operation } => {
                write!(f, "store has no implementation: {operation}")
            }
            Self::Fenced { item_id, epoch } => {
                write!(f, "claim epoch {epoch} is not current for {item_id}")
            }
            Self::MissingItem { item_id } => write!(f, "no such item: {item_id}"),
            Self::NeedsHumanCredential { action } => {
                write!(f, "only a human credential may: {action}")
            }
            Self::EpicAdmission { detail } => write!(f, "graph epic admission refused: {detail}"),
            Self::InvalidStatusTransition { detail } => {
                write!(f, "invalid graph status transition: {detail}")
            }
            Self::InvalidClosePolicy { detail } => write!(f, "invalid close policy: {detail}"),
            Self::InvalidCriteria { detail } => write!(f, "invalid item criteria: {detail}"),
            Self::InvalidGoal { detail } => write!(f, "invalid goal: {detail}"),
            Self::NeedsEvidence { item_id, required } => {
                write!(f, "{item_id} stays held until it has {required}")
            }
            Self::ImportRefused(problems) => {
                write!(f, "import refused: {}", problems.join("; "))
            }
            Self::ImportDryRun(report) => {
                write!(
                    f,
                    "dry run: {} added, {} unchanged, {} left as is",
                    report.added.len(),
                    report.unchanged.len(),
                    report.left_as_is.len()
                )
            }
        }
    }
}

impl Error for StoreError {}

/// The work graph's store: what every backend must answer, and what the
/// conformance suite runs against each one.
///
/// Rules the suite checks, all from
/// [`docs/design/work-graph.md`](../../../docs/design/work-graph.md):
///
/// - Claiming is one transaction that only succeeds when nobody holds the item
///   or the holder's lease has run out, so two claimants can never both win.
/// - Each claim that takes an item raises its epoch, and every later write is
///   checked against that epoch.
/// - Liveness comes from heartbeats, not from a connection staying open, and a
///   lapsed lease nobody has taken over still works for its holder.
/// - Lapsed items go back to the queue as part of the claim query, with no
///   separate timer; [`GraphStore::sweep`] is the operation that does it on
///   demand.
/// - Creating an item, a note or a memory carries a request id, so repeating
///   the call is harmless.
/// - Imported text is `external` text and cannot grant itself trust.
/// - A close policy is set only by a human credential, and refusing a close
///   leaves the item held rather than half-closed.
///
/// # Missing implementations
///
/// A backend that has not implemented an operation returns
/// [`StoreError::MissingImplementation`] naming that operation. It must not
/// answer `Ok` with an empty result instead: the suite treats an empty result
/// as a decision the store made, and checks it against what should have
/// happened.
pub trait GraphStore: Send + Sync {
    /// Store a new item built from `draft`, written with `actor`'s trust.
    /// Repeating the call with the same `request_id` stores it once and
    /// returns the same item.
    fn create_item(
        &self,
        actor: Actor,
        request_id: String,
        draft: Draft,
    ) -> impl Future<Output = Result<Item, StoreError>> + Send;

    /// Add a note to a held item, as its holder. Fenced for any other lease.
    /// The note keeps `actor` as its author and `actor`'s trust; `request_id`
    /// makes a retry harmless.
    fn note(
        &self,
        actor: Actor,
        lease: Lease,
        request_id: String,
        text: String,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Add an edge of `kind` from `from` to `to`. A link never blocks; an edge
    /// that blocks readiness must show up in `ready` and in
    /// [`Show::blocked_by`].
    fn add_edge(
        &self,
        actor: Actor,
        from: String,
        kind: EdgeKind,
        to: String,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Move an item to `status` from outside a claim, as a person's `atomic
    /// graph` command does when they put work aside or reopen it. Closing
    /// checks the same tenant evidence policy as [`Self::finish`]; a close
    /// without required evidence is refused. Every accepted administrative
    /// change leaves the former holder fenced. An administrative close has
    /// unknown terminal outcome, so it cannot trigger conditional compensation.
    fn set_status(
        &self,
        actor: Actor,
        id: String,
        status: Status,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// The item with its notes and what holds it back, or `None` if there is
    /// no such item.
    fn show(
        &self,
        actor: Actor,
        id: String,
    ) -> impl Future<Output = Result<Option<Show>, StoreError>> + Send;

    /// Every item in the store whatever its status, sorted by id: what `export`
    /// walks, and what a check compares against to say that an import or a
    /// refused write changed nothing at all.
    fn all_items(&self, actor: Actor)
    -> impl Future<Output = Result<Vec<Item>, StoreError>> + Send;

    /// Items ready to be taken, best first, at most `limit`: `open`, after
    /// ordering blockers close and conditional compensation targets close
    /// with a recorded failure, and not before `due_at`. A `pinned` item and
    /// a `goal` never appear.
    fn ready(
        &self,
        actor: Actor,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<Item>, StoreError>> + Send;

    /// Claim `id` for `claimant` for `lease_for`, or `None` if it cannot be
    /// taken: held, blocked, closed, missing, deferred, or not yet due. A
    /// credentialed human claim derives `claimant` from the credential.
    ///
    /// Claiming again with the same `claimant` while the lease is still
    /// current, or has lapsed with nobody else taking it, is the same claim:
    /// same epoch, same acquisition, and no second claim event. A claim that
    /// takes an item over raises the epoch.
    fn claim(
        &self,
        actor: Actor,
        claimant: String,
        id: String,
        lease_for: Duration,
    ) -> impl Future<Output = Result<Option<Lease>, StoreError>> + Send;

    /// Take the best ready item, as `claim` does, or `None`. A lapsed item
    /// goes back to the queue here, so an empty answer means nothing was
    /// free at this moment and the caller tries again later.
    fn claim_next(
        &self,
        actor: Actor,
        claimant: String,
        lease_for: Duration,
    ) -> impl Future<Output = Result<Option<Lease>, StoreError>> + Send;

    /// Extend the lease, keeping the epoch. Fenced once someone else holds the
    /// item; still accepted for a lapsed lease nobody has taken over.
    fn heartbeat(
        &self,
        lease: Lease,
        lease_for: Duration,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Close successfully, close with terminal failure, or give it back. Under
    /// the same lease and epoch, a
    /// repeated `Finish::Close` or `Finish::Fail` with the same outcome on a
    /// closed item succeeds and changes nothing; any other finish is fenced.
    /// A successful close the tenant's policy refuses leaves the item
    /// `in_progress`, so the holder can still release it. A terminal failure
    /// records the outcome even when success evidence is absent.
    fn finish(
        &self,
        lease: Lease,
        done: Finish,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Return every item whose lease has run out and nobody has taken over,
    /// raising each one's epoch so its old holder is fenced, and list the ids.
    /// An item still held is untouched, and a second sweep finds nothing.
    fn sweep(&self, actor: Actor) -> impl Future<Output = Result<Vec<String>, StoreError>> + Send;

    /// Keep a memory, written with `actor`'s trust. `request_id` makes a retry
    /// harmless.
    fn remember(
        &self,
        actor: Actor,
        request_id: String,
        key: String,
        value: String,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// The memories a brief may carry: `human` and `approved` text only,
    /// highest trust first, at most `limit`. Agent-written and imported
    /// memories come back only through a request that names them as untrusted
    /// data (M14).
    fn prompt_memories(
        &self,
        actor: Actor,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send;

    /// Set the legacy tenant default for items without a named policy. Only a
    /// human credential may; an empty list is the same as no policy. New
    /// verified-commit defaults are refused because this API has no branch
    /// argument. Historical rows remain readable and fail closed on close.
    fn set_close_policy(
        &self,
        actor: Actor,
        requires: Vec<Evidence>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Set or clear the policy for one named item. This is an authenticated
    /// human write; a verified-commit requirement names the full authorized
    /// `refs/heads/*` branch that the host must check. An empty `requires`
    /// with no branch removes this item override, restoring any legacy
    /// singleton policy rather than silently relaxing it.
    fn set_item_close_policy(
        &self,
        _actor: Actor,
        _item_id: String,
        _requires: Vec<Evidence>,
        _authorized_branch: Option<String>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        std::future::ready(Err(StoreError::MissingImplementation {
            operation: "set_item_close_policy",
        }))
    }

    /// Read an item's effective policy. A named item rule takes precedence;
    /// without one, the legacy global rule applies. `None` means neither is
    /// stored. Reading a missing item returns [`StoreError::MissingItem`].
    fn item_close_policy(
        &self,
        _item_id: &str,
    ) -> impl Future<Output = Result<Option<ItemClosePolicy>, StoreError>> + Send {
        std::future::ready(Err(StoreError::MissingImplementation {
            operation: "item_close_policy",
        }))
    }

    /// Set or clear structured criteria for a named item as the enrolled
    /// operator. Empty criteria clear the record. Imported issue prose is a
    /// separate, lower-trust field and never enters this method by itself.
    fn set_item_criteria(
        &self,
        _actor: Actor,
        _item_id: String,
        _criteria: Vec<Criterion>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        std::future::ready(Err(StoreError::MissingImplementation {
            operation: "set_item_criteria",
        }))
    }

    /// Read the operator-authored criteria for a named item. `None` means no
    /// structured criteria are set; a missing item is an error.
    fn item_criteria(
        &self,
        _item_id: &str,
    ) -> impl Future<Output = Result<Option<ItemCriteria>, StoreError>> + Send {
        std::future::ready(Err(StoreError::MissingImplementation {
            operation: "item_criteria",
        }))
    }

    /// Record a commit against the held item. This model-facing path records
    /// a link only; it cannot assert that Git indexed or branched the commit.
    /// Verified evidence is minted only by the host after checking Git.
    fn link_commit(
        &self,
        lease: Lease,
        sha: String,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Import beads' JSONL export. A committed import requires an enrolled
    /// human credential verified inside its bulk write transaction; a dry run
    /// needs none. Importing one export twice leaves the graph
    /// as it was; text that differs is left alone rather than overwritten;
    /// everything written comes out `external`. With `dry_run` the call answers
    /// [`StoreError::ImportDryRun`] and writes nothing.
    ///
    /// A line that cannot be mapped — a status atomic has no place for, a
    /// generic beads `pinned` flag, or a `blocks` or `parent-child` reference
    /// naming nothing, refuses the whole file with
    /// [`StoreError::ImportRefused`] and writes nothing at all.
    /// Canonical `_type: "memory"` records import with external trust. A pinned
    /// item exported by atomic carries a versioned `metadata.atomic_graph`
    /// marker; only that marked form maps back to the native pinned status.
    fn import(
        &self,
        actor: Actor,
        jsonl: String,
        dry_run: bool,
    ) -> impl Future<Output = Result<Report, StoreError>> + Send;

    /// The whole graph as beads' JSONL, so a project can leave atomic as
    /// straightforwardly as it arrives. Persisted work items appear once, in
    /// id order, with their edges, notes and canonical beads memory records;
    /// gate-store copies are derived and omitted. Native pinned
    /// items use the versioned metadata marker described by [`Self::import`].
    fn export(&self, actor: Actor) -> impl Future<Output = Result<String, StoreError>> + Send;
}
