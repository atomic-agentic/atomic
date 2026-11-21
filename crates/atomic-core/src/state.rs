// SPDX-License-Identifier: Apache-2.0

//! The per-project state directory.
//!
//! Runtime state never lives in the source tree. Each project gets a directory
//! under the user data directory (`~/.local/share/atomic/projects/<id>/` on
//! Linux, the platform equivalent elsewhere), or under `$ATOMIC_HOME/projects/`
//! when `ATOMIC_HOME` is set:
//!
//! ```text
//! <home>/private/<project>/   what is atomic's alone, denied to every atom's
//!                             commands, with every other project's, by one
//!                             entry for the whole of `<home>/private`:
//!     atomic.db     atomic's own database (durable workflows, the gate store)
//!     context/      context checkpoints
//!     runtime.lock  held by the one process that executes the project's runs
//! <state>/ = <home>/projects/<project>/
//!   codex/        Codex's home for atom threads, with its own state database
//!                 (denied to atoms too, apart from the shell snapshots and
//!                 helper links Codex's shell needs)
//!   worktrees/    one worktree per code-changing step
//!   cache/        build caches shared by a project's steps
//!   fixtures/     a bare snapshot of each commit a coding run is bound to
//!                 (see [`crate::fixture`]), each made whole before it
//!                 appears
//!   tmp/          each turn's own temporary directory
//!   scratch/      each atom attempt's scratch directory, and the
//!                 temporary directory its backend's turns share
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
//! (`<home>/private/<project>/runtime.lock`) first, so two of them never recover or run the
//! same work. The operating system releases it when its holder exits,
//! however it exits.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Where one project's local state lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectState {
    root: PathBuf,
    dir: PathBuf,
    private: PathBuf,
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
    /// `ATOMIC_HOME` is relative, and the working directory it is taken
    /// from could not be read.
    #[error("cannot resolve ATOMIC_HOME {path}: {source}")]
    Home {
        /// The value given.
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
    /// State an older atomic left is still in use, so it cannot move yet.
    #[error(
        "{path} is in use by another atomic process, so its state cannot move to {to} yet; \
         stop that process, then run atomic again"
    )]
    InUse {
        /// The database, or execution lock, in use.
        path: PathBuf,
        /// Where it would move.
        to: PathBuf,
    },
    /// State an older atomic left is there, and so is the current layout's,
    /// as after running an older atomic again: which one holds the project's
    /// runs is the operator's to say.
    #[error(
        "{old} is left from an older atomic, and {current} exists too, so neither is used; \
         keep the one that holds this project's runs at {current} and move the other out of \
         the way, then run atomic again"
    )]
    Both {
        /// The older layout's file or directory.
        old: PathBuf,
        /// The current layout's.
        current: PathBuf,
    },
}

