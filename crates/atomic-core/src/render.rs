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
    jsonschema::validator_for(&compiled.program.contracts["Input"])
        .map_err(|e| RenderError(e.to_string()))?
        .validate(run.input)
        .map_err(|e| RenderError(format!("input: {e}")))?;
    if let Some(snapshot) = snapshot {
        jsonschema::validator_for(&compiled.program.contracts["Context"])
            .map_err(|e| RenderError(e.to_string()))?
            .validate(&snapshot.as_value()["context"])
            .map_err(|e| RenderError(format!("context: {e}")))?;
        snapshot
            .verify_for(&compiled.hash, run.run_id)
            .map_err(RenderError)?;
    }
    let result = compiled.program.invoke("render", json!({"input":run.input,"options":{"runId":run.run_id,"origin":run.origin,"contextSnapshot":snapshot,"stepId":step_id}})).map_err(RenderError)?;
    serde_json::from_value(result).map_err(|e| RenderError(e.to_string()))
}

#[cfg(test)]
mod tests;
