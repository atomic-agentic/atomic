// SPDX-License-Identifier: Apache-2.0

//! Private, typed graph proposals for one atom attempt. This module does not
//! touch the committed graph. The host bridge exposes its operations as
//! tools, then publishes an accepted batch under the claim's lease fence.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt;
use std::sync::LazyLock;

use crate::store::Trust;

/// At most this many graph writes can be proposed by one atom attempt.
pub const MAX_OPERATIONS: usize = 64;
/// Maximum encoded size of one graph tool argument.
pub const MAX_ARGUMENT_BYTES: usize = 16 * 1024;

/// The JSON Schema contract for one graph proposal tool argument. The same
/// contract can be handed to the eventual model-tool bridge. The host also
/// enforces the encoded 16 KiB transport limit and semantic graph checks,
/// which JSON Schema cannot express (such as an edge from an item to itself).
pub fn operation_schema() -> &'static Value {
    static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "oneOf": [
                {
                    "type": "object", "additionalProperties": false,
                    "required": ["op", "title", "body", "kind", "priority"],
                    "properties": {
                        "op": {"const": "create"},
                        "title": {"type": "string", "minLength": 1, "maxLength": 256,
                                  "pattern": "^[^\\x00\\r\\n]*[^\\s\\x00\\r\\n][^\\x00\\r\\n]*$"},
                        "body": {"type": "string", "maxLength": 8192, "pattern": "^[^\\x00]*$"},
                        "kind": {"type": "string", "minLength": 1, "maxLength": 64,
                                 "pattern": "^[^\\s\\x00]+$"},
                        "priority": {"type": "integer", "minimum": -2147483648_i64,
                                     "maximum": 2147483647_i64},
                        "parent": {"type": ["string", "null"], "minLength": 1,
                                   "maxLength": 160,
                                   "pattern": "^[^\\x00-\\x1F\\x7F-\\x9F]*[^\\s\\x00-\\x1F\\x7F-\\x9F][^\\x00-\\x1F\\x7F-\\x9F]*$"},
                        "due_at": {"type": ["integer", "null"], "minimum": 0,
                                   "maximum": 9223372036854775807_u64}
                    }
                },
                {
                    "type": "object", "additionalProperties": false,
                    "required": ["op", "item_id", "reason"],
                    "properties": {
                        "op": {"const": "close_intent"},
                        "item_id": {"type": "string", "minLength": 1, "maxLength": 160,
                                    "pattern": "^[^\\x00-\\x1F\\x7F-\\x9F]*[^\\s\\x00-\\x1F\\x7F-\\x9F][^\\x00-\\x1F\\x7F-\\x9F]*$"},
                        "reason": {"type": "string", "minLength": 1, "maxLength": 2048,
                                   "pattern": "^[^\\x00]*[^\\s\\x00][^\\x00]*$"}
                    }
                },
                {
                    "type": "object", "additionalProperties": false,
                    "required": ["op", "from", "kind", "to"],
                    "properties": {
                        "op": {"const": "dep_add"},
                        "from": {"type": "string", "minLength": 1, "maxLength": 160,
                                 "pattern": "^[^\\x00-\\x1F\\x7F-\\x9F]*[^\\s\\x00-\\x1F\\x7F-\\x9F][^\\x00-\\x1F\\x7F-\\x9F]*$"},
                        "kind": {"enum": ["blocks", "parent-child", "conditional-blocks",
                                           "waits-for", "related", "discovered-from",
                                           "relates-to", "duplicates", "supersedes", "replies-to"]},
                        "to": {"type": "string", "minLength": 1, "maxLength": 160,
                               "pattern": "^[^\\x00-\\x1F\\x7F-\\x9F]*[^\\s\\x00-\\x1F\\x7F-\\x9F][^\\x00-\\x1F\\x7F-\\x9F]*$"}
                    }
                },
                {
                    "type": "object", "additionalProperties": false,
                    "required": ["op", "item_id", "text"],
                    "properties": {
                        "op": {"const": "remember"},
                        "item_id": {"type": "string", "minLength": 1, "maxLength": 160,
                                    "pattern": "^[^\\x00-\\x1F\\x7F-\\x9F]*[^\\s\\x00-\\x1F\\x7F-\\x9F][^\\x00-\\x1F\\x7F-\\x9F]*$"},
                        "text": {"type": "string", "minLength": 1, "maxLength": 4096,
                                 "pattern": "^[^\\x00]*[^\\s\\x00][^\\x00]*$"}
                    }
                }
            ]
        })
    });
    &SCHEMA
}

