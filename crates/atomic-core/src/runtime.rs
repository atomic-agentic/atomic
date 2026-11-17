// SPDX-License-Identifier: Apache-2.0

//! Running a typed atom on Codex.
//!
//! [`run_codex_atom`] runs one atom as one Codex turn:
//!
//! 1. the run settings start from the project's defaults and the atom
//!    adjusts them in `configure`;
//! 2. the output contract is lowered to the generation profile, which becomes
//!    the turn's structured-output schema;
//! 3. the atom's instructions are the developer instructions and the
//!    serialized input is the user turn;
//! 4. the final message is parsed, the nulls lowering introduced are removed,
//!    and the result is validated against the full contract, deserialized into
//!    `Out` and checked by the atom's own `validate`.
//!
//! A final message that does not fit gets one repair turn on the same thread,
//! quoting exactly what was wrong; if the repaired answer does not fit
//! either, the atom fails with [`AtomError::Schema`] (or
//! [`AtomError::Validation`] when only its own check failed).

use crate::codex::{self, CodexPaths, ModelEndpoint, TurnSpec};
use crate::typed::{Atom, AtomConfig, AtomError};
use atomic_schema::lower::{self, OPENAI_STRICT};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Where and with what model atoms run.
#[derive(Clone, Debug)]
pub struct CodexRuntime {
    /// Paths for Codex's helpers.
    pub paths: CodexPaths,
    /// Codex's home for atom threads, inside the project's state directory.
    pub codex_home: PathBuf,
    /// The working directory threads run in.
    pub cwd: PathBuf,
    /// The model endpoint.
    pub endpoint: ModelEndpoint,
    /// The project's default run settings.
    pub defaults: AtomConfig,
}

/// What a run records beside its output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunMetadata {
    /// The model the turn used.
    pub model: String,
    /// SHA-256 over the developer instructions, the user turn and the output
    /// schema, so a run can be matched to exactly what was asked.
    pub prompt_hash: String,
    /// How many turns it took (more than one when a repair turn ran).
    pub turns: u32,
}

/// Run `atom` on `input` as one Codex turn.
pub async fn run_codex_atom<A: Atom>(
    atom: &A,
    input: &A::In,
    runtime: &CodexRuntime,
) -> Result<(A::Out, RunMetadata), AtomError> {
    let mut config = runtime.defaults.clone();
    atom.configure(&mut config);

    let contract = atom.output_schema();
    let lowered = lower::lower(&contract, &OPENAI_STRICT).map_err(|e| {
        AtomError::Runtime(format!(
            "the output schema of {} cannot be lowered: {e}",
            A::NAME
        ))
    })?;
    let validator = jsonschema::validator_for(&contract).map_err(|e| {
        AtomError::Runtime(format!(
            "the output schema of {} is not valid: {e}",
            A::NAME
        ))
    })?;

    let prompt = serde_json::to_string_pretty(input)
        .map_err(|e| AtomError::Runtime(format!("serialize the input: {e}")))?;
    let spec = TurnSpec {
        developer_instructions: Some(atom.instructions().into_owned()),
        prompt,
        output_schema: Some(lowered.schema.clone()),
        model: config.model.clone(),
    };
    let model = spec
        .model
        .clone()
        .unwrap_or_else(|| runtime.endpoint.model.clone());
    let mut h = Sha256::new();
    for part in [
        spec.developer_instructions.as_deref().unwrap_or_default(),
        &spec.prompt,
        &lowered.schema.to_string(),
    ] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    let prompt_hash = format!("{:x}", h.finalize());

    let mut out = None;
    let mut failure = None;
    let mut repairs_left = 1;
    let (_, turns) = codex::converse(
        &runtime.paths,
        &runtime.codex_home,
        &runtime.cwd,
        &runtime.endpoint,
        &spec,
        |answer| match accept::<A>(atom, answer, &lowered.nulls_added, &validator) {
            Ok(o) => {
                out = Some(o);
                None
            }
            Err(e @ (AtomError::Schema { .. } | AtomError::Validation(_))) if repairs_left > 0 => {
                repairs_left -= 1;
                let problem = match &e {
                    AtomError::Schema { message, .. } => message.clone(),
                    other => other.to_string(),
                };
                Some(TurnSpec {
                    prompt: format!(
                        "Your answer did not fit the required output: {problem}\n\
                         Reply again with only the corrected JSON, following the same schema."
                    ),
                    output_schema: Some(lowered.schema.clone()),
                    ..TurnSpec::default()
                })
            }
            Err(e) => {
                failure = Some(e);
                None
            }
        },
    )
    .await
    .map_err(|e| AtomError::Model(format!("{e:#}")))?;
    let metadata = RunMetadata {
        model,
        prompt_hash,
        turns,
    };
    match (out, failure) {
        (Some(o), _) => Ok((o, metadata)),
        (None, Some(e)) => Err(e),
        (None, None) => Err(AtomError::Runtime(
            "the turn ended without an answer".into(),
        )),
    }
}

/// Turn a final message into `Out`, or say exactly why it does not fit.
pub(crate) fn accept<A: Atom>(
    atom: &A,
    answer: &str,
    nulls_added: &[String],
    validator: &jsonschema::Validator,
) -> Result<A::Out, AtomError> {
    let schema_error = |message: String| AtomError::Schema {
        message,
        answer: answer.to_owned(),
    };
    let mut value: Value = serde_json::from_str(answer.trim())
        .map_err(|e| schema_error(format!("the answer is not JSON: {e}")))?;
    lower::normalize(&mut value, nulls_added);
    let errors: Vec<String> = validator
        .iter_errors(&value)
        .map(|e| {
            format!(
                "{}: {e}",
                if e.instance_path().to_string().is_empty() {
                    "/".to_owned()
                } else {
                    e.instance_path().to_string()
                }
            )
        })
        .collect();
    if !errors.is_empty() {
        return Err(schema_error(errors.join("; ")));
    }
    let out: A::Out = serde_json::from_value(value)
        .map_err(|e| schema_error(format!("the answer does not deserialize: {e}")))?;
    atom.validate(&out)?;
    Ok(out)
}

#[cfg(test)]
mod tests;
