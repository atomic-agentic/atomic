// SPDX-License-Identifier: Apache-2.0

//! The `Atom` trait: a typed function from `In` to `Out`.
//!
//! An atom runs as one model turn whose final answer is structured by its
//! output schema. Schemas cross this API as JSON values, so an atom may build
//! them by hand, from a file, or with any version of `schemars`; atomic itself
//! depends on none. Atoms configure their run through [`AtomConfig`], atomic's
//! own type, never Codex's, so crates that define atoms do not depend on Codex.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::borrow::Cow;

/// How an atom runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AtomKind {
    /// A full Codex thread: sandbox, shell, patch editing, tools.
    #[default]
    Codex,
    /// One schema-constrained model call.
    Lite,
    /// A command with typed JSON in and out, and no model.
    Exec,
}

/// The sandbox an atom's thread runs in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sandbox {
    /// Read the workspace, write nothing.
    #[default]
    ReadOnly,
    /// Write only inside the step's own worktree or scratch workspace.
    WorkspaceWrite,
}

/// What happens to an approval the model asks for in the middle of a turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Approvals {
    /// Declined: nobody is watching this turn.
    #[default]
    Unattended,
    /// Forwarded to a person.
    Attended,
}

/// The model tiers an atom may name instead of a model, from least to most
/// capable. A project maps each tier to an ordered list of models, so atoms
/// never name a provider. A tier is independent of an atom's kind: a lite
/// atom may run on any tier.
///
/// - `basic`: the smallest, cheapest models, often local, for classify,
///   extract and route steps;
/// - `fast`: quick, low-cost general models, for executors and routine
///   steps;
/// - `advanced`: capable general models, for reviewers and harder steps;
/// - `frontier`: the most capable models available, for planning and the
///   reviews that decide the most.
pub const MODEL_TIERS: &[&str] = &["basic", "fast", "advanced", "frontier"];

/// Run settings an atom may adjust in [`Atom::configure`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AtomConfig {
    /// A model name, or one of the [`MODEL_TIERS`] the project maps; `None`
    /// takes the project default.
    pub model: Option<String>,
    /// The sandbox.
    pub sandbox: Sandbox,
    /// Hosts the step may reach; empty means no network.
    pub network_allow: Vec<String>,
    /// In-turn approvals.
    pub approvals: Approvals,
    /// The atom's own budget limits; the rest inherit the configured
    /// defaults (see [`crate::budget`]).
    pub budget: crate::budget::Budget,
}

/// Why an atom failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AtomError {
    /// The input does not fit the atom's input contract; the model was not
    /// called.
    #[error("the input does not fit the input schema: {0}")]
    Input(String),
    /// The final answer did not fit the output contract, even after the
    /// repair turn.
    #[error("the output does not fit the schema: {message}")]
    Schema {
        /// What was wrong with the last answer.
        message: String,
        /// The last answer, as the model wrote it.
        answer: String,
    },
    /// The output fits the schema but failed the atom's own check.
    #[error("the output failed validation: {0}")]
    Validation(String),
    /// The model or its provider failed.
    #[error("model: {0}")]
    Model(String),
    /// The atom's task panicked; the step fails, the process does not.
    #[error("the atom panicked: {0}")]
    Panicked(String),
    /// A budget cap left no room for the next model call, so it was not
    /// made. Not a failure of the atom: a durable run parks on it.
    #[error("budget: {0}")]
    Budget(String),
    /// An exec atom's command may have run, but how it ended was lost, so
    /// what it did is unknown. A durable step treats this as an interrupted
    /// attempt: its attempt guard decides whether the command may run again.
    #[error("outcome unknown: {0}")]
    OutcomeUnknown(String),
    /// Anything else in running the atom.
    #[error("{0}")]
    Runtime(String),
}

