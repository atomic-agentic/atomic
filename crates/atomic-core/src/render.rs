// SPDX-License-Identifier: Apache-2.0

//! Role-preserving Astro rendering against immutable compiled sources.

use crate::compile::Compiled;
use serde::Deserialize;
use serde_json::{Value, json};

/// Separate developer instructions and tagged user data.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Rendered {
    /// Static instructions and pointers, without interpolated data.
    pub developer: String,
    /// Values tagged with their source path and origin.
    pub user: String,
}

/// A rejected or over-budget render.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("Astro render: {0}")]
pub struct RenderError(pub String);

/// Trusted dispatch identity and input provenance.
#[derive(Clone, Debug)]
pub struct RunInput<'a> {
    /// Stable run identity.
    pub run_id: &'a str,
    /// Schema-validated input.
    pub input: &'a Value,
    /// Parsed configuration.
    pub frontmatter: &'a Value,
    /// Input origin: external, team or agent.
    pub origin: &'a str,
    /// Origins of individual input fields, overriding `origin` for each, as
    /// a workflow's bindings assign them.
    pub origins: Option<&'a std::collections::BTreeMap<String, String>>,
    /// The workflow step this dispatch runs, when there is one; an exec
    /// atom's effect key is derived from it and the run.
    pub step: Option<&'a str>,
}

/// Render a checked source graph without reading the source tree.
pub fn render(compiled: &Compiled, run: &RunInput<'_>) -> Result<Rendered, RenderError> {
    render_with_context(compiled, run, None, None)
}

/// Render with a verified frozen host context and the current dispatch step.
pub fn render_with_context(
    compiled: &Compiled,
    run: &RunInput<'_>,
    snapshot: Option<&crate::context::ContextSnapshot>,
    step_id: Option<&str>,
) -> Result<Rendered, RenderError> {
    render_clocked(compiled, run, snapshot, step_id, None)
}

/// As [`render_with_context`], with helper calls clocked by the durable
/// step's record, so a retried step renders the same prompt.
pub fn render_clocked(
    compiled: &Compiled,
    run: &RunInput<'_>,
    snapshot: Option<&crate::context::ContextSnapshot>,
    step_id: Option<&str>,
    clock: Option<&crate::astro::StepClock>,
) -> Result<Rendered, RenderError> {
    jsonschema::validator_for(&compiled.program.contracts["Input"])
        .map_err(|e| RenderError(e.to_string()))?
        .validate(run.input)
        .map_err(|e| RenderError(format!("input: {e}")))?;
    // Whether this atom reads a public context is decided from its compiled
    // contract, not from what the caller brought: the two must agree exactly.
    // A contextual atom is refused before anything renders when no snapshot
    // came, so no callback of its turn ever sees a `context` that is simply
    // absent; a snapshot is refused for an atom that declares no context for it
    // to be checked against.
    let declared = compiled.program.contracts.get("Context");
    if declared.is_none() && snapshot.is_some() {
        return Err(RenderError(
            "this atom declares no public context".to_owned(),
        ));
    }
    if declared.is_some() && snapshot.is_none() {
        return Err(RenderError("context snapshot required".to_owned()));
    }
    if let (Some(schema), Some(snapshot)) = (declared, snapshot) {
        jsonschema::validator_for(schema)
            .map_err(|e| RenderError(e.to_string()))?
            .validate(&snapshot.as_value()["context"])
            .map_err(|e| RenderError(format!("context: {e}")))?;
        snapshot
            .verify_for(&compiled.hash, run.run_id)
            .map_err(RenderError)?;
    }
    let result = compiled
        .program
        .invoke_clocked(
            "render",
            json!({"input":run.input,"options":{"runId":run.run_id,"origin":run.origin,"origins":run.origins,"contextSnapshot":snapshot,"stepId":step_id}}),
            clock.map(|clock| (clock, "render")),
        )
        .map_err(|e| RenderError(e.to_string()))?;
    serde_json::from_value(result).map_err(|e| RenderError(e.to_string()))
}

#[cfg(test)]
mod helper_tests;
#[cfg(test)]
mod tests;
