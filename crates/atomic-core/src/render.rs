// SPDX-License-Identifier: Apache-2.0

//! Rendering a compiled atom for one run.
//!
//! The developer instructions are the atom's own text: `{% if %}` blocks are
//! kept or dropped by evaluating their CEL condition against the input, and
//! each `{% $input… %}` value is replaced by a pointer to where it appears.
//! The values themselves go only into the user turn, each inside a wrapper
//! that names its path and origin:
//!
//! ```text
//! <data-3f9a1c2b path="input.text" origin="external">…the value, unchanged…</data-3f9a1c2b>
//! ```
//!
//! The suffix is derived from the run and the field, and re-derived until it
//! does not occur in the value, so a value can never close its own wrapper.
//! The value is inserted exactly as it is: nothing is escaped, so code and
//! data arrive uncorrupted.

use crate::compile::Compiled;
use crate::tags::{Piece, Tag};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// One run's rendered prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// The developer instructions: no inserted values, only pointers to them.
    pub developer: String,
    /// The user turn: every inserted value, wrapped.
    pub user: String,
}

/// Why an atom could not be rendered for this input.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RenderError {
    /// A condition could not be evaluated, or is not a boolean.
    #[error("{{% if {condition} %}}: {message}")]
    Condition {
        /// The condition.
        condition: String,
        /// What went wrong.
        message: String,
    },
    /// A value path is not in this input.
    #[error("{path} is not in this input")]
    Missing {
        /// The path.
        path: String,
    },
}

/// What the caller says about the input it supplies.
#[derive(Clone, Debug)]
pub struct RunInput<'a> {
    /// The run's identity: wrapper suffixes are derived from it.
    pub run_id: &'a str,
    /// The input value.
    pub input: &'a Value,
    /// The atom's frontmatter.
    pub frontmatter: &'a Value,
    /// Where the input came from: `team`, `agent` or `external`.
    pub origin: &'a str,
}

/// Render `compiled` for one run.
pub fn render(compiled: &Compiled, run: &RunInput<'_>) -> Result<Rendered, RenderError> {
    let mut developer = String::new();
    let mut user = String::new();
    let mut inserted: Vec<String> = Vec::new();
    // Each open `if`: whether its branch is being emitted.
    let mut stack: Vec<(bool, bool)> = Vec::new(); // (this branch active, condition was true)
    let active = |stack: &[(bool, bool)]| stack.iter().all(|(a, _)| *a);

    for piece in &compiled.pieces {
        match piece {
            Piece::Text(t) => {
                if active(&stack) {
                    developer.push_str(t);
                }
            }
            Piece::Tag(tag, _) => match tag {
                Tag::If { condition } => {
                    let truth = if active(&stack) {
                        evaluate(condition, run)?
                    } else {
                        false
                    };
                    stack.push((truth, truth));
                }
                Tag::Else => {
                    if let Some(top) = stack.last_mut() {
                        top.0 = !top.1;
                    }
                }
                Tag::EndIf => {
                    stack.pop();
                }
                Tag::Value { root, path } if active(&stack) => {
                    let (value, origin) = match root.as_str() {
                        "input" => (run.input, run.origin),
                        "frontmatter" => (run.frontmatter, "atom"),
                        _ => return Err(RenderError::Missing { path: root.clone() }),
                    };
                    let dotted = std::iter::once(root.clone())
                        .chain(path.iter().cloned())
                        .collect::<Vec<_>>()
                        .join(".");
                    let found = lookup(value, path).ok_or_else(|| RenderError::Missing {
                        path: dotted.clone(),
                    })?;
                    let text = match found {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let suffix = suffix_for(run.run_id, &dotted, &text);
                    developer
                        .push_str(&format!("[{dotted}: see <data-{suffix}> in the user turn]"));
                    if !inserted.contains(&dotted) {
                        inserted.push(dotted.clone());
                        user.push_str(&format!(
                            "<data-{suffix} path=\"{dotted}\" origin=\"{origin}\">{text}</data-{suffix}>\n"
                        ));
                    }
                }
                Tag::Value { .. } | Tag::Partial { .. } | Tag::Example { .. } | Tag::EndExample => {
                }
            },
        }
    }
    Ok(Rendered { developer, user })
}

fn lookup<'v>(value: &'v Value, path: &[String]) -> Option<&'v Value> {
    path.iter().try_fold(value, |v, seg| match v {
        Value::Array(items) => seg.parse::<usize>().ok().and_then(|i| items.get(i)),
        _ => v.get(seg),
    })
}

/// A wrapper suffix for this run and field that does not occur in `value`.
fn suffix_for(run_id: &str, path: &str, value: &str) -> String {
    let mut counter = 0u64;
    loop {
        let digest = Sha256::digest(format!("{run_id}\0{path}\0{counter}").as_bytes());
        let suffix = format!("{digest:x}")[..8].to_owned();
        if !value.contains(&format!("data-{suffix}")) {
            return suffix;
        }
        counter += 1;
    }
}

fn evaluate(condition: &str, run: &RunInput<'_>) -> Result<bool, RenderError> {
    let err = |message: String| RenderError::Condition {
        condition: condition.to_owned(),
        message,
    };
    let program = cel::Program::compile(condition).map_err(|e| err(e.to_string()))?;
    let mut context = cel::Context::default();
    context
        .add_variable("input", run.input)
        .map_err(|e| err(e.to_string()))?;
    context
        .add_variable("frontmatter", run.frontmatter)
        .map_err(|e| err(e.to_string()))?;
    match program.execute(&context).map_err(|e| err(e.to_string()))? {
        cel::Value::Bool(b) => Ok(b),
        other => Err(err(format!(
            "the condition is {other:?}, not true or false"
        ))),
    }
}

#[cfg(test)]
mod tests;
