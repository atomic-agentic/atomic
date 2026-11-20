// SPDX-License-Identifier: Apache-2.0

//! Layered configuration.
//!
//! Each layer overrides the one below it:
//!
//! | Layer | Source | Required |
//! | --- | --- | --- |
//! | 1 | Built-in defaults | always present |
//! | 2 | `atomic.yaml` | no |
//! | 3 | `atomic.local.yaml` (personal, not committed) | no |
//! | 4 | `atomic.<profile>.yaml`, when a profile is selected | yes, if selected |
//! | 5 | `ATOMIC_*` environment variables | no |
//! | 6 | Command-line flags | no |
//!
//! Files are read with the restricted YAML loader and merged key by key:
//! mappings merge, anything else replaces. A key the configuration does not
//! define is refused with its file and line, so a typo is never silently
//! ignored.
//!
//! An environment variable names a setting by its path with `ATOMIC_` in front:
//! `model.base_url` is `ATOMIC_MODEL_BASE_URL`. Names are forgiving: case and
//! the separators `_`, `-` and `.` are ignored when matching, so
//! `ATOMIC_MODEL_BASEURL` works too. Variables that match no setting are left
//! alone; atomic uses other `ATOMIC_*` variables for other purposes.
//!
//! Every resolved value records the layer it came from, for `--explain`.

use atomic_schema::yaml;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where a value came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The built-in default.
    Default,
    /// A configuration file, at a line.
    File {
        /// The file.
        path: PathBuf,
        /// The line the value is on.
        line: usize,
    },
    /// An environment variable.
    Env(String),
    /// A command-line flag.
    Flag(String),
    /// Found in the surroundings, for the reason given, where nothing set it.
    Detected(String),
}

/// What to read, and from where.
pub struct Layers<'a> {
    /// The directory holding `atomic.yaml` and its siblings.
    pub dir: &'a Path,
    /// The selected profile, if any.
    pub profile: Option<&'a str>,
    /// Environment lookup, passed in so tests do not depend on the real one.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Names of the environment variables to consider (for example, every
    /// variable in the process environment).
    pub env_names: Vec<String>,
    /// Flag overrides: a dotted setting path and its value.
    pub flags: Vec<(String, Value)>,
}

/// The resolved configuration and where each setting came from.
#[derive(Debug)]
pub struct Resolved<T> {
    /// The configuration.
    pub config: T,
    /// The source of every leaf setting, by dotted path.
    pub sources: BTreeMap<String, Source>,
}

/// Why the configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A file is not valid restricted YAML.
    #[error(transparent)]
    Yaml(#[from] yaml::YamlError),
    /// A file sets something the configuration does not define, or gives it
    /// the wrong shape.
    #[error("{}:{line}: {message}", path.display())]
    Setting {
        /// The file.
        path: PathBuf,
        /// The line.
        line: usize,
        /// What is wrong.
        message: String,
    },
    /// An environment variable or flag has a value of the wrong type.
    #[error("{name}={value:?}: {message}")]
    Value {
        /// The variable or flag.
        name: String,
        /// The value given.
        value: String,
        /// What was expected.
        message: String,
    },
    /// A flag names a setting the configuration does not define.
    #[error("no setting {0}")]
    UnknownFlag(String),
    /// The merged configuration does not fit the configuration type.
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// Resolve a configuration of type `T` through every layer.
pub fn resolve<T>(layers: &Layers<'_>) -> Result<Resolved<T>, ConfigError>
where
    T: Default + Serialize + DeserializeOwned,
{
    let defaults =
        serde_json::to_value(T::default()).map_err(|e| ConfigError::Invalid(e.to_string()))?;
    let mut value = defaults.clone();
    let mut sources = BTreeMap::new();
    record(&defaults, "", &Source::Default, &mut sources);

    let mut files = vec![
        (layers.dir.join("atomic.yaml"), false),
        (layers.dir.join("atomic.local.yaml"), false),
    ];
    if let Some(profile) = layers.profile {
        files.push((layers.dir.join(format!("atomic.{profile}.yaml")), true));
    }
    for (path, required) in files {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => continue,
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        let doc = yaml::load(&path, &text)?;
        if doc.value.is_null() {
            continue;
        }
        merge_file(
            &mut value,
            &defaults,
            &doc,
            "",
            &doc.value,
            &path,
            &mut sources,
        )?;
    }

    let paths = leaf_paths(&defaults);
    let mut env_names = layers.env_names.clone();
    env_names.sort();
    for name in env_names {
        let Some(rest) = name.strip_prefix("ATOMIC_") else {
            continue;
        };
        let wanted = normalize(rest);
        let Some(path) = paths.iter().find(|p| normalize(p) == wanted) else {
            continue;
        };
        let Some(raw) = (layers.env)(&name) else {
            continue;
        };
        let target = pointer_get(&defaults, path).cloned().unwrap_or(Value::Null);
        let parsed = parse_like(&target, &raw).map_err(|message| ConfigError::Value {
            name: name.clone(),
            value: raw.clone(),
            message,
        })?;
        pointer_set(&mut value, path, parsed);
        sources.insert(path.clone(), Source::Env(name.clone()));
    }

    for (flag, v) in &layers.flags {
        let path = paths
            .iter()
            .find(|p| p.as_str() == flag)
            .ok_or_else(|| ConfigError::UnknownFlag(flag.clone()))?;
        pointer_set(&mut value, path, v.clone());
        sources.insert(path.clone(), Source::Flag(flag.clone()));
    }

    let config = serde_json::from_value(value).map_err(|e| ConfigError::Invalid(e.to_string()))?;
    Ok(Resolved { config, sources })
}

fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '_' | '-' | '.'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Dotted paths of every leaf (non-mapping) value in `v`.
fn leaf_paths(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, prefix: &str, out: &mut Vec<String>) {
        match v {
            Value::Object(m) if !m.is_empty() => {
                for (k, child) in m {
                    let p = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(child, &p, out);
                }
            }
            _ => out.push(prefix.to_owned()),
        }
    }
    walk(v, "", &mut out);
    out
}

fn record(v: &Value, prefix: &str, source: &Source, sources: &mut BTreeMap<String, Source>) {
    for p in leaf_paths(v) {
        let full = match (prefix.is_empty(), p.is_empty()) {
            (true, _) => p,
            (false, true) => prefix.to_owned(),
            (false, false) => format!("{prefix}.{p}"),
        };
        sources.insert(full, source.clone());
    }
}

fn pointer_get<'v>(v: &'v Value, dotted: &str) -> Option<&'v Value> {
    dotted.split('.').try_fold(v, |v, k| v.get(k))
}