fn operation_validator() -> &'static jsonschema::Validator {
    static VALIDATOR: LazyLock<jsonschema::Validator> = LazyLock::new(|| {
        jsonschema::validator_for(operation_schema()).expect("static graph operation schema")
    });
    &VALIDATOR
}

fn validate_argument_schema(raw: &Value) -> Result<(), ProposalError> {
    operation_validator()
        .validate(raw)
        .map_err(|e| ProposalError::Invalid(format!("argument schema: {e}")))
}

/// The host's durable identity for the attempt that owns a proposal buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptKey {
    run: String,
    step: String,
    attempt: u32,
}

impl AttemptKey {
    /// Make an attempt key from host-run values, refusing empty or unbounded
    /// names before they can enter a deduplication key.
    pub fn new(
        run: impl Into<String>,
        step: impl Into<String>,
        attempt: u32,
    ) -> Result<Self, ProposalError> {
        let (run, step) = (run.into(), step.into());
        if attempt == 0 || !bounded_name(&run) || !bounded_name(&step) {
            return Err(ProposalError::Invalid(
                "run, step and positive attempt must be bounded host values".into(),
            ));
        }
        Ok(Self { run, step, attempt })
    }
}

fn bounded_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

/// An operation the model may propose. No actor, trust label, request ID,
/// status, or SQL is supplied by the model. The host validates references
/// against the current graph when an accepted batch is published.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum GraphOp {
    /// Propose one open work item with a host-derived provisional ID.
    Create {
        /// One-line item title.
        title: String,
        /// Item description and criteria as untrusted agent text.
        body: String,
        /// Item kind, such as `task` or `bug`.
        kind: String,
        /// Lower values are selected first.
        priority: i32,
        /// Optional parent item, which must exist or be proposed earlier.
        #[serde(default)]
        parent: Option<String>,
        /// Optional Unix-millisecond due time.
        #[serde(default)]
        due_at: Option<i64>,
    },
    /// Ask the host to close an item when its evidence policy is satisfied.
    CloseIntent {
        /// Existing item ID or an earlier provisional ID.
        item_id: String,
        /// Bounded reason, stored as agent-authored text if accepted.
        reason: String,
    },
    /// Propose a dependency or nonblocking graph edge.
    DepAdd {
        /// Source item ID.
        from: String,
        /// One recognized edge kind.
        kind: String,
        /// Target item ID.
        to: String,
    },
    /// Propose a note; it remains agent text unless a human approves it.
    Remember {
        /// Item receiving the note.
        item_id: String,
        /// Bounded note text.
        text: String,
    },
}

/// An invalid or full private proposal buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposalError {
    /// The tool argument or attempt identity is invalid.
    Invalid(String),
    /// This attempt already proposed its maximum number of operations.
    Full,
}

impl fmt::Display for ProposalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => write!(f, "invalid graph proposal: {reason}"),
            Self::Full => write!(
                f,
                "graph proposal buffer is full ({MAX_OPERATIONS} operations)"
            ),
        }
    }
}

impl std::error::Error for ProposalError {}

/// The host-generated receipt returned to the proposing atom. It never says
/// that the operation is committed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProposalReceipt {
    /// Zero-based position in this attempt's private batch.
    pub index: usize,
    /// Stable host deduplication key for later publication.
    pub request_id: String,
    /// Provisional item ID for a `create` operation.
    pub provisional_id: Option<String>,
}

/// One validated operation and the identities the host assigned it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedOp {
    /// The validated operation.
    pub op: GraphOp,
    /// Its host-generated receipt.
    pub receipt: ProposalReceipt,
}

/// One note in a graph view, with its provisional status explicit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ViewNote {
    /// Note content, still untrusted if proposed by an atom.
    pub text: String,
    /// Host-recorded author; provisional model text is attributed to this attempt.
    pub author: String,
    /// Trust of the text, never supplied through a model operation.
    pub trust: ViewTrust,
    /// Whether this note exists only in the current attempt.
    pub provisional: bool,
}

