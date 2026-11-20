// SPDX-License-Identifier: Apache-2.0

//! Binding a coding run to a fixture repository at a revision.
//!
//! The operator names a repository already on disk and a revision in it
//! (`--fixture <repo> --fixture-rev <rev>`). At admission atomic resolves
//! the revision to a commit, copies every object that commit reaches into a
//! bare snapshot of its own in the state directory, and records the source,
//! the revision as given, the commit and its tree. The run's worktrees are
//! detached checkouts of that snapshot, so neither the fixture's checkout nor
//! the developer's is ever written to, and later changes to the source,
//! such as a branch that moves, cannot alter an admitted run.
//!
//! Nothing is fetched: objects are copied as one pack, with no transport,
//! and a submodule, Git LFS content or a repository borrowing objects from
//! another (alternates) is refused rather than half copied. Every input
//! field typed `format: git-commit` must name a commit in the snapshot, and
//! a field named `base` must name the bound commit itself.

use crate::git::{Git, GitError, is_sha};
use crate::state::ProjectState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// A run's fixture, as admission bound it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureBinding {
    /// The repository the operator named, as found.
    pub source: PathBuf,
    /// The revision as given, such as a branch name.
    pub rev: String,
    /// The commit it resolved to, which the run is pinned to.
    pub commit: String,
    /// That commit's tree.
    pub tree: String,
    /// The bare snapshot holding every object the commit reaches.
    pub snapshot: PathBuf,
    /// Where the run's worktrees of the snapshot go.
    pub worktrees: PathBuf,
}

