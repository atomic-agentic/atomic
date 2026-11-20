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
//!     runtime.lock  held by the one process that executes the project's runs
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
//!
//! One process at a time executes a project's runs: `atomic run`,
//! `atomic runs resume` and `atomic serve` each take the [`ExecutionLock`]
//! (`private/runtime.lock`) first, so two of them never recover or run the
//! same work. The operating system releases it when its holder exits,
//! however it exits.

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
    /// A private directory is open to other users, or reached through a
    /// link to one, and cannot be made private.
    #[error("{path} {why}")]
    Unsafe {
        /// The directory.
        path: PathBuf,
        /// What is wrong, and what to do.
        why: String,
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

    /// The file the execution lock is held on.
    pub fn lock_file(&self) -> PathBuf {
        self.private_dir().join("runtime.lock")
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
                return Err(LockError::Held {
                    owner: holder(&path),
                    path,
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

/// Who holds the lock at `path`, as it wrote: `pid <n> (<command>)`. A
/// holder writes only after it has the lock, so the file can be empty, or
/// still say who held it last; words whose process is gone are not
/// reported as the holder's.
fn holder(path: &Path) -> String {
    for _ in 0..10 {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let text = text.trim();
        let pid = text
            .strip_prefix("pid ")
            .and_then(|rest| rest.split_whitespace().next());
        let current = pid.is_some_and(running);
        if current {
            return text.to_owned();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    "another process, which has not yet said which".into()
}

/// Whether process `pid` exists.
fn running(pid: &str) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = pid.parse::<libc::pid_t>() else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 sends nothing; it only checks that the process
        // exists and may be signalled.
        #[allow(unsafe_code)]
        let found = unsafe { libc::kill(pid, 0) } == 0;
        found || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        !pid.is_empty()
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

    /// Hold it until this process exits, however it exits: never released
    /// while anything this process started may still be running, such as a
    /// command a blocked thread is waiting on as the process stops. The
    /// file is closed on exec, so no child keeps it.
    pub fn hold_until_exit(self) {
        std::mem::forget(self);
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

/// The directory of the person's own configuration, which no project
/// supplies: `$XDG_CONFIG_HOME/atomic`, by default `~/.config/atomic`, on
/// every platform. `None` when neither variable, as `env` reads them, names
/// an absolute directory.
pub fn user_config_dir_in(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let absolute = |name: &str| env(name).map(PathBuf::from).filter(|p| p.is_absolute());
    absolute("XDG_CONFIG_HOME")
        .map(|d| d.join("atomic"))
        .or_else(|| absolute("HOME").map(|h| h.join(".config/atomic")))
}

/// [`user_config_dir_in`] this process's environment.
pub fn user_config_dir() -> Option<PathBuf> {
    user_config_dir_in(&|name| std::env::var(name).ok())
}

/// This process's effective user id.
#[cfg(unix)]
#[allow(unsafe_code)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
}

/// Make sure `dir` is readable by its owner only: create it so if it is
/// missing, and close it if it is open to others. A directory already closed
/// is left as it is, so one mounted read-only, such as a Kubernetes secret,
/// still works. A symbolic link is never followed to change what it points
/// to: it is accepted only when its target is a directory the user owns and
/// no one else can enter.
pub(crate) fn private_dir(dir: &Path) -> Result<(), StateError> {
    let fail = |source| StateError::Create {
        path: dir.to_owned(),
        source,
    };
    let refuse = |why: String| StateError::Unsafe {
        path: dir.to_owned(),
        why,
    };
    let meta = match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            return builder.create(dir).map_err(fail);
        }
        Err(e) => return Err(fail(e)),
        Ok(meta) => meta,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.file_type().is_symlink() {
            let target = std::fs::metadata(dir).map_err(fail)?;
            if target.is_dir() && target.uid() == effective_uid() && target.mode() & 0o077 == 0 {
                return Ok(());
            }
            return Err(refuse(
                "is a symbolic link to a directory that is not yours alone: make the target \
                 a directory you own with mode 0700, or replace the link with one"
                    .into(),
            ));
        }
        if !meta.is_dir() {
            return Err(refuse("is not a directory".into()));
        }
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
                refuse(format!(
                    "is open to other users (mode {mode:o}) and cannot be made private: {e}; \
                     make it mode 0700"
                ))
            })?;
        }
    }
    #[cfg(not(unix))]
    if !meta.is_dir() {
        return Err(refuse("is not a directory".into()));
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

    #[cfg(unix)]
    #[test]
    fn a_private_directory_is_closed_once_and_a_link_is_never_followed_open() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("atomic-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let mode = |p: &Path| {
            std::fs::symlink_metadata(p)
                .expect("meta")
                .permissions()
                .mode()
                & 0o777
        };
        let set = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).expect("chmod")
        };
        // Missing: made private.
        private_dir(&dir.join("new/inner")).expect("made");
        assert_eq!(mode(&dir.join("new/inner")), 0o700);
        // Open to others: closed.
        std::fs::create_dir(dir.join("open")).expect("open");
        set(&dir.join("open"), 0o755);
        private_dir(&dir.join("open")).expect("closed");
        assert_eq!(mode(&dir.join("open")), 0o700);
        // Already closed, as a read-only mount would be: left alone.
        std::fs::create_dir(dir.join("mounted")).expect("mounted");
        set(&dir.join("mounted"), 0o500);
        private_dir(&dir.join("mounted")).expect("accepted");
        assert_eq!(mode(&dir.join("mounted")), 0o500, "not touched");
        // A link to a private directory of one's own is accepted.
        std::os::unix::fs::symlink(dir.join("open"), dir.join("to-private")).expect("link");
        private_dir(&dir.join("to-private")).expect("a private target");
        // A link to an open one is refused, and the target left as it is.
        std::fs::create_dir(dir.join("shared")).expect("shared");
        set(&dir.join("shared"), 0o755);
        std::os::unix::fs::symlink(dir.join("shared"), dir.join("to-shared")).expect("link");
        assert!(matches!(
            private_dir(&dir.join("to-shared")),
            Err(StateError::Unsafe { .. })
        ));
        assert_eq!(
            mode(&dir.join("shared")),
            0o755,
            "never chmodded through the link"
        );
        set(&dir.join("mounted"), 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn a_holder_that_has_not_yet_said_who_it_is_is_not_misnamed() {
        let dir = std::env::temp_dir().join(format!("atomic-lock-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let state = ProjectState {
            root: dir.clone(),
            dir: dir.join("state"),
        };
        state.create().expect("state");
        // A previous holder's words, left in the file, and a new holder that
        // has the lock but has not written yet.
        std::fs::write(state.lock_file(), "pid 2147483000 (atomic serve)").expect("stale");
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(state.lock_file())
            .expect("open");
        holder.try_lock().expect("hold");
        match state.lock_execution("atomic runs resume") {
            Err(LockError::Held { owner, .. }) => {
                assert!(!owner.contains("2147483000"), "{owner}");
                assert!(owner.contains("has not yet said"), "{owner}");
            }
            other => panic!("{other:?}"),
        }
        // Once it has, its own words are reported.
        std::fs::write(
            state.lock_file(),
            format!("pid {} (atomic serve)", std::process::id()),
        )
        .expect("owner");
        match state.lock_execution("atomic runs resume") {
            Err(LockError::Held { owner, .. }) => {
                assert_eq!(owner, format!("pid {} (atomic serve)", std::process::id()));
            }
            other => panic!("{other:?}"),
        }
    }

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