/// A typed atom.
///
/// ```
/// use atomic_core::{Atom, AtomError};
/// use serde::{Deserialize, Serialize};
/// use serde_json::{Value, json};
/// use std::borrow::Cow;
///
/// #[derive(Serialize)]
/// struct Note { text: String }
///
/// #[derive(Deserialize)]
/// struct Category { category: String }
///
/// struct Classify;
///
/// impl Atom for Classify {
///     type In = Note;
///     type Out = Category;
///     const NAME: &'static str = "classify-note";
///     fn input_schema(&self) -> Value {
///         json!({"type": "object", "required": ["text"], "properties": {"text": {"type": "string"}}})
///     }
///     fn output_schema(&self) -> Value {
///         json!({"type": "object", "required": ["category"],
///                "properties": {"category": {"enum": ["bug", "feature", "question"]}}})
///     }
///     fn instructions(&self) -> Cow<'static, str> {
///         "Classify the note as bug, feature or question.".into()
///     }
///     fn validate(&self, out: &Category) -> Result<(), AtomError> {
///         if out.category.is_empty() { Err(AtomError::Validation("empty".into())) } else { Ok(()) }
///     }
/// }
/// ```
pub trait Atom: Send + Sync {
    /// The input type.
    type In: Serialize + Send + Sync;
    /// The output type.
    type Out: DeserializeOwned + Send;
    /// The atom's name, as workflows refer to it.
    const NAME: &'static str;

    /// The input contract, JSON Schema 2020-12.
    fn input_schema(&self) -> Value;
    /// The output contract, JSON Schema 2020-12.
    fn output_schema(&self) -> Value;
    /// The instructions the model is given.
    fn instructions(&self) -> Cow<'static, str>;

    /// How the atom runs.
    fn kind(&self) -> AtomKind {
        AtomKind::Codex
    }

    /// Adjust run settings, which start from the project's defaults.
    fn configure(&self, _config: &mut AtomConfig) {}

    /// Check an output that already fits the schema.
    fn validate(&self, _out: &Self::Out) -> Result<(), AtomError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Serialize)]
    struct Request {
        diff: String,
    }

    #[derive(Deserialize)]
    struct Verdict {
        verdict: String,
    }

    struct Review;

    impl Atom for Review {
        type In = Request;
        type Out = Verdict;
        const NAME: &'static str = "review-patch";
        fn input_schema(&self) -> Value {
            json!({"type": "object", "additionalProperties": false, "required": ["diff"],
                   "properties": {"diff": {"type": "string"}}})
        }
        fn output_schema(&self) -> Value {
            json!({"type": "object", "additionalProperties": false, "required": ["verdict"],
                   "properties": {"verdict": {"enum": ["approve", "reject"]}}})
        }
        fn instructions(&self) -> Cow<'static, str> {
            "Review the diff.".into()
        }
        fn configure(&self, config: &mut AtomConfig) {
            config.model = Some("advanced".into());
        }
        fn validate(&self, out: &Verdict) -> Result<(), AtomError> {
            (out.verdict != "approve")
                .then_some(())
                .ok_or(AtomError::Validation("never approve in tests".into()))
        }
    }

    #[test]
    fn a_rust_atom_yields_its_schemas_as_json_values() {
        let atom = Review;
        assert_eq!(Review::NAME, "review-patch");
        assert_eq!(atom.kind(), AtomKind::Codex);
        assert_eq!(atom.input_schema()["required"], json!(["diff"]));
        // The output contract lowers to the strict generation profile.
        let lowered = atomic_schema::lower::lower(
            &atom.output_schema(),
            &atomic_schema::lower::OPENAI_STRICT,
        )
        .expect("lowers");
        assert!(
            atomic_schema::lower::check(&lowered.schema, &atomic_schema::lower::OPENAI_STRICT)
                .is_empty()
        );
        let mut config = AtomConfig::default();
        atom.configure(&mut config);
        assert_eq!(config.model.as_deref(), Some("advanced"));
        assert_eq!(MODEL_TIERS, ["basic", "fast", "advanced", "frontier"]);
        assert_eq!(config.sandbox, Sandbox::ReadOnly);
        let out: Verdict = serde_json::from_value(json!({"verdict": "approve"})).expect("parses");
        assert!(matches!(atom.validate(&out), Err(AtomError::Validation(_))));
        // The input serializes to what the input schema describes.
        let request = serde_json::to_value(Request { diff: "+x".into() }).expect("serializes");
        assert_eq!(request, json!({"diff": "+x"}));
    }
}
