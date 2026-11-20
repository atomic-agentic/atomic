// SPDX-License-Identifier: Apache-2.0

//! The project manifest and project discovery.
//!
//! A project is defined by its manifest, not by a folder name. From a starting
//! directory atomic walks up to the first directory holding `atomic.yaml`
//! (a dedicated layout) or `.atomic/atomic.yaml` (embedded in an existing
//! repository); a directory holding both is an error. The first manifest found
//! is the project, so a nested project's manifest bounds discovery from inside
//! it. Paths in the manifest are relative to the manifest's directory.
//!
//! The smallest manifest is:
//!
//! ```yaml
//! atomic: 1
//! name: billing-service
//! ```
//!
//! A workspace manifest lists member projects by glob, relative to it:
//!
//! ```yaml
//! atomic: 1
//! name: engineering
//! workspace:
//!   members: [shared, services/*]
//! ```
//!
//! Each member keeps its own manifest and is still discovered on its own; a
//! member also sees the schemas its fellow members share (see
//! [`workspace_of`]).

use atomic_schema::yaml;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The manifest format versions this atomic reads.
pub const FORMAT_VERSION: u64 = 1;

/// The default glob lists, per collection. A manifest's `collections` entry
/// replaces the default for that collection only.
pub const DEFAULT_COLLECTIONS: &[(&str, &[&str])] = &[
    ("atoms", &["atoms/**/*.atom.astro", "**/*.atom.astro"]),
    (
        "workflows",
        &["workflows/**/*.workflow.yaml", "**/*.workflow.yaml"],
    ),
    (
        "schemas",
        &["schemas/**/*.schema.yaml", "schemas/**/*.schema.json"],
    ),
    ("evals", &["evals/**/*.yaml", "evals/**/*.jsonl"]),
    (
        "toolsets",
        &["toolsets/**/*.openapi.yaml", "toolsets/**/*.openapi.json"],
    ),
];

/// A loaded project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    /// The manifest file.
    pub manifest: PathBuf,
    /// The directory paths in the manifest are relative to.
    pub dir: PathBuf,
    /// The project's name: `name` from the manifest, else the directory name.
    pub name: String,
    /// Glob lists by collection, defaults merged with the manifest's.
    pub collections: BTreeMap<String, Vec<String>>,
    /// Extra ignore globs.
    pub ignore: Vec<String>,
    /// Where generated files go, relative to `dir`.
    pub generated: PathBuf,
    /// The member globs, when this manifest declares a workspace.
    pub members: Option<Vec<String>>,
}

/// A workspace: its manifest's project and its members, by directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    /// The workspace manifest's own project.
    pub root: Project,
    /// Every member project, in directory order.
    pub members: Vec<Project>,
}

/// Why a project could not be found or loaded.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// Both manifest forms are in one directory.
    #[error(
        "{dir} holds both atomic.yaml and .atomic/atomic.yaml; keep one (atomic.yaml for a \
         repository of agents, .atomic/atomic.yaml to stay out of an existing repository's way)",
        dir = .0.display()
    )]
    BothManifests(PathBuf),
    /// No manifest between the start directory and the filesystem root.
    #[error("no atomic.yaml or .atomic/atomic.yaml in {} or any parent directory", .0.display())]
    NotFound(PathBuf),
    /// The manifest could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The manifest.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The manifest is not valid restricted YAML.
    #[error(transparent)]
    Yaml(#[from] yaml::YamlError),
    /// The manifest is valid YAML but not a valid manifest.
    #[error("{}:{line}: {message}", path.display())]
    Invalid {
        /// The manifest.
        path: PathBuf,
        /// The line.
        line: usize,
        /// What is wrong.
        message: String,
    },
}

/// Find the manifest for `start` by walking up its ancestors.
pub fn find_manifest(start: &Path) -> Result<PathBuf, ProjectError> {
    let start = std::fs::canonicalize(start).map_err(|source| ProjectError::Read {
        path: start.to_owned(),
        source,
    })?;
    for dir in start.ancestors() {
        let dedicated = dir.join("atomic.yaml");
        let embedded = dir.join(".atomic").join("atomic.yaml");
        match (dedicated.is_file(), embedded.is_file()) {
            (true, true) => return Err(ProjectError::BothManifests(dir.to_owned())),
            (true, false) => return Ok(dedicated),
            (false, true) => return Ok(embedded),
            (false, false) => {}
        }
    }
    Err(ProjectError::NotFound(start))
}

/// Find and load the project for `start`.
pub fn discover(start: &Path) -> Result<Project, ProjectError> {
    load(&find_manifest(start)?)
}

