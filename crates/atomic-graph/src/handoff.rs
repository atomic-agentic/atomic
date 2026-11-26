// SPDX-License-Identifier: Apache-2.0
//! Bounded, agent-trust handoffs between graph claim holders.

use atomic_core::git::{Repo, TreeEntry, is_sha};
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
    /// Host-observed Git tree entries at release, never model-supplied values.
    pub files: Vec<FileAtRelease>,
}

/// Immutable Git tree identity for one path. Mode matters: a symlink or
/// executable-bit change is a change even when the object ID matches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFingerprint {
    /// Git tree mode, for example `100644` or `120000`.
    pub mode: String,
    /// Git object ID (SHA-1 or SHA-256, matching the repository).
    pub id: String,
}

/// A repository-relative file as observed by the host at release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAtRelease {
    /// Repository-relative path.
    pub path: String,
    /// Git tree identity, or None if the path was deleted.
    pub entry: Option<GitFingerprint>,
}

/// A file's relation to the next claimant's observed workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffFileStatus {
    /// Repository-relative path.
    pub path: String,
    /// Whether the host-observed contents differ from the released snapshot.
    pub changed: bool,
}

/// A bounded host snapshot. Omitted paths are explicit so a release remains
/// possible even when the candidate exceeds the brief's file budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSnapshot {
    /// Safely representable changed paths kept within the brief's budget.
    pub files: Vec<FileAtRelease>,
    /// Changed paths omitted due to the file budget or path policy.
    pub omitted: usize,
    /// Whether Git tree inspection failed after commit provenance was checked.
    pub unavailable: bool,
}

impl FileSnapshot {
    /// Preserve release liveness when file inspection fails.
    pub fn unavailable() -> Self {
        Self {
            files: Vec::new(),
            omitted: 0,
            unavailable: true,
        }
    }

    /// Bounded explanation appended to the agent-trust handoff narrative.
    pub fn note(&self) -> String {
        if self.unavailable {
            " Host file comparison was unavailable; inspect the candidate commit.".into()
        } else if self.omitted > 0 {
            format!(
                " Host omitted {} changed path(s) from the bounded file comparison.",
                self.omitted
            )
        } else {
            String::new()
        }
    }
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
            if !valid_path(&file.path) || !seen.insert(&file.path) {
                return Err("handoff file path is unsafe or repeated".into());
            }
            if file.entry.as_ref().is_some_and(|entry| !valid_entry(entry)) {
                return Err("handoff file identity is invalid".into());
            }
        }
        Ok(())
    }

    /// The caller supplies a fresh host-observed Git tree identity for every
    /// recorded path. A missing map entry is an error, never no-change proof.
    pub fn compare_files(
        &self,
        current: &BTreeMap<String, Option<GitFingerprint>>,
    ) -> Result<Vec<HandoffFileStatus>, String> {
        self.validate()?;
        self.files
            .iter()
            .map(|file| {
                let observed = current
                    .get(&file.path)
                    .ok_or_else(|| format!("current handoff file is unobserved: {}", file.path))?;
                if observed.as_ref().is_some_and(|entry| !valid_entry(entry)) {
                    return Err(format!(
                        "current handoff file identity is invalid: {}",
                        file.path
                    ));
                }
                Ok(HandoffFileStatus {
                    path: file.path.clone(),
                    changed: observed != &file.entry,
                })
            })
            .collect()
    }

    /// Compare the released entries with a next claimant's pinned Git base.
    /// The caller runs this outside the claim transaction; no worktree path or
    /// symlink is read, only immutable repository objects.
    pub fn compare_git(
        &self,
        repo: &Repo,
        next_base: &str,
    ) -> Result<Vec<HandoffFileStatus>, String> {
        let entries = repo.entries(next_base).map_err(|error| error.to_string())?;
        let tree = entries_by_path(entries);
        let current = self
            .files
            .iter()
            .map(|file| (file.path.clone(), tree.get(&file.path).cloned()))
            .collect();
        self.compare_files(&current)
    }
}

fn valid_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && value.len() <= 256
        && !value.chars().any(|c| matches!(c, '\\' | ':' | '\0'))
        && !path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn valid_entry(entry: &GitFingerprint) -> bool {
    matches!(
        entry.mode.as_str(),
        "100644" | "100755" | "120000" | "160000"
    ) && is_sha(&entry.id)
}

fn entries_by_path(entries: Vec<TreeEntry>) -> BTreeMap<String, GitFingerprint> {
    entries
        .into_iter()
        .map(|entry| {
            (
                entry.path,
                GitFingerprint {
                    mode: entry.mode,
                    id: entry.id,
                },
            )
        })
        .collect()
}

