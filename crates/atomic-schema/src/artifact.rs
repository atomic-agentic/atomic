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