/// Load the manifest at `path`.
pub fn load(path: &Path) -> Result<Project, ProjectError> {
    let text = std::fs::read_to_string(path).map_err(|source| ProjectError::Read {
        path: path.to_owned(),
        source,
    })?;
    let doc = yaml::load(path, &text)?;
    let dir = path.parent().map(Path::to_owned).unwrap_or_default();
    let invalid = |pointer: &str, message: String| ProjectError::Invalid {
        path: path.to_owned(),
        line: doc.position(pointer).map_or(1, |p| p.line),
        message,
    };
    let Value::Object(map) = &doc.value else {
        return Err(invalid(
            "",
            "a manifest is a mapping, starting with `atomic: 1`".into(),
        ));
    };

    match map.get("atomic") {
        Some(Value::Number(n)) if n.as_u64() == Some(FORMAT_VERSION) => {}
        Some(Value::Number(n)) => {
            return Err(invalid(
                "/atomic",
                format!(
                    "format version {n} is not one this atomic reads (it reads {FORMAT_VERSION})"
                ),
            ));
        }
        Some(other) => {
            return Err(invalid(
                "/atomic",
                format!("`atomic` is the format version, a number, not {other}"),
            ));
        }
        None => {
            return Err(invalid(
                "",
                format!("missing the format version: add `atomic: {FORMAT_VERSION}`"),
            ));
        }
    }

    for key in map.keys() {
        let pointer = format!("/{key}");
        match key.as_str() {
            "atomic" | "name" | "collections" | "ignore" | "generated" | "workspace" => {}
            // Settings, which share the file: the configuration layers read
            // and check them (see `config`).
            "model" | "database" | "runner" | "executor_id" | "budget" => {}
            "packages" | "exports" | "package" | "models" => {
                return Err(invalid(
                    &pointer,
                    format!("`{key}` is not supported by this version of atomic yet"),
                ));
            }
            _ => return Err(invalid(&pointer, format!("unknown manifest key `{key}`"))),
        }
    }

    let name = match map.get("name") {
        None => dir
            .file_name()
            .filter(|n| *n != ".atomic")
            .or_else(|| dir.parent().and_then(Path::file_name))
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".into()),
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(other) => {
            return Err(invalid(
                "/name",
                format!("`name` must be a non-empty string, not {other}"),
            ));
        }
    };

    let mut collections: BTreeMap<String, Vec<String>> = DEFAULT_COLLECTIONS
        .iter()
        .map(|(k, globs)| {
            (
                (*k).to_owned(),
                globs.iter().map(|g| (*g).to_owned()).collect(),
            )
        })
        .collect();
    if let Some(value) = map.get("collections") {
        let Value::Object(given) = value else {
            return Err(invalid(
                "/collections",
                "`collections` maps a collection to a list of globs".into(),
            ));
        };
        for (k, v) in given {
            let pointer = format!("/collections/{k}");
            if !collections.contains_key(k) {
                return Err(invalid(&pointer, format!("unknown collection `{k}`")));
            }
            collections.insert(
                k.clone(),
                string_list(v).ok_or_else(|| {
                    invalid(
                        &pointer,
                        format!("`collections.{k}` must be a list of globs"),
                    )
                })?,
            );
        }
    }

    let ignore = match map.get("ignore") {
        None => Vec::new(),
        Some(v) => string_list(v)
            .ok_or_else(|| invalid("/ignore", "`ignore` must be a list of globs".into()))?,
    };
    let generated = match map.get("generated") {
        None => PathBuf::from(".atomic/cache"),
        Some(Value::String(s)) => PathBuf::from(s),
        Some(other) => {
            return Err(invalid(
                "/generated",
                format!("`generated` must be a path, not {other}"),
            ));
        }
    };

    let members = match map.get("workspace") {
        None => None,
        Some(Value::Object(workspace)) => {
            if let Some(key) = workspace.keys().find(|key| *key != "members") {
                return Err(invalid(
                    &format!("/workspace/{key}"),
                    format!("`workspace.{key}` is not supported by this version of atomic yet"),
                ));
            }
            let members = workspace
                .get("members")
                .and_then(string_list)
                .filter(|members| !members.is_empty())
                .ok_or_else(|| {
                    invalid(
                        "/workspace/members",
                        "`workspace.members` lists member directories, as globs relative to the manifest".into(),
                    )
                })?;
            for member in &members {
                if Path::new(member).is_absolute() || member.split('/').any(|part| part == "..") {
                    return Err(invalid(
                        "/workspace/members",
                        format!("member `{member}` must be inside the workspace"),
                    ));
                }
            }
            Some(members)
        }
        Some(_) => {
            return Err(invalid(
                "/workspace",
                "`workspace` is a mapping with `members`".into(),
            ));
        }
    };

    Ok(Project {
        manifest: path.to_owned(),
        dir,
        name,
        collections,
        ignore,
        generated,
        members,
    })
}