impl ProjectState {
    /// The state of a project at `root` kept under `home` as an atomic home,
    /// for tests that must not touch the real one.
    #[cfg(test)]
    pub(crate) fn under(home: &Path, root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            dir: home.join("projects/test"),
            private: home.join("private/test"),
        }
    }

    /// Resolve the state directory for the project rooted at `root`, without
    /// touching the filesystem beyond resolving `root`.
    pub fn resolve(root: &Path) -> Result<Self, StateError> {
        let root = std::fs::canonicalize(root).map_err(|source| StateError::Root {
            path: root.to_owned(),
            source,
        })?;
        let home = base_dir()?;
        let id = project_id(&root);
        Ok(Self {
            dir: home.join("projects").join(&id),
            private: home.join("private").join(&id),
            root,
        })
    }

    /// Create the state directory and its fixed subdirectories, and the
    /// project's private directory, readable by its owner only, as is the
    /// home's `private/` above it.
    ///
    /// A database or context directory an older atomic left in the state
    /// directory, at its top or in its `private/`, moves into the private
    /// directory: only while this process holds the state directory's
    /// migration lock, and never while another process has that database
    /// open ([`StateError::InUse`]).
    pub fn create(&self) -> Result<(), StateError> {
        let create = |d: &Path| {
            std::fs::create_dir_all(d).map_err(|source| StateError::Create {
                path: d.to_owned(),
                source,
            })
        };
        create(&self.dir)?;
        create(&self.codex_home())?;
        private_dir(&self.private_root())?;
        private_dir(&self.private)?;
        let legacy: Vec<PathBuf> = [self.dir.join("private"), self.dir.clone()]
            .into_iter()
            .filter(|dir| LEGACY.iter().any(|name| dir.join(name).exists()))
            .collect();
        if legacy.is_empty() {
            return Ok(());
        }
        let fail = |path: &Path| {
            let path = path.to_owned();
            move |source| StateError::Create { path, source }
        };
        // One process migrates at a time.
        let lock_path = self.dir.join("migrate.lock");
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(fail(&lock_path))?;
        lock.lock().map_err(fail(&lock_path))?;
        for dir in legacy {
            // Both layouts at once: moving would lose one of them, and
            // ignoring the old one could hide the project's runs.
            for name in LEGACY {
                let (old, current) = (dir.join(name), self.private.join(name));
                if old.exists() && current.exists() {
                    return Err(StateError::Both { old, current });
                }
            }
            let database = dir.join("atomic.db");
            if database_in_use(&database) {
                return Err(StateError::InUse {
                    path: database,
                    to: self.database(),
                });
            }
            // An older atomic kept its execution lock in `<state>/private`: a
            // process holding it may still run the project's work. It is
            // held here until the state has moved, so no older atomic starts
            // on it meanwhile.
            let old_lock = dir.join("runtime.lock");
            let old_layout_lock = dir != self.dir && old_lock.exists();
            let held = if old_layout_lock {
                let Some(held) = hold_lock(&old_lock) else {
                    return Err(StateError::InUse {
                        path: old_lock,
                        to: self.lock_file(),
                    });
                };
                held
            } else {
                None
            };
            for name in LEGACY {
                let (old, new) = (dir.join(name), self.private.join(name));
                if old.exists() {
                    std::fs::rename(&old, &new).map_err(fail(&new))?;
                }
            }
            if old_layout_lock {
                let _ = std::fs::remove_file(&old_lock);
            }
            drop(held);
            if dir != self.dir {
                // The old `<state>/private`, now empty.
                let _ = std::fs::remove_dir(&dir);
            }
        }
        drop(lock);
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

    /// What is this project's alone: denied to every atom's commands.
    pub fn private_dir(&self) -> PathBuf {
        self.private.clone()
    }

    /// Where every project of this atomic home keeps its private state, its
    /// own included: one directory that a sandbox denies whole.
    pub fn private_root(&self) -> PathBuf {
        self.private
            .parent()
            .map(Path::to_owned)
            .unwrap_or_else(|| self.private.clone())
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

/// What an older layout kept in the state directory, at its top or in its
/// `private/`.
const LEGACY: [&str; 4] = ["atomic.db", "atomic.db-wal", "atomic.db-shm", "context"];

/// The lock on the file at `path`, held until the file returned is dropped,
/// or `None` when another process holds it. A file that cannot be opened or
/// locked at all holds nothing back.
fn hold_lock(path: &Path) -> Option<Option<std::fs::File>> {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return Some(None);
    };
    match file.try_lock() {
        Ok(()) => Some(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => None,
        Err(std::fs::TryLockError::Error(_)) => Some(None),
    }
}

/// Whether another process has the SQLite database at `path` open in WAL
/// mode: each such connection holds a read lock on its shared-memory file's
/// "DMS" byte (offset 128) for as long as it is open.
fn database_in_use(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut shm = path.as_os_str().to_owned();
        shm.push("-shm");
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&shm)
        else {
            return false;
        };
        // SAFETY: a zeroed flock is a valid value of a plain C struct.
        #[allow(unsafe_code)]
        let mut probe: libc::flock = unsafe { std::mem::zeroed() };
        probe.l_type = libc::F_WRLCK as _;
        probe.l_whence = libc::SEEK_SET as _;
        probe.l_start = 128;
        probe.l_len = 1;
        // SAFETY: F_GETLK reads and writes only `probe`, a valid flock.
        #[allow(unsafe_code)]
        let asked = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut probe) };
        asked == 0 && probe.l_type != libc::F_UNLCK as libc::c_short
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Where atomic keeps every project's state: `$ATOMIC_HOME`, or `atomic` in
/// the user data directory.
pub fn base_dir() -> Result<PathBuf, StateError> {
    match atomic_home()? {
        Some(home) => Ok(home),
        None => Ok(dirs::data_dir()
            .ok_or(StateError::NoDataDir)?
            .join("atomic")),
    }
}

/// `$ATOMIC_HOME`, if it is set, as an absolute path: one given relative
/// is taken from the working directory atomic started in, so it names the
/// same directory wherever it is used, such as by a helper Codex starts in
/// another directory, with its `..` resolved as far as it exists.
fn atomic_home() -> Result<Option<PathBuf>, StateError> {
    let Some(home) = std::env::var_os("ATOMIC_HOME").filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let home = PathBuf::from(home);
    if home.is_absolute() {
        return Ok(Some(home));
    }
    let joined = std::env::current_dir()
        .map_err(|source| StateError::Home {
            path: home.clone(),
            source,
        })?
        .join(&home);
    let mut rest = Vec::new();
    let mut existing = joined.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            return Ok(Some(rest.iter().rev().fold(real, |p, part| p.join(part))));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return Ok(Some(joined)),
        }
    }
}

