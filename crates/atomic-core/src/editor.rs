// SPDX-License-Identifier: Apache-2.0

//! JSON Schemas for editors: `atomic schemas` writes one for workflow files
//! and one for the manifest into the project's generated directory. A
//! `# yaml-language-server: $schema=…` comment at the top of a file points an
//! editor at them, for completion and inline errors. The workflow schema is
//! the project's own: it lists the atoms a step may name, the input fields
//! each atom's `with` may map, and the schema IDs a contract may reference.
//! `atomic check` stays authoritative; these schemas only help an editor.

use crate::atom::Atom;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The workflow-file schema for a project with `atoms` and the schema `ids`
/// its registry holds.
pub fn workflow_schema(atoms: &BTreeMap<String, Atom>, ids: &[String]) -> Value {
    let names: Vec<&str> = atoms.keys().map(String::as_str).collect();
    let atom = if names.is_empty() {
        json!({ "type": "string", "description": "The atom this step runs, by name." })
    } else {
        json!({ "enum": names, "description": "The atom this step runs, by name." })
    };
    // Each atom's input fields are the keys its step may map.
    let mut by_atom = Vec::new();
    for (name, atom) in atoms {
        let fields: Vec<String> = atom.program.contracts["Input"]["properties"]
            .as_object()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default();
        if fields.is_empty() {
            continue;
        }
        let described: Map<String, Value> = fields
            .iter()
            .map(|field| {
                let doc = atom.program.contracts["Input"]["properties"][field]["description"]
                    .as_str()
                    .map(|d| format!("{d} A CEL expression."))
                    .unwrap_or_else(|| format!("`{name}` input `{field}`, as a CEL expression."));
                (
                    field.clone(),
                    json!({ "type": "string", "description": doc }),
                )
            })
            .collect();
        by_atom.push(json!({
            "if": { "properties": { "atom": { "const": name } }, "required": ["atom"] },
            "then": { "properties": {
                "with": { "type": "object", "properties": described, "additionalProperties": false },
                "origins": { "type": "object", "propertyNames": { "enum": fields } },
            } },
        }));
    }
    let reference = if ids.is_empty() {
        json!({ "type": "string" })
    } else {
        json!({ "anyOf": [{ "enum": ids }, { "type": "string" }] })
    };
    let contract = json!({
        "description": "A full JSON Schema, or `{ $ref: <file or schema ID> }`.",
        "type": "object",
        "properties": { "$ref": reference },
    });
    let mut step = Map::new();
    for key in crate::workflow::STEP {
        let value = match *key {
            "atom" => atom.clone(),
            "gate" => json!({ "$ref": "#/$defs/gate" }),
            "after" => {
                json!({ "type": "array", "items": { "type": "string" }, "description": "Steps this one waits for." })
            }
            "with" => {
                json!({ "type": "object", "additionalProperties": { "type": "string" }, "description": "Input fields, each a CEL expression." })
            }
            "origins" => {
                json!({ "type": "object", "additionalProperties": { "enum": ["external", "agent", "team"] }, "description": "Lower a mapped field's origin." })
            }
            "worktree" => {
                json!({ "type": "boolean", "description": "Run a codex atom in a worktree at its mapped `base`." })
            }
            other => json!({ "description": format!("`{other}`") }),
        };
        step.insert((*key).to_owned(), value);
    }
    let mut gate = Map::new();
    for key in crate::workflow::GATE {
        let value = match *key {
            "approvers" => json!({ "type": "array", "items": { "type": "string" } }),
            "timeout" => json!({ "type": "string", "description": "Such as `2h` or `3d`." }),
            _ => json!({ "type": "string", "description": "A CEL expression." }),
        };
        gate.insert((*key).to_owned(), value);
    }
    let mut top = Map::new();
    for key in crate::workflow::TOP {
        let value = match *key {
            "atomic" => json!({ "const": crate::project::FORMAT_VERSION }),
            "name" => json!({ "type": "string" }),
            "version" => json!({ "type": "integer", "minimum": 1 }),
            "input" | "output" => json!({ "$ref": "#/$defs/contract" }),
            "steps" => {
                json!({ "type": "object", "additionalProperties": { "$ref": "#/$defs/step" } })
            }
            "returns" => {
                json!({ "type": "string", "description": "A CEL expression for the result." })
            }
            "budget" => json!({ "type": "object", "additionalProperties": false, "properties": {
                "tokens": { "type": "integer", "minimum": 0 },
                "minutes": { "type": "number", "minimum": 0 },
                "cost_usd": { "type": "number", "minimum": 0 },
            } }),
            other => json!({ "description": format!("`{other}`") }),
        };
        top.insert((*key).to_owned(), value);
    }
    let mut step = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": step,
        "oneOf": [{ "required": ["atom"] }, { "required": ["gate"] }],
    });
    if !by_atom.is_empty() {
        step["allOf"] = Value::Array(by_atom);
    }
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "atomic workflow",
        "type": "object",
        "additionalProperties": false,
        "required": ["atomic", "steps"],
        "properties": top,
        "$defs": {
            "contract": contract,
            "step": step,
            "gate": { "type": "object", "additionalProperties": false, "required": ["title"], "properties": gate },
        },
    })
}

/// The manifest schema.
pub fn manifest_schema() -> Value {
    let collections: Map<String, Value> = crate::project::DEFAULT_COLLECTIONS
        .iter()
        .map(|(name, globs)| {
            (
                (*name).to_owned(),
                json!({ "type": "array", "items": { "type": "string" }, "description": format!("Globs; the default is {}.", globs.join(", ")) }),
            )
        })
        .collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "atomic manifest",
        "type": "object",
        "additionalProperties": false,
        "required": ["atomic"],
        "properties": {
            "atomic": { "const": crate::project::FORMAT_VERSION },
            "name": { "type": "string", "minLength": 1 },
            "collections": { "type": "object", "additionalProperties": false, "properties": collections },
            "ignore": { "type": "array", "items": { "type": "string" } },
            "generated": { "type": "string", "description": "Where generated files go; the default is .atomic/cache." },
            "workspace": {
                "type": "object", "additionalProperties": false, "required": ["members"],
                "properties": { "members": { "type": "array", "minItems": 1, "items": { "type": "string" }, "description": "Member directories, as globs." } },
            },
        },
    })
}

#[cfg(test)]
mod tests;
