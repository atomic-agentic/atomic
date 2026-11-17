// SPDX-License-Identifier: Apache-2.0

//! References to content-addressed artifacts: `artifact://sha256/<digest>`.
//!
//! Step outputs, and later graph items and messages, hold these references,
//! never the bytes.

use std::fmt;
use std::str::FromStr;

const PREFIX: &str = "artifact://sha256/";

/// A reference to an artifact by the SHA-256 of its content.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactRef {
    digest: String,
}

/// Why a string is not an artifact reference.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("not an artifact reference ({reason}): {text:?}")]
pub struct ArtifactRefError {
    text: String,
    reason: &'static str,
}

impl ArtifactRef {
    /// A reference from a lowercase hex SHA-256 digest.
    pub fn from_digest(digest: &str) -> Result<Self, ArtifactRefError> {
        if digest.len() == 64
            && digest
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            Ok(Self {
                digest: digest.to_owned(),
            })
        } else {
            Err(ArtifactRefError {
                text: digest.to_owned(),
                reason: "a digest is 64 lowercase hex digits",
            })
        }
    }

    /// The lowercase hex SHA-256 digest.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The `artifact://sha256/<digest>` form.
    pub fn uri(&self) -> String {
        format!("{PREFIX}{}", self.digest)
    }
}

impl fmt::Display for ArtifactRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}{}", self.digest)
    }
}

impl FromStr for ArtifactRef {
    type Err = ArtifactRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let digest = s.strip_prefix(PREFIX).ok_or(ArtifactRefError {
            text: s.to_owned(),
            reason: "it must start with artifact://sha256/",
        })?;
        Self::from_digest(digest).map_err(|e| ArtifactRefError {
            text: s.to_owned(),
            ..e
        })
    }
}

impl serde::Serialize for ArtifactRef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.uri())
    }
}

impl<'de> serde::Deserialize<'de> for ArtifactRef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_bad_forms() {
        let d = "a".repeat(64);
        let r: ArtifactRef = format!("artifact://sha256/{d}").parse().expect("parses");
        assert_eq!(r.digest(), d);
        assert_eq!(r.to_string(), format!("artifact://sha256/{d}"));
        assert_eq!(
            serde_json::to_value(&r).expect("json"),
            serde_json::json!(r.uri())
        );
        for bad in [
            "artifact://sha256/abc",
            "artifact://md5/x",
            &format!("artifact://sha256/{}", "A".repeat(64)),
        ] {
            assert!(bad.parse::<ArtifactRef>().is_err(), "{bad}");
        }
    }
}

/// The `$id` of the built-in artifact reference schema.
pub const ARTIFACT_SCHEMA_ID: &str = "atomic:artifact";

/// The built-in schema `{ "$ref": "atomic:artifact" }` names: a string holding
/// an artifact reference. A `mediaType` beside the `$ref` says what the
/// artifact must be.
pub fn artifact_schema() -> serde_json::Value {
    serde_json::json!({
        "$id": ARTIFACT_SCHEMA_ID,
        "type": "string",
        "pattern": "^artifact://sha256/[0-9a-f]{64}$",
        "description": "A reference to a stored artifact: artifact://sha256/<digest>."
    })
}

/// Lets validation ask the artifact store about a reference.
pub trait ArtifactLookup {
    /// The media types recorded for the artifact, or `None` if it is not in
    /// the store.
    fn media_types(&self, artifact: &ArtifactRef) -> Option<Vec<String>>;
}

/// An artifact reference in a value, where it is, and the media type its
/// schema requires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundRef {
    /// JSON Pointer into the value.
    pub at: String,
    /// The reference as written.
    pub text: String,
    /// The `mediaType` the schema requires, if any.
    pub media_type: Option<String>,
}

/// Every artifact reference `value` holds where `schema` expects one,
/// following `properties`, `items` and local `$defs` references.
pub fn find_refs(schema: &serde_json::Value, value: &serde_json::Value) -> Vec<FoundRef> {
    let mut out = Vec::new();
    walk(schema, schema, value, "", &mut out, 0);
    out
}

fn walk(
    root: &serde_json::Value,
    schema: &serde_json::Value,
    value: &serde_json::Value,
    at: &str,
    out: &mut Vec<FoundRef>,
    depth: usize,
) {
    use serde_json::Value;
    if depth > 64 {
        return;
    }
    match schema.get("$ref").and_then(Value::as_str) {
        Some(ARTIFACT_SCHEMA_ID) => {
            if let Some(text) = value.as_str() {
                out.push(FoundRef {
                    at: at.to_owned(),
                    text: text.to_owned(),
                    media_type: schema
                        .get("mediaType")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
            return;
        }
        Some(local) if local.starts_with("#/") => {
            if let Some(target) = root.pointer(&local[1..]) {
                walk(root, target, value, at, out, depth + 1);
            }
        }
        _ => {}
    }
    if let (Some(props), Some(obj)) = (
        schema.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) {
        for (k, s) in props {
            if let Some(v) = obj.get(k) {
                let child = format!("{at}/{}", k.replace('~', "~0").replace('/', "~1"));
                walk(root, s, v, &child, out, depth + 1);
            }
        }
    }
    if let (Some(items), Some(arr)) = (schema.get("items"), value.as_array()) {
        for (i, v) in arr.iter().enumerate() {
            walk(root, items, v, &format!("{at}/{i}"), out, depth + 1);
        }
    }
    if let Some(Value::Array(branches)) = schema.get("anyOf") {
        for b in branches {
            walk(root, b, value, at, out, depth + 1);
        }
    }
}

/// Why a value's artifact references do not hold.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ArtifactCheckError {
    /// The referenced artifact is not in the store.
    #[error("{at}: no artifact {text} in the store")]
    Missing {
        /// Where in the value.
        at: String,
        /// The reference.
        text: String,
    },
    /// The artifact exists but is not of the required media type.
    #[error("{at}: {text} is {found:?}, not {expected}")]
    WrongMediaType {
        /// Where in the value.
        at: String,
        /// The reference.
        text: String,
        /// What the schema requires.
        expected: String,
        /// What the store records.
        found: Vec<String>,
    },
    /// The text is not a reference at all.
    #[error("{at}: {source}")]
    Malformed {
        /// Where in the value.
        at: String,
        /// Why.
        source: ArtifactRefError,
    },
}

/// Check every artifact reference `value` holds against the store: it must
/// exist and, where its schema says, have the right media type.
pub fn check_refs(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    store: &dyn ArtifactLookup,
) -> Result<(), ArtifactCheckError> {
    for found in find_refs(schema, value) {
        let artifact: ArtifactRef =
            found
                .text
                .parse()
                .map_err(|source| ArtifactCheckError::Malformed {
                    at: found.at.clone(),
                    source,
                })?;
        let types = store
            .media_types(&artifact)
            .ok_or_else(|| ArtifactCheckError::Missing {
                at: found.at.clone(),
                text: found.text.clone(),
            })?;
        if let Some(expected) = found.media_type
            && !types.contains(&expected)
        {
            return Err(ArtifactCheckError::WrongMediaType {
                at: found.at,
                text: found.text,
                expected,
                found: types,
            });
        }
    }
    Ok(())
}
