// SPDX-License-Identifier: Apache-2.0
//! Typed, bounded goal records stored alongside pinned graph items.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A goal file's durable definition. The file name supplies the item ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalDefinition {
    /// Human-readable title.
    pub title: String,
    /// Goal brief or context.
    #[serde(default)]
    pub description: String,
    /// Optional read-only measure declaration; wake execution is later work.
    #[serde(default)]
    pub measure: Option<GoalMeasure>,
    /// Token allowance for each UTC period.
    pub budget: GoalBudget,
    /// Atoms tasks of this goal may use.
    #[serde(default)]
    pub atoms: Vec<String>,
    /// Toolsets tasks of this goal may use.
    #[serde(default)]
    pub toolsets: Vec<String>,
    /// People allowed to approve work for this goal.
    #[serde(default)]
    pub approvers: Vec<String>,
    /// Declarative wake sources; storing one does not fire it.
    #[serde(default)]
    pub wake: Vec<GoalWake>,
}

/// A read-only measure to sample once for each wake.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalMeasure {
    /// Named read-only tools.
    Tools {
        /// Read-only tool names, sampled by a later wake runtime.
        names: Vec<String>,
    },
    /// A CEL expression over their results.
    Cel {
        /// CEL source, evaluated by a later wake runtime.
        expr: String,
    },
}

/// The fixed period whose spent tokens are subtracted from a goal budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPeriod {
    /// UTC calendar day.
    Day,
    /// UTC seven-day period since the Unix epoch.
    Week,
}

/// A goal's token allowance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalBudget {
    /// The period in which `tokens` may be spent.
    pub period: GoalPeriod,
    /// Maximum model tokens in one period.
    pub tokens: u64,
    /// Maximum wakes in one period, including no-action wakes.
    pub runs: u64,
}

/// A future wake source, stored as policy but not executed by M5.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalWake {
    /// Schedule expression for M16's trigger runtime.
    Schedule {
        /// Schedule expression.
        cron: String,
    },
    /// Dedupe key for a webhook trigger.
    Webhook {
        /// Webhook dedupe key.
        key: String,
    },
    /// Event topic name.
    Event {
        /// Event topic.
        topic: String,
    },
}

impl GoalDefinition {
    /// Refuse unbounded or ambiguous declarations before any graph write.
    pub fn validate(&self) -> Result<(), String> {
        fn short(value: &str, what: &str, max: usize) -> Result<(), String> {
            if value.trim().is_empty() || value.len() > max || value.contains('\0') {
                return Err(format!("{what} must be nonempty and at most {max} bytes"));
            }
            Ok(())
        }
        short(&self.title, "goal title", 200)?;
        if self.description.len() > 16_384 {
            return Err("goal description exceeds 16384 bytes".into());
        }
        if self.budget.tokens == 0 || self.budget.tokens > i64::MAX as u64 {
            return Err("goal token budget must be between 1 and i64::MAX".into());
        }
        if self.budget.runs == 0 || self.budget.runs > i64::MAX as u64 {
            return Err("goal run budget must be between 1 and i64::MAX".into());
        }
        for (name, values) in [
            ("atoms", &self.atoms),
            ("toolsets", &self.toolsets),
            ("approvers", &self.approvers),
        ] {
            if values.len() > 64 {
                return Err(format!("goal {name} has more than 64 entries"));
            }
            for value in values {
                short(value, name, 128)?;
            }
        }
        if let Some(measure) = &self.measure {
            match measure {
                GoalMeasure::Tools { names } => {
                    if names.is_empty() || names.len() > 16 {
                        return Err("goal measure tools must have 1..=16 names".into());
                    }
                    for name in names {
                        short(name, "measure tool", 128)?;
                    }
                }
                GoalMeasure::Cel { expr } => short(expr, "measure CEL", 2_048)?,
            }
        }
        if self.wake.len() > 32 {
            return Err("goal has more than 32 wake declarations".into());
        }
        for wake in &self.wake {
            match wake {
                GoalWake::Schedule { cron } => short(cron, "wake schedule", 128)?,
                GoalWake::Webhook { key } => short(key, "wake webhook key", 128)?,
                GoalWake::Event { topic } => short(topic, "wake event topic", 128)?,
            }
        }
        Ok(())
    }
}

