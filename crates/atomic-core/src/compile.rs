// SPDX-License-Identifier: Apache-2.0

//! Compilation metadata for immutable Astro source graphs.

use crate::astro::Program;
use crate::atom::{Atom, AtomError};
use atomic_schema::registry::Registry;
use serde_json::Value;

/// A schema-validated few-shot example.
#[derive(Clone, Debug, PartialEq)]
pub struct Example {
    /// Example name.
    pub name: String,
    /// Example input.
    pub input: Value,
    /// Expected output.
    pub output: Value,
}

/// A checked Astro atom ready to render without reading source files.
#[derive(Clone, Debug)]
pub struct Compiled {
    /// Digest of the entry and all imported components.
    pub hash: String,
    /// Captured, immutable sources.
    pub program: Program,
    /// Checked examples declared in config.
    pub examples: Vec<Example>,
}

/// Compile a loaded atom, checking every declared example against the full contracts.
pub fn compile(atom: &Atom, _registry: &Registry) -> Result<Compiled, AtomError> {
    let fail = |message| AtomError {
        file: atom.file.clone(),
        line: 1,
        path: "/examples".into(),
        message,
    };
    let mut examples = Vec::new();
    if let Some(values) = atom.frontmatter.get("examples") {
        let values = values
            .as_array()
            .ok_or_else(|| fail("examples must be an array".into()))?;
        let input = jsonschema::validator_for(&atom.program.contracts["Input"])
            .map_err(|e| fail(e.to_string()))?;
        let output = jsonschema::validator_for(&atom.program.contracts["Output"])
            .map_err(|e| fail(e.to_string()))?;
        for value in values {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| fail("example needs a name".into()))?;
            let i = value
                .get("input")
                .ok_or_else(|| fail("example needs input".into()))?;
            let o = value
                .get("output")
                .ok_or_else(|| fail("example needs output".into()))?;
            input
                .validate(i)
                .map_err(|e| fail(format!("example {name} input: {e}")))?;
            output
                .validate(o)
                .map_err(|e| fail(format!("example {name} output: {e}")))?;
            examples.push(Example {
                name: name.into(),
                input: i.clone(),
                output: o.clone(),
            });
        }
    }
    Ok(Compiled {
        hash: atom.program.hash(),
        program: atom.program.clone(),
        examples,
    })
}

#[cfg(test)]
mod tests;
