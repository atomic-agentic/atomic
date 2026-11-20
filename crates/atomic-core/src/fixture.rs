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
//! and a submodule, Git LFS content, a repository borrowing objects from
//! another (alternates), a shallow or grafted history, or one that is not
//! SHA-1 is refused rather than half copied. Every input field typed
//! `format: git-commit` must name a commit of the fixture's history at its
//! bound revision, and a field named `base` the bound commit itself. Each
//! worktree step starts from the bound commit or from a commit an earlier
//! step of the same run made, never from another run's.

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
    /// The workflow's input schema types a commit where atomic does not
    /// check it.
    #[error("{0}")]
    Schema(String),
    /// Something other than a snapshot atomic made is where the snapshot
    /// goes.
    #[error(
        "{} is not a snapshot of the fixture's commit that atomic made",
        .0.display()
    )]
    Occupied(PathBuf),
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
    let checkout = git
        .checkout_at(&source)
        .map_err(|e| path_error(&format!("not a git repository: {e}")))?;
    // The repository named, never one git finds above it.
    let named = match &checkout.top {
        Some(top) => std::fs::canonicalize(top).ok(),
        None => std::fs::canonicalize(&checkout.git_dir).ok(),
    };
    if named.as_deref() != Some(source.as_path()) {
        return Err(path_error(&format!(
            "is inside the git repository at {}, not a repository of its own; name the \
             repository's top",
            named.unwrap_or_default().display()
        )));
    }
    let repo = git.repo(&checkout.common_dir).without_filters()?;
    // A revision such as HEAD is the checkout's own, in a linked worktree too.
    let resolver = match &checkout.top {
        Some(top) => git.worktree(&checkout.git_dir, top).without_filters()?,
        None => repo.clone(),
    };
    let format = repo.object_format()?;
    if format != "sha1" {
        return Err(FixtureError::Unsupported(format!(
            "{} names its objects with {format}, and atomic binds SHA-1 repositories only",
            source.display()
        )));
    }
    let alternates = checkout.common_dir.join("objects/info/alternates");
    if std::fs::read_to_string(&alternates).is_ok_and(|a| !a.trim().is_empty()) {
        return Err(FixtureError::Unsupported(format!(
            "{} borrows objects from another repository ({}); atomic copies a fixture \
             whole, so make it a repository that holds its own objects (git repack -a -d, then \
             remove the alternates file)",
            source.display(),
            alternates.display()
        )));
    }
    let grafts = checkout.common_dir.join("info/grafts");
    if repo.is_shallow()? || grafts.exists() {
        return Err(FixtureError::Unsupported(format!(
            "{} has a cut-short history (it is shallow, or has grafts), so a copy of it would \
             be cut short too; bind a repository with its whole history (git fetch --unshallow)",
            source.display()
        )));
    }
    let commit = resolver
        .resolve_commit(rev)
        .map_err(|e| FixtureError::Revision {
            rev: rev.to_owned(),
            repo: source.clone(),
            why: e.to_string(),
        })?;
    let tree = repo.tree_of(&commit)?;
    let entries = repo.entries(&commit)?;
    if let Some(entry) = entries.iter().find(|e| e.kind == "commit") {
        return Err(FixtureError::Unsupported(format!(
            "commit {commit} has a submodule at {}, which atomic does not fetch; check its \
             content in as files, or bind a revision without it",
            entry.path
        )));
    }
    for entry in &entries {
        let attributes = entry.path == ".gitattributes" || entry.path.ends_with("/.gitattributes");
        if entry.kind == "blob" && attributes && uses_lfs(&repo.blob_by_id(&entry.id)?) {
            return Err(FixtureError::Unsupported(format!(
                "commit {commit} keeps files in Git LFS ({}), whose content atomic does not \
                 fetch; check the files in whole, or bind a revision without LFS",
                entry.path
            )));
        }
    }
    let snapshot = snapshot(state, &git, &repo, &commit)?;
    Ok(FixtureBinding {
        source,
        rev: rev.to_owned(),
        commit,
        tree,
        snapshot,
        worktrees: state.dir().join("worktrees"),
    })
}

/// Whether a `.gitattributes` file's text gives any path the LFS filter.
fn uses_lfs(attributes: &[u8]) -> bool {
    String::from_utf8_lossy(attributes)
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .any(|line| line.split_whitespace().skip(1).any(|a| a == "filter=lfs"))
}