/// The manifest in `dir` itself, if it has one.
fn manifest_in(dir: &Path) -> Result<Option<PathBuf>, ProjectError> {
    let dedicated = dir.join("atomic.yaml");
    let embedded = dir.join(".atomic").join("atomic.yaml");
    match (dedicated.is_file(), embedded.is_file()) {
        (true, true) => Err(ProjectError::BothManifests(dir.to_owned())),
        (true, false) => Ok(Some(dedicated)),
        (false, true) => Ok(Some(embedded)),
        (false, false) => Ok(None),
    }
}

/// The members of the workspace `root` declares: every directory under it
/// that holds a manifest and matches one of its member globs. A literal
/// member with no manifest is an error.
pub fn members(root: &Project) -> Result<Vec<Project>, ProjectError> {
    let globs = root.members.clone().unwrap_or_default();
    let invalid = |message: String| ProjectError::Invalid {
        path: root.manifest.clone(),
        line: 1,
        message,
    };
    let mut builder = globset::GlobSetBuilder::new();
    for glob in &globs {
        let glob = globset::GlobBuilder::new(glob.trim_end_matches('/'))
            .literal_separator(true)
            .build()
            .map_err(|e| invalid(format!("workspace member `{glob}`: {e}")))?;
        builder.add(glob);
    }
    let set = builder
        .build()
        .map_err(|e| invalid(format!("workspace members: {e}")))?;
    let base = root.root().to_owned();
    let mut found = Vec::new();
    let walker = ignore::WalkBuilder::new(&base)
        .require_git(false)
        .max_depth(Some(8))
        .build();
    for entry in walker {
        let entry = entry.map_err(|e| invalid(format!("walking the workspace: {e}")))?;
        if !entry.file_type().is_some_and(|t| t.is_dir()) || entry.path() == base {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&base) else {
            continue;
        };
        if set.is_match(relative)
            && let Some(manifest) = manifest_in(entry.path())?
        {
            found.push(load(&manifest)?);
        }
    }
    for glob in &globs {
        let literal = !glob.contains(['*', '?', '[', '{']);
        if literal
            && !found
                .iter()
                .any(|m| m.root() == base.join(glob.trim_end_matches('/')))
        {
            return Err(invalid(format!(
                "workspace member `{glob}` holds no atomic.yaml or .atomic/atomic.yaml"
            )));
        }
    }
    found.sort_by(|a, b| a.dir.cmp(&b.dir));
    Ok(found)
}

/// The workspace `project` belongs to: itself, if it declares one, or the
/// nearest manifest above it, if that manifest declares a workspace whose
/// members include `project`. A plain manifest above it bounds the search,
/// as a nested project bounds discovery.
pub fn workspace_of(project: &Project) -> Result<Option<Workspace>, ProjectError> {
    if project.members.is_some() {
        return Ok(Some(Workspace {
            root: project.clone(),
            members: members(project)?,
        }));
    }
    let here = std::fs::canonicalize(project.root()).map_err(|source| ProjectError::Read {
        path: project.root().to_owned(),
        source,
    })?;
    for dir in here.ancestors().skip(1) {
        let Some(manifest) = manifest_in(dir)? else {
            continue;
        };
        let root = load(&manifest)?;
        if root.members.is_none() {
            return Ok(None);
        }
        let members = members(&root)?;
        let canonical = |p: &Project| std::fs::canonicalize(p.root()).ok();
        if members
            .iter()
            .any(|m| canonical(m).as_deref() == Some(here.as_path()))
        {
            return Ok(Some(Workspace { root, members }));
        }
        return Ok(None);
    }
    Ok(None)
}

impl Project {
    /// The project's root: the manifest's directory, or for an embedded
    /// project (`.atomic/atomic.yaml`) the directory holding `.atomic/`.
    pub fn root(&self) -> &Path {
        if self.dir.file_name().is_some_and(|n| n == ".atomic") {
            self.dir.parent().unwrap_or(&self.dir)
        } else {
            &self.dir
        }
    }
}

fn string_list(v: &Value) -> Option<Vec<String>> {
    v.as_array()?
        .iter()
        .map(|x| x.as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests;
