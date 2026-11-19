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

    /// The reproduction: `about` refers to the whole contract with `#`.
    fn recursive() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"attachment": {"anyOf": [
                {"type": "null"},
                {"type": "object", "required": ["file", "about"],
                 "properties": {"file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
                                "about": {"$ref": "#"}}}
            ]}}
        })
    }

    fn attachment(c: char, about: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"attachment": {"file": reference(c), "about": about}})
    }

    #[test]
    fn a_root_reference_inside_a_branch_means_the_contract() {
        let schema = recursive();
        assert_eq!(
            check_refs(&schema, &attachment('a', serde_json::json!({})), &Store),
            Ok(())
        );
        assert_eq!(
            check_refs(&schema, &serde_json::json!({"attachment": null}), &Store),
            Ok(())
        );
        assert!(matches!(
            check_refs(&schema, &attachment('c', serde_json::json!({})), &Store),
            Err(ArtifactCheckError::Missing { .. })
        ));
        assert_eq!(
            expected_type(check_refs(
                &schema,
                &attachment('b', serde_json::json!({})),
                &Store
            ))
            .as_deref(),
            Some("text/x-diff")
        );
        // Two alternatives that both refer to the contract: the diff branch
        // fits, so the text branch adds no requirement.
        let either = serde_json::json!({
            "type": "object",
            "properties": {"attachment": {"anyOf": [
                {"type": "object", "required": ["file", "about"],
                 "properties": {"file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
                                "about": {"$ref": "#"}}},
                {"type": "object", "required": ["file", "about"],
                 "properties": {"file": {"$ref": "atomic:artifact", "mediaType": "text/plain"},
                                "about": {"$ref": "#"}}}
            ]}}
        });
        for c in ['a', 'b'] {
            assert_eq!(
                check_refs(&either, &attachment(c, serde_json::json!({})), &Store),
                Ok(()),
                "{c}"
            );
        }
        // `#` is followed into the nested value too.
        let nested = attachment('a', attachment('c', serde_json::json!({})));
        match check_refs(&schema, &nested, &Store) {
            Err(ArtifactCheckError::Missing { at, .. }) => {
                assert_eq!(at, "/attachment/about/attachment/file");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_branch_whose_fit_cannot_be_decided_is_still_held() {
        // The second branch refers to a schema outside the contract, so no
        // validator can be built for it.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"attachment": {"anyOf": [
                {"type": "null"},
                {"type": "object",
                 "properties": {"file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
                                "extra": {"$ref": "https://example.invalid/other.schema.json"}}}
            ]}}
        });
        let value = |c| serde_json::json!({"attachment": {"file": reference(c)}});
        assert!(matches!(
            check_refs(&schema, &value('c'), &Store),
            Err(ArtifactCheckError::Missing { .. })
        ));
        assert_eq!(
            expected_type(check_refs(&schema, &value('b'), &Store)).as_deref(),
            Some("text/x-diff")
        );
        assert_eq!(check_refs(&schema, &value('a'), &Store), Ok(()));
        assert_eq!(
            check_refs(&schema, &serde_json::json!({"attachment": null}), &Store),
            Ok(())
        );
        // An undecided branch with no references of its own does not accept a
        // value that no branch definitely fits.
        let schema = serde_json::json!({"anyOf": [
            {"type": "object", "required": ["kind", "file"],
             "properties": {"kind": {"const": "diff"},
                            "file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}}},
            {"$ref": "https://example.invalid/other.schema.json"}
        ]});
        assert!(matches!(
            check_refs(
                &schema,
                &serde_json::json!({"file": reference('c')}),
                &Store
            ),
            Err(ArtifactCheckError::Missing { .. })
        ));
    }

    #[test]
    fn when_no_branch_fits_every_branch_is_held() {
        // `kind` matches neither tag, so the value breaks the schema; its
        // references are still held, to every branch.
        let schema = tagged("anyOf");
        assert!(matches!(
            check_refs(&schema, &payload("other", 'c'), &Store),
            Err(ArtifactCheckError::Missing { .. })
        ));
        assert!(matches!(
            check_refs(&schema, &payload("other", 'a'), &Store),
            Err(ArtifactCheckError::WrongMediaType { .. })
        ));
    }

    #[test]
    fn branches_under_definitions_and_root_pointers_are_decided() {
        let branch = |kind: &str, media: &str| {
            serde_json::json!({"type": "object", "required": ["kind", "file"],
                               "properties": {"kind": {"const": kind},
                                              "file": {"$ref": "atomic:artifact", "mediaType": media}}})
        };
        let definitions = serde_json::json!({
            "type": "object",
            "properties": {"payload": {"anyOf": [
                {"$ref": "#/definitions/diff"}, {"$ref": "#/definitions/note"}
            ]}},
            "definitions": {"diff": branch("diff", "text/x-diff"), "note": branch("text", "text/plain")}
        });
        // The second branch cannot be compiled on its own: it points into the
        // contract's properties.
        let pointer = serde_json::json!({
            "type": "object",
            "properties": {
                "payload": {"anyOf": [branch("diff", "text/x-diff"), {"$ref": "#/properties/note"}]},
                "note": branch("text", "text/plain")
            }
        });
        for schema in [definitions, pointer] {
            assert_eq!(check_refs(&schema, &payload("diff", 'a'), &Store), Ok(()));
            assert_eq!(check_refs(&schema, &payload("text", 'b'), &Store), Ok(()));
            assert_eq!(
                expected_type(check_refs(&schema, &payload("diff", 'b'), &Store)).as_deref(),
                Some("text/x-diff"),
                "{schema}"
            );
            assert_eq!(
                expected_type(check_refs(&schema, &payload("text", 'a'), &Store)).as_deref(),
                Some("text/plain"),
                "{schema}"
            );
        }
    }

    #[test]
    fn each_branch_is_compiled_once_per_check() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"items": {"type": "array", "items": tagged("anyOf")["properties"]["payload"]}},
            "$defs": tagged("anyOf")["$defs"]
        });
        let items: Vec<_> = (0..5000)
            .map(|i| {
                if i % 2 == 0 {
                    serde_json::json!({"kind": "diff", "file": reference('a')})
                } else {
                    serde_json::json!({"kind": "text", "file": reference('b')})
                }
            })
            .collect();
        let value = serde_json::json!({ "items": items });
        let started = std::time::Instant::now();
        let (result, compiled) = Checker::new(&schema, &Store).check(&value);
        let took = started.elapsed();
        eprintln!("5000 tagged items: {took:?}");
        assert_eq!(result, Ok(()));
        assert_eq!(compiled, 2, "one validator per branch");
        assert!(took < std::time::Duration::from_secs(5), "{took:?}");
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
///
/// Whether a value fits a branch is decided against the whole `schema`, so a
/// `$ref` inside a branch means what it means in the contract. The check fails
/// closed: when no branch definitely fits (the value breaks the schema, or a
/// branch's fit cannot be decided), every branch's references must hold.
pub fn check_refs(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    store: &dyn ArtifactLookup,
) -> Result<(), ArtifactCheckError> {
    Checker::new(schema, store).check(value).0
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

/// The URI the contract is served under while branch fits are decided, when
/// it has no absolute `$id` of its own.
const CONTRACT_URI: &str = "urn:atomic:contract";

/// One `check_refs` call: the contract, the store, and each branch's
/// validator, compiled the first time it is needed.
struct Checker<'a> {
    root: &'a serde_json::Value,
    store: &'a dyn ArtifactLookup,
    base: String,
    known: std::sync::Arc<std::collections::BTreeMap<String, serde_json::Value>>,
    branches: std::collections::HashMap<String, Option<jsonschema::Validator>>,
    compiled: usize,
}