/// Trust carried by text in a graph view. A provisional proposal is external
/// until a host publisher assigns its final stored provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewTrust {
    /// Text written by a person.
    Human,
    /// Agent text approved by a person.
    Approved,
    /// Text written by an agent.
    Agent,
    /// Imported or provisional model text.
    External,
}

impl From<Trust> for ViewTrust {
    fn from(value: Trust) -> Self {
        match value {
            Trust::Human => Self::Human,
            Trust::Approved => Self::Approved,
            Trust::Agent => Self::Agent,
            Trust::External => Self::External,
        }
    }
}

/// One outgoing edge in a graph view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ViewEdge {
    /// Edge kind.
    pub kind: String,
    /// Target item ID.
    pub to: String,
    /// Whether this edge exists only in the current attempt.
    pub provisional: bool,
}

/// A graph-show projection. Callers construct committed views from their
/// read-only store query; only this module adds private proposals on top.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphView {
    /// Item ID.
    pub id: String,
    /// Item title.
    pub title: String,
    /// Item body.
    pub body: String,
    /// Host-recorded author of the item text, or this attempt for a proposal.
    pub author: String,
    /// Trust of the item text.
    pub trust: ViewTrust,
    /// Item kind.
    pub kind: String,
    /// Item priority.
    pub priority: i32,
    /// Committed status. A close intent does not change it.
    pub status: String,
    /// Parent item ID.
    pub parent: Option<String>,
    /// Optional Unix-millisecond due time.
    pub due_at: Option<i64>,
    /// Whether any part of this view is private to the current attempt.
    pub provisional: bool,
    /// A proposed close, still subject to policy and review.
    pub close_intent: Option<ViewNote>,
    /// Notes visible in this view.
    pub notes: Vec<ViewNote>,
    /// Outgoing edges visible in this view.
    pub edges: Vec<ViewEdge>,
    /// Committed items currently blocking readiness. A proposed edge is not
    /// added here until publication refreshes the committed blocker projection.
    pub blocked_by: Vec<String>,
}

/// Private proposals belonging to exactly one atom attempt. This type has
/// no database handle and no method that mutates committed graph state.
/// It cannot be cloned: the host must own one append log for a live attempt,
/// because a second fork at the same index would reuse its stable request ID.
///
/// ```compile_fail
/// use atomic_graph::proposal::{AttemptKey, ProposalBuffer};
/// let key = AttemptKey::new("run", "step", 1).unwrap();
/// let buffer = ProposalBuffer::new(key);
/// let _fork = buffer.clone();
/// ```
#[derive(Debug)]
pub struct ProposalBuffer {
    attempt: AttemptKey,
    operations: Vec<RecordedOp>,
}

impl ProposalBuffer {
    /// Start an empty private buffer for one host-identified attempt.
    pub fn new(attempt: AttemptKey) -> Self {
        Self {
            attempt,
            operations: Vec::new(),
        }
    }

    /// The owning attempt, for the host's private read overlay.
    pub fn attempt(&self) -> &AttemptKey {
        &self.attempt
    }

    /// Parse and validate a JSON tool argument before assigning its stable
    /// host identity. An unknown key, including `actor`, `trust`, or `sql`,
    /// is refused by the full JSON Schema even if a provider omits a check.
    pub fn propose_raw(&mut self, raw: Value) -> Result<ProposalReceipt, ProposalError> {
        if serde_json::to_vec(&raw)
            .map_err(|e| ProposalError::Invalid(e.to_string()))?
            .len()
            > MAX_ARGUMENT_BYTES
        {
            return Err(ProposalError::Invalid(
                "tool argument exceeds 16 KiB".into(),
            ));
        }
        validate_argument_schema(&raw)?;
        let op: GraphOp =
            serde_json::from_value(raw).map_err(|e| ProposalError::Invalid(e.to_string()))?;
        self.propose(op)
    }

    /// Validate a typed operation again at the host boundary. This catches
    /// hand-built values as well as deserialized tool arguments.
    pub fn propose(&mut self, op: GraphOp) -> Result<ProposalReceipt, ProposalError> {
        if self.operations.len() == MAX_OPERATIONS {
            return Err(ProposalError::Full);
        }
        validate(&op)?;
        let index = self.operations.len();
        let hash = identity(&self.attempt, index);
        let receipt = ProposalReceipt {
            index,
            request_id: format!("atomic-graph-op:{hash}"),
            provisional_id: matches!(op, GraphOp::Create { .. })
                .then(|| format!("at-p{}", &hash[..24])),
        };
        self.operations.push(RecordedOp {
            op,
            receipt: receipt.clone(),
        });
        Ok(receipt)
    }