/// Why a fixture could not be bound. Each is found before anything runs.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// The path names no git repository.
    #[error("{path}: {why}")]
    Path {
        /// The path given.
        path: PathBuf,
        /// What is wrong with it.
        why: String,
    },
    /// The revision names no commit in the repository.
    #[error("revision {rev:?} is not a commit in {repo}: {why}")]
    Revision {
        /// The revision given.
        rev: String,
        /// The repository.
        repo: PathBuf,
        /// What git said.
        why: String,
    },
    /// The commit needs what atomic does not copy.
    #[error("{0}")]
    Unsupported(String),
    /// An input field typed `format: git-commit` does not fit the fixture.
    #[error("input {pointer}: {why}")]
    Input {
        /// The field, as a JSON Pointer into the input.
        pointer: String,
        /// What is wrong with it.
        why: String,
    },
    /// git failed while making the snapshot.
    #[error("making the fixture snapshot: {0}")]
    Git(#[from] GitError),
    /// The state directory could not be written.
    #[error("making the fixture snapshot in {path}: {source}")]
    Io {
        /// The directory.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

/// The ref that keeps a snapshot's commit, and every object it reaches.
fn keep_ref(commit: &str) -> String {
    format!("refs/atomic/fixture/{commit}")
}

/// Bind `source` at `rev` for a run of the project at `state`: resolve,
/// check and snapshot it. A snapshot of the same commit is reused.
pub fn bind(
    state: &ProjectState,
    source: &Path,
    rev: &str,
) -> Result<FixtureBinding, FixtureError> {
    let path_error = |why: &str| FixtureError::Path {
        path: source.to_owned(),
        why: why.to_owned(),
    };
    if !source.is_dir() {
        return Err(path_error("no such directory"));
    }
    let source = std::fs::canonicalize(source).map_err(|e| path_error(&e.to_string()))?;
    let git = Git::find()?;
    let repo = git
        .discover(&source)
        .map_err(|e| path_error(&format!("not a git repository: {e}")))?;
    let alternates = repo.git_dir().join("objects/info/alternates");
    if std::fs::read_to_string(&alternates).is_ok_and(|a| !a.trim().is_empty()) {
        return Err(FixtureError::Unsupported(format!(
            "{} borrows objects from another repository ({}); atomic copies a fixture \
             whole, so make it a repository that holds its own objects (git repack -a -d, then \
             remove the alternates file)",
            source.display(),
            alternates.display()
        )));
    }
    let commit = repo
        .resolve_commit(rev)
        .map_err(|e| FixtureError::Revision {
            rev: rev.to_owned(),
            repo: source.clone(),
            why: e.to_string(),
        })?;
    let tree = repo.tree_of(&commit)?;
    let entries = repo.entries(&commit)?;
    if let Some((_, _, path)) = entries.iter().find(|(_, kind, _)| kind == "commit") {
        return Err(FixtureError::Unsupported(format!(
            "commit {commit} has a submodule at {path}, which atomic does not fetch; check \
             its content in as files, or bind a revision without it"
        )));
    }
    for (_, kind, path) in &entries {
        let attributes = path == ".gitattributes" || path.ends_with("/.gitattributes");
        if kind == "blob"
            && attributes
            && String::from_utf8_lossy(&repo.blob(&commit, path)?).contains("filter=lfs")
        {
            return Err(FixtureError::Unsupported(format!(
                "commit {commit} keeps files in Git LFS ({path}), whose content atomic does \
                 not fetch; check the files in whole, or bind a revision without LFS"
            )));
        }
    }

    let dir = state.dir().join("fixtures");
    std::fs::create_dir_all(&dir).map_err(|source| FixtureError::Io {
        path: dir.clone(),
        source,
    })?;
    let snapshot = dir.join(format!("{commit}.git"));
    let copy = if snapshot.join("HEAD").is_file() {
        git.repo(&snapshot)
    } else {
        git.init_bare(&snapshot)?
    };
    let keep = keep_ref(&commit);
    if copy.ref_target(&keep)?.as_deref() != Some(commit.as_str()) {
        repo.copy_reachable(&commit, &copy)?;
        if let Err(e) = copy.create_ref(&keep, &commit) {
            // Made meanwhile by another admission of the same commit.
            if copy.ref_target(&keep)?.as_deref() != Some(commit.as_str()) {
                return Err(e.into());
            }
        }
    }
    if copy.resolve_commit(&commit)? != commit {
        return Err(FixtureError::Unsupported(format!(
            "the snapshot of {commit} does not hold it"
        )));
    }
    Ok(FixtureBinding {
        source,
        rev: rev.to_owned(),
        commit,
        tree,
        snapshot,
        worktrees: state.dir().join("worktrees"),
    })
}

/// Whether `sha` looks made up: every digit the same, as in the all-ones
/// and all-zeros placeholders of offline examples.
fn placeholder(sha: &str) -> bool {
    let mut digits = sha.chars();
    digits
        .next()
        .is_some_and(|first| digits.all(|c| c == first))
}

/// Check every field of `input` that `schema` types `format: git-commit`:
/// a whole commit ID, no placeholder, a commit in the snapshot, and, for a
/// field named `base`, the bound commit itself.
pub fn check_commits(
    binding: &FixtureBinding,
    schema: &Value,
    input: &Value,
) -> Result<(), FixtureError> {
    let git = Git::find()?;
    let snapshot = git.repo(&binding.snapshot);
    let mut found = Vec::new();
    commit_fields(schema, input, "", &mut found);
    for (pointer, value) in found {
        let fail = |why: String| FixtureError::Input {
            pointer: if pointer.is_empty() {
                "/".to_owned()
            } else {
                pointer.clone()
            },
            why,
        };
        let Some(sha) = value.as_str() else {
            return Err(fail("a git-commit field holds a commit ID".into()));
        };
        if !is_sha(sha) {
            return Err(fail(format!("{sha:?} is not a whole commit ID")));
        }
        if placeholder(sha) {
            return Err(fail(format!(
                "{sha} is a placeholder, not a commit; give a commit of the fixture, such as \
                 {}",
                binding.commit
            )));
        }
        if snapshot.resolve_commit(sha).ok().as_deref() != Some(sha) {
            return Err(fail(format!(
                "{sha} is not a commit of the fixture at {} ({})",
                binding.rev, binding.commit
            )));
        }
        if pointer.rsplit('/').next() == Some("base") && sha != binding.commit {
            return Err(fail(format!(
                "the base {sha} is not the fixture's revision {} ({}); give that commit, or \
                 bind the fixture at {sha}",
                binding.rev, binding.commit
            )));
        }
    }
    Ok(())
}

/// The fields of `value` that `schema` types `format: git-commit`, with
/// their pointers, through properties and items.
fn commit_fields(schema: &Value, value: &Value, pointer: &str, out: &mut Vec<(String, Value)>) {
    if schema.get("format").and_then(Value::as_str) == Some("git-commit") {
        out.push((pointer.to_owned(), value.clone()));
        return;
    }
    if let (Some(properties), Some(object)) = (
        schema.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) {
        for (key, sub) in properties {
            if let Some(field) = object.get(key) {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                commit_fields(sub, field, &format!("{pointer}/{escaped}"), out);
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), value.as_array()) {
        for (i, item) in array.iter().enumerate() {
            commit_fields(items, item, &format!("{pointer}/{i}"), out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn placeholders_are_every_digit_the_same() {
        assert!(placeholder("1111111111111111111111111111111111111111"));
        assert!(placeholder("0000000000000000000000000000000000000000"));
        assert!(!placeholder("1111111111111111111111111111111111111112"));
    }

    #[test]
    fn commit_fields_are_found_through_properties_and_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "task": {"type": "object", "properties": {
                    "base": {"type": "string", "format": "git-commit"},
                    "title": {"type": "string"}
                }},
                "parents": {"type": "array", "items": {"type": "string", "format": "git-commit"}},
                "base": {"type": "string", "format": "git-commit"}
            }
        });
        let input = json!({
            "task": {"base": "a", "title": "t"},
            "parents": ["b", "c"],
            "base": "d"
        });
        let mut found = Vec::new();
        commit_fields(&schema, &input, "", &mut found);
        // In the schema's key order, which depends on how serde_json is
        // built: compared as a set.
        let mut pointers: Vec<&str> = found.iter().map(|(p, _)| p.as_str()).collect();
        pointers.sort_unstable();
        assert_eq!(
            pointers,
            ["/base", "/parents/0", "/parents/1", "/task/base"]
        );
    }
}
