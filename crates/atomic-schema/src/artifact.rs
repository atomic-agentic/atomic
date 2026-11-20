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
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
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

    /// What `check_refs` says, in a word.
    fn verdict(schema: &serde_json::Value, value: &serde_json::Value) -> &'static str {
        match check_refs(schema, value, &Store) {
            Ok(()) => "ok",
            Err(ArtifactCheckError::Missing { .. }) => "missing",
            Err(ArtifactCheckError::WrongMediaType { .. }) => "wrong",
            Err(ArtifactCheckError::Malformed { .. }) => "malformed",
            Err(ArtifactCheckError::TooDeep { .. }) => "too deep",
            Err(ArtifactCheckError::Unsupported { .. }) => "unsupported",
            Err(ArtifactCheckError::TooComplex { .. }) => "too complex",
        }
    }

    /// The four standard answers for a tagged `payload`.
    fn four(schema: &serde_json::Value) -> [&'static str; 4] {
        [
            verdict(schema, &payload("diff", 'a')),
            verdict(schema, &payload("text", 'b')),
            verdict(schema, &payload("diff", 'b')),
            verdict(schema, &payload("diff", 'c')),
        ]
    }

    const FOUR: [&str; 4] = ["ok", "ok", "wrong", "missing"];

    #[test]
    fn a_branch_whose_fit_cannot_be_decided_is_still_held() {
        // The second branch's pattern is not a regular expression, so no
        // validator can be built for it; the first branch compiles.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"attachment": {"anyOf": [
                {"type": "null"},
                {"type": "object",
                 "properties": {"file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
                                "note": {"type": "string", "pattern": "("}}}
            ]}}
        });
        let value = |c| serde_json::json!({"attachment": {"file": reference(c)}});
        let mut checker = Checker::new(&schema, &Store);
        assert_eq!(
            checker.fits("/properties/attachment/anyOf/0", &value('a')["attachment"]),
            Some(false),
            "the null branch is decided"
        );
        assert_eq!(
            checker.fits("/properties/attachment/anyOf/1", &value('a')["attachment"]),
            None,
            "the pattern branch is not"
        );
        assert_eq!(verdict(&schema, &value('c')), "missing");
        assert_eq!(verdict(&schema, &value('b')), "wrong");
        assert_eq!(verdict(&schema, &value('a')), "ok");
        assert_eq!(
            verdict(&schema, &serde_json::json!({"attachment": null})),
            "ok"
        );
        // An undecided branch with no references of its own does not accept a
        // value that no branch definitely fits.
        let schema = serde_json::json!({"anyOf": [
            {"type": "object", "required": ["kind", "file"],
             "properties": {"kind": {"const": "diff"},
                            "file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}}},
            {"type": "object", "properties": {"note": {"type": "string", "pattern": "("}}}
        ]});
        let mut checker = Checker::new(&schema, &Store);
        let value = serde_json::json!({"file": reference('c')});
        assert_eq!(checker.fits("/anyOf/0", &value), Some(false));
        assert_eq!(checker.fits("/anyOf/1", &value), None);
        assert_eq!(verdict(&schema, &value), "missing");
    }

    /// A branch tagged by `$ref` siblings, which drafts before 2019-09 ignore.
    fn sibling_tagged(draft: &str) -> serde_json::Value {
        serde_json::json!({
            "$schema": draft,
            "type": "object",
            "properties": {"result": {"anyOf": [
                {"$ref": "#/definitions/Base", "required": ["kind"],
                 "properties": {"kind": {"const": "none"}}},
                {"type": "object", "required": ["kind", "file"],
                 "properties": {"kind": {"const": "diff"},
                                "file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}}}
            ]}},
            "definitions": {"Base": {"type": "object"}}
        })
    }

    #[test]
    fn an_older_schema_draft_is_read_as_2020_12() {
        let result = |c| serde_json::json!({"result": {"kind": "diff", "file": reference(c)}});
        for draft in [
            "http://json-schema.org/draft-07/schema#",
            "http://json-schema.org/draft-06/schema#",
            "http://json-schema.org/draft-04/schema#",
            "https://json-schema.org/draft/2019-09/schema",
            "https://json-schema.org/draft/2020-12/schema",
        ] {
            let schema = sibling_tagged(draft);
            let got = ['a', 'c', 'b'].map(|c| verdict(&schema, &result(c)));
            assert_eq!(got, ["ok", "missing", "wrong"], "{draft}");
            let mut plain = tagged("anyOf");
            plain["$schema"] = serde_json::json!(draft);
            assert_eq!(four(&plain), FOUR, "{draft}");
        }
        // Keywords draft-07 does not know still close a branch.
        let closed = |branch: serde_json::Value| {
            serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {"result": {"anyOf": [
                    branch,
                    {"type": "object", "required": ["file"],
                     "properties": {"file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}}}
                ]}}
            })
        };
        for branch in [
            serde_json::json!({"type": "object", "properties": {"note": {"type": "string"}},
                               "unevaluatedProperties": false}),
            serde_json::json!({"type": "object", "dependentRequired": {"file": ["plain"]}}),
        ] {
            let schema = closed(branch);
            let file = |c| serde_json::json!({"result": {"file": reference(c)}});
            assert_eq!(
                ['a', 'c', 'b'].map(|c| verdict(&schema, &file(c))),
                ["ok", "missing", "wrong"],
                "{schema}"
            );
        }
    }

    #[test]
    fn the_contract_id_is_read_as_the_validator_reads_it() {
        for id in [
            "https://example.com/schemas/answer.json",
            "answer.schema.json",
            "/schemas/answer",
            "../x",
            "https://Example.COM/a",
            "HTTP://example.com/a",
            "https://example.com:443/a",
            "https://example.com/a/../b",
            "https://example.com/%7euser/a",
            "https://example.com/a#",
            "schemas/v1/answer.schema.json",
            "./a/b",
            "https://example.com/a?b=c",
            "a?b",
            "HTTPS://EXAMPLE.COM/A/./B/../C?Q",
            "urn:example:answer",
            "URN:atomic:contract",
            "",
            "#",
        ] {
            let mut schema = tagged("anyOf");
            schema["$id"] = serde_json::json!(id);
            assert_eq!(four(&schema), FOUR, "{id:?}");
        }
    }

    #[test]
    fn odd_keys_are_escaped_in_pointers_and_fragments() {
        for key in [
            "a b",
            "a/b",
            "a~b",
            "a~1b",
            "a%20b",
            "a%b",
            "a#b",
            "\u{e4}\u{1F600}",
            "a?b",
            "a\"b",
            "",
            "$ref",
            "a\\b",
            "a\nb",
            "<a>",
            "0",
            "anyOf",
        ] {
            let schema = serde_json::json!({
                "type": "object",
                "properties": {key: {"type": "object",
                                     "properties": {"payload": tagged("anyOf")["properties"]["payload"]}}},
                "$defs": tagged("anyOf")["$defs"]
            });
            let got = [("diff", 'a'), ("text", 'b'), ("diff", 'b'), ("diff", 'c')]
                .map(|(kind, c)| verdict(&schema, &serde_json::json!({key: payload(kind, c)})));
            assert_eq!(got, FOUR, "{key:?}");
        }
        // Branches reached by escaped `$defs` names.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"payload": {"anyOf": [{"$ref": "#/$defs/a~1b"}, {"$ref": "#/$defs/c~0d"}]}},
            "$defs": {"a/b": tagged("anyOf")["properties"]["payload"]["anyOf"][0],
                      "c~d": tagged("anyOf")["$defs"]["note"]}
        });
        assert_eq!(four(&schema), FOUR);
    }

    #[test]
    fn embedded_resources_do_not_stand_in_for_the_contract() {
        let file = serde_json::json!({"anyOf": [
            {"type": "null"},
            {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}
        ]});
        let decoy = |id: &str| serde_json::json!({"$id": id, "properties": {"file": {"anyOf": [true, true]}}});
        for (root_id, defs) in [
            (
                None,
                serde_json::json!({"evil": decoy("urn:atomic:contract")}),
            ),
            (
                Some("https://example.com/c"),
                serde_json::json!({"evil": decoy("https://example.com/c")}),
            ),
            (
                Some("answer.json"),
                serde_json::json!({"evil": decoy("urn:atomic:contract")}),
            ),
            (
                None,
                serde_json::json!({"evil": {"$id": "atomic:artifact", "type": "string"}}),
            ),
        ] {
            let mut schema = serde_json::json!({
                "type": "object", "properties": {"file": file.clone()}, "$defs": defs
            });
            if let Some(id) = root_id {
                schema["$id"] = serde_json::json!(id);
            }
            let got = ['a', 'c', 'b']
                .map(|c| verdict(&schema, &serde_json::json!({"file": reference(c)})));
            assert_eq!(got, ["ok", "missing", "wrong"], "{schema}");
        }
    }

    /// The text branch of `tagged`, alone.
    fn note() -> serde_json::Value {
        tagged("anyOf")["$defs"]["note"].clone()
    }

    /// `tagged`, with the text branch replaced and other `$defs` added.
    fn tagged_with(second: serde_json::Value, defs: serde_json::Value) -> serde_json::Value {
        let mut schema = tagged("anyOf");
        schema["properties"]["payload"]["anyOf"][1] = second;
        for (k, v) in defs.as_object().expect("an object") {
            schema["$defs"][k] = v.clone();
        }
        schema
    }

    #[test]
    fn relative_ids_and_refs_in_a_contract_without_an_id() {
        let mut relative = note();
        relative["$id"] = serde_json::json!("note.json");
        let cases = [
            // An embedded resource with a relative id, reached by pointer.
            tagged_with(
                serde_json::json!({"$ref": "#/$defs/note"}),
                serde_json::json!({"note": relative.clone()}),
            ),
            // A relative id nowhere near the union.
            tagged_with(
                serde_json::json!({"$ref": "#/$defs/note"}),
                serde_json::json!({"other": {"$id": "other.json", "type": "string"}}),
            ),
            tagged_with(
                serde_json::json!({"$ref": "#/$defs/note"}),
                serde_json::json!({"other": {"$id": "x/y/z.json", "type": "string"}}),
            ),
        ];
        for schema in cases {
            assert_eq!(four(&schema), FOUR, "{schema}");
        }
        // A reference by the embedded resource's relative id is one the
        // reference check cannot follow, so the contract is refused.
        let schema = tagged_with(
            serde_json::json!({"$ref": "note.json"}),
            serde_json::json!({"note": relative}),
        );
        assert_eq!(four(&schema), ["unsupported"; 4]);
        // A relative reference to the contract itself.
        let mut schema = tagged("anyOf");
        schema["properties"]["again"] = serde_json::json!({"$ref": ""});
        assert_eq!(four(&schema), FOUR);
    }

    #[test]
    fn a_schema_keyword_that_names_no_known_draft() {
        for meta in [
            serde_json::json!("not a uri"),
            serde_json::json!("%%%"),
            serde_json::json!("custom-meta.json"),
            serde_json::json!("https://example.com/custom-meta"),
            serde_json::json!("urn:x:meta"),
            serde_json::json!("http://[bad"),
            serde_json::json!(""),
            serde_json::json!(7),
        ] {
            let mut schema = tagged("anyOf");
            schema["$schema"] = meta.clone();
            assert_eq!(four(&schema), FOUR, "{meta}");
        }
    }

    #[test]
    fn a_contract_that_cannot_be_indexed_fails_closed() {
        // `a b` is no URI reference, so the contract cannot be indexed and no
        // branch is decided. The open first branch must not then accept.
        let schema = serde_json::json!({
            "$id": "a b",
            "anyOf": [
                {"type": "object", "properties": {"note": {"type": "string"}}},
                {"type": "object", "required": ["file"],
                 "properties": {"file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}}}
            ]
        });
        assert!(Checker::new(&schema, &Store).registry.is_none());
        let file = |c| serde_json::json!({"file": reference(c)});
        assert_eq!(verdict(&schema, &file('c')), "missing");
        assert_eq!(verdict(&schema, &file('b')), "wrong");
        assert_eq!(verdict(&schema, &file('a')), "ok");
    }

    #[test]
    fn nested_decoys_and_the_wrapper_uri_cannot_stand_in() {
        let file = serde_json::json!({"anyOf": [
            {"type": "null"},
            {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}
        ]});
        let decoy = |id: &str| serde_json::json!({"$id": id, "properties": {"file": {"anyOf": [true, true]}}});
        for (root_id, defs) in [
            (
                None,
                serde_json::json!({"a": {"$id": "urn:other:x", "$defs": {"b": decoy("urn:atomic:contract")}}}),
            ),
            (
                None,
                serde_json::json!({"a": {"$id": "urn:other:x", "$defs": {"b": decoy("json-schema:///")}}}),
            ),
            (
                Some("https://example.com/c"),
                serde_json::json!({"a": {"$id": "https://example.com/other", "$defs": {"b": decoy("https://example.com/c")}}}),
            ),
            (
                Some("https://example.com/c"),
                serde_json::json!({"a": {"$id": "https://example.com/x/y", "$defs": {"b": decoy("../c")}}}),
            ),
            (
                Some("answer.json"),
                serde_json::json!({"a": {"$id": "urn:other:x", "$defs": {"b": decoy("json-schema:///answer.json")}}}),
            ),
            (None, serde_json::json!({"a": decoy("urn:atomic:branch")})),
        ] {
            let mut schema = serde_json::json!({
                "type": "object", "properties": {"file": file.clone()}, "$defs": defs
            });
            if let Some(id) = root_id {
                schema["$id"] = serde_json::json!(id);
            }
            let got =
                ['c', 'b'].map(|c| verdict(&schema, &serde_json::json!({"file": reference(c)})));
            assert_eq!(got, ["missing", "wrong"], "{schema}");
        }
        // A branch that refers to an embedded resource claiming the wrapper's URI.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"result": {"anyOf": [
                {"$ref": "urn:atomic:branch"},
                {"type": "object", "required": ["kind", "file"],
                 "properties": {"kind": {"const": "diff"},
                                "file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}}}
            ]}},
            "$defs": {"x": {"$id": "urn:atomic:branch", "type": "object", "required": ["kind"],
                            "properties": {"kind": {"const": "none"}}}}
        });
        let result = |c| serde_json::json!({"result": {"kind": "diff", "file": reference(c)}});
        // The reference check cannot follow `$ref: urn:atomic:branch`, so the
        // contract is refused outright.
        assert_eq!(
            ['a', 'c', 'b'].map(|c| verdict(&schema, &result(c))),
            ["unsupported"; 3]
        );
        // A contract named by the wrapper's URI fails closed.
        let mut schema = tagged("anyOf");
        schema["$id"] = serde_json::json!("urn:atomic:branch");
        assert_eq!(
            [
                verdict(&schema, &payload("diff", 'b')),
                verdict(&schema, &payload("diff", 'c'))
            ],
            ["wrong", "missing"]
        );
    }

    #[test]
    fn nested_and_overlapping_alternatives() {
        // anyOf inside oneOf.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"payload": {"oneOf": [
                {"anyOf": [tagged("anyOf")["properties"]["payload"]["anyOf"][0], {"$ref": "#/$defs/note"}]},
                {"type": "null"}
            ]}},
            "$defs": tagged("anyOf")["$defs"]
        });
        assert_eq!(four(&schema), FOUR);
        assert_eq!(verdict(&schema, &payload("text", 'a')), "wrong");
        assert_eq!(
            verdict(&schema, &serde_json::json!({"payload": null})),
            "ok"
        );
        // A value both branches fit: either branch's media type will do.
        let loose = |media: &str| {
            serde_json::json!({"type": "object",
                               "properties": {"file": {"$ref": "atomic:artifact", "mediaType": media}}})
        };
        let schema = serde_json::json!({"anyOf": [loose("text/x-diff"), loose("text/plain")]});
        let file = |c| serde_json::json!({"file": reference(c)});
        assert_eq!(
            ['a', 'b', 'c'].map(|c| verdict(&schema, &file(c))),
            ["ok", "ok", "missing"]
        );
        // allOf of two anyOfs: both must accept.
        let schema = serde_json::json!({"allOf": [
            {"anyOf": [loose("text/x-diff"), loose("text/plain")]},
            {"anyOf": [loose("text/plain"), {"type": "null"}]}
        ]});
        assert_eq!(
            ['a', 'b'].map(|c| verdict(&schema, &file(c))),
            ["wrong", "ok"]
        );
    }

    #[test]
    fn contracts_checked_in_turn_do_not_share_branches() {
        // The same pointers, opposite tags, both under the fixed URN.
        let a = serde_json::json!({"type": "object", "properties": {"payload": {"anyOf": [
            tagged("anyOf")["properties"]["payload"]["anyOf"][0],
            tagged("anyOf")["$defs"]["note"]]}}});
        let mut b = a.clone();
        b["properties"]["payload"]["anyOf"][0]["properties"]["kind"]["const"] =
            serde_json::json!("text");
        b["properties"]["payload"]["anyOf"][1]["properties"]["kind"]["const"] =
            serde_json::json!("diff");
        for _ in 0..3 {
            assert_eq!(verdict(&a, &payload("diff", 'a')), "ok");
            assert_eq!(verdict(&b, &payload("diff", 'a')), "wrong");
            assert_eq!(verdict(&b, &payload("diff", 'b')), "ok");
            assert_eq!(verdict(&a, &payload("diff", 'b')), "wrong");
        }
    }

    #[test]
    fn a_root_reference_holds_whatever_the_contract_id() {
        for id in [
            None,
            Some("https://example.com/report"),
            Some("report.json"),
            Some("urn:example:report"),
        ] {
            let mut schema = serde_json::json!({
                "type": "object", "additionalProperties": false, "required": ["title", "attachment"],
                "properties": {
                    "title": {"type": "string"},
                    "attachment": {"anyOf": [
                        {"type": "null"},
                        {"type": "object", "additionalProperties": false, "required": ["file", "about"],
                         "properties": {"file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
                                        "about": {"$ref": "#"}}}
                    ]}
                }
            });
            if let Some(id) = id {
                schema["$id"] = serde_json::json!(id);
            }
            let leaf = serde_json::json!({"title": "v", "attachment": null});
            let report = |c, about| serde_json::json!({"title": "t", "attachment": {"file": reference(c), "about": about}});
            for (c, want) in [('a', "ok"), ('c', "missing"), ('b', "wrong")] {
                assert_eq!(
                    verdict(&schema, &report(c, leaf.clone())),
                    want,
                    "{id:?} {c}"
                );
                assert_eq!(
                    verdict(&schema, &report('a', report(c, leaf.clone()))),
                    want,
                    "{id:?} nested {c}"
                );
            }
        }
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

    /// `{file, child}` nested `levels` deep, with `bottom` the deepest file.
    fn chain(levels: usize, bottom: char) -> serde_json::Value {
        let mut value = serde_json::json!({"file": reference(bottom), "child": null});
        for _ in 0..levels {
            value = serde_json::json!({"file": reference('a'), "child": value});
        }
        value
    }

    fn recursive_file() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
                "child": {"anyOf": [{"type": "null"}, {"$ref": "#"}]}
            }
        })
    }

    #[test]
    fn a_recursive_contract_is_checked_at_any_depth() {
        let schema = recursive_file();
        for levels in [0, 1, 20, 32, 70, 100] {
            assert_eq!(verdict(&schema, &chain(levels, 'a')), "ok", "{levels}");
            assert_eq!(verdict(&schema, &chain(levels, 'c')), "missing", "{levels}");
            assert_eq!(verdict(&schema, &chain(levels, 'b')), "wrong", "{levels}");
        }
        // Nested lists, too.
        let schema = serde_json::json!({"$ref": "#/$defs/tree", "$defs": {"tree": {"anyOf": [
            {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
            {"type": "array", "items": {"$ref": "#/$defs/tree"}}
        ]}}});
        let mut value = reference('c');
        for _ in 0..40 {
            value = serde_json::json!([value]);
        }
        assert_eq!(verdict(&schema, &value), "missing");
    }

    #[test]
    fn references_that_make_no_progress_are_refused() {
        // A cycle of references that never moves into the value.
        let schema = serde_json::json!({
            "$ref": "#/$defs/a",
            "$defs": {
                "a": {"$ref": "#/$defs/b"},
                "b": {"allOf": [{"$ref": "#/$defs/a"}], "properties": {"file": {"$ref": "atomic:artifact"}}}
            }
        });
        assert_eq!(
            verdict(&schema, &serde_json::json!({"file": reference('a')})),
            "too deep"
        );
    }

    #[test]
    fn overlapping_recursive_alternatives_take_linear_time() {
        for k in [2usize, 3] {
            let branches: Vec<_> = (0..k)
                .map(|i| {
                    serde_json::json!({"type": "object", "properties": {
                        "file": {"$ref": "atomic:artifact",
                                 "mediaType": if i == 0 { "text/x-diff" } else { "text/plain" }},
                        "child": {"$ref": "#"}}})
                })
                .collect();
            let schema = serde_json::json!({ "anyOf": branches });
            let mut value = serde_json::json!({"file": reference('c')});
            for _ in 0..24 {
                value = serde_json::json!({ "child": value });
            }
            let started = std::time::Instant::now();
            assert_eq!(verdict(&schema, &value), "missing", "{k}");
            let took = started.elapsed();
            assert!(took < std::time::Duration::from_secs(3), "{k}: {took:?}");
        }
    }

    #[test]
    fn maps_tuples_and_dependent_schemas_are_followed() {
        let map = serde_json::json!({"type": "object",
            "additionalProperties": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
            "properties": {"note": {"type": "string"}}});
        assert_eq!(
            verdict(&map, &serde_json::json!({"x": reference('a'), "note": "n"})),
            "ok"
        );
        assert_eq!(
            verdict(
                &map,
                &serde_json::json!({"x": reference('a'), "y": reference('b')})
            ),
            "wrong"
        );
        assert_eq!(
            verdict(&map, &serde_json::json!({"y": reference('c')})),
            "missing"
        );
        let tuple = serde_json::json!({"type": "array", "prefixItems": [
            {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
            {"$ref": "atomic:artifact", "mediaType": "text/plain"}
        ], "items": {"$ref": "atomic:artifact"}});
        assert_eq!(
            verdict(
                &tuple,
                &serde_json::json!([reference('a'), reference('b'), reference('a')])
            ),
            "ok"
        );
        assert_eq!(
            verdict(&tuple, &serde_json::json!([reference('b'), reference('a')])),
            "wrong"
        );
        assert_eq!(
            verdict(
                &tuple,
                &serde_json::json!([reference('a'), reference('b'), reference('c')])
            ),
            "missing"
        );
        let dependent = serde_json::json!({"type": "object", "dependentSchemas": {
            "file": {"properties": {"file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}}}}});
        assert_eq!(
            verdict(&dependent, &serde_json::json!({"file": reference('a')})),
            "ok"
        );
        assert_eq!(
            verdict(&dependent, &serde_json::json!({"file": reference('b')})),
            "wrong"
        );
    }

    #[test]
    fn percent_encoded_pointers_are_followed() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"file": {"$ref": "#/$defs/a%20diff"}, "other": {"$ref": "#/$defs/50%25"}},
            "$defs": {"a diff": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
                      "50%": {"$ref": "atomic:artifact", "mediaType": "text/plain"}}
        });
        assert_eq!(
            verdict(
                &schema,
                &serde_json::json!({"file": reference('a'), "other": reference('b')})
            ),
            "ok"
        );
        assert_eq!(
            verdict(&schema, &serde_json::json!({"file": reference('b')})),
            "wrong"
        );
        assert_eq!(
            verdict(&schema, &serde_json::json!({"other": reference('a')})),
            "wrong"
        );
    }

    #[test]
    fn contracts_whose_references_cannot_be_followed_are_refused() {
        let art = || serde_json::json!({"$ref": "atomic:artifact", "mediaType": "text/x-diff"});
        let refused = [
            serde_json::json!({"type": "object", "patternProperties": {"^f": art()}}),
            serde_json::json!({"type": "array", "contains": art()}),
            serde_json::json!({"if": {"type": "object"}, "then": {"properties": {"file": art()}}}),
            serde_json::json!({"if": {"type": "object"}, "else": {"properties": {"file": art()}}}),
            serde_json::json!({"if": {"properties": {"file": art()}}}),
            serde_json::json!({"not": {"properties": {"file": art()}}}),
            serde_json::json!({"type": "object", "propertyNames": art()}),
            serde_json::json!({"type": "object", "unevaluatedProperties": art()}),
            serde_json::json!({"type": "array", "unevaluatedItems": art()}),
            serde_json::json!({"type": "array", "additionalItems": art()}),
            serde_json::json!({"type": "object", "dependencies": {"file": {"properties": {"file": art()}}}}),
            // Reached through an unfollowed keyword by a reference.
            serde_json::json!({"type": "array", "contains": {"$ref": "#/$defs/f"}, "$defs": {"f": art()}}),
            // additionalProperties beside patternProperties.
            serde_json::json!({"type": "object", "patternProperties": {"^x": {"type": "string"}},
                               "additionalProperties": art()}),
            // References the walk cannot resolve.
            serde_json::json!({"properties": {"file": {"$ref": "#diff"}},
                               "$defs": {"d": {"$anchor": "diff", "$ref": "atomic:artifact"}}}),
            serde_json::json!({"$dynamicAnchor": "node", "properties": {"file": art(),
                               "child": {"$dynamicRef": "#node"}}}),
            serde_json::json!({"$id": "https://example.com/r", "properties": {"file": art(),
                               "child": {"$ref": "https://example.com/r"}}}),
            serde_json::json!({"properties": {"file": art(), "child": {"$ref": "other.json"}}}),
            serde_json::json!({"properties": {"file": art(), "child": {"$ref": "#/$defs/missing"}}}),
            // Embedded resources and drafts.
            serde_json::json!({"properties": {"file": art(), "x": {"$id": "https://example.com/x",
                               "properties": {"y": {"$ref": "#"}}}}}),
            serde_json::json!({"properties": {"file": art(),
                               "x": {"$schema": "http://json-schema.org/draft-07/schema#", "type": "string"}}}),
            // A media type that is not beside the artifact reference.
            serde_json::json!({"properties": {"file": {"$ref": "#/$defs/f", "mediaType": "text/x-diff"}},
                               "$defs": {"f": {"$ref": "atomic:artifact"}}}),
            serde_json::json!({"properties": {"file": {"type": "string", "mediaType": "text/x-diff"}}}),
            serde_json::json!({"properties": {"file": {"$ref": "atomic:artifact", "mediaType": 7}}}),
        ];
        for schema in refused {
            assert!(check_contract(&schema).is_err(), "{schema}");
            assert_eq!(
                verdict(&schema, &serde_json::json!({"file": reference('a')})),
                "unsupported",
                "{schema}"
            );
        }
        // The same keywords are fine in a contract with no artifact references,
        // and so are artifact references in definitions nothing reaches.
        let accepted = [
            serde_json::json!({"type": "array", "contains": {"type": "string"}, "$id": "https://example.com/a"}),
            serde_json::json!({"properties": {"x": {"$ref": "#anchor"}}, "$defs": {"a": {"$anchor": "anchor"}}}),
            serde_json::json!({"properties": {"x": {"$schema": "http://json-schema.org/draft-07/schema#"}}}),
            serde_json::json!({"properties": {"file": art()}, "$defs": {"unused": {"contains": art()}},
                               "$schema": "http://json-schema.org/draft-07/schema#"}),
            serde_json::json!({"properties": {"file": art(), "self": {"$ref": "#"}, "again": {"$ref": ""}}}),
            serde_json::json!({"properties": {"file": art(), "x": {"$id": "https://example.com/x", "type": "string"}}}),
        ];
        for schema in accepted {
            assert_eq!(check_contract(&schema), Ok(()), "{schema}");
        }
    }

    #[test]
    fn a_contract_too_large_to_check_fails_closed() {
        // Every branch reaches one large shared definition.
        let props: serde_json::Map<String, serde_json::Value> = (0..60_000)
            .map(|i| (format!("p{i}"), serde_json::json!({"type": "string"})))
            .collect();
        let branches: Vec<_> = (0..8)
            .map(|i| {
                serde_json::json!({"type": "object", "required": ["kind", "file"], "properties": {
                "kind": {"const": format!("k{i}")},
                "file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"},
                "shared": {"$ref": "#/$defs/shared"}}})
            })
            .collect();
        let schema =
            serde_json::json!({"anyOf": branches, "$defs": {"shared": {"properties": props}}});
        let value = serde_json::json!({"kind": "k7", "file": reference('a')});
        let started = std::time::Instant::now();
        assert_eq!(verdict(&schema, &value), "too complex");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        // A small contract with many branches is fine.
        let branches: Vec<_> = (0..200)
            .map(|i| {
                serde_json::json!({"type": "object", "required": ["kind", "file"], "properties": {
                "kind": {"const": format!("k{i}")},
                "file": {"$ref": "atomic:artifact", "mediaType": "text/x-diff"}}})
            })
            .collect();
        let schema = serde_json::json!({ "anyOf": branches });
        assert_eq!(
            verdict(
                &schema,
                &serde_json::json!({"kind": "k199", "file": reference('a')})
            ),
            "ok"
        );
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
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
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
    /// The contract's references go round, or run on, without moving into
    /// the value.
    #[error("{at}: the contract's references go round without reaching a value")]
    TooDeep {
        /// Where in the value.
        at: String,
    },
    /// The contract holds artifact references in a form the check cannot
    /// follow (see [`check_contract`]).
    #[error("schema {at}: {reason}")]
    Unsupported {
        /// A JSON Pointer into the contract.
        at: String,
        /// What is not supported.
        reason: String,
    },
    /// Deciding which alternatives the value fits would compile more of the
    /// contract than one check allows.
    #[error("{at}: the contract is too large to check its artifact references")]
    TooComplex {
        /// Where in the value.
        at: String,
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
///
/// A contract with no artifact references passes at once. One whose
/// references [`check_contract`] refuses fails every check.
pub fn check_refs(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    store: &dyn ArtifactLookup,
) -> Result<(), ArtifactCheckError> {
    check_contract(schema)?;
    if !holds_artifact_refs(schema) {
        return Ok(());
    }
    Checker::new(schema, store).check(value).0
}

/// Keywords whose subschemas the reference check follows into the value.
const FOLLOWED_ONE: &[&str] = &["items", "additionalProperties"];
const FOLLOWED_MAP: &[&str] = &["properties", "dependentSchemas"];
const FOLLOWED_LIST: &[&str] = &["prefixItems", "allOf", "anyOf", "oneOf"];
/// Keywords holding subschemas the check does not follow.
const UNFOLLOWED_ONE: &[&str] = &[
    "contains",
    "if",
    "then",
    "else",
    "not",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
    "additionalItems",
    "contentSchema",
];
const UNFOLLOWED_MAP: &[&str] = &["patternProperties", "dependencies"];
/// Keywords that only hold definitions, reached by reference.
const DEFINITIONS: &[&str] = &["$defs", "definitions"];

/// Whether `schema` holds an `atomic:artifact` reference anywhere.
pub fn holds_artifact_refs(schema: &serde_json::Value) -> bool {
    match schema {
        serde_json::Value::Object(map) => {
            map.get("$ref").and_then(serde_json::Value::as_str) == Some(ARTIFACT_SCHEMA_ID)
                || map.values().any(holds_artifact_refs)
        }
        serde_json::Value::Array(items) => items.iter().any(holds_artifact_refs),
        _ => false,
    }
}

/// Whether `key` is a key anywhere in `value`.
fn mentions(value: &serde_json::Value, key: &str) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            map.contains_key(key) || map.values().any(|v| mentions(v, key))
        }
        serde_json::Value::Array(items) => items.iter().any(|v| mentions(v, key)),
        _ => false,
    }
}

/// The JSON Pointer a local `$ref` (`""`, `#` or `#/...`, percent-encoded)
/// names, or `None` for any other reference.
fn local_pointer(r: &str) -> Option<String> {
    if r.is_empty() {
        return Some(String::new());
    }
    r.strip_prefix('#')
        .filter(|p| p.is_empty() || p.starts_with('/'))
        .and_then(decode)
}

/// Refuse a contract whose artifact references the check could not follow,
/// so that none of them can be skipped. A contract with no artifact
/// references is always accepted. In one that has them:
///
/// - an artifact reference must be reached only through `properties`,
///   `items`, `prefixItems`, `additionalProperties` (with no
///   `patternProperties` beside it), `dependentSchemas`, `allOf`, `anyOf`,
///   `oneOf` and local references, not under `contains`, `if`, `then`,
///   `else`, `not`, `patternProperties` or any other keyword;
/// - every `$ref` is `atomic:artifact`, `#`, or a `#/...` pointer into the
///   contract, so no `$anchor`, `$dynamicRef`, relative or absolute URI;
/// - no subschema has a `$schema` of its own, and none inside an embedded
///   resource (a subschema with its own `$id`) uses a local `$ref`, whose
///   `#` would mean that resource;
/// - `mediaType` appears only beside `$ref: atomic:artifact`, as a string.
///
/// A contract that has no artifact reference but uses `mediaType` is
/// checked too, since that `mediaType` would never be enforced.
pub fn check_contract(schema: &serde_json::Value) -> Result<(), ArtifactCheckError> {
    if !holds_artifact_refs(schema) && !mentions(schema, "mediaType") {
        return Ok(());
    }
    lint(schema, schema, "", false)?;
    let mut seen = std::collections::HashSet::new();
    reach(schema, "", None, &mut seen)
}

fn unsupported(at: &str, reason: impl Into<String>) -> ArtifactCheckError {
    ArtifactCheckError::Unsupported {
        at: if at.is_empty() { "/".into() } else { at.into() },
        reason: reason.into(),
    }
}

/// The subschemas `node` holds, with the pointer to each and the keyword
/// that holds it.
fn subschemas<'v>(
    node: &'v serde_json::Value,
    at: &str,
) -> Vec<(String, &'static str, &'v serde_json::Value)> {
    use serde_json::Value;
    let mut out = Vec::new();
    let Some(map) = node.as_object() else {
        return out;
    };
    let ones = FOLLOWED_ONE.iter().chain(UNFOLLOWED_ONE);
    for &k in ones {
        match map.get(k) {
            Some(Value::Array(items)) => {
                for (i, s) in items.iter().enumerate() {
                    out.push((format!("{at}/{k}/{i}"), k, s));
                }
            }
            Some(s) => out.push((format!("{at}/{k}"), k, s)),
            None => {}
        }
    }
    for &k in FOLLOWED_MAP.iter().chain(UNFOLLOWED_MAP).chain(DEFINITIONS) {
        if let Some(Value::Object(entries)) = map.get(k) {
            for (name, s) in entries {
                out.push((format!("{at}/{k}/{}", token(name)), k, s));
            }
        }
    }
    for &k in FOLLOWED_LIST {
        if let Some(Value::Array(items)) = map.get(k) {
            for (i, s) in items.iter().enumerate() {
                out.push((format!("{at}/{k}/{i}"), k, s));
            }
        }
    }
    out
}