    /// The validated private batch for a future host publisher. Its contents
    /// are not committed until an accepted candidate is published.
    pub fn operations(&self) -> &[RecordedOp] {
        &self.operations
    }

    /// Show committed data with this attempt's proposals overlaid. A caller
    /// from another attempt receives only its supplied committed view.
    pub fn show(
        &self,
        caller: &AttemptKey,
        item_id: &str,
        committed: Option<GraphView>,
    ) -> Option<GraphView> {
        let mut view = committed.filter(|item| item.id == item_id);
        if caller != &self.attempt {
            return view;
        }
        for recorded in &self.operations {
            match &recorded.op {
                GraphOp::Create {
                    title,
                    body,
                    kind,
                    priority,
                    parent,
                    due_at,
                } if recorded.receipt.provisional_id.as_deref() == Some(item_id) => {
                    view = Some(GraphView {
                        id: item_id.to_owned(),
                        title: title.clone(),
                        body: body.clone(),
                        author: self.provisional_author(),
                        trust: ViewTrust::External,
                        kind: kind.clone(),
                        priority: *priority,
                        status: "open".into(),
                        parent: parent.clone(),
                        due_at: *due_at,
                        provisional: true,
                        close_intent: None,
                        notes: Vec::new(),
                        edges: Vec::new(),
                        blocked_by: Vec::new(),
                    });
                }
                GraphOp::CloseIntent {
                    item_id: target,
                    reason,
                } if target == item_id => {
                    if let Some(item) = &mut view {
                        item.close_intent = Some(ViewNote {
                            text: reason.clone(),
                            author: self.provisional_author(),
                            trust: ViewTrust::External,
                            provisional: true,
                        });
                        item.provisional = true;
                    }
                }
                GraphOp::DepAdd { from, kind, to } if from == item_id => {
                    if let Some(item) = &mut view {
                        item.edges.push(ViewEdge {
                            kind: kind.clone(),
                            to: to.clone(),
                            provisional: true,
                        });
                        item.provisional = true;
                    }
                }
                GraphOp::Remember {
                    item_id: target,
                    text,
                } if target == item_id => {
                    if let Some(item) = &mut view {
                        item.notes.push(ViewNote {
                            text: text.clone(),
                            author: self.provisional_author(),
                            trust: ViewTrust::External,
                            provisional: true,
                        });
                        item.provisional = true;
                    }
                }
                _ => {}
            }
        }
        view
    }

    fn provisional_author(&self) -> String {
        format!(
            "atom:{}:{}:{}",
            self.attempt.run, self.attempt.step, self.attempt.attempt
        )
    }
}

fn identity(attempt: &AttemptKey, index: usize) -> String {
    let mut hash = Sha256::new();
    for part in ["atomic.graph.proposal.v1", &attempt.run, &attempt.step] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.update(attempt.attempt.to_be_bytes());
    hash.update((index as u64).to_be_bytes());
    format!("{:x}", hash.finalize())
}

fn bounded_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.chars().count() <= max && !value.contains('\0')
}

fn bounded_id(value: &str) -> bool {
    bounded_text(value, 160) && !value.chars().any(char::is_control)
}