/// Serves the contract and the built-in artifact schema, and nothing else.
struct Contract(std::sync::Arc<std::collections::BTreeMap<String, serde_json::Value>>);

impl jsonschema::Retrieve for Contract {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let key = uri.as_str().split('#').next().unwrap_or_default();
        self.0
            .get(key)
            .cloned()
            .ok_or_else(|| format!("{key} is not part of the contract").into())
    }
}

/// `pointer` as a URI fragment: every byte outside the fragment's safe set
/// is percent-encoded, `%` included.
fn fragment(pointer: &str) -> String {
    let mut out = String::with_capacity(pointer.len());
    for byte in pointer.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A JSON Pointer token, escaped.
fn token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

impl<'a> Checker<'a> {
    fn new(root: &'a serde_json::Value, store: &'a dyn ArtifactLookup) -> Self {
        // The contract keeps its own base URI when it has one, so its
        // references resolve as they do when the whole answer is validated.
        let base = root
            .get("$id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| url::Url::parse(id).is_ok())
            .map(|id| id.trim_end_matches('#').to_owned())
            .unwrap_or_else(|| CONTRACT_URI.to_owned());
        let known = std::sync::Arc::new(std::collections::BTreeMap::from([
            (base.clone(), root.clone()),
            (ARTIFACT_SCHEMA_ID.to_owned(), artifact_schema()),
        ]));
        Self {
            root,
            store,
            base,
            known,
            branches: std::collections::HashMap::new(),
            compiled: 0,
        }
    }

    /// The result, and how many branch validators were compiled.
    fn check(mut self, value: &serde_json::Value) -> (Result<(), ArtifactCheckError>, usize) {
        let result = self.check_at("", value, "", 0);
        (result, self.compiled)
    }

    /// Whether `value` fits the subschema at `pointer` in the contract, or
    /// `None` when that cannot be decided.
    fn fits(&mut self, pointer: &str, value: &serde_json::Value) -> Option<bool> {
        if !self.branches.contains_key(pointer) {
            self.compiled += 1;
            let wrapper =
                serde_json::json!({"$ref": format!("{}#{}", self.base, fragment(pointer))});
            let validator = jsonschema::options()
                .with_draft(jsonschema::Draft::Draft202012)
                .with_retriever(Contract(std::sync::Arc::clone(&self.known)))
                .build(&wrapper)
                .ok();
            self.branches.insert(pointer.to_owned(), validator);
        }
        self.branches[pointer].as_ref().map(|v| v.is_valid(value))
    }

    fn check_at(
        &mut self,
        pointer: &str,
        value: &serde_json::Value,
        at: &str,
        depth: usize,
    ) -> Result<(), ArtifactCheckError> {
        use serde_json::Value;
        if depth > 64 {
            return Ok(());
        }
        let root = self.root;
        let Some(schema) = root.pointer(pointer) else {
            return Ok(());
        };
        match schema.get("$ref").and_then(Value::as_str) {
            Some(ARTIFACT_SCHEMA_ID) => {
                return match value.as_str() {
                    Some(text) => check_one(
                        text,
                        at,
                        schema.get("mediaType").and_then(Value::as_str),
                        self.store,
                    ),
                    None => Ok(()),
                };
            }
            Some(local)
                if (local == "#" || local.starts_with("#/"))
                    && root.pointer(&local[1..]).is_some() =>
            {
                self.check_at(&local[1..], value, at, depth + 1)?;
            }
            _ => {}
        }
        if let (Some(props), Some(obj)) = (
            schema.get("properties").and_then(Value::as_object),
            value.as_object(),
        ) {
            for k in props.keys() {
                if let Some(v) = obj.get(k) {
                    let child = format!("{at}/{}", token(k));
                    let sub = format!("{pointer}/properties/{}", token(k));
                    self.check_at(&sub, v, &child, depth + 1)?;
                }
            }
        }
        if let (Some(_), Some(arr)) = (schema.get("items"), value.as_array()) {
            let sub = format!("{pointer}/items");
            for (i, v) in arr.iter().enumerate() {
                self.check_at(&sub, v, &format!("{at}/{i}"), depth + 1)?;
            }
        }
        // Every `allOf` branch applies, so every branch's references must hold.
        if let Some(Value::Array(branches)) = schema.get("allOf") {
            for i in 0..branches.len() {
                self.check_at(&format!("{pointer}/allOf/{i}"), value, at, depth + 1)?;
            }
        }
        // `anyOf` and `oneOf` are alternatives. A branch the value fits
        // accepts it when the branch's own references hold. Whether exactly
        // one `oneOf` branch fits is the schema validator's question.
        for keyword in ["anyOf", "oneOf"] {
            let Some(Value::Array(branches)) = schema.get(keyword) else {
                continue;
            };
            let mut first_error = None;
            let mut accepted = false;
            let mut held = Vec::new();
            for i in 0..branches.len() {
                let sub = format!("{pointer}/{keyword}/{i}");
                match self.fits(&sub, value) {
                    Some(true) => match self.check_at(&sub, value, at, depth + 1) {
                        Ok(()) => {
                            accepted = true;
                            break;
                        }
                        Err(e) => {
                            first_error.get_or_insert(e);
                        }
                    },
                    Some(false) => held.push(sub),
                    // Undecided: it cannot accept the value on its own, and
                    // its references are held if nothing else accepts.
                    None => held.push(sub),
                }
            }
            if accepted {
                continue;
            }
            if let Some(e) = first_error {
                return Err(e);
            }
            // No branch definitely fits: fail closed, holding every branch's
            // references.
            for sub in held {
                self.check_at(&sub, value, at, depth + 1)?;
            }
        }
        Ok(())
    }
}
