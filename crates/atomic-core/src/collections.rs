// SPDX-License-Identifier: Apache-2.0

//! Collections: the project's atoms, workflows, schemas and evals, found by
//! glob.
//!
//! Each collection has default globs (see [`crate::project`]), which a
//! manifest entry replaces. Files are found under the manifest's directory,
//! honouring `.gitignore`, the manifest's `ignore` globs and the generated
//! directory, and stopping at any subdirectory that holds its own manifest.
//! A file matched by several globs is found once. Two atoms, or two workflows,
//! with one name are an error. A workflow's name is the `name` it declares,
//! or else its file name; an atom's is its file name.

use crate::project::Project;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One file in a collection.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceFile {
    /// The name it is referenced by, from its file name unless its
    /// frontmatter sets another.
    pub name: String,
    /// Its canonical path.
    pub path: PathBuf,
}

/// Why the collections could not be gathered.
#[derive(Debug, thiserror::Error)]
pub enum CollectError {
    /// A removed atom frontend remains in the project.
    #[error("{}: Markdown/MDX atoms have been replaced; migrate this file to .atom.astro", .path.display())]
    LegacyAtom {
        /// Legacy source file.
        path: PathBuf,
    },
    /// A glob is not valid.
    #[error("collection {collection}: bad glob {glob:?}: {message}")]
    Glob {
        /// The collection.
        collection: String,
        /// The glob.
        glob: String,
        /// What is wrong.
        message: String,
    },
    /// The directory walk failed.
    #[error("walking {}: {message}", dir.display())]
    Walk {
        /// The project directory.
        dir: PathBuf,
        /// What failed.
        message: String,
    },
    /// Two files in one collection share a name.
    #[error("two {collection} are named {name:?}: {} and {}; rename one, or set `name` in one of them", first.display(), second.display())]
    DuplicateName {
        /// The collection.
        collection: String,
        /// The name.
        name: String,
        /// One file.
        first: PathBuf,
        /// The other.
        second: PathBuf,
    },
}

/// Collections whose names must be unique.
const NAMED: &[&str] = &["atoms", "workflows"];

/// The suffixes that end a file name, per collection; the rest is the name.
fn default_name(collection: &str, relative: &Path) -> String {
    let file = relative
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let suffixes: &[&str] = match collection {
        "atoms" => &[".atom.astro"],
        "workflows" => &[".workflow.yaml"],
        "schemas" => &[".schema.yaml", ".schema.json"],
        "toolsets" => &[".openapi.yaml", ".openapi.json"],
        _ => &[],
    };
    for s in suffixes {
        if let Some(stem) = file.strip_suffix(s) {
            return stem.to_owned();
        }
    }
    // Evals are named by their path, so files in different folders differ.
    let rel = relative.to_string_lossy();
    rel.rsplit_once('.')
        .map_or(rel.to_string(), |(stem, _)| stem.to_owned())
}

