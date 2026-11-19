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

    /// A store holding one diff, `a…`, and one text note, `b…`.
    struct Store;

    impl ArtifactLookup for Store {
        fn media_types(&self, artifact: &ArtifactRef) -> Option<Vec<String>> {
            match artifact.digest().as_bytes().first() {
                Some(b'a') => Some(vec!["text/x-diff".into()]),
                Some(b'b') => Some(vec!["text/plain".into()]),
                _ => None,
            }
        }
    }

    fn reference(c: char) -> serde_json::Value {
        serde_json::json!(format!("artifact://sha256/{}", c.to_string().repeat(64)))
    }

    fn file(alternatives: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"type": "object", "required": ["file"],
                           "properties": {"file": {"anyOf": alternatives}}})
    }

    #[test]
    fn one_alternative_that_accepts_the_reference_is_enough() {
        let either = file(serde_json::json!([
            {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
            {"$ref": "atomic:artifact", "mediaType": "text/plain"}
        ]));
        for c in ['a', 'b'] {
            let value = serde_json::json!({"file": reference(c)});
            assert_eq!(check_refs(&either, &value, &Store), Ok(()), "{c}");
        }
        // Neither alternative holds a missing artifact.
        let missing = serde_json::json!({"file": reference('c')});
        assert!(matches!(
            check_refs(&either, &missing, &Store),
            Err(ArtifactCheckError::Missing { .. })
        ));
    }

    #[test]
    fn an_alternative_without_references_counts_only_if_the_value_fits_it() {
        // `null` does not excuse a missing artifact: a string is not null.
        let or_null = file(serde_json::json!([
            {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
            {"type": "null"}
        ]));
        assert!(matches!(
            check_refs(
                &or_null,
                &serde_json::json!({"file": reference('c')}),
                &Store
            ),
            Err(ArtifactCheckError::Missing { .. })
        ));
        assert_eq!(
            check_refs(&or_null, &serde_json::json!({"file": null}), &Store),
            Ok(())
        );
        // A plain-string alternative accepts text that is no reference at all.
        let or_text = file(serde_json::json!([
            {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
            {"type": "string", "maxLength": 20}
        ]));
        assert_eq!(
            check_refs(&or_text, &serde_json::json!({"file": "hello"}), &Store),
            Ok(())
        );
        // A wrong type with no other alternative to fall back on still fails.
        let diff_only = file(serde_json::json!([
            {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}
        ]));
        assert!(matches!(
            check_refs(
                &diff_only,
                &serde_json::json!({"file": reference('b')}),
                &Store
            ),
            Err(ArtifactCheckError::WrongMediaType { .. })
        ));
    }

    fn expected_type(result: Result<(), ArtifactCheckError>) -> Option<String> {
        match result {
            Err(ArtifactCheckError::WrongMediaType { expected, .. }) => Some(expected),
            other => panic!("expected a wrong media type, got {other:?}"),
        }
    }

    #[test]
    fn artifact_alternatives_accept_one_matching_media_type() {
        let either = file(serde_json::json!([
            {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
            {"$ref": "atomic:artifact", "mediaType": "text/plain"}
        ]));
        let diff = serde_json::json!({"file": reference('a')});
        assert_eq!(check_refs(&either, &diff, &Store), Ok(()));
        // When every alternative fails, the first one's reason is reported.
        let neither = file(serde_json::json!([
            {"$ref": "atomic:artifact", "mediaType": "text/markdown"},
            {"$ref": "atomic:artifact", "mediaType": "application/json"}
        ]));
        assert_eq!(
            expected_type(check_refs(&neither, &diff, &Store)).as_deref(),
            Some("text/markdown")
        );
    }

    /// `payload` is a diff or a text note, told apart by `kind`.
    fn tagged(keyword: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"payload": {keyword: [
                {"type": "object", "required": ["kind", "file"],
                 "properties": {"kind": {"const": "diff"},
                                "file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}}},
                {"$ref": "#/$defs/note"}
            ]}},
            "$defs": {"note": {"type": "object", "required": ["kind", "file"],
                               "properties": {"kind": {"const": "text"},
                                              "file": {"$ref": "atomic:artifact", "mediaType": "text/plain"}}}}
        })
    }

    fn payload(kind: &str, c: char) -> serde_json::Value {
        serde_json::json!({"payload": {"kind": kind, "file": reference(c)}})
    }

    #[test]
    fn artifact_alternative_must_match_the_object_tag() {
        for keyword in ["anyOf", "oneOf"] {
            let schema = tagged(keyword);
            assert_eq!(check_refs(&schema, &payload("diff", 'a'), &Store), Ok(()));
            assert_eq!(check_refs(&schema, &payload("text", 'b'), &Store), Ok(()));
            // The branch the tag selects decides; the other adds nothing.
            assert_eq!(
                expected_type(check_refs(&schema, &payload("diff", 'b'), &Store)).as_deref(),
                Some("text/x-diff"),
                "{keyword}"
            );
            assert_eq!(
                expected_type(check_refs(&schema, &payload("text", 'a'), &Store)).as_deref(),
                Some("text/plain"),
                "{keyword}"
            );
            assert!(
                matches!(
                    check_refs(&schema, &payload("diff", 'c'), &Store),
                    Err(ArtifactCheckError::Missing { .. })
                ),
                "{keyword}"
            );
        }
    }

    #[test]
    fn tagged_alternatives_inside_a_list_are_checked_per_item() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"items": {"type": "array", "items": tagged("anyOf")["properties"]["payload"]}},
            "$defs": tagged("anyOf")["$defs"]
        });
        let good = serde_json::json!({"items": [
            {"kind": "diff", "file": reference('a')},
            {"kind": "text", "file": reference('b')}
        ]});
        assert_eq!(check_refs(&schema, &good, &Store), Ok(()));
        let bad = serde_json::json!({"items": [
            {"kind": "text", "file": reference('b')},
            {"kind": "diff", "file": reference('b')}
        ]});
        match check_refs(&schema, &bad, &Store) {
            Err(ArtifactCheckError::WrongMediaType { at, expected, .. }) => {
                assert_eq!(
                    (at.as_str(), expected.as_str()),
                    ("/items/1/file", "text/x-diff")
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_all_of_branch_must_hold() {
        let schema = serde_json::json!({"allOf": [
            {"type": "object", "properties": {"file": {"$ref": "atomic:artifact"}}},
            {"properties": {"file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}}}
        ]});
        assert_eq!(
            check_refs(
                &schema,
                &serde_json::json!({"file": reference('a')}),
                &Store
            ),
            Ok(())
        );
        assert_eq!(
            expected_type(check_refs(
                &schema,
                &serde_json::json!({"file": reference('b')}),
                &Store
            ))
            .as_deref(),
            Some("text/x-diff")
        );
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
/// following `properties`, `items`, local `$defs` references and the branches
/// of `anyOf`, `oneOf` and `allOf`. Every branch contributes, so a reference
/// under alternatives can appear once for each branch that expects one.
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
    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(branches)) = schema.get(keyword) {
            for b in branches {
                walk(root, b, value, at, out, depth + 1);
            }
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
/// exist and, where its schema says, have the right media type. Combinators
/// mean what they mean in JSON Schema. Every `allOf` branch's references must
/// hold. `anyOf` and `oneOf` are alternatives: the value is fine if one branch
/// accepts it, and a branch accepts it when the value fits the branch and the
/// branch's own references hold. A branch the value does not fit adds no
/// requirement, so `{kind: "diff", file}` is held to the diff branch's media
/// type and not to a text branch's.
pub fn check_refs(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    store: &dyn ArtifactLookup,
) -> Result<(), ArtifactCheckError> {
    check_at(schema, schema, value, "", store, 0)
}

fn check_one(
    text: &str,
    at: &str,
    media_type: Option<&str>,
    store: &dyn ArtifactLookup,
) -> Result<(), ArtifactCheckError> {
    let artifact: ArtifactRef = text
        .parse()
        .map_err(|source| ArtifactCheckError::Malformed {
            at: at.to_owned(),
            source,
        })?;
    let types = store
        .media_types(&artifact)
        .ok_or_else(|| ArtifactCheckError::Missing {
            at: at.to_owned(),
            text: text.to_owned(),
        })?;
    if let Some(expected) = media_type
        && !types.iter().any(|t| t == expected)
    {
        return Err(ArtifactCheckError::WrongMediaType {
            at: at.to_owned(),
            text: text.to_owned(),
            expected: expected.to_owned(),
            found: types,
        });
    }
    Ok(())
}

/// Whether `value` fits `branch` as JSON Schema, with the root's
/// definitions in reach, or `None` when the branch cannot be compiled on its
/// own (a reference into the root outside its definitions, say). A branch
/// whose fit is unknown is treated as one the value fits, so its artifact
/// requirements still apply.
fn fits(
    root: &serde_json::Value,
    branch: &serde_json::Value,
    value: &serde_json::Value,
) -> Option<bool> {
    let mut schema = branch.clone();
    if let Some(obj) = schema.as_object_mut() {
        for defs in ["$defs", "definitions"] {
            if let Some(found) = root.get(defs) {
                obj.entry(defs).or_insert_with(|| found.clone());
            }
        }
    }
    crate::registry::Registry::new()
        .validator_for(&schema)
        .ok()
        .map(|v| v.is_valid(value))
}

fn check_at(
    root: &serde_json::Value,
    schema: &serde_json::Value,
    value: &serde_json::Value,
    at: &str,
    store: &dyn ArtifactLookup,
    depth: usize,
) -> Result<(), ArtifactCheckError> {
    use serde_json::Value;
    if depth > 64 {
        return Ok(());
    }
    match schema.get("$ref").and_then(Value::as_str) {
        Some(ARTIFACT_SCHEMA_ID) => {
            return match value.as_str() {
                Some(text) => check_one(
                    text,
                    at,
                    schema.get("mediaType").and_then(Value::as_str),
                    store,
                ),
                None => Ok(()),
            };
        }
        Some(local) if local.starts_with("#/") => {
            if let Some(target) = root.pointer(&local[1..]) {
                check_at(root, target, value, at, store, depth + 1)?;
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
                check_at(root, s, v, &child, store, depth + 1)?;
            }
        }
    }
    if let (Some(items), Some(arr)) = (schema.get("items"), value.as_array()) {
        for (i, v) in arr.iter().enumerate() {
            check_at(root, items, v, &format!("{at}/{i}"), store, depth + 1)?;
        }
    }
    // Every `allOf` branch applies, so every branch's references must hold.
    if let Some(Value::Array(branches)) = schema.get("allOf") {
        for b in branches {
            check_at(root, b, value, at, store, depth + 1)?;
        }
    }
    // `anyOf` and `oneOf` are alternatives. Only a branch the value fits can
    // accept it, and a branch that fits accepts it when its own references
    // hold. Whether exactly one `oneOf` branch fits is the schema validator's
    // question, already answered; here a fitting branch that holds is enough.
    for keyword in ["anyOf", "oneOf"] {
        let Some(Value::Array(branches)) = schema.get(keyword) else {
            continue;
        };
        let mut first_error = None;
        for b in branches {
            if fits(root, b, value) == Some(false) {
                continue;
            }
            match check_at(root, b, value, at, store, depth + 1) {
                Ok(()) => {
                    first_error = None;
                    break;
                }
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        if let Some(e) = first_error {
            return Err(e);
        }
        // No branch fits: the schema validator rejects the value itself.
    }
    Ok(())
}