/// Capture only changed paths from the host-recorded candidate Git trees.
/// Git's safe runner disables external drivers and hooks. A deletion has no
/// entry in the candidate tree and remains explicit in the record.
pub fn snapshot_changed_files(
    repo: &Repo,
    base: &str,
    commit: &str,
) -> Result<FileSnapshot, String> {
    let changed = repo
        .changed_paths(base, commit)
        .map_err(|error| error.to_string())?;
    let tree = entries_by_path(repo.entries(commit).map_err(|error| error.to_string())?);
    let mut files = Vec::new();
    let mut omitted = 0;
    for path in changed {
        if !valid_path(&path) || files.len() >= MAX_FILES {
            omitted += 1;
            continue;
        }
        files.push(FileAtRelease {
            entry: tree.get(&path).cloned(),
            path,
        });
    }
    let record = Handoff {
        attempted: String::new(),
        failed: String::new(),
        learned: String::new(),
        files: files.clone(),
    };
    record.validate()?;
    Ok(FileSnapshot {
        files,
        omitted,
        unavailable: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::git::Git;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fp(value: char) -> GitFingerprint {
        GitFingerprint {
            mode: "100644".into(),
            id: value.to_string().repeat(40),
        }
    }

    #[test]
    fn later_claimant_sees_changed_and_unchanged_files() {
        let handoff = Handoff {
            attempted: "Updated the parser".into(),
            failed: "The fixture did not pass".into(),
            learned: "The parser needs an EOF case".into(),
            files: vec![
                FileAtRelease {
                    path: "src/parser.rs".into(),
                    entry: Some(fp('a')),
                },
                FileAtRelease {
                    path: "src/fixture.rs".into(),
                    entry: Some(fp('b')),
                },
            ],
        };
        let current = BTreeMap::from([
            ("src/parser.rs".into(), Some(fp('a'))),
            ("src/fixture.rs".into(), Some(fp('c'))),
        ]);
        assert_eq!(
            handoff.compare_files(&current).expect("observed files"),
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
                entry: None,
            }],
        };
        assert!(handoff.validate().is_err());
        handoff.files[0].path = "safe.txt".into();
        handoff.files[0].entry = Some(GitFingerprint {
            mode: "100644".into(),
            id: "not a digest".into(),
        });
        assert!(handoff.validate().is_err());
    }

    #[test]
    fn snapshots_and_compares_immutable_git_trees() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "atomic-handoff-git-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("scratch repository");
        let git = Git::find().expect("Git");
        let command = |args: &[&str]| -> String {
            let output = Command::new(git.program())
                .args([
                    "-c",
                    "user.name=synthetic-author",
                    "-c",
                    "user.email=synthetic@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                ])
                .args(args)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("Git command");
            assert!(output.status.success(), "git {args:?}: {output:?}");
            String::from_utf8(output.stdout)
                .expect("Git output")
                .trim()
                .to_owned()
        };
        command(&["init", "-q"]);
        std::fs::write(root.join("changed.txt"), "old").expect("first file");
        std::fs::write(root.join("unchanged.txt"), "original").expect("second file");
        command(&["add", "."]);
        command(&["commit", "-q", "-m", "base"]);
        let base = command(&["rev-parse", "HEAD"]);
        std::fs::write(root.join("changed.txt"), "release").expect("release edit");
        std::fs::write(root.join("unchanged.txt"), "release").expect("release edit");
        command(&["add", "."]);
        command(&["commit", "-q", "-m", "release"]);
        let release = command(&["rev-parse", "HEAD"]);
        let repo = git.discover(&root).expect("safe repository");
        let handoff = Handoff {
            attempted: "edited two files".into(),
            failed: "criterion failed".into(),
            learned: "change one file".into(),
            files: snapshot_changed_files(&repo, &base, &release)
                .expect("snapshot")
                .files,
        };
        assert_eq!(handoff.files.len(), 2);
        assert_eq!(
            handoff.compare_git(&repo, &release).expect("same tree"),
            vec![
                HandoffFileStatus {
                    path: "changed.txt".into(),
                    changed: false
                },
                HandoffFileStatus {
                    path: "unchanged.txt".into(),
                    changed: false
                },
            ]
        );
        std::fs::write(root.join("changed.txt"), "next claimant").expect("next edit");
        command(&["add", "."]);
        command(&["commit", "-q", "-m", "next"]);
        let next = command(&["rev-parse", "HEAD"]);
        assert_eq!(
            handoff.compare_git(&repo, &next).expect("next tree"),
            vec![
                HandoffFileStatus {
                    path: "changed.txt".into(),
                    changed: true
                },
                HandoffFileStatus {
                    path: "unchanged.txt".into(),
                    changed: false
                },
            ]
        );
        for index in 0..130 {
            std::fs::write(root.join(format!("bulk-{index:03}.txt")), "bulk").expect("bulk edit");
        }
        std::fs::write(root.join("colon:name.txt"), "valid Git path").expect("punctuated edit");
        command(&["add", "."]);
        command(&["commit", "-q", "-m", "large release"]);
        let large = command(&["rev-parse", "HEAD"]);
        let bounded = snapshot_changed_files(&repo, &next, &large).expect("bounded snapshot");
        assert_eq!(bounded.files.len(), MAX_FILES);
        assert_eq!(bounded.omitted, 3);
        assert!(!bounded.unavailable);
        assert!(bounded.note().contains("omitted 3 changed path"));
        assert!(bounded.files.iter().all(|file| !file.path.contains(':')));
        std::fs::remove_dir_all(root).expect("scratch cleanup");
    }
}
