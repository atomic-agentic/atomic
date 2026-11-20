// SPDX-License-Identifier: Apache-2.0

//! The per-project state directory.
//!
//! Runtime state never lives in the source tree. Each project gets a directory
//! under the user data directory (`~/.local/share/atomic/projects/<id>/` on
//! Linux, the platform equivalent elsewhere), or under `$ATOMIC_HOME/projects/`
//! when `ATOMIC_HOME` is set:
//!
//! ```text
//! <state>/
//!   private/      what is atomic's alone, denied to every atom's commands:
//!     atomic.db     atomic's own database (durable workflows, the gate store)
//!     context/      context checkpoints
//!   codex/        Codex's home for atom threads, with its own state database
//!                 (denied to atoms too, apart from the shell snapshots and
//!                 helper links Codex's shell needs)
//!   worktrees/    one worktree per code-changing step
//!   cache/        build caches shared by a project's steps
//!   tmp/          each turn's own temporary directory
//! ```
//!
//! Atom threads therefore never mix with the user's own Codex sessions.
//! Beside the project directories, atomic keeps two user-wide private
//! directories, also denied to atoms: the context key
//! ([`crate::context::default_key_dir`]) and the opt-in credential file
//! store ([`credentials_dir`]).

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Where one project's local state lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectState {
    root: PathBuf,
    dir: PathBuf,
}

/// Why the state directory could not be resolved or created.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The project root could not be resolved to an absolute path.
    #[error("cannot resolve the project root {path}: {source}")]
    Root {
        /// The path given.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// Neither `ATOMIC_HOME` nor a user data directory is available.
    #[error("no user data directory on this platform; set ATOMIC_HOME")]
    NoDataDir,
    /// The directory could not be created.
    #[error("cannot create the state directory {path}: {source}")]
    Create {
        /// The directory.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

impl ProjectState {
    /// Resolve the state directory for the project rooted at `root`, without
    /// touching the filesystem beyond resolving `root`.
    pub fn resolve(root: &Path) -> Result<Self, StateError> {
        let root = std::fs::canonicalize(root).map_err(|source| StateError::Root {
            path: root.to_owned(),
            source,
        })?;
        let dir = base_dir()?.join("projects").join(project_id(&root));
        Ok(Self { root, dir })
    }

    /// Create the state directory and its fixed subdirectories, the private
    /// one readable by its owner only. A database or context directory an
    /// older atomic left at the top of the state directory moves into
    /// `private/`.
    pub fn create(&self) -> Result<(), StateError> {
        let create = |d: &Path| {
            std::fs::create_dir_all(d).map_err(|source| StateError::Create {
                path: d.to_owned(),
                source,
            })
        };
        create(&self.dir)?;
        create(&self.codex_home())?;
        private_dir(&self.private_dir())?;
        for name in ["atomic.db", "atomic.db-wal", "atomic.db-shm", "context"] {
            let (old, new) = (self.dir.join(name), self.private_dir().join(name));
            if old.exists() && !new.exists() {
                std::fs::rename(&old, &new).map_err(|source| StateError::Create {
                    path: new.clone(),
                    source,
                })?;
            }
        }
        Ok(())
    }

    /// The project root this state belongs to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The state directory itself.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// What is atomic's alone: denied to every atom's commands.
    pub fn private_dir(&self) -> PathBuf {
        self.dir.join("private")
    }

    /// atomic's own database file.
    pub fn database(&self) -> PathBuf {
        self.private_dir().join("atomic.db")
    }

    /// Where context checkpoints are kept.
    pub fn context_dir(&self) -> PathBuf {
        self.private_dir().join("context")
    }

    /// Codex's home for atom threads.
    pub fn codex_home(&self) -> PathBuf {
        self.dir.join("codex")
    }
}

/// Where atomic keeps every project's state: `$ATOMIC_HOME`, or `atomic` in
/// the user data directory.
pub fn base_dir() -> Result<PathBuf, StateError> {
    match std::env::var_os("ATOMIC_HOME").filter(|v| !v.is_empty()) {
        Some(home) => Ok(PathBuf::from(home)),
        None => Ok(dirs::data_dir()
            .ok_or(StateError::NoDataDir)?
            .join("atomic")),
    }
}

/// Where the opt-in file store keeps operator credentials (see
/// [`crate::credential::FileSecrets`]): `$ATOMIC_HOME/credentials`, or
/// `atomic/credentials` in the user configuration directory. Outside every
/// project and every state directory.
pub fn credentials_dir() -> Result<PathBuf, StateError> {
    match std::env::var_os("ATOMIC_HOME").filter(|v| !v.is_empty()) {
        Some(home) => Ok(PathBuf::from(home).join("credentials")),
        None => Ok(dirs::config_dir()
            .ok_or(StateError::NoDataDir)?
            .join("atomic/credentials")),
    }
}

/// Create `dir` if needed, readable by its owner only.
pub(crate) fn private_dir(dir: &Path) -> Result<(), StateError> {
    let fail = |source| StateError::Create {
        path: dir.to_owned(),
        source,
    };
    if !dir.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir).map_err(fail)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(fail)?;
    }
    Ok(())
}

/// A readable, collision-resistant id: the root's last component, then the
/// first 12 hex digits of the SHA-256 of its full path.
fn project_id(root: &Path) -> String {
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let hash = Sha256::digest(root.as_os_str().as_encoded_bytes());
    format!("{name}-{}", &format!("{hash:x}")[..12])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_layout_moves_into_the_private_directory() {
        let dir = std::env::temp_dir().join(format!("atomic-state-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = ProjectState {
            root: dir.join("project"),
            dir: dir.join("state"),
        };
        std::fs::create_dir_all(state.dir().join("context")).expect("old layout");
        std::fs::write(state.dir().join("atomic.db"), "db").expect("old database");
        state.create().expect("create");
        assert_eq!(
            std::fs::read_to_string(state.database()).expect("moved"),
            "db"
        );
        assert!(state.context_dir().is_dir());
        assert!(!state.dir().join("atomic.db").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(state.private_dir())
                .expect("private")
                .permissions();
            assert_eq!(mode.mode() & 0o777, 0o700);
        }
        state.create().expect("again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_id_is_stable_and_names_the_project() {
        let a = project_id(Path::new("/work/My Repo"));
        assert_eq!(a, project_id(Path::new("/work/My Repo")));
        assert!(a.starts_with("My_Repo-"), "{a}");
        assert_ne!(a, project_id(Path::new("/other/My Repo")));
    }
}