/// Where the opt-in file store keeps operator credentials (see
/// [`crate::credential::FileSecrets`]): `$ATOMIC_HOME/credentials`, or
/// `atomic/credentials` in the user configuration directory. Outside every
/// project and every state directory.
pub fn credentials_dir() -> Result<PathBuf, StateError> {
    match atomic_home()? {
        Some(home) => Ok(home.join("credentials")),
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
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
}

/// The home directory the password database gives this process's user,
/// whatever `HOME` says; `None` when the user has no entry or its home is
/// not an absolute path.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn passwd_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: an all-zero `passwd` is a valid value of the plain C struct.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes only into `entry` and `buffer`, both of the
    // sizes given and alive for the call, and sets `found` to null or to
    // `entry`.
    let code = unsafe {
        libc::getpwuid_r(
            effective_uid(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if code != 0 || found.is_null() || entry.pw_dir.is_null() {
        return None;
    }
    // SAFETY: on success `pw_dir` points at a NUL-terminated string inside
    // `buffer`, which is still alive.
    let dir = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
    let home = PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()));
    home.is_absolute().then_some(home)
}

/// See the Unix version: there is no password database to read here.
#[cfg(not(unix))]
pub fn passwd_home() -> Option<PathBuf> {
    None
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
            private: dir.join("private/p"),
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
            private: dir.join("private/p"),
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
            private: dir.join("private/p"),
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

    /// The layout before every project's private state moved under the
    /// home's `private/`: `<state>/private/` moves to `<home>/private/<id>`,
    /// but not while another process has its database open.
    #[test]
    fn a_state_private_directory_moves_once_its_database_is_closed() {
        let dir = std::env::temp_dir().join(format!("atomic-state-moved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = ProjectState {
            root: dir.join("project"),
            dir: dir.join("home/projects/p"),
            private: dir.join("home/private/p"),
        };
        let old = state.dir().join("private");
        std::fs::create_dir_all(old.join("context")).expect("old layout");
        let old_db = old.join("atomic.db");
        // Another process holds the old database open, in WAL mode.
        let mut holder = std::process::Command::new("python3")
            .args([
                "-c",
                "import sqlite3, sys, time\n\
                 c = sqlite3.connect(sys.argv[1]); c.execute('PRAGMA journal_mode=WAL')\n\
                 c.execute('CREATE TABLE IF NOT EXISTS t (x)'); c.execute('INSERT INTO t VALUES (1)'); c.commit()\n\
                 print('open', flush=True); time.sleep(60)",
            ])
            .arg(&old_db)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("python3");
        let mut ready = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(holder.stdout.as_mut().expect("stdout")),
            &mut ready,
        )
        .expect("ready");
        assert_eq!(ready.trim(), "open");
        match state.create() {
            Err(StateError::InUse { path, .. }) => assert_eq!(path, old_db),
            other => panic!("moved under an open database: {other:?}"),
        }
        assert!(old_db.exists(), "nothing moved");
        holder.kill().expect("stop the holder");
        holder.wait().expect("reaped");
        // An older atomic's execution lock, still held, stops the move too.
        let old_lock = old.join("runtime.lock");
        let held = std::fs::File::create(&old_lock).expect("old lock");
        held.lock().expect("hold");
        match state.create() {
            Err(StateError::InUse { path, to }) => {
                assert_eq!(path, old_lock);
                assert_eq!(to, state.lock_file());
            }
            other => panic!("moved under a held execution lock: {other:?}"),
        }
        assert!(old_db.exists(), "nothing moved");
        drop(held);
        state.create().expect("moves once closed");
        assert!(state.database().is_file());
        assert!(state.context_dir().is_dir());
        assert!(!old.exists(), "the old private directory is gone");
        assert_eq!(state.private_root(), dir.join("home/private"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both layouts at once, as after running an older atomic again: the
    /// old state is neither moved over the new nor ignored.
    #[test]
    fn an_older_layout_beside_the_current_one_is_refused() {
        let dir = std::env::temp_dir().join(format!("atomic-state-both-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = ProjectState {
            root: dir.join("project"),
            dir: dir.join("home/projects/p"),
            private: dir.join("home/private/p"),
        };
        state.create().expect("the current layout");
        std::fs::write(state.database(), "current").expect("current database");
        let old = state.dir().join("private");
        std::fs::create_dir_all(&old).expect("old layout");
        std::fs::write(old.join("atomic.db"), "old").expect("old database");
        match state.create() {
            Err(StateError::Both { old: o, current }) => {
                assert_eq!(o, old.join("atomic.db"));
                assert_eq!(current, state.database());
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(state.database()).expect("db"),
            "current"
        );
        assert_eq!(
            std::fs::read_to_string(old.join("atomic.db")).expect("db"),
            "old"
        );
        // Once the operator has moved the old one away, atomic runs again.
        std::fs::remove_file(old.join("atomic.db")).expect("moved away");
        state.create().expect("one layout");
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