/// A recorded wake's decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalOutcome {
    /// The goal chose work.
    Act,
    /// The goal deliberately did nothing.
    NoAction,
}

impl GoalOutcome {
    /// Stored word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Act => "act",
            Self::NoAction => "no_action",
        }
    }
}

/// Human feedback category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalFeedbackKind {
    /// A free-text note.
    Note,
    /// A rejected gate's reason.
    GateRejection,
    /// An answer to a question gate.
    Answer,
    /// Direct steering.
    Steering,
}

impl GoalFeedbackKind {
    /// Stored word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::GateRejection => "gate_rejection",
            Self::Answer => "answer",
            Self::Steering => "steering",
        }
    }
}

/// Which dedupe value the goal ignores until its expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalIgnoreKind {
    /// A trigger's dedupe key.
    DedupeKey,
    /// A candidate plan's fingerprint.
    Fingerprint,
}

impl GoalIgnoreKind {
    /// Stored word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DedupeKey => "dedupe_key",
            Self::Fingerprint => "fingerprint",
        }
    }
}

/// One stored sample.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalSample {
    /// Wake that caused this sample.
    pub wake_id: String,
    /// Milliseconds since the Unix epoch.
    pub sampled_at: i64,
    /// The read-only measure result.
    pub value: Value,
}

/// One recorded wake and its outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalWakeRecord {
    /// Stable wake dedupe key.
    pub wake_id: String,
    /// Milliseconds since the Unix epoch.
    pub happened_at: i64,
    /// Whether work was selected.
    pub outcome: GoalOutcome,
    /// Human-readable reason.
    pub reason: String,
    /// Model tokens charged against this period.
    pub spent_tokens: u64,
}

/// One dated entry supplied by a person.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalFeedback {
    /// Feedback category.
    pub kind: GoalFeedbackKind,
    /// Human text, which remains marked as human provenance.
    pub text: String,
    /// Credential-derived principal.
    pub principal: String,
    /// Milliseconds since the Unix epoch.
    pub written_at: i64,
}

/// A dedupe key or fingerprint whose expiry has not passed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalIgnore {
    /// The type of value.
    pub kind: GoalIgnoreKind,
    /// Dedupe key or fingerprint.
    pub value: String,
    /// Milliseconds since the Unix epoch.
    pub expires_at: i64,
    /// Why this value was ignored.
    pub reason: String,
}

/// A child item summarized for a goal brief.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalChild {
    /// Graph item ID.
    pub id: String,
    /// Graph status word.
    pub status: String,
}

/// One row of `atomic goals list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSummary {
    /// Stable graph item ID.
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Whether new wakes are admitted.
    pub active: bool,
}

/// Bounded, typed context for a goal's next run and for `atomic goals show`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalBrief {
    /// The pinned goal item.
    pub id: String,
    /// Its current definition.
    pub definition: GoalDefinition,
    /// Whether wakes are admitted.
    pub active: bool,
    /// Revision of the definition.
    pub revision: u64,
    /// Most recent measure sample, if any.
    pub measure: Option<GoalSample>,
    /// Newest human feedback first, at most 20 entries.
    pub feedback: Vec<GoalFeedback>,
    /// Only entries whose expiry lies after the brief's clock.
    pub ignores: Vec<GoalIgnore>,
    /// Latest wakes first, at most 10 entries.
    pub wakes: Vec<GoalWakeRecord>,
    /// Direct children not yet closed.
    pub open_children: Vec<GoalChild>,
    /// Direct children that closed.
    pub closed_children: Vec<GoalChild>,
    /// Number of direct children, including those outside this bounded brief.
    pub total_children: u64,
    /// True when the brief omitted children after the first 200, ordered with open first.
    pub children_truncated: bool,
    /// Tokens still available in the current UTC period.
    pub tokens_remaining: u64,
    /// Wakes still available in the current UTC period.
    pub runs_remaining: u64,
}
