// SPDX-License-Identifier: Apache-2.0

//! The compact schema form: the YAML syntax Dotprompt calls Picoschema,
//! compiled to JSON Schema 2020-12 when a schema loads.
//!
//! ```yaml
//! verdict(enum): [approve, request_changes, reject]
//! summary: string, at most 600 characters
//! findings(array):
//!   file: string
//!   line?: integer
//!   severity(enum): [blocker, major, minor, nit]
//!   message: string
//! ```
//!
//! A field is `name: type, description`, with `string`, `integer`, `number`,
//! `boolean`, `null` or `any`. Fields are required unless the name ends in
//! `?`, and no other properties are allowed unless `(*): type` says which.
//! `name(array): type` or a nested mapping is a list, `name(object):` a
//! nested object and `name(enum): [a, b]` one of the values; the parentheses
//! may carry a description, as in `name(array, the lines):`. atomic stays
//! compatible with Picoschema, with one difference: a `?` field may be
//! omitted, and is not also nullable.
//!
//! The form has no constraints. A schema that needs `minLength`, `pattern`,
//! `format` or the like is written as full JSON Schema.

use serde_json::{Map, Value, json};

/// Keywords whose presence at a schema's root marks it as full JSON Schema.
/// Picoschema itself looks only at `type` and `properties`; atomic also
/// treats these as JSON Schema, so a full schema such as `{enum: [a, b]}` is
/// never read as a compact object with a field named `enum`.
const JSON_SCHEMA_KEYWORDS: &[&str] = &[
    "$ref",
    "$schema",
    "$id",
    "$defs",
    "$anchor",
    "$comment",
    "$dynamicRef",
    "$dynamicAnchor",
    "$vocabulary",
    "type",
    "properties",
    "patternProperties",
    "additionalProperties",
    "unevaluatedProperties",
    "propertyNames",
    "required",
    "enum",
    "const",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
    "items",
    "prefixItems",
    "unevaluatedItems",
    "contains",
    "dependentSchemas",
];

/// The other JSON Schema keywords: constraints and annotations. Several are
/// also natural field names (`title`, `description`, `format`, `default`), so
/// one marks the root as full JSON Schema only when its value is not a
/// compact field: `{minLength: 3}` and `{description: A city}` are JSON
/// Schema, `{description: string, what happened}` a compact field.
const VALUE_KEYWORDS: &[&str] = &[
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "pattern",
    "maxItems",
    "minItems",
    "uniqueItems",
    "maxContains",
    "minContains",
    "maxProperties",
    "minProperties",
    "dependentRequired",
    "format",
    "contentEncoding",
    "contentMediaType",
    "contentSchema",
    "title",
    "description",
    "default",
    "deprecated",
    "readOnly",
    "writeOnly",
    "examples",
    "definitions",
    "dependencies",
];

const SCALARS: &[&str] = &["string", "integer", "number", "boolean", "null", "any"];

/// A compact schema atomic cannot compile.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("compact schema at {path}: {message}")]
pub struct CompactError {
    /// A JSON Pointer to the field, such as `/findings/line`.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

/// Whether `value` reads as a compact scalar field, `type, description`.
fn is_field(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        let kind = text.split_once(',').map_or(text, |(kind, _)| kind);
        SCALARS.contains(&kind.trim())
    })
}

/// Whether `schema` is written in the compact form: a non-empty mapping
/// whose root has no structural JSON Schema keyword, and no constraint or
/// annotation keyword with a value other than a compact field.
pub fn is_compact(schema: &Value) -> bool {
    schema.as_object().is_some_and(|object| {
        !object.is_empty()
            && !object.iter().any(|(key, value)| {
                JSON_SCHEMA_KEYWORDS.contains(&key.as_str())
                    || (VALUE_KEYWORDS.contains(&key.as_str()) && !is_field(value))
            })
    })
}

/// `schema` as JSON Schema: compiled when it is compact, unchanged otherwise.
///
/// A root that mixes the two is refused: one with a structural keyword such
/// as `type` or `const` and a key that only reads as a compact field (`name:
/// string`, `name?: ...` or `name(array): ...`). Read as JSON Schema, the
/// field would be an unknown keyword and silently ignored.
pub fn to_json_schema(schema: &Value) -> Result<Value, CompactError> {
    if is_compact(schema) {
        return expand(schema);
    }
    if let Some(object) = schema.as_object()
        && let Some(keyword) = object
            .keys()
            .find(|key| JSON_SCHEMA_KEYWORDS.contains(&key.as_str()))
    {
        for (key, value) in object {
            let known = JSON_SCHEMA_KEYWORDS.contains(&key.as_str())
                || VALUE_KEYWORDS.contains(&key.as_str())
                || key.starts_with("x-");
            let compact_syntax = key.ends_with('?') || key.contains('(');
            if !known && (compact_syntax || is_field(value)) {
                return Err(fail(
                    &format!("/{key}"),
                    format!(
                        "`{key}` reads as a compact field, but the schema's root also has the \
                         JSON Schema keyword `{keyword}`, so it would be read as full JSON Schema \
                         and `{key}` ignored; rename the field `{keyword}` or write the whole \
                         schema as JSON Schema"
                    ),
                ));
            }
        }
    }
    Ok(schema.clone())
}

