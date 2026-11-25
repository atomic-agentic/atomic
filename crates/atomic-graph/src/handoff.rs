// SPDX-License-Identifier: Apache-2.0
//! Bounded, agent-trust handoffs between graph claim holders.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

const MAX_TEXT_CHARS: usize = 1024;
const MAX_FILES: usize = 128;

/// What the previous holder learned before it released the item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    /// What the previous holder tried.
    pub attempted: String,
    /// What stopped the previous holder.
    pub failed: String,
    /// What the next holder should know before trying again.
    pub learned: String,
    /// Host-observed file digests at release, never model-supplied digests.
    pub files: Vec<FileAtRelease>,
}

/// A repository-relative file as observed by the host at release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAtRelease {
    /// Repository-relative path.
    pub path: String,
    /// SHA-256 of the file bytes, or None if the file did not exist.
    pub sha256: Option<String>,
}

/// A file's relation to the next claimant's observed workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffFileStatus {
    /// Repository-relative path.
    pub path: String,
    /// Whether the host-observed contents differ from the released snapshot.
    pub changed: bool,
}

impl Handoff {
    /// Reject oversized narratives and unsafe or duplicate file paths before
    /// the record enters a claim-release transaction.
    pub fn validate(&self) -> Result<(), String> {
        if [&self.attempted, &self.failed, &self.learned]
            .iter()
            .any(|value| value.chars().count() > MAX_TEXT_CHARS)
        {
            return Err("handoff narrative exceeds 1024 characters".into());
        }
        if self.files.len() > MAX_FILES {
            return Err("handoff has too many files".into());
        }
        let mut seen = BTreeSet::new();
        for file in &self.files {
            let path = Path::new(&file.path);
            if file.path.is_empty()
                || file.path.len() > 256
                || file.path.chars().any(|c| matches!(c, '\\' | ':' | '\0'))
                || path.is_absolute()
                || !path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
                || !seen.insert(&file.path)
            {
                return Err("handoff file path is unsafe or repeated".into());
            }
            if file.sha256.as_ref().is_some_and(|sha| {
                sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            }) {
                return Err("handoff file digest is not SHA-256 hex".into());
            }
        }
        Ok(())
    }

    /// The caller supplies a fresh host-observed digest for every recorded
    /// path. A missing map entry is an error, never evidence of no change.
    pub fn compare_files(
        &self,
        current: &BTreeMap<String, Option<String>>,
    ) -> Result<Vec<HandoffFileStatus>, String> {
        self.validate()?;
        self.files
            .iter()
            .map(|file| {
                let observed = current
                    .get(&file.path)
                    .ok_or_else(|| format!("current handoff file is unobserved: {}", file.path))?;
                if observed.as_ref().is_some_and(|sha| {
                    sha.len() != 64 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit())
                }) {
                    return Err(format!(
                        "current handoff file digest is invalid: {}",
                        file.path
                    ));
                }
                Ok(HandoffFileStatus {
                    path: file.path.clone(),
                    changed: observed != &file.sha256,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_claimant_sees_changed_and_unchanged_files() {
        let handoff = Handoff {
            attempted: "Updated the parser".into(),
            failed: "The fixture did not pass".into(),
            learned: "The parser needs an EOF case".into(),
            files: vec![
                FileAtRelease {
                    path: "src/parser.rs".into(),
                    sha256: Some("a".repeat(64)),
                },
                FileAtRelease {
                    path: "src/fixture.rs".into(),
                    sha256: Some("b".repeat(64)),
                },
            ],
        };
        let current = BTreeMap::from([
            ("src/parser.rs".into(), Some("a".repeat(64))),
            ("src/fixture.rs".into(), Some("c".repeat(64))),
        ]);
        assert_eq!(
            handoff.compare_files(&current).unwrap(),
            vec![
                HandoffFileStatus {
                    path: "src/parser.rs".into(),
                    changed: false
                },
                HandoffFileStatus {
                    path: "src/fixture.rs".into(),
                    changed: true
                },
            ]
        );
        let mut unobserved = current;
        unobserved.remove("src/parser.rs");
        assert!(handoff.compare_files(&unobserved).is_err());
    }

    #[test]
    fn invalid_paths_and_digests_are_refused() {
        let mut handoff = Handoff {
            attempted: String::new(),
            failed: String::new(),
            learned: String::new(),
            files: vec![FileAtRelease {
                path: "../secret".into(),
                sha256: None,
            }],
        };
        assert!(handoff.validate().is_err());
        handoff.files[0].path = "safe.txt".into();
        handoff.files[0].sha256 = Some("not a digest".into());
        assert!(handoff.validate().is_err());
    }
}