fn pointer_set(v: &mut Value, dotted: &str, new: Value) {
    let mut cur = v;
    let keys: Vec<&str> = dotted.split('.').collect();
    for (i, k) in keys.iter().enumerate() {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        let map = cur.as_object_mut().expect("just made an object");
        if i + 1 == keys.len() {
            map.insert((*k).to_owned(), new);
            return;
        }
        cur = map
            .entry((*k).to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
    }
}

/// Merge one file's mapping into `value`, refusing keys the defaults do not
/// have. An empty mapping in the defaults is open: any keys may be set there.
#[allow(clippy::too_many_arguments)]
fn merge_file(
    value: &mut Value,
    defaults: &Value,
    doc: &yaml::Document,
    pointer: &str,
    incoming: &Value,
    path: &Path,
    sources: &mut BTreeMap<String, Source>,
) -> Result<(), ConfigError> {
    let line = doc.position(pointer).map_or(1, |p| p.line);
    let (Value::Object(into), Value::Object(from)) = (&mut *value, incoming) else {
        return Err(ConfigError::Setting {
            path: path.to_owned(),
            line,
            message: "expected a mapping".into(),
        });
    };
    let open = defaults.as_object().is_some_and(Map::is_empty);
    for (k, v) in from {
        let child_pointer = format!("{pointer}/{}", k.replace('~', "~0").replace('/', "~1"));
        let child_line = doc.position(&child_pointer).map_or(line, |p| p.line);
        let dotted = child_pointer.trim_start_matches('/').replace('/', ".");
        let default_child = defaults.get(k);
        if default_child.is_none() && !open {
            return Err(ConfigError::Setting {
                path: path.to_owned(),
                line: child_line,
                message: format!("unknown setting {dotted:?}"),
            });
        }
        match (default_child, v) {
            (Some(d @ Value::Object(dm)), Value::Object(_)) if !dm.is_empty() => {
                let slot = into
                    .entry(k.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
                merge_file(slot, d, doc, &child_pointer, v, path, sources)?;
            }
            (Some(Value::Object(dm)), other) if !dm.is_empty() => {
                return Err(ConfigError::Setting {
                    path: path.to_owned(),
                    line: child_line,
                    message: format!("{dotted} is a section, not a value (found {other})"),
                });
            }
            _ => {
                into.insert(k.clone(), v.clone());
                sources.insert(
                    dotted,
                    Source::File {
                        path: path.to_owned(),
                        line: child_line,
                    },
                );
            }
        }
    }
    Ok(())
}

/// Parse an environment value as the type its default has.
fn parse_like(target: &Value, raw: &str) -> Result<Value, String> {
    match target {
        Value::Bool(_) => match raw.to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(Value::Bool(true)),
            "false" | "0" => Ok(Value::Bool(false)),
            _ => Err("expected true or false".into()),
        },
        Value::Number(n) if n.is_u64() || n.is_i64() => raw
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| "expected an integer".into()),
        Value::Number(_) => raw
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| "expected a number".into()),
        Value::Array(_) | Value::Object(_) => {
            serde_json::from_str(raw).map_err(|e| format!("expected JSON: {e}"))
        }
        Value::String(_) => Ok(Value::String(raw.to_owned())),
        // An optional setting (default null): read the value as a YAML
        // scalar, so numbers and booleans keep their type; else a string.
        _ => Ok(yaml::load(Path::new("environment"), raw)
            .ok()
            .map(|d| d.value)
            .filter(|v| !v.is_object() && !v.is_array())
            .unwrap_or_else(|| Value::String(raw.to_owned()))),
    }
}

#[cfg(test)]
mod tests;