/// Compile a compact schema to JSON Schema 2020-12.
pub fn expand(schema: &Value) -> Result<Value, CompactError> {
    let mut compiled = object(schema, "")?;
    if let Value::Object(map) = &mut compiled {
        let mut root = Map::new();
        root.insert(
            "$schema".into(),
            json!("https://json-schema.org/draft/2020-12/schema"),
        );
        root.append(map);
        compiled = Value::Object(root);
    }
    Ok(compiled)
}

fn fail(path: &str, message: impl Into<String>) -> CompactError {
    CompactError {
        path: if path.is_empty() {
            "/".into()
        } else {
            path.into()
        },
        message: message.into(),
    }
}

/// A field name: the name, whether it ends in `?`, and an optional
/// `(kind, description)`.
struct Key<'a> {
    name: &'a str,
    optional: bool,
    kind: Option<&'a str>,
    description: Option<&'a str>,
}

fn key(text: &str) -> Option<Key<'_>> {
    let (head, paren) = match text.find('(') {
        Some(0) if text.starts_with("(*)") => {
            let rest = &text[3..];
            return match rest {
                "" => Some(Key {
                    name: "(*)",
                    optional: false,
                    kind: None,
                    description: None,
                }),
                _ => None,
            };
        }
        Some(i) => {
            let inner = text[i..].strip_prefix('(')?.strip_suffix(')')?;
            (&text[..i], Some(inner))
        }
        None => (text, None),
    };
    let (name, optional) = match head.strip_suffix('?') {
        Some(name) => (name, true),
        None => (head, false),
    };
    let valid = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        return None;
    }
    let (kind, description) = match paren {
        None => (None, None),
        Some(inner) => match inner.split_once(',') {
            Some((kind, description)) => (Some(kind.trim()), Some(description.trim())),
            None => (Some(inner.trim()), None),
        },
    };
    Some(Key {
        name,
        optional,
        kind,
        description: description.filter(|d| !d.is_empty()),
    })
}

fn scalar(value: &Value, path: &str) -> Result<Value, CompactError> {
    let Some(text) = value.as_str() else {
        return Err(fail(
            path,
            format!(
                "expected `type, description`, found {value}; a nested object is written \
                 `name(object):`, and constraints need full JSON Schema"
            ),
        ));
    };
    let (kind, description) = match text.split_once(',') {
        Some((kind, description)) => (kind.trim(), description.trim()),
        None => (text.trim(), ""),
    };
    if !SCALARS.contains(&kind) {
        return Err(fail(
            path,
            format!(
                "unknown type `{kind}`; the compact form has {}, and constraints such as \
                 minLength, pattern or format need full JSON Schema",
                SCALARS.join(", ")
            ),
        ));
    }
    let mut schema = Map::new();
    if kind != "any" {
        schema.insert("type".into(), json!(kind));
    }
    if !description.is_empty() {
        schema.insert("description".into(), json!(description));
    }
    Ok(Value::Object(schema))
}

fn object(value: &Value, path: &str) -> Result<Value, CompactError> {
    let Some(fields) = value.as_object().filter(|fields| !fields.is_empty()) else {
        return Err(fail(path, "expected a mapping of fields"));
    };
    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut additional = json!(false);
    for (text, value) in fields {
        let Some(key) = key(text) else {
            return Err(fail(path, format!("cannot read the field name `{text}`")));
        };
        let at = format!("{path}/{}", key.name);
        let mut schema = match key.kind {
            None => scalar(value, &at)?,
            Some("array") => {
                let items = if value.is_object() {
                    object(value, &at)?
                } else {
                    scalar(value, &at)?
                };
                json!({ "type": "array", "items": items })
            }
            Some("object") => object(value, &at)?,
            Some("enum") => match value.as_array() {
                Some(values)
                    if !values.is_empty()
                        && values.iter().all(|v| !v.is_object() && !v.is_array()) =>
                {
                    json!({ "enum": values })
                }
                _ => return Err(fail(&at, "an enum is a non-empty list of values")),
            },
            Some(other) => {
                return Err(fail(
                    &at,
                    format!("unknown kind `({other})`; the kinds are array, object and enum"),
                ));
            }
        };
        if let (Some(description), Value::Object(map)) = (key.description, &mut schema) {
            map.insert("description".into(), json!(description));
        }
        if key.name == "(*)" {
            additional = schema;
        } else {
            if properties.contains_key(key.name) {
                return Err(fail(&at, "the field is declared twice"));
            }
            properties.insert(key.name.to_owned(), schema);
            if !key.optional {
                required.push(json!(key.name));
            }
        }
    }
    let mut compiled = Map::new();
    compiled.insert("type".into(), json!("object"));
    compiled.insert("additionalProperties".into(), additional);
    if !required.is_empty() {
        compiled.insert("required".into(), Value::Array(required));
    }
    compiled.insert("properties".into(), Value::Object(properties));
    Ok(Value::Object(compiled))
}

#[cfg(test)]
mod tests;
