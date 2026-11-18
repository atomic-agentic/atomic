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
//!   atomic.db     atomic's own database (durable workflows)
//!   codex/        Codex's home for atom threads, with its own state database
//!   runtime.lock  held by the one process that executes the project's runs
//! ```
//!
//! Atom threads therefore never mix with the user's own Codex sessions.
//!
//! One process at a time executes a project's runs: `atomic run`,
//! `atomic runs resume` and `atomic serve` each take the [`ExecutionLock`]
//! first, so two of them never recover or run the same work. The operating
//! system releases it when its holder exits, however it exits.

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
        let base = match std::env::var_os("ATOMIC_HOME").filter(|v| !v.is_empty()) {
            Some(home) => PathBuf::from(home),
            None => dirs::data_dir()
                .ok_or(StateError::NoDataDir)?
                .join("atomic"),
        };
        let dir = base.join("projects").join(project_id(&root));
        Ok(Self { root, dir })
    }

    /// Create the state directory and its fixed subdirectories.
    pub fn create(&self) -> Result<(), StateError> {
        for d in [self.dir.clone(), self.codex_home()] {
            std::fs::create_dir_all(&d).map_err(|source| StateError::Create {
                path: d.clone(),
                source,
            })?;
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

    /// atomic's own database file.
    pub fn database(&self) -> PathBuf {
        self.dir.join("atomic.db")
    }

    /// Codex's home for atom threads.
    pub fn codex_home(&self) -> PathBuf {
        self.dir.join("codex")
    }

    /// The file the execution lock is held on.
    pub fn lock_file(&self) -> PathBuf {
        self.dir.join("runtime.lock")
    }

    /// Take the project's execution lock for `command`, or say who holds it.
    /// The state directory must exist.
    pub fn lock_execution(&self, command: &str) -> Result<ExecutionLock, LockError> {
        let path = self.lock_file();
        let io = |source| LockError::Io {
            path: path.clone(),
            source,
        };
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                let mut owner = String::new();
                let _ = std::io::Read::read_to_string(&mut file, &mut owner);
                return Err(LockError::Held {
                    path,
                    owner: owner.trim().to_owned(),
                });
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(io(e)),
        }
        // The holder writes who it is, for anyone refused the lock.
        let owner = format!("pid {} ({command})", std::process::id());
        file.set_len(0).map_err(io)?;
        std::io::Write::write_all(&mut file, owner.as_bytes()).map_err(io)?;
        Ok(ExecutionLock { _file: file, owner })
    }
}

/// The project's execution lock, held until it is dropped or the process
/// exits.
#[derive(Debug)]
pub struct ExecutionLock {
    _file: std::fs::File,
    owner: String,
}

impl ExecutionLock {
    /// Who holds it, as others are told: this process and its command.
    pub fn owner(&self) -> &str {
        &self.owner
    }
}

/// Why the execution lock was not taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another process holds it.
    #[error("another atomic process is executing this project's runs: {owner}")]
    Held {
        /// The lock file.
        path: PathBuf,
        /// What its holder wrote: its process id and command.
        owner: String,
    },
    /// The lock file could not be opened or written.
    #[error("cannot take the execution lock {}: {source}", path.display())]
    Io {
        /// The lock file.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
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
    fn one_process_at_a_time_holds_the_execution_lock() {
        let dir = std::env::temp_dir().join(format!("atomic-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let state = ProjectState {
            root: dir.clone(),
            dir: dir.join("state"),
        };
        state.create().expect("state");
        let held = state.lock_execution("atomic serve").expect("free");
        // A second open of the file is refused while the first holds it,
        // and is told who holds it.
        match state.lock_execution("atomic runs resume") {
            Err(LockError::Held { owner, .. }) => {
                assert_eq!(owner, held.owner());
                assert!(owner.contains("atomic serve"), "{owner}");
            }
            other => panic!("{other:?}"),
        }
        drop(held);
        let again = state
            .lock_execution("atomic runs resume")
            .expect("released");
        assert!(again.owner().contains("resume"));
    }

    #[test]
    fn the_id_is_stable_and_names_the_project() {
        let a = project_id(Path::new("/work/My Repo"));
        assert_eq!(a, project_id(Path::new("/work/My Repo")));
        assert!(a.starts_with("My_Repo-"), "{a}");
        assert_ne!(a, project_id(Path::new("/other/My Repo")));
    }
}