/// The name a workflow declares for itself at its top level, if any. A file
/// that cannot be read or parsed declares none here; its loader reports what
/// is wrong with it.
///
/// An atom's name in a collection is its file name: its frontmatter is
/// TypeScript the compiler reads, and its generated declarations are named
/// after the file, so one project cannot hold two atom files with one name.
/// A `name` an atom declares is checked once the atom is loaded.
fn declared_name(collection: &str, path: &Path) -> Option<String> {
    if collection != "workflows" {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let doc = atomic_schema::yaml::load(path, &text).ok()?;
    match doc.value.get("name")? {
        Value::String(n) if !n.is_empty() => Some(n.clone()),
        _ => None,
    }
}

fn globset(collection: &str, globs: &[String]) -> Result<GlobSet, CollectError> {
    let mut b = GlobSetBuilder::new();
    for g in globs {
        b.add(Glob::new(g).map_err(|e| CollectError::Glob {
            collection: collection.to_owned(),
            glob: g.clone(),
            message: e.to_string(),
        })?);
    }
    b.build().map_err(|e| CollectError::Glob {
        collection: collection.to_owned(),
        glob: globs.join(", "),
        message: e.to_string(),
    })
}

/// Gather every collection of `project`, each sorted by name.
pub fn collect(project: &Project) -> Result<BTreeMap<String, Vec<SourceFile>>, CollectError> {
    let root = std::fs::canonicalize(&project.dir).map_err(|e| CollectError::Walk {
        dir: project.dir.clone(),
        message: e.to_string(),
    })?;
    let sets: Vec<(String, GlobSet)> = project
        .collections
        .iter()
        .map(|(c, globs)| Ok((c.clone(), globset(c, globs)?)))
        .collect::<Result<_, CollectError>>()?;
    let ignored = globset("ignore", &project.ignore)?;
    let generated = root.join(&project.generated);

    let walk_root = root.clone();
    let walker = ignore::WalkBuilder::new(&root)
        .require_git(false)
        .filter_entry(move |e| {
            let p = e.path();
            if p == walk_root {
                return true;
            }
            // Another project's manifest bounds this one.
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            !(is_dir
                && (p.join("atomic.yaml").is_file() || p.join(".atomic/atomic.yaml").is_file()))
        })
        .build();

    let mut found: BTreeMap<String, BTreeMap<PathBuf, SourceFile>> = sets
        .iter()
        .map(|(c, _)| (c.clone(), BTreeMap::new()))
        .collect();
    for entry in walker {
        let entry = entry.map_err(|e| CollectError::Walk {
            dir: root.clone(),
            message: e.to_string(),
        })?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if path.starts_with(&generated) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(&root) else {
            continue;
        };
        if ignored.is_match(relative) {
            continue;
        }
        if path
            .to_str()
            .is_some_and(|name| name.ends_with(".atom.md") || name.ends_with(".atom.mdx"))
        {
            return Err(CollectError::LegacyAtom {
                path: path.to_owned(),
            });
        }
        for (collection, set) in &sets {
            if set.is_match(relative) {
                let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
                let name = declared_name(collection, &canonical)
                    .unwrap_or_else(|| default_name(collection, relative));
                found
                    .get_mut(collection)
                    .expect("every collection has a map")
                    .insert(
                        canonical.clone(),
                        SourceFile {
                            name,
                            path: canonical,
                        },
                    );
            }
        }
    }

    let mut out = BTreeMap::new();
    for (collection, files) in found {
        let mut files: Vec<SourceFile> = files.into_values().collect();
        files.sort();
        if NAMED.contains(&collection.as_str()) {
            for pair in files.windows(2) {
                if pair[0].name == pair[1].name {
                    return Err(CollectError::DuplicateName {
                        collection: collection.clone(),
                        name: pair[0].name.clone(),
                        first: pair[0].path.clone(),
                        second: pair[1].path.clone(),
                    });
                }
            }
        }
        out.insert(collection, files);
    }
    Ok(out)
}

#[cfg(test)]
mod tests;

/// Load the schemas `project` can reference into `registry`: its own, then
/// those of its fellow workspace members (see
/// [`crate::project::workspace_of`]). The project's own schema wins over a
/// member's with the same ID; two members sharing an ID is an error, as two
/// schemas sharing one in a project are. Nothing is fetched.
pub fn load_schemas(
    project: &Project,
    registry: &mut atomic_schema::registry::Registry,
) -> Result<(), String> {
    use atomic_schema::registry::SchemaError;
    let own = collect(project).map_err(|e| e.to_string())?;
    for schema in &own["schemas"] {
        registry
            .load_file(&schema.path)
            .map_err(|e| e.to_string())?;
    }
    let Some(workspace) = crate::project::workspace_of(project).map_err(|e| e.to_string())? else {
        return Ok(());
    };
    let here = std::fs::canonicalize(project.root()).map_err(|e| e.to_string())?;
    for member in &workspace.members {
        let root = std::fs::canonicalize(member.root()).map_err(|e| e.to_string())?;
        if root == here {
            continue;
        }
        let files = collect(member).map_err(|e| e.to_string())?;
        for schema in &files["schemas"] {
            match registry.load_file(&schema.path) {
                Ok(_) => {}
                // The project's own schema takes precedence.
                Err(SchemaError::DuplicateId { first, .. }) if first.starts_with(&here) => {}
                Err(e) => {
                    return Err(format!("workspace member {}: {e}", member.name));
                }
            }
        }
    }
    Ok(())
}