/// The bare snapshot of `commit` in the state directory, made if missing:
/// built whole in a directory of this admission's own, checked to hold every
/// object the commit reaches, and only then moved into place, so neither a
/// kill part way nor another admission of the same commit at once leaves a
/// snapshot that is not whole. One already in place is used once its keep
/// ref names the commit; anything else there is refused, never removed,
/// since runs may be working in it.
fn snapshot(
    state: &ProjectState,
    git: &Git,
    repo: &crate::git::Repo,
    commit: &str,
) -> Result<PathBuf, FixtureError> {
    let io = |path: &Path| {
        let path = path.to_owned();
        move |source| FixtureError::Io { path, source }
    };
    let dir = state.dir().join("fixtures");
    std::fs::create_dir_all(&dir).map_err(io(&dir))?;
    let at = dir.join(format!("{commit}.git"));
    let keep = keep_ref(commit);
    // Only a whole snapshot is ever moved into place, so one there with its
    // keep ref is whole, and is not walked again on every admission.
    let placed = |path: &Path| -> Result<bool, FixtureError> {
        if !path.join("HEAD").is_file() {
            return Ok(false);
        }
        Ok(git.repo(path).ref_target(&keep)?.as_deref() == Some(commit))
    };
    if placed(&at)? {
        return Ok(at);
    }
    if at.symlink_metadata().is_ok() {
        return Err(FixtureError::Occupied(at));
    }
    static BUILDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let building = dir.join(format!(
        ".{commit}.{}.{}.{}.git",
        std::process::id(),
        BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let built = (|| -> Result<(), FixtureError> {
        let copy = git.init_bare(&building)?;
        repo.copy_reachable(commit, &copy)?;
        if !copy.has_everything(commit)? {
            return Err(FixtureError::Unsupported(format!(
                "the copy of {commit} does not hold every object it reaches; the repository \
                 is missing some of its history"
            )));
        }
        copy.create_ref(&keep, commit)?;
        Ok(())
    })();
    if let Err(e) = built {
        let _ = std::fs::remove_dir_all(&building);
        return Err(e);
    }
    match std::fs::rename(&building, &at) {
        Ok(()) => Ok(at),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&building);
            // Another admission moved its own into place first.
            if placed(&at)? {
                Ok(at)
            } else if at.symlink_metadata().is_ok() {
                Err(FixtureError::Occupied(at))
            } else {
                Err(io(&at)(e))
            }
        }
    }
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
/// a whole commit ID, no placeholder, a commit of the fixture's history at
/// its bound revision (the bound commit or one of its ancestors, never a
/// commit only the snapshot holds, such as another run's), and, for a
/// field named `base`, the bound commit itself. A schema that types a
/// commit where this walk does not reach, such as under `oneOf`, is
/// refused rather than left unchecked.
pub fn check_commits(
    binding: &FixtureBinding,
    schema: &Value,
    input: &Value,
) -> Result<(), FixtureError> {
    let reachable = commit_schemas(schema);
    let everywhere = count_commit_formats(schema);
    if reachable != everywhere {
        return Err(FixtureError::Schema(format!(
            "the workflow's input types {} of its {everywhere} commit fields \
             (`format: git-commit`) under keywords atomic does not check, such as `oneOf`, \
             `allOf`, `prefixItems`, `additionalProperties` or a `$ref`; type each commit \
             through `properties` and `items` only",
            everywhere - reachable
        )));
    }
    let git = Git::find()?;
    let snapshot = git.repo(&binding.snapshot);
    let mut found = Vec::new();
    commit_fields(schema, input, "", &mut found);
    for (pointer, value, nullable) in found {
        let fail = |why: String| FixtureError::Input {
            pointer: pointer.clone(),
            why,
        };
        let Some(sha) = value.as_str() else {
            if value.is_null() && nullable {
                continue;
            }
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
        let known = snapshot.resolve_commit(sha).ok().as_deref() == Some(sha);
        if !known || !snapshot.is_ancestor(sha, &binding.commit)? {
            return Err(fail(format!(
                "{sha} is not in the history of the fixture at {} ({})",
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

/// Check the base a worktree step of a run bound to `binding` starts from:
/// the bound commit, still holding the tree recorded at admission, or a
/// commit this run accepted from one of its own earlier steps (`accepted`),
/// which must also descend from the bound commit. Anything else the
/// snapshot holds, such as a commit another run made on top of the same
/// fixture, is refused.
pub fn check_base(
    binding: &FixtureBinding,
    base: &str,
    accepted: &[String],
) -> Result<(), FixtureError> {
    let git = Git::find()?;
    let snapshot = git.repo(&binding.snapshot);
    if snapshot.tree_of(&binding.commit)? != binding.tree {
        return Err(FixtureError::Unsupported(format!(
            "the snapshot's commit {} no longer has the tree {} recorded when the run was \
             admitted",
            binding.commit, binding.tree
        )));
    }
    let own = base == binding.commit || accepted.iter().any(|c| c == base);
    if !is_sha(base) || !own || !snapshot.is_ancestor(&binding.commit, base)? {
        return Err(FixtureError::Input {
            pointer: "/base".into(),
            why: format!(
                "{base} is neither the fixture's commit {} nor a commit an earlier step of \
                 this run made on top of it",
                binding.commit
            ),
        });
    }
    Ok(())
}

/// How many subschemas anywhere in `schema` say `format: git-commit`.
fn count_commit_formats(schema: &Value) -> usize {
    match schema {
        Value::Object(map) => {
            usize::from(map.get("format").and_then(Value::as_str) == Some("git-commit"))
                + map.values().map(count_commit_formats).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(count_commit_formats).sum(),
        _ => 0,
    }
}

/// How many of them the commit walk reaches: through `properties` and
/// `items` only, from the top. One under any other keyword, such as
/// `oneOf`, `prefixItems`, `additionalProperties` or a `$ref`'s target, is
/// not reached.
fn commit_schemas(schema: &Value) -> usize {
    let Some(map) = schema.as_object() else {
        return 0;
    };
    let here = usize::from(map.get("format").and_then(Value::as_str) == Some("git-commit"));
    let below = map
        .get("properties")
        .and_then(Value::as_object)
        .map_or(0, |p| p.values().map(commit_schemas).sum::<usize>())
        + map.get("items").map_or(0, commit_schemas);
    here + below
}

/// The fields of `value` that `schema` types `format: git-commit`, with
/// their pointers and whether their schema's type allows null, through
/// properties and items.
fn commit_fields(
    schema: &Value,
    value: &Value,
    pointer: &str,
    out: &mut Vec<(String, Value, bool)>,
) {
    if schema.get("format").and_then(Value::as_str) == Some("git-commit") {
        let nullable = match schema.get("type") {
            Some(Value::String(t)) => t == "null",
            Some(Value::Array(types)) => types.iter().any(|t| t == "null"),
            _ => false,
        };
        out.push((pointer.to_owned(), value.clone(), nullable));
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
    fn a_commit_typed_where_the_walk_does_not_reach_is_counted_out() {
        let walked = json!({"type": "object", "properties": {
            "base": {"type": "string", "format": "git-commit"},
            "parents": {"type": "array", "items": {"format": "git-commit"}}
        }});
        assert_eq!(commit_schemas(&walked), 2);
        assert_eq!(count_commit_formats(&walked), 2);
        for hidden in [
            json!({"oneOf": [{"type": "string", "format": "git-commit"}]}),
            json!({"type": "object", "additionalProperties": {"format": "git-commit"}}),
            json!({"type": "array", "prefixItems": [{"format": "git-commit"}]}),
            json!({"type": "object", "properties": {"base": {"$ref": "#/$defs/c"}},
                   "$defs": {"c": {"format": "git-commit"}}}),
        ] {
            assert_ne!(
                commit_schemas(&hidden),
                count_commit_formats(&hidden),
                "{hidden}"
            );
        }
    }

    #[test]
    fn lfs_is_found_by_its_attribute_not_by_its_name_in_a_comment() {
        assert!(uses_lfs(b"*.bin filter=lfs diff=lfs merge=lfs -text\n"));
        assert!(uses_lfs(b"# big files\nassets/** filter=lfs\n"));
        assert!(!uses_lfs(b"# we do not use filter=lfs here\n*.txt text\n"));
        assert!(!uses_lfs(b"*.txt filter=lfs-not\n"));
    }

    #[test]
    fn admissions_of_one_commit_at_once_share_one_whole_snapshot() {
        let dir = std::env::temp_dir().join(format!("atomic-fixture-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let source = dir.join("source");
        std::fs::create_dir_all(&source).expect("source");
        let git = |args: &[&str]| {
            let out = std::process::Command::new(Git::find().expect("git").program())
                .args(["-c", "user.name=dev", "-c", "user.email=dev@example.com"])
                .args(args)
                .current_dir(&source)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .expect("git");
            assert!(out.status.success(), "{out:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        git(&["init", "-q"]);
        std::fs::write(source.join("a.txt"), "a\n").expect("file");
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "a"]);
        let commit = git(&["rev-parse", "HEAD"]);
        let state = ProjectState::under(&dir.join("home"), &source);
        std::fs::create_dir_all(state.dir()).expect("state");
        let bound: Vec<_> = (0..8)
            .map(|_| {
                let (state, source) = (state.clone(), source.clone());
                std::thread::spawn(move || bind(&state, &source, "HEAD"))
            })
            .collect();
        for one in bound {
            let binding = one.join().expect("thread").expect("bound");
            assert_eq!(binding.commit, commit);
        }
        let snapshots: Vec<_> = std::fs::read_dir(state.dir().join("fixtures"))
            .expect("fixtures")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            snapshots,
            [format!("{commit}.git")],
            "no half-built ones left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// git as a person runs it in `dir`, with no configuration of theirs.
    fn git_at(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new(Git::find().expect("git").program())
            .args(["-c", "user.name=dev", "-c", "user.email=dev@example.com"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git");
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// A fixture with one commit bound for a scratch project, and a commit
    /// the snapshot holds on top of it and one beside it, as other runs'.
    fn bound(tag: &str) -> (std::path::PathBuf, FixtureBinding, String, String) {
        let dir = std::env::temp_dir().join(format!("atomic-fixture-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let source = dir.join("source");
        std::fs::create_dir_all(&source).expect("source");
        git_at(&source, &["init", "-q"]);
        std::fs::write(source.join("a.txt"), "a\n").expect("file");
        git_at(&source, &["add", "a.txt"]);
        git_at(&source, &["commit", "-q", "-m", "a"]);
        let state = ProjectState::under(&dir.join("home"), &source);
        std::fs::create_dir_all(state.dir()).expect("state");
        let binding = bind(&state, &source, "HEAD").expect("bound");
        let snapshot = binding.snapshot.clone();
        let tree = binding.tree.clone();
        let on_top = git_at(
            &snapshot,
            &["commit-tree", &tree, "-p", &binding.commit, "-m", "on top"],
        );
        let beside = git_at(&snapshot, &["commit-tree", &tree, "-m", "beside"]);
        (dir, binding, on_top, beside)
    }

    #[test]
    #[cfg(unix)]
    fn a_partial_clone_missing_an_object_is_refused_and_its_promisor_is_never_asked() {
        let dir =
            std::env::temp_dir().join(format!("atomic-fixture-promisor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let source = dir.join("source");
        std::fs::create_dir_all(&source).expect("source");
        git_at(&source, &["init", "-q"]);
        std::fs::write(source.join("a.txt"), "a\n").expect("file");
        git_at(&source, &["add", "a.txt"]);
        git_at(&source, &["commit", "-q", "-m", "a"]);
        // The blob goes, and a promisor remote that would run a program
        // to fetch it, with its transport allowed, takes its place.
        let blob = git_at(&source, &["rev-parse", "HEAD:a.txt"]);
        std::fs::remove_file(
            source
                .join(".git/objects")
                .join(&blob[..2])
                .join(&blob[2..]),
        )
        .expect("loose blob");
        let marker = dir.join("fetched");
        let fetch = dir.join("fetch.sh");
        std::fs::write(&fetch, format!("#!/bin/sh\ntouch '{}'\n", marker.display()))
            .expect("fetch");
        std::fs::set_permissions(&fetch, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("mode");
        for (key, value) in [
            ("core.repositoryFormatVersion", "1".to_owned()),
            ("extensions.partialClone", "origin".to_owned()),
            ("remote.origin.promisor", "true".to_owned()),
            ("remote.origin.url", format!("ext::{} %S", fetch.display())),
            ("protocol.ext.allow", "always".to_owned()),
        ] {
            git_at(&source, &["config", key, &value]);
        }
        let state = ProjectState::under(&dir.join("home"), &source);
        std::fs::create_dir_all(state.dir()).expect("state");
        bind(&state, &source, "HEAD").expect_err("an object is missing");
        assert!(!marker.exists(), "the promisor remote was asked");
        let left: Vec<_> = std::fs::read_dir(state.dir().join("fixtures"))
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        assert!(left.is_empty(), "no snapshot is left: {left:?}");
        // The positive control: git as a person runs it does ask.
        let _ = std::process::Command::new(Git::find().expect("git").program())
            .args(["cat-file", "-p", &blob])
            .current_dir(&source)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git");
        assert!(marker.exists(), "the marker is seen when git fetches");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn what_is_in_a_snapshots_place_and_is_not_one_is_refused_and_left_alone() {
        let (dir, binding, _, _) = bound("in-place");
        let state = ProjectState::under(&dir.join("home"), &dir.join("source"));
        // Take the snapshot's keep ref away, as a damaged or foreign one
        // would lack it, with a run's work beside it.
        git_at(
            &binding.snapshot,
            &["update-ref", "-d", &keep_ref(&binding.commit)],
        );
        std::fs::write(binding.snapshot.join("work.txt"), "a run's\n").expect("work");
        let refused = bind(&state, &dir.join("source"), "HEAD").expect_err("not ours");
        assert!(
            refused
                .to_string()
                .contains(&binding.snapshot.display().to_string()),
            "{refused}"
        );
        assert!(binding.snapshot.join("work.txt").is_file(), "left alone");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_worktree_starts_from_the_bound_commit_or_one_of_its_own_runs_only() {
        let (dir, binding, on_top, beside) = bound("base");
        check_base(&binding, &binding.commit, &[]).expect("the bound commit");
        check_base(&binding, &on_top, std::slice::from_ref(&on_top))
            .expect("a commit an earlier step of the run made on top of it");
        // On top of the bound commit, but another run's.
        let refused = check_base(&binding, &on_top, &[]).expect_err("another run's commit");
        assert!(matches!(refused, FixtureError::Input { .. }), "{refused}");
        // Accepted, yet not on top of the bound commit.
        let refused = check_base(&binding, &beside, std::slice::from_ref(&beside))
            .expect_err("a commit beside the bound one");
        assert!(matches!(refused, FixtureError::Input { .. }), "{refused}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_input_commit_is_one_of_the_fixtures_history_not_one_the_snapshot_happens_to_hold() {
        let (dir, binding, on_top, beside) = bound("history");
        let schema = json!({"type": "object", "properties": {
            "parents": {"type": "array", "items": {"type": "string", "format": "git-commit"}}
        }});
        check_commits(&binding, &schema, &json!({"parents": [binding.commit]})).expect("its own");
        for other in [on_top, beside] {
            let refused = check_commits(&binding, &schema, &json!({"parents": [other]}))
                .expect_err("not in its history");
            assert!(
                refused.to_string().contains("not in the history"),
                "{refused}"
            );
        }
        // A commit typed where the walk does not reach is refused, not
        // left unchecked.
        let hidden = json!({"type": "object", "properties": {
            "base": {"oneOf": [{"type": "string", "format": "git-commit"}]}
        }});
        let refused = check_commits(&binding, &hidden, &json!({"base": binding.commit}))
            .expect_err("unchecked");
        assert!(matches!(refused, FixtureError::Schema(_)), "{refused}");
        // A field that may be null may be left null; one that may not, not.
        let optional = |types: Value| {
            json!({"type": "object", "properties": {
                "parent": {"type": types, "format": "git-commit"}
            }})
        };
        check_commits(
            &binding,
            &optional(json!(["string", "null"])),
            &json!({"parent": null}),
        )
        .expect("a nullable field left null");
        check_commits(
            &binding,
            &optional(json!("string")),
            &json!({"parent": null}),
        )
        .expect_err("a field that is not nullable");
        // A commit at the top of the input has the root pointer, "".
        let top = json!({"type": "string", "format": "git-commit"});
        match check_commits(&binding, &top, &json!("not-a-sha")) {
            Err(FixtureError::Input { pointer, .. }) => assert_eq!(pointer, ""),
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

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
        let mut pointers: Vec<&str> = found.iter().map(|(p, _, _)| p.as_str()).collect();
        pointers.sort_unstable();
        assert_eq!(
            pointers,
            ["/base", "/parents/0", "/parents/1", "/task/base"]
        );
    }
}