/// The structural rules, over every subschema. `embedded` is whether `node`
/// is inside a resource with its own `$id`.
fn lint(
    root: &serde_json::Value,
    node: &serde_json::Value,
    at: &str,
    embedded: bool,
) -> Result<(), ArtifactCheckError> {
    use serde_json::Value;
    let Some(map) = node.as_object() else {
        return Ok(());
    };
    if !at.is_empty() && map.contains_key("$schema") {
        return Err(unsupported(at, "a subschema with its own `$schema`"));
    }
    let embedded = embedded || (!at.is_empty() && map.contains_key("$id"));
    for k in ["$dynamicRef", "$recursiveRef"] {
        if map.contains_key(k) {
            return Err(unsupported(
                at,
                format!("`{k}`, which the check cannot follow"),
            ));
        }
    }
    let artifact = map.get("$ref").and_then(Value::as_str) == Some(ARTIFACT_SCHEMA_ID);
    match map.get("$ref") {
        None => {}
        Some(Value::String(r)) if r == ARTIFACT_SCHEMA_ID => {}
        Some(Value::String(r)) => match local_pointer(r) {
            Some(_) if embedded => {
                return Err(unsupported(
                    at,
                    format!(
                        "`$ref: {r}` inside an embedded resource, where it names that resource"
                    ),
                ));
            }
            Some(p) if root.pointer(&p).is_some() => {}
            _ => {
                return Err(unsupported(
                    at,
                    format!("`$ref: {r}`, which is not a pointer into the contract"),
                ));
            }
        },
        Some(_) => return Err(unsupported(at, "a `$ref` that is not a string")),
    }
    match map.get("mediaType") {
        None => {}
        Some(Value::String(_)) if artifact => {}
        Some(Value::String(_)) => {
            return Err(unsupported(
                at,
                "a `mediaType` that is not beside `$ref: atomic:artifact`",
            ));
        }
        Some(_) => return Err(unsupported(at, "a `mediaType` that is not a string")),
    }
    for (sub, _, s) in subschemas(node, at) {
        lint(root, s, &sub, embedded)?;
    }
    Ok(())
}