fn validate(op: &GraphOp) -> Result<(), ProposalError> {
    let invalid = |why: &str| ProposalError::Invalid(why.into());
    match op {
        GraphOp::Create {
            title,
            body,
            kind,
            parent,
            due_at,
            ..
        } => {
            if !bounded_text(title, 256) || title.contains('\n') || title.contains('\r') {
                return Err(invalid("create title must be a bounded line"));
            }
            if body.chars().count() > 8192 || body.contains('\0') {
                return Err(invalid(
                    "create body exceeds 8192 characters or contains NUL",
                ));
            }
            if !bounded_text(kind, 64) || kind.chars().any(char::is_whitespace) {
                return Err(invalid("create kind is invalid"));
            }
            if parent.as_deref().is_some_and(|id| !bounded_id(id)) {
                return Err(invalid("create parent is invalid"));
            }
            if due_at.is_some_and(|time| time < 0) {
                return Err(invalid("create due_at precedes the Unix epoch"));
            }
        }
        GraphOp::CloseIntent { item_id, reason } => {
            if !bounded_id(item_id) || !bounded_text(reason, 2048) {
                return Err(invalid("close intent has an invalid item or reason"));
            }
        }
        GraphOp::DepAdd { from, kind, to } => {
            const EDGE_KINDS: &[&str] = &[
                "blocks",
                "parent-child",
                "conditional-blocks",
                "waits-for",
                "related",
                "discovered-from",
                "relates-to",
                "duplicates",
                "supersedes",
                "replies-to",
            ];
            if !bounded_id(from)
                || !bounded_id(to)
                || from == to
                || !EDGE_KINDS.contains(&kind.as_str())
            {
                return Err(invalid("dependency has invalid endpoints or kind"));
            }
        }
        GraphOp::Remember { item_id, text } => {
            if !bounded_id(item_id) || !bounded_text(text, 4096) {
                return Err(invalid("remember has an invalid item or text"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteStore;
    use crate::store::{Actor, GraphStore};
    use serde_json::json;
    use std::time::SystemTime;

    fn attempt(number: u32) -> AttemptKey {
        AttemptKey::new("run-one", "graph.00000.fix", number)
            .expect("valid graph proposal test operation")
    }

    fn create() -> Value {
        json!({"op":"create","title":"Follow up","body":"Check edge case","kind":"task","priority":2,"due_at":12345})
    }

    #[test]
    fn identities_are_stable_and_host_owned() {
        let mut first = ProposalBuffer::new(attempt(1));
        let a = first
            .propose_raw(create())
            .expect("valid graph proposal test operation");
        let b = first
            .propose_raw(create())
            .expect("valid graph proposal test operation");
        let mut replay = ProposalBuffer::new(attempt(1));
        assert_eq!(
            replay
                .propose_raw(create())
                .expect("valid graph proposal test operation"),
            a
        );
        assert_ne!(a.request_id, b.request_id);
        assert_ne!(a.provisional_id, b.provisional_id);
        assert_ne!(
            ProposalBuffer::new(attempt(2))
                .propose_raw(create())
                .expect("valid graph proposal test operation"),
            a
        );
        assert!(
            a.provisional_id
                .as_deref()
                .expect("valid graph proposal test operation")
                .starts_with("at-p")
        );
        for field in ["actor", "trust", "sql", "request_id", "id", "status"] {
            let mut raw = create();
            raw.as_object_mut()
                .expect("valid graph proposal test operation")
                .insert(field.into(), json!("forged"));
            assert!(first.propose_raw(raw).is_err(), "{field}");
        }
        assert_eq!(
            first.operations().len(),
            2,
            "rejected arguments never enter the buffer"
        );
    }

    #[test]
    fn raw_tool_arguments_obey_the_exported_full_schema() {
        let schema = operation_schema();
        let validator = jsonschema::validator_for(schema).expect("graph operation schema");
        for valid in [
            create(),
            json!({"op":"close_intent","item_id":"at-a","reason":"done"}),
            json!({"op":"dep_add","from":"at-a","kind":"blocks","to":"at-b"}),
            json!({"op":"remember","item_id":"at-a","text":"follow up"}),
        ] {
            assert!(validator.is_valid(&valid), "valid operation: {valid}");
        }
        let mut buffer = ProposalBuffer::new(attempt(1));
        for invalid in [
            json!({"op":"create","title":"","body":"","kind":"task","priority":2}),
            json!({"op":"create","title":"follow up","body":"","kind":"task","priority":2,"actor":"human"}),
            json!({"op":"close_intent","item_id":"at-a","reason":""}),
            json!({"op":"dep_add","from":"at-a","kind":"unknown","to":"at-b"}),
            json!({"op":"remember","item_id":"at-a","text":""}),
        ] {
            assert!(
                !validator.is_valid(&invalid),
                "invalid operation: {invalid}"
            );
            let result = buffer.propose_raw(invalid);
            assert!(
                matches!(result, Err(ProposalError::Invalid(reason)) if reason.contains("argument schema")),
                "raw argument must be refused by the host schema"
            );
        }
        assert!(buffer.operations().is_empty());
    }

    #[test]
    fn exported_schema_and_host_agree_on_text_and_integer_boundaries() {
        let validator =
            jsonschema::validator_for(operation_schema()).expect("graph operation schema");
        let mut buffer = ProposalBuffer::new(attempt(1));
        for valid in [
            json!({"op":"create","title":"é".repeat(256),"body":"Résumé",
                   "kind":"task","priority":i32::MAX,"due_at":i64::MAX}),
            json!({"op":"close_intent","item_id":"at-α","reason":"✓"}),
            json!({"op":"dep_add","from":"at-α","kind":"blocks","to":"at-β"}),
            json!({"op":"remember","item_id":"at-α","text":"Résumé"}),
        ] {
            assert!(validator.is_valid(&valid), "schema must accept {valid}");
            buffer
                .propose_raw(valid)
                .expect("schema-valid raw operation");
        }
        assert_eq!(buffer.operations().len(), 4);
        for invalid in [
            json!({"op":"create","title":"é".repeat(257),"body":"","kind":"task","priority":2}),
            json!({"op":"create","title":" \t ","body":"","kind":"task","priority":2}),
            json!({"op":"create","title":"two\nlines","body":"","kind":"task","priority":2}),
            json!({"op":"create","title":"valid","body":"","kind":"two words","priority":2}),
            json!({"op":"create","title":"valid","body":"","kind":"task","priority":i64::from(i32::MAX)+1}),
            json!({"op":"create","title":"valid","body":"","kind":"task","priority":2,"due_at":u64::MAX}),
            json!({"op":"remember","item_id":"\u{0007}","text":"valid"}),
            json!({"op":"remember","item_id":"at-α","text":" \t "}),
        ] {
            assert!(
                !validator.is_valid(&invalid),
                "schema must refuse {invalid}"
            );
            assert!(
                matches!(buffer.propose_raw(invalid), Err(ProposalError::Invalid(reason)) if reason.contains("argument schema")),
                "host must refuse through the same schema"
            );
        }
        assert_eq!(buffer.operations().len(), 4);
    }

    #[test]
    fn divergent_operations_share_one_append_log_and_get_distinct_ids() {
        let mut buffer = ProposalBuffer::new(attempt(1));
        let first = buffer
            .propose_raw(create())
            .expect("valid graph proposal test operation");
        let second = buffer
            .propose(GraphOp::Create {
                title: "Different follow up".into(),
                body: "Different work".into(),
                kind: "task".into(),
                priority: 2,
                parent: None,
                due_at: None,
            })
            .expect("valid graph proposal test operation");
        assert_eq!((first.index, second.index), (0, 1));
        assert_ne!(first.request_id, second.request_id);
        assert_ne!(first.provisional_id, second.provisional_id);
        assert_eq!(buffer.operations().len(), 2);
    }

    #[test]
    fn same_attempt_sees_provisional_create_edge_note_and_close_intent() {
        let owner = attempt(1);
        let other = attempt(2);
        let mut buffer = ProposalBuffer::new(owner.clone());
        let id = buffer
            .propose_raw(create())
            .expect("valid graph proposal test operation")
            .provisional_id
            .expect("valid graph proposal test operation");
        buffer
            .propose(GraphOp::DepAdd {
                from: id.clone(),
                kind: "discovered-from".into(),
                to: "at-original".into(),
            })
            .expect("valid graph proposal test operation");
        buffer
            .propose(GraphOp::Remember {
                item_id: id.clone(),
                text: "What was found".into(),
            })
            .expect("valid graph proposal test operation");
        buffer
            .propose(GraphOp::CloseIntent {
                item_id: id.clone(),
                reason: "complete".into(),
            })
            .expect("valid graph proposal test operation");
        let mine = buffer
            .show(&owner, &id, None)
            .expect("valid graph proposal test operation");
        assert!(mine.provisional);
        assert_eq!(
            mine.status, "open",
            "close intent cannot claim a committed close"
        );
        assert_eq!(mine.due_at, Some(12345));
        assert_eq!(mine.trust, ViewTrust::External);
        assert_eq!(mine.author, "atom:run-one:graph.00000.fix:1");
        let close = mine
            .close_intent
            .as_ref()
            .expect("valid graph proposal test operation");
        assert_eq!(close.text, "complete");
        assert_eq!(close.trust, ViewTrust::External);
        assert_eq!(close.author, mine.author);
        assert_eq!(mine.edges[0].to, "at-original");
        assert!(mine.edges[0].provisional);
        assert_eq!(mine.notes[0].text, "What was found");
        assert_eq!(mine.notes[0].trust, ViewTrust::External);
        assert_eq!(mine.notes[0].author, mine.author);
        assert!(mine.notes[0].provisional);
        assert!(buffer.show(&other, &id, None).is_none());
    }

    #[test]
    fn rejected_or_invalid_batch_never_changes_committed_view() {
        let database = std::env::temp_dir().join(format!(
            "graph-proposal-unpublished-{}-{}.db",
            std::process::id(),
            attempt(1).attempt,
        ));
        let _ = std::fs::remove_file(&database);
        let store = SqliteStore::open(&database, SystemTime::now)
            .expect("valid graph proposal test operation");
        let owner = attempt(1);
        let mut buffer = ProposalBuffer::new(owner.clone());
        let committed = GraphView {
            id: "at-original".into(),
            title: "Original".into(),
            body: "Work".into(),
            author: "maintainer".into(),
            trust: ViewTrust::Human,
            kind: "task".into(),
            priority: 2,
            status: "open".into(),
            parent: None,
            due_at: Some(98765),
            provisional: false,
            close_intent: None,
            notes: vec![ViewNote {
                text: "Approved note".into(),
                author: "reviewer".into(),
                trust: ViewTrust::Approved,
                provisional: false,
            }],
            edges: vec![],
            blocked_by: vec![],
        };
        buffer
            .propose(GraphOp::Remember {
                item_id: "at-original".into(),
                text: "draft note".into(),
            })
            .expect("valid graph proposal test operation");
        let shown = buffer
            .show(&owner, "at-original", Some(committed.clone()))
            .expect("valid graph proposal test operation");
        assert!(shown.provisional);
        assert_eq!(shown.due_at, Some(98765));
        assert_eq!(
            (shown.author.as_str(), shown.trust),
            ("maintainer", ViewTrust::Human)
        );
        assert_eq!(shown.notes.len(), 2);
        assert_eq!(shown.notes[0].trust, ViewTrust::Approved);
        assert_eq!(shown.notes[0].author, "reviewer");
        assert_eq!(shown.notes[1].trust, ViewTrust::External);
        assert_eq!(committed.notes.len(), 1, "the committed input is untouched");
        assert_eq!(
            buffer.show(&attempt(2), "at-original", Some(committed.clone())),
            Some(committed.clone())
        );
        let before = buffer.operations().len();
        assert!(
            buffer
                .propose_raw(json!({"op":"dep_add","from":"a","kind":"blocks","to":"a"}))
                .is_err()
        );
        assert_eq!(buffer.operations().len(), before);
        drop(buffer); // rejected candidate: no operation can reach a store from this buffer
        assert!(!committed.provisional);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("valid graph proposal test operation");
        assert!(
            runtime
                .block_on(store.all_items(Actor::agent("other attempt")))
                .expect("valid graph proposal test operation")
                .is_empty()
        );
        drop(store);
        let _ = std::fs::remove_file(database);
    }

    #[test]
    fn bounds_refuse_oversized_arguments_and_full_batches() {
        let mut buffer = ProposalBuffer::new(attempt(1));
        assert!(
            buffer
                .propose_raw(json!({"op":"remember","item_id":"at-x","text":"x".repeat(16_385)}))
                .is_err()
        );
        assert!(buffer.propose_raw(json!({"op":"create","title":"x","body":"x".repeat(8193),"kind":"task","priority":2})).is_err());
        assert!(
            buffer
                .propose(GraphOp::Create {
                    title: "x".into(),
                    body: "x".repeat(8193),
                    kind: "task".into(),
                    priority: 2,
                    parent: None,
                    due_at: None,
                })
                .is_err(),
            "typed host operations must keep the body bound too"
        );
        assert!(buffer.propose_raw(json!({"op":"create","title":"x","body":"","kind":"task","priority":2,"due_at":-1})).is_err());
        assert_eq!(buffer.operations().len(), 0);
        for _ in 0..MAX_OPERATIONS {
            buffer
                .propose_raw(create())
                .expect("valid graph proposal test operation");
        }
        assert_eq!(buffer.propose_raw(create()), Err(ProposalError::Full));
    }
}
