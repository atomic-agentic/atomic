// SPDX-License-Identifier: Apache-2.0

//! The schema registry.
//!
//! Schemas are JSON Schema 2020-12, written as YAML (`*.schema.yaml`, read with
//! the restricted loader) or JSON (`*.schema.json`). Each is registered under
//! its `$id`, or, without one, under the `file://` URI of its file. A `$ref`
//! resolves against its schema's base URI, as JSON Schema defines, and every
//! reference must name a registered schema: loading never fetches anything
//! from the network.
//!
//! Two schemas may not share an `$id`, and one schema may not exist as both a
//! `.yaml` and a `.json` file.

use crate::yaml;
use jsonschema::{Draft, Retrieve, Uri, Validator};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One registered schema.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Its URI: the `$id`, or `file://<canonical path>`.
    pub id: String,
    /// The file it came from.
    pub file: PathBuf,
    /// The schema as JSON.
    pub schema: Value,
}

/// Why a schema could not be registered or compiled.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// The file could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The file is not valid restricted YAML.
    #[error(transparent)]
    Yaml(#[from] yaml::YamlError),
    /// The file is not valid JSON.
    #[error("{}: {source}", path.display())]
    Json {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        source: serde_json::Error,
    },
    /// The file name is not a schema file name.
    #[error("{}: a schema file is named *.schema.yaml or *.schema.json", .0.display())]
    NotASchemaFile(PathBuf),
    /// One schema exists as both YAML and JSON.
    #[error("{} and {} are the same schema written twice; keep one", yaml.display(), json.display())]
    BothForms {
        /// The YAML file.
        yaml: PathBuf,
        /// The JSON file.
        json: PathBuf,
    },
    /// Two schemas share one `$id`.
    #[error("{} and {} both have $id {id}", first.display(), second.display())]
    DuplicateId {
        /// The id.
        id: String,
        /// The schema registered first.
        first: PathBuf,
        /// The schema that collided with it.
        second: PathBuf,
    },
    /// No schema is registered under an id.
    #[error("no schema registered as {0}")]
    Unknown(String),
    /// The schema is not valid JSON Schema, or a reference does not resolve.
    #[error("schema {id}: {message}")]
    Invalid {
        /// The schema's id.
        id: String,
        /// What is wrong.
        message: String,
    },
}

/// Schemas by id.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    entries: BTreeMap<String, Entry>,
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// The other form of a schema file (`.yaml` for `.json` and back).
fn twin(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let other = if let Some(stem) = name.strip_suffix(".schema.yaml") {
        format!("{stem}.schema.json")
    } else if let Some(stem) = name.strip_suffix(".schema.json") {
        format!("{stem}.schema.yaml")
    } else {
        return None;
    };
    Some(path.with_file_name(other))
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read, parse and register the schema at `path`. Returns its id.
    pub fn load_file(&mut self, path: &Path) -> Result<String, SchemaError> {
        let path = std::fs::canonicalize(path).map_err(|source| SchemaError::Read {
            path: path.to_owned(),
            source,
        })?;
        let other = twin(&path).ok_or_else(|| SchemaError::NotASchemaFile(path.clone()))?;
        if other.is_file() {
            let (yaml, json) = if path.to_string_lossy().ends_with(".yaml") {
                (path.clone(), other)
            } else {
                (other, path.clone())
            };
            return Err(SchemaError::BothForms { yaml, json });
        }
        let text = std::fs::read_to_string(&path).map_err(|source| SchemaError::Read {
            path: path.clone(),
            source,
        })?;
        let schema = if path.to_string_lossy().ends_with(".yaml") {
            yaml::load(&path, &text)?.value
        } else {
            serde_json::from_str(&text).map_err(|source| SchemaError::Json {
                path: path.clone(),
                source,
            })?
        };
        self.register(path, schema)
    }

    /// Register `schema`, which came from `file`. Returns its id.
    pub fn register(&mut self, file: PathBuf, schema: Value) -> Result<String, SchemaError> {
        let id = match schema.get("$id").and_then(Value::as_str) {
            Some(id) => id.trim_end_matches('#').to_owned(),
            None => file_uri(&file),
        };
        if let Some(first) = self.entries.get(&id)
            && first.file != file
        {
            return Err(SchemaError::DuplicateId {
                id,
                first: first.file.clone(),
                second: file,
            });
        }
        // Compile it now, so a bad schema fails when it loads.
        self.entries.insert(
            id.clone(),
            Entry {
                id: id.clone(),
                file,
                schema,
            },
        );
        // A reference to a schema not loaded yet is checked later, by `check`.
        if let Err(e) = self.validator(&id)
            && !matches!(e, SchemaError::Invalid { ref message, .. } if message.contains("no schema registered"))
        {
            self.entries.remove(&id);
            return Err(e);
        }
        Ok(id)
    }

    /// The registered schema with this id.
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.get(id.trim_end_matches('#'))
    }

    /// Every registered schema.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    /// A validator for the full contract of schema `id`.
    pub fn validator(&self, id: &str) -> Result<Validator, SchemaError> {
        let entry = self
            .get(id)
            .ok_or_else(|| SchemaError::Unknown(id.to_owned()))?;
        let known: Arc<BTreeMap<String, Value>> = Arc::new(
            self.entries
                .iter()
                .map(|(k, e)| (k.clone(), e.schema.clone()))
                .collect(),
        );
        jsonschema::options()
            .with_draft(Draft::Draft202012)
            .with_base_uri(entry.id.clone())
            .with_retriever(Registered(known))
            .build(&entry.schema)
            .map_err(|e| SchemaError::Invalid {
                id: entry.id.clone(),
                message: e.to_string(),
            })
    }
}

impl Registry {
    /// Compile every registered schema, so every reference is checked once
    /// all schemas are loaded.
    pub fn check(&self) -> Result<(), SchemaError> {
        for id in self.entries.keys() {
            self.validator(id)?;
        }
        Ok(())
    }
}

/// The sidecar schema for one role of an atom: for `review.atom.md` and role
/// `input`, `review.input.schema.yaml` or `review.input.schema.json` beside it.
/// Both existing is an error.
pub fn sidecar(atom: &Path, role: &str) -> Result<Option<PathBuf>, SchemaError> {
    let Some(name) = atom.file_name().and_then(|n| n.to_str()) else {
        return Ok(None);
    };
    let stem = name.split(".atom.").next().unwrap_or(name);
    let yaml = atom.with_file_name(format!("{stem}.{role}.schema.yaml"));
    let json = atom.with_file_name(format!("{stem}.{role}.schema.json"));
    match (yaml.is_file(), json.is_file()) {
        (true, true) => Err(SchemaError::BothForms { yaml, json }),
        (true, false) => Ok(Some(yaml)),
        (false, true) => Ok(Some(json)),
        (false, false) => Ok(None),
    }
}

/// Serves registered schemas, and nothing else.
struct Registered(Arc<BTreeMap<String, Value>>);

impl Retrieve for Registered {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let key = uri.as_str().split('#').next().unwrap_or_default();
        self.0.get(key).cloned().ok_or_else(|| {
            format!("no schema registered as {key}; schemas are never fetched").into()
        })
    }
}

#[cfg(test)]
mod tests;