/// Walk what the check walks, from `at`, and refuse an artifact reference
/// reached through a keyword it does not follow (`under`).
fn reach(
    root: &serde_json::Value,
    at: &str,
    under: Option<&'static str>,
    seen: &mut std::collections::HashSet<(String, Option<&'static str>)>,
) -> Result<(), ArtifactCheckError> {
    use serde_json::Value;
    if !seen.insert((at.to_owned(), under)) {
        return Ok(());
    }
    let Some(node) = root.pointer(at) else {
        return Ok(());
    };
    match node.get("$ref").and_then(Value::as_str) {
        Some(ARTIFACT_SCHEMA_ID) => {
            if let Some(keyword) = under {
                return Err(unsupported(
                    at,
                    format!(
                        "an artifact reference under `{keyword}`, which the check does not follow"
                    ),
                ));
            }
        }
        Some(r) => {
            if let Some(target) = local_pointer(r) {
                reach(root, &target, under, seen)?;
            }
        }
        None => {}
    }
    let patterned = node.get("patternProperties").is_some();
    for (sub, keyword, _) in subschemas(node, at) {
        if DEFINITIONS.contains(&keyword) {
            continue;
        }
        let followed = (FOLLOWED_ONE.contains(&keyword)
            || FOLLOWED_MAP.contains(&keyword)
            || FOLLOWED_LIST.contains(&keyword))
            && !(keyword == "additionalProperties" && patterned)
            && !(keyword == "items" && node.get("items").is_some_and(Value::is_array));
        let under = under.or((!followed).then_some(keyword));
        reach(root, &sub, under, seen)?;
    }
    Ok(())
}

/// A URI fragment, percent-decoded.
fn decode(fragment: &str) -> Option<String> {
    let bytes = fragment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
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

/// The URI the contract is indexed under when its `$id` is not a URI
/// reference at all. Such a contract cannot be indexed, so every branch is
/// undecided and the check fails closed.
const CONTRACT_URI: &str = "urn:atomic:contract";

/// The base URI the branch wrappers are compiled under. A contract that
/// claims it is refused, so a branch can never refer to its own wrapper.
const BRANCH_URI: &str = "urn:atomic:branch";

/// One `check_refs` call: the contract, the store, and each branch's
/// validator, compiled the first time it is needed.
struct Checker<'a> {
    root: &'a serde_json::Value,
    store: &'a dyn ArtifactLookup,
    base: String,
    registry: Option<jsonschema::Registry<'a>>,
    branches: std::collections::HashMap<String, Option<jsonschema::Validator>>,
    compiled: usize,
    /// How many schema nodes branch validators may still compile.
    budget: usize,
    /// Whether the budget ran out.
    exhausted: bool,
    /// Each finished (schema pointer, value pointer) and its result: the
    /// value at a value pointer never changes during a check, so neither does
    /// the result, and overlapping alternatives are each walked once.
    done: std::collections::HashMap<(String, String), Result<(), ArtifactCheckError>>,
    /// The (schema pointer, value pointer) pairs being walked.
    walking: std::collections::HashSet<(String, String)>,
}

/// How many schema nodes, summed over the branch validators one check
/// compiles, it may compile. Past this the check fails closed.
const COMPILE_BUDGET: usize = 200_000;

/// How many references and combinators in a row the check follows without
/// moving into the value before it gives up.
const MAX_HOPS: usize = 128;

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
        // The URI the answer's validator gives the contract: its `$id`, or
        // "" when it has none, resolved against jsonschema's default base
        // (json-schema:///) and normalised as jsonschema does it. Relative
        // `$id`s and `$ref`s inside it then resolve as they do when the whole
        // answer is validated.
        let base = root
            .get("$id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let base = jsonschema::uri::from_str(base.trim_end_matches('#'))
            .map(|uri| uri.as_str().to_owned())
            .unwrap_or_else(|_| CONTRACT_URI.to_owned());
        // Indexed once per check, as Draft 2020-12 whatever its `$schema`
        // says, because that is the draft the answer's validator forces, and
        // without fetching meta-schemas a `$schema` names. A contract that
        // cannot be indexed leaves every branch undecided, so the check fails
        // closed.
        let registry = jsonschema::Registry::new()
            .draft(jsonschema::Draft::Draft202012)
            .retrieve_known_draft_meta_schemas(false)
            .add(
                &base,
                jsonschema::Draft::Draft202012.create_resource_ref(root),
            )
            .and_then(|b| b.add(ARTIFACT_SCHEMA_ID, artifact_schema()))
            .and_then(jsonschema::RegistryBuilder::prepare)
            .ok()
            // The contract must be what its URI names (an embedded resource
            // claiming the same URI must not replace it), and nothing in it
            // may claim the URI the branch wrappers are compiled under.
            .filter(|registry| {
                !registry.contains_resource(BRANCH_URI)
                    && jsonschema::uri::from_str(&base).is_ok_and(|uri| {
                        registry
                            .resolver(uri)
                            .lookup("")
                            .is_ok_and(|found| std::ptr::eq(found.contents(), root))
                    })
            });
        Self {
            root,
            store,
            base,
            registry,
            branches: std::collections::HashMap::new(),
            compiled: 0,
            budget: COMPILE_BUDGET,
            exhausted: false,
            done: std::collections::HashMap::new(),
            walking: std::collections::HashSet::new(),
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
            // What the validator would compile: everything the branch reaches.
            let size = reachable_size(self.root, pointer, self.budget.saturating_add(1));
            let validator = if size > self.budget {
                self.exhausted = true;
                None
            } else {
                self.budget -= size;
                let wrapper =
                    serde_json::json!({"$ref": format!("{}#{}", self.base, fragment(pointer))});
                self.registry.as_ref().and_then(|registry| {
                    jsonschema::options()
                        .with_draft(jsonschema::Draft::Draft202012)
                        .with_base_uri(BRANCH_URI)
                        .with_registry(registry)
                        .build(&wrapper)
                        .ok()
                })
            };
            self.branches.insert(pointer.to_owned(), validator);
        }
        self.branches[pointer].as_ref().map(|v| v.is_valid(value))
    }

    fn check_at(
        &mut self,
        pointer: &str,
        value: &serde_json::Value,
        at: &str,
        hops: usize,
    ) -> Result<(), ArtifactCheckError> {
        let key = (pointer.to_owned(), at.to_owned());
        if let Some(result) = self.done.get(&key) {
            return result.clone();
        }
        // Back at a pair still being walked, or too long without progress:
        // the references go round.
        if hops > MAX_HOPS || !self.walking.insert(key.clone()) {
            return Err(ArtifactCheckError::TooDeep { at: at.to_owned() });
        }
        let result = self.walk(pointer, value, at, hops);
        self.walking.remove(&key);
        self.done.insert(key, result.clone());
        result
    }

    fn walk(
        &mut self,
        pointer: &str,
        value: &serde_json::Value,
        at: &str,
        hops: usize,
    ) -> Result<(), ArtifactCheckError> {
        use serde_json::Value;
        let root = self.root;
        let Some(schema) = root.pointer(pointer) else {
            return Err(unsupported(
                pointer,
                "a pointer that names nothing in the contract",
            ));
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
            Some(local) => {
                // check_contract has made every other reference a pointer
                // into the contract.
                let target = local_pointer(local)
                    .ok_or_else(|| unsupported(pointer, format!("`$ref: {local}`")))?;
                self.check_at(&target, value, at, hops + 1)?;
            }
            None => {}
        }
        // Into the value: a fresh run of hops.
        if let Some(obj) = value.as_object() {
            let props = schema.get("properties").and_then(Value::as_object);
            let extra = schema
                .get("additionalProperties")
                .filter(|_| schema.get("patternProperties").is_none());
            for (k, v) in obj {
                let sub = match props.and_then(|p| p.get(k)) {
                    Some(_) => format!("{pointer}/properties/{}", token(k)),
                    None if extra.is_some() => format!("{pointer}/additionalProperties"),
                    None => continue,
                };
                self.check_at(&sub, v, &format!("{at}/{}", token(k)), 0)?;
            }
            if let Some(dependent) = schema.get("dependentSchemas").and_then(Value::as_object) {
                for k in dependent.keys() {
                    if obj.contains_key(k) {
                        let sub = format!("{pointer}/dependentSchemas/{}", token(k));
                        self.check_at(&sub, value, at, hops + 1)?;
                    }
                }
            }
        }
        if let Some(arr) = value.as_array() {
            let prefix = schema
                .get("prefixItems")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let items = schema.get("items").is_some_and(|i| !i.is_array());
            for (i, v) in arr.iter().enumerate() {
                let sub = if i < prefix {
                    format!("{pointer}/prefixItems/{i}")
                } else if items {
                    format!("{pointer}/items")
                } else {
                    continue;
                };
                self.check_at(&sub, v, &format!("{at}/{i}"), 0)?;
            }
        }
        // Every `allOf` branch applies, so every branch's references must hold.
        if let Some(Value::Array(branches)) = schema.get("allOf") {
            for i in 0..branches.len() {
                self.check_at(&format!("{pointer}/allOf/{i}"), value, at, hops + 1)?;
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
                let fit = self.fits(&sub, value);
                if self.exhausted {
                    return Err(ArtifactCheckError::TooComplex { at: at.to_owned() });
                }
                match fit {
                    Some(true) => match self.check_at(&sub, value, at, hops + 1) {
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
                self.check_at(&sub, value, at, hops + 1)?;
            }
        }
        Ok(())
    }
}

/// How many schema nodes are reachable from `pointer`, following local
/// references, counting no further than `limit`.
fn reachable_size(root: &serde_json::Value, pointer: &str, limit: usize) -> usize {
    use serde_json::Value;
    let mut seen = std::collections::HashSet::new();
    let mut stack: Vec<&Value> = root.pointer(pointer).into_iter().collect();
    let mut count = 0;
    while let Some(node) = stack.pop() {
        if count >= limit {
            break;
        }
        if !seen.insert(std::ptr::from_ref(node)) {
            continue;
        }
        count += 1;
        match node {
            Value::Object(map) => {
                if let Some(target) = map
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(local_pointer)
                    .and_then(|p| root.pointer(&p))
                {
                    stack.push(target);
                }
                stack.extend(
                    map.iter()
                        .filter(|(k, _)| !DEFINITIONS.contains(&k.as_str()))
                        .map(|(_, v)| v),
                );
            }
            Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    count
}
