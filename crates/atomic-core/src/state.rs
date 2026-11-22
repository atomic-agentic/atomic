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
//!     codex/        what Codex records of the project's threads: its state
//!                   databases and its logs
//!     runtime.lock  held by the one process that executes the project's runs
//! <home>/scratch/<project>/   the project's backends' scratch roots, denied
//!                             to every atom's commands, with every other
//!                             project's, by one entry for the whole of
//!                             `<home>/scratch`, an attempt's own apart:
//!     run/          each atom attempt's scratch directory of `atomic run`,
//!                   `atomic serve` and `atomic eval`, and the temporary
//!                   directory their turns share
//!     debug/        the same for `atomic debug codex-prompt` and `atomic debug
//!                   probe`
//! <state>/ = <home>/projects/<project>/
//!   codex/        Codex's home for atom threads: it keeps no thread
//!                 records; its helper links are the only part a turn's
//!                 shell reads, the rest denied to atoms
//!   worktrees/    one worktree per code-changing step
//!   cache/        build caches shared by a project's steps
//!   fixtures/     a bare snapshot of each commit a coding run is bound to
//!                 (see [`crate::fixture`]), each made whole before it
//!                 appears
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
//! (`<home>/private/<project>/runtime.lock`) first, so two of them never recover or run the
//! same work. The operating system releases it when its holder exits,
//! however it exits.
//!
//! The home must not be the person's own configuration directory
//! ([`user_config_dir`]) or a directory inside it: every atom's commands are
//! denied that directory whole, so [`ProjectState::create`] refuses a state
//! directory there before it creates anything — and refuses one whose path it
//! cannot resolve, since then it cannot say where that home is.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Where one project's local state lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectState {
    root: PathBuf,
    dir: PathBuf,
    private: PathBuf,
    /// Its backends' scratch roots' parent, in the home's `scratch/`, beside
    /// every other project's.
    scratch: PathBuf,
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
    /// The state directory is the person's own configuration directory, or a
    /// directory inside it, where every atom's commands are denied whole:
    /// the state there would be denied with it. Its words are the refusal's
    /// own ([`OutsideConfig`]), and nothing else is said for them.
    #[error("{0}")]
    InsideConfig(OutsideConfig),
    /// A directory the state directory is placed by could not be resolved, so
    /// atomic cannot tell where the state would go and runs nowhere.
    #[error("cannot resolve {} {path}: {why}", side.words())]
    Resolve {
        /// Which of the two directories this is: the state's own home, or the
        /// person's configuration directory ([`ResolveSide`]).
        side: ResolveSide,
        /// The directory, as it was given.
        path: PathBuf,
        /// The part of it that could not be looked up, and what the
        /// filesystem said.
        why: String,
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

/// [`outside_user_config`]'s own report: a state directory that is
/// [`user_config_dir`], or sits inside it. It names `ATOMIC_HOME` as the person
/// set it, the state directory that spelling leads to as it really is, and the
/// configuration directory — so which of their own settings put the state
/// there is visible from the message alone, including when no home is set and
/// the default data directory leads there.
#[derive(Debug)]
pub struct OutsideConfig {
    /// The state directory, as it really is.
    pub state: PathBuf,
    /// `ATOMIC_HOME` as the person set it, word for word; `None` when nothing
    /// sets it and the default data directory gives this instead.
    pub given: Option<PathBuf>,
    /// The person's configuration directory, as it really is.
    pub config: PathBuf,
}

impl std::fmt::Display for OutsideConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.given {
            Some(home) => write!(
                f,
                "ATOMIC_HOME is set to {home}, which puts atomic's state directory {state} \
                 inside {config}, your own configuration directory, which every atom's \
                 commands are denied",
                home = home.display(),
                state = self.state.display(),
                config = self.config.display(),
            ),
            None => write!(
                f,
                "ATOMIC_HOME is not set to a directory, so atomic's state directory {state} \
                 is inside {config}, your own configuration directory, which every atom's \
                 commands are denied",
                state = self.state.display(),
                config = self.config.display(),
            ),
        }
    }
}

impl OutsideConfig {
    /// What to change so that the state goes elsewhere than the configuration
    /// directory, in the words of the action atomic shows.
    pub fn fix(&self) -> String {
        format!(
            "set ATOMIC_HOME to a directory outside {}, or point XDG_CONFIG_HOME, or HOME, \
             elsewhere; a worktree or a cache inside your own configuration directory is out of \
             atoms' commands' reach",
            self.config.display()
        )
    }
}

/// Which of the two directories [`real_path`] was placing when it could not:
/// carried on [`StateError::Resolve`] so that what to change names the side
/// that actually failed, since the two come from different places.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveSide {
    /// The state directory, wherever `ATOMIC_HOME` puts it or, with no home
    /// set, the platform's user data directory.
    State,
    /// The person's own configuration directory, wherever `XDG_CONFIG_HOME`,
    /// `HOME` or the password database puts it.
    Config,
}

impl ResolveSide {
    /// What the directory is called in the refusal ([`StateError::Resolve`]).
    pub fn words(&self) -> &'static str {
        match self {
            ResolveSide::State => "atomic's state directory",
            ResolveSide::Config => "your configuration directory",
        }
    }

    /// What to change so that this directory can be placed, in the words of
    /// the action atomic shows: the state's home is `ATOMIC_HOME`'s to fix and
    /// the configuration directory is `HOME`'s or `XDG_CONFIG_HOME`'s, so the
    /// advice for one is no remedy for the other.
    pub fn fix(&self) -> String {
        match self {
            ResolveSide::State => {
                "set ATOMIC_HOME to a directory atomic can place, with a directory above it \
                 that exists and can be read; until then atomic cannot tell whether its state \
                 would be inside your own configuration directory"
                    .into()
            }
            // Not "missing": a name below the deepest directory that is there
            // is what placing folds without asking, and is no reason to refuse.
            // What cannot be placed is a directory no one may enter, a path
            // through something that is not a directory, a link that leads
            // nowhere, a link that leads round in a circle, and whatever else
            // the filesystem says when asked — see [`real_path`], which refuses
            // on every answer but "not there".
            ResolveSide::Config => {
                "fix HOME, or XDG_CONFIG_HOME, so that your configuration directory can be \
                 placed: it, or a directory above it, cannot be entered, passes through \
                 something that is not a directory, is a link that leads nowhere, or cannot \
                 otherwise be looked up; until then atomic cannot tell whether its state \
                 would be inside that directory"
                    .into()
            }
        }
    }
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
            scratch: home.join("scratch/test"),
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
            scratch: home.join("scratch").join(&id),
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
    ///
    /// A state directory that is the person's own configuration directory, or
    /// inside it, is refused here, before anything is created
    /// ([`outside_user_config`]) — and so is one whose path cannot be resolved,
    /// rather than run on a spelling that was never compared with the denial
    /// ([`StateError::Resolve`]).
    pub fn create(&self) -> Result<(), StateError> {
        outside_user_config(&self.dir)?;
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
        self.move_codex_records()?;
        self.retire_legacy_scratch()?;
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

    /// The project's id: its root's last component and a hash of its full
    /// path, which names its state directory. A store several projects
    /// share tells their records apart by `<manifest name>/<id>`, so the
    /// same project checked out at another path, or renamed, is another
    /// project there.
    pub fn id(&self) -> String {
        self.dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
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

    /// Where Codex keeps what it records of the project's threads, its state
    /// databases and logs: in the project's private directory, which every
    /// project's commands are denied, not in its Codex home.
    pub fn codex_records(&self) -> PathBuf {
        self.private.join("codex")
    }

    /// Hold the state directory's migration lock while `body` runs: one
    /// process migrates at a time. Taken and let go around each migration,
    /// never nested, since a second lock on the same file waits forever.
    fn migrating<T>(&self, body: impl FnOnce() -> Result<T, StateError>) -> Result<T, StateError> {
        let lock_path = self.dir.join("migrate.lock");
        let fail = |source| StateError::Create {
            path: lock_path.clone(),
            source,
        };
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(fail)?;
        lock.lock().map_err(fail)?;
        let out = body();
        drop(lock);
        out
    }

    /// Move what an older atomic let Codex record in the Codex home, its
    /// `*.sqlite` databases with their `-wal` and `-shm` files and its
    /// `log/`, into [`ProjectState::codex_records`], where no command can
    /// read it. A database moves as one unit, its sidecars first and its main
    /// file last, so no `-wal` is ever paired with another database's file.
    /// One already in the records keeps its place, and the old unit is
    /// removed whole. A database another process still has open is left,
    /// with a warning, to move once it is not. Under the migration lock; a
    /// file that is gone when its turn comes was moved by another process.
    fn move_codex_records(&self) -> Result<(), StateError> {
        let home = self.codex_home();
        let names: Vec<String> = match std::fs::read_dir(&home) {
            Ok(entries) => entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(StateError::Create { path: home, source }),
        };
        let mut bases: Vec<String> = names
            .iter()
            .filter_map(|n| {
                ["-wal", "-shm", ""]
                    .iter()
                    .find_map(|tail| n.strip_suffix(&format!(".sqlite{tail}")))
                    .map(str::to_owned)
            })
            .collect();
        bases.sort();
        bases.dedup();
        let has_log = names.iter().any(|n| n == "log");
        if bases.is_empty() && !has_log {
            return Ok(());
        }
        let records = self.codex_records();
        self.migrating(|| {
            private_dir(&records)?;
            for base in &bases {
                let main = home.join(format!("{base}.sqlite"));
                if database_in_use(&main) {
                    eprintln!(
                        "atomic: {} is in use, so it stays in the Codex home until it is not",
                        main.display()
                    );
                    continue;
                }
                let keep = records.join(format!("{base}.sqlite")).exists();
                // Sidecars first, the main file last.
                for tail in ["-wal", "-shm", ""] {
                    let name = format!("{base}.sqlite{tail}");
                    let (old, new) = (home.join(&name), records.join(&name));
                    if keep {
                        remove_record(&old)?;
                    } else {
                        move_record(&old, &new)?;
                    }
                }
            }
            if has_log {
                let (old, new) = (home.join("log"), records.join("log"));
                if new.exists() {
                    remove_record(&old)?;
                } else {
                    move_record(&old, &new)?;
                }
            }
            Ok(())
        })
    }

    /// The scratch roots an older atomic kept in the state directory,
    /// `<state>/scratch` and `<state>/scratch-debug`, while they are still
    /// there: denied to every backend's commands until they are gone.
    fn legacy_scratch_roots(&self) -> Vec<PathBuf> {
        [self.dir.join("scratch"), self.dir.join("scratch-debug")]
            .into_iter()
            // Only one surely not there is left out: one that cannot be
            // looked at is denied.
            .filter(|root| {
                !matches!(std::fs::symlink_metadata(root), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
            })
            .collect()
    }

    /// Retire the scratch roots an older atomic kept in the state directory:
    /// what attempts of processes that no longer run left there goes, and a
    /// root goes once nothing else is in it. One whose attempts may still
    /// run, as under an older atomic still running, stays, and stays denied.
    /// Under the migration lock.
    fn retire_legacy_scratch(&self) -> Result<(), StateError> {
        let roots = self.legacy_scratch_roots();
        if roots.is_empty() {
            return Ok(());
        }
        self.migrating(|| {
            for root in &roots {
                if let Err(e) = atomic_exec::retire_scratch_root(root) {
                    eprintln!(
                        "atomic: could not remove the old scratch directory {}: {e}",
                        root.display()
                    );
                }
            }
            Ok(())
        })
    }

    /// The scratch roots of the project's backends: `atomic run`, `atomic
    /// serve` and `atomic eval`'s, and that of `atomic debug codex-prompt` and
    /// `atomic debug probe`, which is apart because those commands take no
    /// execution lock. They lie in [`ProjectState::scratch_home`], beside
    /// every other project's.
    fn scratch_roots(&self) -> [PathBuf; 2] {
        [self.scratch.join("run"), self.scratch.join("debug")]
    }

    /// Where every project of the home keeps its backends' scratch roots,
    /// `<home>/scratch/`: denied whole, by one entry however many projects
    /// there are, to every backend's commands, their own attempt's directory
    /// apart, so no turn or command reads another attempt's, of this project
    /// or another.
    pub fn scratch_home(&self) -> PathBuf {
        self.scratch
            .parent()
            .map(Path::to_owned)
            .unwrap_or_else(|| self.scratch.clone())
    }

    /// The local backend `atomic run`, `atomic serve` and `atomic eval` run
    /// atoms on.
    pub fn run_backend(&self) -> atomic_exec::LocalBackend {
        let [run, _] = self.scratch_roots();
        atomic_exec::LocalBackend::in_dir(run).denying(self.denied_scratch())
    }

    /// What every backend of the project denies its commands: the home's
    /// whole `scratch/`, and the scratch roots an older atomic kept in the
    /// state directory, while they are still there.
    fn denied_scratch(&self) -> Vec<PathBuf> {
        let mut denied = vec![self.scratch_home()];
        denied.extend(self.legacy_scratch_roots());
        denied
    }

    /// The local backend `atomic debug codex-prompt` and `atomic debug probe`
    /// run their turn on, beside the one runs use.
    pub fn debug_backend(&self) -> atomic_exec::LocalBackend {
        let [_, debug] = self.scratch_roots();
        atomic_exec::LocalBackend::in_dir(debug).denying(self.denied_scratch())
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

/// Remove a record a Codex home kept, a file or the `log/` directory; one
/// already gone is no error.
fn remove_record(path: &Path) -> Result<(), StateError> {
    let gone = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match gone {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(StateError::Create {
            path: path.to_owned(),
            source: e,
        }),
        _ => Ok(()),
    }
}

/// Move a record a Codex home kept to `new`: renamed, or, where `new` is on
/// another filesystem, copied and then removed. One already gone is no
/// error: another process moved it.
fn move_record(old: &Path, new: &Path) -> Result<(), StateError> {
    let fail = |source| StateError::Create {
        path: new.to_owned(),
        source,
    };
    match std::fs::rename(old, new) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            copy_record(old, new).map_err(fail)?;
            remove_record(old)
        }
        Err(e) => Err(fail(e)),
    }
}

/// Copy a record, a file or a directory of files, to `new`.
fn copy_record(old: &Path, new: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(old)?.is_dir() {
        std::fs::create_dir_all(new)?;
        for entry in std::fs::read_dir(old)? {
            let entry = entry?;
            copy_record(&entry.path(), &new.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(old, new).map(|_| ())
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

/// `path` as it really is: its nearest existing ancestor with every link
/// resolved, and the rest of it as written. A directory not made yet is
/// where making it would put it, a linked parent followed.
pub fn as_it_really_is(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(at) {
            return rest.iter().rev().fold(real, |p, part| p.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                at = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

/// Every checkout holding `dir`, a path as it really is: each directory
/// around it with a `.git`, and for a linked worktree the checkout whose
/// repository it shares, which its `.git` file names through the
/// repository's `commondir`. Of this user's own `.git`, a `.git` or
/// `commondir` that is not a small regular file, as git writes them, is
/// refused rather than read: a pipe or a device there would hang or fill
/// memory. Another user's is read through no link and refuses nothing (see
/// [`checkouts_holding_as`]).
pub fn checkouts_holding(dir: &Path) -> Result<Vec<PathBuf>, String> {
    #[cfg(unix)]
    let me = Some(effective_uid());
    #[cfg(not(unix))]
    let me = None;
    checkouts_holding_as(dir, me)
}

/// [`checkouts_holding`], for the user `me` (`None` where files have no
/// owner to compare).
///
/// A `.git` that is there, or that cannot be looked at, makes its directory
/// a checkout. What a `.git` links to may refuse the project only when it is
/// this user's own and no hard link another user could have made to a file
/// of theirs. Any other `.git`, such as one planted in `/tmp` above every
/// project there, is still followed when it is a regular file that reads as
/// a worktree's link, so a real worktree written by another user (under
/// `sudo`, or seen through a user namespace) still holds the project in its
/// main checkout; but nothing wrong with it refuses anything. The trade-off:
/// a well-formed plant that names a directory with a `.git` makes that
/// directory count as a checkout too, refusing a sign-in or a configuration
/// kept there.
fn checkouts_holding_as(dir: &Path, me: Option<u32>) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    for checkout in dir.ancestors().filter(|d| present(&d.join(".git"))) {
        found.push(checkout.to_owned());
        let git = checkout.join(".git");
        let main = if answerable(&git, me) {
            main_checkout(&git, true)?
        } else if std::fs::symlink_metadata(&git).is_ok_and(|m| m.is_file()) {
            // Opened through no link at all, the `.git` and its
            // `commondir` alike: what another user's names is never opened,
            // a device or a pipe included.
            main_checkout(&git, false).unwrap_or(None)
        } else {
            None
        };
        if let Some(main) = main
            && !found.contains(&main)
        {
            found.push(main);
        }
    }
    Ok(found)
}

/// Whether there is something at `path`: anything but a lookup that finds
/// nothing, so one this user cannot make counts.
fn present(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ),
    }
}

/// Whether what the `.git` at `git` says may refuse the project: it is
/// `me`'s own, and not a file with other hard links, which another user
/// could have made to a file of `me`'s.
fn answerable(git: &Path, me: Option<u32>) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        owned_by(git, me)
            && std::fs::symlink_metadata(git)
                .is_ok_and(|meta| !(meta.is_file() && meta.nlink() > 1))
    }
    #[cfg(not(unix))]
    owned_by(git, me)
}

/// Whether `path` itself, not what a link there leads to, is owned by `me`;
/// with no owner to compare, it counts as owned.
fn owned_by(path: &Path, me: Option<u32>) -> bool {
    #[cfg(unix)]
    if let Some(me) = me {
        use std::os::unix::fs::MetadataExt;
        return std::fs::symlink_metadata(path).is_ok_and(|m| m.uid() == me);
    }
    let _ = (path, me);
    true
}

/// The most a `.git` file or a `commondir` is read for: a path or two.
const LINK_FILE_MAX: u64 = 4096;

/// How a link file is opened: without waiting on a pipe's writer, and
/// never as the process's controlling terminal.
#[cfg(unix)]
const LINK_OPEN_FLAGS: i32 = NOT_WAITING | libc::O_NOCTTY;

/// What makes an open of a link file not wait on a pipe's writer.
#[cfg(unix)]
const NOT_WAITING: i32 = libc::O_NONBLOCK;

/// What makes an open of another user's link file refuse a link where it
/// ends, so it opens nothing that link names.
#[cfg(unix)]
const NOT_THROUGH_A_LINK: i32 = libc::O_NOFOLLOW;

/// The main checkout of the linked worktree whose `.git` is `git`; `None`
/// for a `.git` directory, a submodule's file (no `commondir`), or a bare
/// repository.
fn main_checkout(git: &Path, through_links: bool) -> Result<Option<PathBuf>, String> {
    if std::fs::metadata(git).is_ok_and(|m| m.is_dir()) {
        return Ok(None);
    }
    let Some(text) = link_file(git, through_links)? else {
        return Ok(None);
    };
    let Some(gitdir) = text.lines().find_map(|l| l.strip_prefix("gitdir:")) else {
        return Ok(None);
    };
    let Some(parent) = git.parent() else {
        return Ok(None);
    };
    let gitdir = parent.join(gitdir.trim());
    let Some(common) = link_file(&gitdir.join("commondir"), through_links)? else {
        return Ok(None);
    };
    let Ok(common) = std::fs::canonicalize(gitdir.join(common.trim())) else {
        return Ok(None);
    };
    if common.file_name().is_none_or(|n| n != ".git") {
        return Ok(None);
    }
    Ok(common.parent().map(Path::to_owned))
}

/// The text of `path`, a small regular file, or `None` when there is none:
/// opened without blocking, so a pipe there cannot hang the read, and
/// refused when it is anything but a regular file of at most
/// [`LINK_FILE_MAX`] bytes.
fn link_file(path: &Path, through_links: bool) -> Result<Option<String>, String> {
    use std::io::Read;
    let refused = |why: &str| {
        Err(format!(
            "{} {why}, which git does not write there; remove it or fix the checkout",
            path.display()
        ))
    };
    let mut open = std::fs::OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut open,
        if through_links {
            LINK_OPEN_FLAGS
        } else {
            LINK_OPEN_FLAGS | NOT_THROUGH_A_LINK
        },
    );
    #[cfg(not(unix))]
    let _ = through_links;
    let file = match open.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    match file.metadata() {
        Ok(m) if !m.is_file() => return refused("is not a regular file"),
        Ok(m) if m.len() > LINK_FILE_MAX => return refused("is larger than a link file"),
        Ok(_) => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let mut text = String::new();
    file.take(LINK_FILE_MAX + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if text.len() as u64 > LINK_FILE_MAX {
        return refused("is larger than a link file");
    }
    Ok(Some(text))
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
/// supplies: `$XDG_CONFIG_HOME/atomic`, else `~/.config/atomic` under `HOME`,
/// else under `passwd`, the home the password database gives — the home `dirs`
/// takes to the same place when the environment names none, and the one
/// [`crate::codex::real_homes`] denies a person's secrets under. A relative
/// `XDG_CONFIG_HOME` is no configuration directory and gives way to `HOME`,
/// as the XDG rule says; a relative `HOME` gives way to `passwd`. `None` only
/// when none of the three names an absolute directory.
///
/// Both the denial of this directory to atoms' commands
/// ([`crate::codex::StateGuard::deny_state_dir`]) and the refusal of a state
/// directory inside it ([`outside_user_config`]) come from [`user_config_dir`],
/// so a directory no atom's command can reach is never taken for a place the
/// state may live.
pub fn user_config_dir_in(
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    passwd: &Option<PathBuf>,
) -> Option<PathBuf> {
    let absolute = |name: &str| env(name).map(PathBuf::from).filter(|p| p.is_absolute());
    absolute("XDG_CONFIG_HOME")
        .or_else(|| absolute("HOME").map(|h| h.join(".config")))
        .or_else(|| passwd_config(passwd))
        .map(|d| d.join("atomic"))
}

/// Where the configuration of a person with no absolute home in the
/// environment lives: under `passwd`, the home the password database gives,
/// when it is an absolute path. This is where `dirs` takes the configuration
/// directory when the environment names none, and where
/// [`crate::codex::real_homes`] denies a person's secrets, so it is a
/// configuration directory for the denial and for the state check alike.
fn passwd_config(passwd: &Option<PathBuf>) -> Option<PathBuf> {
    let home = passwd.as_deref()?;
    if !home.is_absolute() {
        return None;
    }
    Some(home.join(".config"))
}

/// [`user_config_dir_in`] with this process's `XDG_CONFIG_HOME` and `HOME`,
/// read as the operating system gives them, and the home the password
/// database gives this process's user.
pub fn user_config_dir() -> Option<PathBuf> {
    user_config_dir_in(&process_env, &process_passwd_home())
}

// What [`process_env`] and [`process_passwd_home`] answer: this process's own
// environment and password database, or, in a test run of this crate only, what
// a test has said they are for its own thread — see [`as_process`], which says
// it, and [`TestProcessGuard`], which puts this process's own back.
//
// The entry point above reads the environment and the password database
// itself, so without this their *wiring* — that they ask the password database
// for a home when the environment names none — could be tested only against
// this machine's user and this process's real environment, which a test must
// not depend on and must not change under the other tests running beside it.
// A build that is not this crate's own tests compiles the override away:
// nothing outside those tests can set it, and no variable, file or atom can.

/// A variable of this process's environment, as the operating system gives it;
/// for the thread a test runs on, what `as_process` said this thread's
/// environment holds instead.
fn process_env(name: &str) -> Option<std::ffi::OsString> {
    #[cfg(test)]
    if let Some(said) = PROCESS.with(|p| {
        p.borrow()
            .as_ref()
            .map(|process| process.env.get(name).cloned())
    }) {
        return said;
    }
    std::env::var_os(name)
}

/// The home this process's user has in the password database; for the thread a
/// test runs on, what `as_process` said the password database gives instead.
fn process_passwd_home() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(said) = PROCESS.with(|p| p.borrow().as_ref().map(|process| process.passwd.clone()))
    {
        return said;
    }
    passwd_home()
}

#[cfg(test)]
thread_local! {
    static PROCESS: std::cell::RefCell<Option<TestProcess>> = const {
        std::cell::RefCell::new(None)
    };
}

/// What a test says this process's environment and password database are.
#[cfg(test)]
struct TestProcess {
    env: std::collections::BTreeMap<String, std::ffi::OsString>,
    passwd: Option<PathBuf>,
}

/// Say, for this test thread, that the environment holds exactly `vars` — a
/// `name` whose value starting with `/` names a directory below `scratch` —
/// and that the password database gives `passwd`, below `scratch` with a
/// leading `/`. The answer comes back as [`process_env`] and
/// [`process_passwd_home`] until the returned guard is dropped, which puts
/// this process's own back however the thread ends.
#[cfg(test)]
fn as_process(scratch: &Path, vars: &[(&str, &str)], passwd: Option<&str>) -> TestProcessGuard {
    let below = |value: &str| match value.strip_prefix('/') {
        Some(rest) => scratch.join(rest).into_os_string(),
        None => std::ffi::OsString::from(value),
    };
    PROCESS.with(|p| {
        *p.borrow_mut() = Some(TestProcess {
            env: vars
                .iter()
                .map(|(name, value)| ((*name).to_owned(), below(value)))
                .collect(),
            passwd: passwd.map(|home| scratch.join(home)),
        });
    });
    TestProcessGuard(())
}

/// Puts this process's own environment and password database back for its
/// thread; see [`as_process`].
#[cfg(test)]
struct TestProcessGuard(());

#[cfg(test)]
impl Drop for TestProcessGuard {
    fn drop(&mut self) {
        PROCESS.with(|p| *p.borrow_mut() = None);
    }
}

/// `path`, absolute and with the directories that already exist in it
/// resolved: a symbolic link among them is followed to where it really
/// points. The components below the last of those cannot be links yet, so
/// they are folded without asking the filesystem: `.` goes, and each `..`
/// pops the name before it, which is where the operating system would land
/// once those names do exist.
///
/// `absolute` makes a relative path absolute, which is how a home named
/// relative to the directory the command was run in reaches the comparison.
/// Nothing comes back for a path that cannot be made absolute, or whose deepest
/// existing directory cannot be looked up, or because a directory above that
/// one cannot be looked up at all: a directory nobody may look in is not a
/// directory that is missing, and the walk must not read it as one. Nothing
/// else must come back either: the unresolved spelling would compare as a
/// stranger to the configuration directory and let the state through, into the
/// place no atom's command can reach. [`StateError::Resolve`] names the side
/// that could not be placed, and the run is refused.
fn real_path(
    side: ResolveSide,
    path: &Path,
    absolute: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Result<PathBuf, StateError> {
    let unresolved = |why: String| StateError::Resolve {
        side,
        path: path.to_owned(),
        why,
    };
    let absolute = absolute(path).ok_or_else(|| {
        unresolved("it is not an absolute path and the directory to read it from is unknown".into())
    })?;
    // The deepest prefix of `absolute` that exists. A directory is there only
    // where its parent is, so below that prefix every component is missing
    // too, and none of them can be a link. Only `NotFound` says a prefix is
    // missing: a prefix that cannot be looked up at all — a directory nobody
    // may enter, a failing device — says nothing about what is in it, and
    // walking on as if it were empty would place what is below it by its
    // spelling, which is the one thing this must not do.
    let mut found = None;
    for prefix in absolute.ancestors() {
        match std::fs::symlink_metadata(prefix) {
            Ok(_) => {
                found = Some(prefix);
                break;
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(unresolved(format!(
                    "{} cannot be looked up: {source}",
                    prefix.display()
                )));
            }
        }
    }
    let (found, below) = match found {
        // Not even the root of `absolute` is there, which an absolute path
        // always is. Nothing is known about where this leads.
        None => {
            return Err(unresolved(format!(
                "{} is not there, and neither is any directory above it",
                absolute.display()
            )));
        }
        // `found` is `absolute`'s own spelling cut at a separator, so this
        // gives what comes after it, links and all, unchanged.
        Some(found) => match absolute.strip_prefix(found) {
            Ok(below) => (found, below),
            Err(_) => {
                return Err(unresolved(format!(
                    "{} is above {}, which does not contain it",
                    found.display(),
                    absolute.display()
                )));
            }
        },
    };
    let mut out = std::fs::canonicalize(found).map_err(|source| {
        unresolved(if found == absolute.as_path() {
            format!("it cannot be read: {source}")
        } else {
            format!("{} cannot be read: {source}", found.display())
        })
    })?;
    for part in below.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

/// Refuse a state directory that is [`user_config_dir`], or sits inside it.
/// Every atom's commands are denied the person's own configuration directory
/// whole (see [`crate::codex::StateGuard`]), and a denied path inside another
/// denied path is dropped, so a state directory there is denied with it: its
/// worktrees and caches fall out of reach of the commands that must use them,
/// which fail saying nothing of where the cause is. Both sides are placed
/// with [`real_path`] first — absolute, links followed, `..` folded — and
/// compared by path components, so a link into the configuration directory
/// and a `..` that leads into it are caught, and a sibling of it, such as
/// `~/.config/atomic-state`, is not.
///
/// `env` is how the person's `ATOMIC_HOME` and `XDG_CONFIG_HOME` and `HOME`
/// are read, `passwd` is the home the password database gives, and `absolute`
/// makes a relative path absolute.
///
/// The configuration directory compared against is [`user_config_dir_in`] as
/// `env` and `passwd` give it — the same resolution
/// [`crate::codex::StateGuard::deny_state_dir`] denies every atom's command
/// from, so the two cannot name different directories. When none of them names
/// an absolute directory there is no configuration directory for a state to be
/// swallowed by and the denial covers nothing either, so this returns `Ok` and
/// [`ProjectState::create`] makes the state. Nothing runs on such settings
/// either: the same missing homes are why [`crate::codex::real_homes`] says it
/// knows no home, and with it [`crate::codex::StateGuard::deny_state_dir`]
/// refuses to build the denial every atom's command needs. A state directory
/// that cannot be placed is refused rather than compared as a stranger, and so
/// is one with a directory above it that cannot be looked up — see
/// [`real_path`].
pub fn outside_user_config(state_dir: &Path) -> Result<(), StateError> {
    outside_user_config_in(state_dir, &process_env, &process_passwd_home(), &placed)
}

/// Where the path `path` is read from, which is what a relative spelling
/// needs before it can be compared with anything: [`std::path::absolute`],
/// which joins the working directory without folding the names that are not
/// there yet. `None` when the working directory cannot be read.
///
/// Most homes a command brings to [`outside_user_config`] are absolute before
/// they get here: [`atomic_home`] reads a relative `ATOMIC_HOME` from the
/// working directory itself, and the configuration directory is kept only when
/// `XDG_CONFIG_HOME`, `HOME` or the password database names an absolute one.
/// The default data directory is absolute too, except in one case: the platform
/// rule [`base_dir`] falls back to — `dirs::data_dir` — takes a `HOME` that is
/// not absolute as the home it gives, so a person who sets a relative `HOME`,
/// sets no `ATOMIC_HOME` and sets no `XDG_DATA_HOME` gets
/// `<relative HOME>/.local/share/atomic`, which is relative and comes here as it
/// stands, read from the working directory like any other relative spelling. So
/// this is the backstop for a relative
/// `ATOMIC_HOME` and the live path for that one — and a backstop that must
/// still work, because a spelling compared unresolved is the one thing that
/// lets a state into the denied directory. The tests that keep it living are
/// `a_relative_state_directory_is_placed_from_the_working_directory`, which asks
/// [`outside_user_config`] itself, with the spelling relative, and
/// `a_relative_home_gives_a_state_directory_placed_from_the_working_directory`
/// in `crates/atomic/tests/state_cli.rs`, which is what a command does with a
/// relative `HOME` and no `ATOMIC_HOME`.
fn placed(path: &Path) -> Option<PathBuf> {
    std::path::absolute(path).ok()
}

/// [`outside_user_config`] with the environment as `env` reads it, the
/// password home as `passwd` gives it and relative paths as `absolute` makes
/// them absolute.
pub fn outside_user_config_in(
    state_dir: &Path,
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    passwd: &Option<PathBuf>,
    absolute: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Result<(), StateError> {
    let Some(config) = user_config_dir_in(env, passwd) else {
        return Ok(());
    };
    let (state, config) = (
        real_path(ResolveSide::State, state_dir, absolute)?,
        real_path(ResolveSide::Config, &config, absolute)?,
    );
    if state.starts_with(&config) {
        // The person's own spelling, word for word, so the message names what
        // they set; the state directory beside it says where it leads. Nothing
        // sets it when it is unset or empty, and then the default data
        // directory is what leads there.
        let given = env("ATOMIC_HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from);
        return Err(StateError::InsideConfig(OutsideConfig {
            state,
            given,
            config,
        }));
    }
    Ok(())
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

/// The name the password database gives `uid`; `None` when it has no entry
/// or the name is not UTF-8.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn passwd_name(uid: u32) -> Option<String> {
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: an all-zero `passwd` is a valid value of the plain C struct.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes only into `entry` and `buffer`, both of the
    // sizes given and alive for the call, and sets `found` to null or to
    // `entry`.
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if code != 0 || found.is_null() || entry.pw_name.is_null() {
        return None;
    }
    // SAFETY: on success `pw_name` points at a NUL-terminated string inside
    // `buffer`, which is still alive.
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
    name.to_str().ok().map(str::to_owned)
}

/// Make sure `dir` is readable by its owner only: create it so if it is
/// missing, and close it if it is open to others. A directory already closed
/// is left as it is, so one mounted read-only, such as a Kubernetes secret,
/// still works. A symbolic link is never followed to change what it points
/// to: it is accepted only when its target is a directory the user owns and
/// no one else can enter.
pub(crate) fn private_dir(dir: &Path) -> Result<(), StateError> {
    if existing_private_dir(dir)? {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(|source| StateError::Create {
        path: dir.to_owned(),
        source,
    })
}

/// [`private_dir`]'s checks on `dir` where it is, made private as they
/// make it, without making it: whether it is there. Not there, nothing is
/// made.
pub(crate) fn existing_private_dir(dir: &Path) -> Result<bool, StateError> {
    let fail = |source| StateError::Create {
        path: dir.to_owned(),
        source,
    };
    let refuse = |why: String| StateError::Unsafe {
        path: dir.to_owned(),
        why,
    };
    let meta = match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(fail(e)),
        Ok(meta) => meta,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.file_type().is_symlink() {
            let target = std::fs::metadata(dir).map_err(fail)?;
            if target.is_dir() && target.uid() == effective_uid() && target.mode() & 0o077 == 0 {
                return Ok(true);
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
    Ok(true)
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

    /// What an older atomic let Codex record in the project's Codex home, its
    /// databases and logs, moves into the project's private directory when
    /// the state is created; what a turn's shell needs there stays.
    #[test]
    fn codex_records_move_out_of_the_codex_home() {
        let home =
            std::env::temp_dir().join(format!("atomic-codex-records-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let root = home.join("project");
        std::fs::create_dir_all(&root).expect("project");
        let state = ProjectState::under(&home, &root);
        let codex = state.codex_home();
        std::fs::create_dir_all(codex.join("log")).expect("log");
        std::fs::create_dir_all(codex.join("tmp/arg0-linux")).expect("arg0");
        for name in [
            "state_5.sqlite",
            "state_5.sqlite-wal",
            "logs_2.sqlite",
            "log/codex.log",
        ] {
            std::fs::write(codex.join(name), "records").expect("record");
        }
        state.create().expect("created");
        let records = state.codex_records();
        assert!(records.starts_with(state.private_dir()));
        for name in [
            "state_5.sqlite",
            "state_5.sqlite-wal",
            "logs_2.sqlite",
            "log/codex.log",
        ] {
            assert!(
                !codex.join(name).exists(),
                "{name} is still in the Codex home"
            );
            assert!(records.join(name).exists(), "{name} did not move");
        }
        assert!(
            codex.join("tmp/arg0-linux").is_dir(),
            "the helper links went too"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The scratch roots an older atomic kept in the state directory are
    /// denied to every backend's commands while they are there, and go when
    /// the state is next created, once what is in them belonged to
    /// processes that no longer run. One holding a live attempt stays, and
    /// stays denied.
    #[test]
    fn old_scratch_roots_are_denied_until_they_are_retired() {
        use atomic_exec::ExecutionBackend;
        let home = std::env::temp_dir().join(format!("atomic-old-scratch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let root = home.join("project");
        std::fs::create_dir_all(&root).expect("project");
        let state = ProjectState::under(&home, &root);
        let (old, old_debug) = (
            state.dir().join("scratch"),
            state.dir().join("scratch-debug"),
        );
        // A dead turn's HOME, with what it kept there.
        let dead = old.join("fix-0123/999999999.1-a0");
        std::fs::create_dir_all(&dead).expect("dead attempt");
        std::fs::write(dead.join(".bash_history"), "a secret typed in a turn").expect("secret");
        // One whose owner may still run: this process, which is alive.
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
        let start = stat
            .rsplit_once(')')
            .and_then(|(_, after)| after.split_whitespace().nth(19))
            .unwrap_or("0")
            .to_owned();
        let live = old_debug.join(format!("check-4567/{}.{start}-a1", std::process::id()));
        std::fs::create_dir_all(&live).expect("live attempt");
        for backend in [state.run_backend(), state.debug_backend()] {
            let workspace = backend
                .open(&atomic_exec::Open {
                    label: "t".into(),
                    run: "r".into(),
                    step: "s".into(),
                    attempt: "a1".into(),
                    source: Some(root.clone()),
                    ..Default::default()
                })
                .expect("opens");
            for dir in [&old, &old_debug] {
                assert!(
                    workspace.denied.contains(dir),
                    "{} is not denied: {:?}",
                    dir.display(),
                    workspace.denied
                );
            }
            backend.close(&workspace).expect("closes");
        }
        state.create().expect("created");
        assert!(!old.exists(), "the dead attempts' old root stayed");
        assert!(live.exists(), "a live attempt's scratch went");
        let backend = state.run_backend();
        let workspace = backend
            .open(&atomic_exec::Open {
                label: "t".into(),
                run: "r".into(),
                step: "s".into(),
                attempt: "a2".into(),
                source: Some(root.clone()),
                ..Default::default()
            })
            .expect("opens");
        assert!(
            workspace.denied.contains(&old_debug),
            "{:?}",
            workspace.denied
        );
        assert!(!workspace.denied.contains(&old), "{:?}", workspace.denied);
        backend.close(&workspace).expect("closes");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A Codex database another process still has open stays in the Codex
    /// home, and the state is created all the same; it moves once closed.
    #[test]
    fn a_codex_database_in_use_moves_only_once_closed() {
        let home = std::env::temp_dir().join(format!("atomic-codex-in-use-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let root = home.join("project");
        std::fs::create_dir_all(&root).expect("project");
        let state = ProjectState::under(&home, &root);
        let codex = state.codex_home();
        std::fs::create_dir_all(&codex).expect("codex home");
        let db = codex.join("state_5.sqlite");
        let mut holder = std::process::Command::new("python3")
            .args([
                "-c",
                "import sqlite3, sys, time\n\
                 c = sqlite3.connect(sys.argv[1]); c.execute('PRAGMA journal_mode=WAL')\n\
                 c.execute('CREATE TABLE IF NOT EXISTS t (x)'); c.execute('INSERT INTO t VALUES (1)'); c.commit()\n\
                 print('open', flush=True); time.sleep(60)",
            ])
            .arg(&db)
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
        state.create().expect("created while the database is open");
        assert!(db.exists(), "an open database was moved");
        holder.kill().expect("stop the holder");
        holder.wait().expect("reap");
        state.create().expect("created");
        assert!(!db.exists(), "the closed database stayed");
        assert!(state.codex_records().join("state_5.sqlite").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A Codex database moves as one unit: where the records already hold
    /// one by that name, the old one and its sidecars are removed whole and
    /// none of them is paired with the kept file. Two starts at once both
    /// succeed, and each record moves once.
    #[test]
    fn codex_databases_move_whole_and_two_starts_agree() {
        let home = std::env::temp_dir().join(format!("atomic-codex-units-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let root = home.join("project");
        std::fs::create_dir_all(&root).expect("project");
        let state = ProjectState::under(&home, &root);
        let codex = state.codex_home();
        std::fs::create_dir_all(&codex).expect("codex home");
        let records = state.codex_records();
        std::fs::create_dir_all(&records).expect("records");
        std::fs::write(records.join("state_5.sqlite"), "kept").expect("kept");
        std::fs::write(codex.join("state_5.sqlite"), "old").expect("old");
        std::fs::write(codex.join("state_5.sqlite-wal"), "old wal").expect("old wal");
        std::fs::write(codex.join("logs_2.sqlite"), "logs").expect("logs");
        std::fs::write(codex.join("logs_2.sqlite-wal"), "logs wal").expect("logs wal");
        let starts: Vec<_> = (0..2)
            .map(|_| {
                let state = state.clone();
                std::thread::spawn(move || state.create())
            })
            .collect();
        for start in starts {
            start.join().expect("thread").expect("both starts succeed");
        }
        assert_eq!(
            std::fs::read_to_string(records.join("state_5.sqlite")).expect("kept"),
            "kept"
        );
        assert!(
            !records.join("state_5.sqlite-wal").exists(),
            "an old -wal was paired with the kept database"
        );
        assert!(
            !codex.join("state_5.sqlite").exists() && !codex.join("state_5.sqlite-wal").exists()
        );
        assert_eq!(
            std::fs::read_to_string(records.join("logs_2.sqlite")).expect("logs"),
            "logs"
        );
        assert_eq!(
            std::fs::read_to_string(records.join("logs_2.sqlite-wal")).expect("logs wal"),
            "logs wal"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// No backend's commands may read another's attempts, of this project or
    /// another: each backend of the project keeps its scratch root in the
    /// home's `scratch/`, beside every other project's, and denies its
    /// workspaces that directory whole, by one entry, which it makes if
    /// missing; its own attempt's directory is apart.
    #[test]
    fn every_backend_denies_every_projects_scratch() {
        use atomic_exec::ExecutionBackend;
        let home =
            std::env::temp_dir().join(format!("atomic-scratch-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let root = home.join("project");
        std::fs::create_dir_all(&root).expect("project");
        let state = ProjectState::under(&home, &root);
        let all = home.join("scratch");
        assert_eq!(state.scratch_home(), all);
        let roots = [all.join("test/run"), all.join("test/debug")];
        // Another project's, which no backend of this one may read.
        let other = all.join("other/run");
        for (what, backend, own) in [
            ("run", state.run_backend(), &roots[0]),
            ("debug", state.debug_backend(), &roots[1]),
        ] {
            let workspace = backend
                .open(&atomic_exec::Open {
                    label: what.into(),
                    run: "r".into(),
                    step: "s".into(),
                    attempt: "a1".into(),
                    source: Some(root.clone()),
                    ..Default::default()
                })
                .expect("opens");
            let scratch = workspace.scratch.clone().expect("scratch");
            assert!(scratch.starts_with(own), "{what}: {}", scratch.display());
            assert!(
                workspace.denied.contains(&all),
                "{what}: {:?}",
                workspace.denied
            );
            for dir in roots.iter().chain([&other]) {
                assert!(
                    workspace.denied.iter().any(|d| dir.starts_with(d)),
                    "{what}: {} is not denied: {:?}",
                    dir.display(),
                    workspace.denied
                );
            }
            assert!(all.is_dir(), "{what}: {} was not made", all.display());
            backend.close(&workspace).expect("closes");
            let _ = std::fs::remove_dir_all(&all);
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A `.git` another user owns above a project, here a link to a device,
    /// counts its directory as a checkout and refuses nothing: it is not a
    /// regular file, so it is not read. The same link, this user's own, is
    /// read, and refused.
    #[cfg(unix)]
    #[test]
    fn another_users_git_above_a_project_is_a_checkout_and_refuses_nothing() {
        let dir = std::env::temp_dir().join(format!("atomic-foreign-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project")).expect("project");
        std::os::unix::fs::symlink("/dev/zero", dir.join(".git")).expect(".git");
        let project = dir.join("project");
        let me = effective_uid();
        let held = checkouts_holding_as(&project, Some(me.wrapping_add(1))).expect("not read");
        assert!(held.contains(&dir), "{held:?}");
        let refused = checkouts_holding_as(&project, Some(me)).expect_err("read, and refused");
        assert!(refused.contains("is not a regular file"), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Another user's `.git` that is a worktree's link, well formed, is
    /// still followed to its main checkout, which then holds the project;
    /// one of theirs that is malformed refuses nothing.
    #[cfg(unix)]
    #[test]
    fn another_users_worktree_link_is_followed_and_a_malformed_one_ignored() {
        let dir = std::env::temp_dir().join(format!("atomic-foreign-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let main = dir.join("main");
        let gitdir = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&gitdir).expect("main");
        std::fs::write(gitdir.join("commondir"), "../..\n").expect("commondir");
        std::fs::create_dir_all(dir.join("wt/project")).expect("worktree");
        std::fs::write(
            dir.join("wt/.git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .expect(".git");
        let project = dir.join("wt/project");
        let other = Some(effective_uid().wrapping_add(1));
        let held = checkouts_holding_as(&project, other).expect("read");
        assert!(
            held.contains(&std::fs::canonicalize(&main).expect("main")),
            "another user's worktree lost its main checkout: {held:?}"
        );
        std::fs::write(gitdir.join("commondir"), "x".repeat(5000)).expect("commondir");
        let held = checkouts_holding_as(&project, other).expect("refuses nothing");
        assert!(held.contains(&dir.join("wt")), "{held:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.git` that is a hard link to a file of this user's, which another
    /// user could make where links are not protected, refuses nothing, and
    /// one this user cannot look at still counts as a checkout.
    #[cfg(unix)]
    #[test]
    fn a_hard_linked_git_refuses_nothing_and_one_out_of_reach_still_counts() {
        let dir = std::env::temp_dir().join(format!("atomic-linked-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project")).expect("project");
        std::fs::write(dir.join("mine"), "x".repeat(5000)).expect("a file of mine");
        std::fs::hard_link(dir.join("mine"), dir.join(".git")).expect("hard link");
        let me = Some(effective_uid());
        let held = checkouts_holding_as(&dir.join("project"), me).expect("refuses nothing");
        assert!(held.contains(&dir), "{held:?}");
        // Out of reach: a directory this user cannot look in.
        let closed = dir.join("closed");
        std::fs::create_dir_all(closed.join("inner/project")).expect("closed");
        std::fs::create_dir_all(closed.join("inner/.git")).expect(".git");
        let mode = |m: u32| {
            std::fs::set_permissions(
                closed.join("inner"),
                std::os::unix::fs::PermissionsExt::from_mode(m),
            )
            .expect("mode")
        };
        mode(0o000);
        // Opened again however the test ends.
        struct Reopened(PathBuf);
        impl Drop for Reopened {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(
                    &self.0,
                    std::os::unix::fs::PermissionsExt::from_mode(0o700),
                );
            }
        }
        let _reopened = Reopened(closed.join("inner"));
        assert!(
            std::fs::read_dir(closed.join("inner")).is_err(),
            "mode 000 did not keep this user out (root?): run the test as a user"
        );
        let held =
            checkouts_holding_as(&closed.join("inner/project"), me).expect("refuses nothing");
        assert!(
            held.contains(&closed.join("inner")),
            "taken for absent: {held:?}"
        );
        drop(_reopened);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Another user's worktree link whose `commondir` is a link to a pipe
    /// opens nothing the link names: the open is refused at the link, so the
    /// pipe, or a device in its place, is never opened. This user's own is
    /// opened, and refused as not a regular file.
    #[cfg(unix)]
    #[test]
    fn another_users_link_file_is_opened_through_no_link() {
        let dir = std::env::temp_dir().join(format!("atomic-foreign-fifo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let gitdir = dir.join("att/wt");
        std::fs::create_dir_all(&gitdir).expect("gitdir");
        let fifo = dir.join("pipe");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("path");
        // SAFETY: `name` is a NUL-terminated path that outlives the call.
        #[allow(unsafe_code)]
        let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo");
        std::os::unix::fs::symlink(&fifo, gitdir.join("commondir")).expect("commondir");
        let foreign = link_file(&gitdir.join("commondir"), false).expect_err("refused");
        assert!(
            foreign.contains(&format!("os error {}", libc::ELOOP)),
            "the link was followed: {foreign}"
        );
        let own = link_file(&gitdir.join("commondir"), true).expect_err("refused");
        assert!(own.contains("is not a regular file"), "{own}");
        std::fs::create_dir_all(dir.join("project")).expect("project");
        std::fs::write(dir.join(".git"), format!("gitdir: {}\n", gitdir.display())).expect(".git");
        let held =
            checkouts_holding_as(&dir.join("project"), Some(effective_uid().wrapping_add(1)))
                .expect("refuses nothing");
        assert!(held.contains(&dir), "{held:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A worktree's `.git` or its repository's `commondir` that is a device,
    /// or larger than git writes, is refused without being read to its end,
    /// and says which file; a well-formed pair names the main checkout.
    #[cfg(unix)]
    #[test]
    fn a_link_file_that_is_not_a_small_regular_file_is_refused() {
        let dir = std::env::temp_dir().join(format!("atomic-link-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let main = dir.join("main");
        let gitdir = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&gitdir).expect("main");
        std::fs::create_dir_all(dir.join("wt/project")).expect("worktree");
        std::fs::write(
            dir.join("wt/.git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .expect(".git");
        let project = dir.join("wt/project");
        let commondir = gitdir.join("commondir");
        std::fs::write(&commondir, "../..\n").expect("commondir");
        let held = checkouts_holding(&project).expect("read");
        assert!(
            held.contains(&std::fs::canonicalize(&main).expect("main")),
            "{held:?}"
        );

        // A pipe with no writer, which a blocking open would wait on for
        // ever: refused, and in time.
        std::fs::remove_file(&commondir).expect("remove");
        let name = std::ffi::CString::new(commondir.as_os_str().as_encoded_bytes()).expect("path");
        // SAFETY: `name` is a NUL-terminated path that outlives the call.
        #[allow(unsafe_code)]
        let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo");
        let (send, done) = std::sync::mpsc::channel();
        let looked = project.clone();
        std::thread::spawn(move || {
            let _ = send.send(checkouts_holding(&looked));
        });
        let refused = done
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the read of a pipe waited instead of refusing it")
            .expect_err("a pipe");
        assert!(refused.contains("is not a regular file"), "{refused}");

        // A device, which never ends.
        std::fs::remove_file(&commondir).expect("remove");
        std::os::unix::fs::symlink("/dev/zero", &commondir).expect("link");
        let refused = checkouts_holding(&project).expect_err("a device");
        assert!(refused.contains("is not a regular file"), "{refused}");
        assert!(refused.contains("commondir"), "{refused}");

        // Larger than a link file.
        std::fs::remove_file(&commondir).expect("remove");
        std::fs::write(&commondir, "x".repeat(5000)).expect("commondir");
        let refused = checkouts_holding(&project).expect_err("too large");
        assert!(refused.contains("is larger than a link file"), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
            scratch: dir.join("scratch/p"),
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
            scratch: dir.join("scratch/p"),
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
            scratch: dir.join("scratch/p"),
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
            scratch: dir.join("home/scratch/p"),
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
            scratch: dir.join("home/scratch/p"),
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

    /// What came back for a home, in the words atomic would show: the paths
    /// the refusal is made of, or the part that could not be placed.
    fn words(outcome: &Result<(), StateError>) -> String {
        match outcome {
            Ok(()) => "accepted".to_string(),
            Err(StateError::InsideConfig(outside)) => format!(
                "inside: given {:?}, state {}, config {}",
                outside.given.as_deref().map(Path::display),
                outside.state.display(),
                outside.config.display(),
            ),
            Err(e @ StateError::Resolve { .. }) => format!("refused: {e}"),
            Err(e) => panic!("not the check's own refusal: {e}"),
        }
    }

    /// A throwaway directory with `make` created in it and `links` standing
    /// where they are named: the filesystem a case is checked against. A path
    /// that starts with `/` is below it.
    ///
    /// The system's temporary directory is placed before a case is built in it,
    /// because every path a case compares against is built from what comes back
    /// here and the check compares real paths: where `/tmp` is itself a link, an
    /// unplaced one would compare as a stranger to everything. What is made goes
    /// when the case ends, however it ends — see [`Scratch`].
    fn setup(tag: &str, make: &[&str], links: &[(&str, &str)]) -> Scratch {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let scratch = std::env::temp_dir().join(format!(
            "atomic-place-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).expect("the throwaway directory");
        let scratch =
            Scratch(std::fs::canonicalize(&scratch).expect("the throwaway directory placed"));
        for dir in make {
            std::fs::create_dir_all(scratch.join(dir)).expect("a directory of the case");
        }
        for (from, to) in links {
            std::os::unix::fs::symlink(scratch.join(to), scratch.join(from)).expect("the link");
        }
        scratch
    }

    /// A throwaway directory that goes, with everything below it, when the case
    /// that made it ends — including a case that ends in a failed assertion.
    struct Scratch(PathBuf);

    impl std::ops::Deref for Scratch {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            reopen(&self.0);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Makes every directory below `dir` openable by its owner again, so that
    /// nothing below it can be stranded: a case may close a directory to its
    /// owner alone to make a lookup fail, and its contents cannot be reached,
    /// and so cannot be removed, until it is open again. A link is never opened
    /// and never changed, so a case that links out of the throwaway directory
    /// cannot have what it points at changed by the tidying.
    fn reopen(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                reopen(&path);
            }
        }
    }

    /// The check a command makes of the home `home`, with the environment as
    /// `vars` gives it, the password home as `passwd` gives it and a relative
    /// spelling read from `cwd` — no part of this process's own environment.
    /// `home` and `vars` name a path below the throwaway directory with a
    /// leading `/`, and anything else word for word, which is how its owner
    /// wrote it; `cwd: None` is a caller that cannot place a relative path at
    /// all.
    ///
    /// A case whose names are not there yet is the point: the check has to say
    /// where such a home leads, which is what an unresolved spelling cannot.
    fn check(
        scratch: &Path,
        vars: &[(&str, &str)],
        home: &str,
        cwd: Option<&str>,
        passwd: Option<&str>,
    ) -> Result<(), StateError> {
        let below = |value: &str| -> std::ffi::OsString {
            match value.strip_prefix('/') {
                Some(rest) => scratch.join(rest).into_os_string(),
                None => std::ffi::OsString::from(value),
            }
        };
        let env = |name: &str| -> Option<std::ffi::OsString> {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| below(value))
        };
        outside_user_config_in(
            Path::new(&below(home)),
            &env,
            &passwd.map(|p| scratch.join(p)),
            &|path: &Path| -> Option<PathBuf> {
                if path.is_absolute() {
                    Some(path.to_path_buf())
                } else {
                    cwd.map(|cwd| scratch.join(cwd).join(path))
                }
            },
        )
    }

    /// [`setup`] and [`check`] together.
    fn place(
        tag: &str,
        make: &[&str],
        links: &[(&str, &str)],
        vars: &[(&str, &str)],
        home: &str,
        cwd: Option<&str>,
        passwd: Option<&str>,
    ) -> (Scratch, Result<(), StateError>) {
        let scratch = setup(tag, make, links);
        let outcome = check(&scratch, vars, home, cwd, passwd);
        (scratch, outcome)
    }

    /// How `words` is to say the home `home` was given.
    fn given(scratch: &Path, home: &str) -> String {
        match home.strip_prefix('/') {
            Some(rest) => scratch.join(rest).display().to_string(),
            None => home.to_string(),
        }
    }

    /// A home whose only absolute path is `~/.config/atomic`, nothing of it
    /// created yet: the check is made of directories that do not exist.
    #[test]
    fn a_home_that_is_the_configuration_directory_is_refused_before_it_exists() {
        let home = "/home/.config/atomic";
        let (scratch, outcome) = place(
            "equal",
            &[],
            &[],
            &[("HOME", "/home"), ("ATOMIC_HOME", home)],
            home,
            None,
            None,
        );
        let config = scratch.join("home/.config/atomic");
        assert_eq!(
            words(&outcome),
            format!(
                "inside: given Some({given:?}), state {}, config {}",
                config.display(),
                config.display(),
                given = given(&scratch, home),
            ),
            "not the refusal of a home that is the configuration directory"
        );
        assert!(
            !scratch.join("home/.config").exists(),
            "the check created what it was refusing"
        );
    }

    /// A case of the three shapes a spelling can take: a name for its
    /// throwaway directory, the directories to make in it, the home as its
    /// owner wrote it, where that home lands below the throwaway directory,
    /// and the directory a relative spelling is read from.
    type Placed<'a> = (&'a str, &'a [&'a str], &'a str, &'a str, Option<&'a str>);

    /// A case of the environment: a name, the variables as the person set
    /// them, and the home.
    type Spelled<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);

    /// The three shapes a spelling can hide a home in: a `..` over a name that
    /// is not there, a `..` back into the configuration directory, and a
    /// relative name read from inside it. All three are the configuration
    /// directory or a directory inside it, and a check that compared the
    /// spelling would let all three through.
    #[test]
    fn a_home_is_placed_and_not_read() {
        let cases: &[Placed] = &[
            (
                "dotdot-missing",
                &["home/.config/atomic"],
                "/home/.config/atomic-state/../atomic",
                "home/.config/atomic",
                None,
            ),
            (
                "dotdot-inside",
                &["home/.config/atomic"],
                "/home/.config/atomic/../atomic/state",
                "home/.config/atomic/state",
                None,
            ),
            (
                "relative",
                &["home/.config/atomic"],
                "state",
                "home/.config/atomic/state",
                Some("home/.config/atomic"),
            ),
        ];
        for (tag, make, home, inside, cwd) in cases {
            let vars = [("HOME", "/home"), ("ATOMIC_HOME", *home)];
            let (scratch, outcome) = place(tag, make, &[], &vars, home, *cwd, None);
            let state = scratch.join(inside);
            let config = scratch.join("home/.config/atomic");
            assert_eq!(
                words(&outcome),
                format!(
                    "inside: given Some({given:?}), state {}, config {}",
                    state.display(),
                    config.display(),
                    given = given(&scratch, home),
                ),
                "{tag}: the home was read instead of placed"
            );
        }
    }

    /// A home spelled further out than the configuration directory, which a
    /// string comparison would refuse and a placed one does not.
    #[test]
    fn a_home_beside_the_configuration_directory_still_runs() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("sibling", "/home/.config/atomic-state", None),
            ("out-and-back", "/home/.config/atomic/../atomic-state", None),
            ("dot", "/home/./x/../.config/atomic-state", None),
        ];
        for (tag, home, cwd) in cases {
            let (_, outcome) = place(
                tag,
                &["home/.config/atomic"],
                &[],
                &[("HOME", "/home")],
                home,
                *cwd,
                None,
            );
            assert_eq!(words(&outcome), "accepted", "{tag}");
        }
    }

    /// A configuration directory that is itself a link, and a home that is one
    /// leading into it: both sides are placed, so both links are followed and
    /// the refusal says where the directories really are.
    #[test]
    fn a_home_is_placed_through_a_link() {
        let (scratch, outcome) = place(
            "linked-config",
            &["settings/atomic", "home/.config"],
            &[("home/.config/atomic", "settings/atomic")],
            &[("HOME", "/home"), ("ATOMIC_HOME", "/settings/atomic/state")],
            "/settings/atomic/state",
            None,
            None,
        );
        let settings = scratch.join("settings/atomic");
        assert_eq!(
            words(&outcome),
            format!(
                "inside: given Some(\"{s}/state\"), state {s}/state, config {s}",
                s = settings.display()
            ),
            "the link named in the environment was not followed"
        );
        let (scratch, outcome) = place(
            "linked-home",
            &["home/.config/atomic"],
            &[("far", "home/.config/atomic")],
            &[("HOME", "/home"), ("ATOMIC_HOME", "/far")],
            "/far",
            None,
            None,
        );
        let config = scratch.join("home/.config/atomic");
        assert_eq!(
            words(&outcome),
            format!(
                "inside: given Some(\"{far}\"), state {config}, config {config}",
                far = scratch.join("far").display(),
                config = config.display()
            ),
            "the link given as the home was not followed"
        );
    }

    /// A relative `XDG_CONFIG_HOME` names no configuration directory and gives
    /// way to `HOME`, which is the directory the denial is made of. With no
    /// absolute home anywhere there is no configuration directory to compare
    /// with, and an absolute home is accepted as it is.
    #[test]
    fn the_configuration_directory_is_the_absolute_one() {
        let cases: &[Spelled] = &[
            (
                "xdg-relative",
                &[("XDG_CONFIG_HOME", "settings"), ("HOME", "/home")],
                "/data",
            ),
            ("no-home", &[], "/data"),
            ("home-relative", &[("HOME", "home")], "/data"),
        ];
        for (tag, vars, home) in cases {
            let (_, outcome) = place(tag, &["home/.config/atomic"], &[], vars, home, None, None);
            assert_eq!(words(&outcome), "accepted", "{tag}");
        }
        // And with `HOME` absolute, it is the configuration directory under it
        // that the home is compared with, not the one the relative variable
        // would name.
        let (scratch, outcome) = place(
            "xdg-relative-inside",
            &["home/.config/atomic"],
            &[],
            &[("XDG_CONFIG_HOME", "settings"), ("HOME", "/home")],
            "/home/.config/atomic/state",
            None,
            None,
        );
        assert!(
            words(&outcome).starts_with("inside:"),
            "a home inside the configuration directory `HOME` gives was \
             accepted because a relative `XDG_CONFIG_HOME` named another: {}",
            scratch.display()
        );
    }

    /// With no home in the environment, the home the password database gives
    /// is the configuration directory, as it is for the denial; giving that
    /// home to the check is what makes the two agree.
    #[test]
    fn a_home_inside_the_password_home_is_refused() {
        let (scratch, outcome) = place(
            "passwd",
            &["real-home/.config/atomic"],
            &[],
            &[("ATOMIC_HOME", "/real-home/.config/atomic/state")],
            "/real-home/.config/atomic/state",
            None,
            Some("real-home"),
        );
        let real = scratch.join("real-home/.config/atomic");
        assert_eq!(
            words(&outcome),
            format!(
                "inside: given Some(\"{r}/state\"), state {r}/state, config {r}",
                r = real.display()
            ),
            "the home the password database gives was not compared with"
        );
        // A `HOME` that is an absolute path wins over the password database,
        // as it does for the denial.
        let (_, outcome) = place(
            "passwd-beats-nothing",
            &["real-home/.config/atomic", "home/.config"],
            &[],
            &[("HOME", "/home")],
            "/real-home/.config/atomic/state",
            None,
            Some("real-home"),
        );
        assert_eq!(words(&outcome), "accepted", "HOME was ignored");
    }

    /// Nothing sets the home — or nothing useful does, which is the same to
    /// the state directory that goes to the default data directory. The refusal
    /// has no `ATOMIC_HOME` spelling to name then, and says so.
    #[test]
    fn a_home_no_atomic_home_sets_is_refused_by_what_it_is() {
        let cases: &[(&str, &[(&str, &str)])] = &[
            ("unset", &[("XDG_CONFIG_HOME", "/settings")]),
            (
                "empty",
                &[("XDG_CONFIG_HOME", "/settings"), ("ATOMIC_HOME", "")],
            ),
        ];
        for (tag, vars) in cases {
            let (scratch, outcome) = place(
                tag,
                &["settings/atomic"],
                &[],
                vars,
                // Where the default data directory leads, which is what a
                // command hands the check when nothing sets the home.
                "/settings/atomic/state",
                None,
                None,
            );
            let settings = scratch.join("settings/atomic");
            assert_eq!(
                words(&outcome),
                format!(
                    "inside: given None, state {s}/state, config {s}",
                    s = settings.display()
                ),
                "ATOMIC_HOME {tag} was not refused for what it is"
            );
        }
    }

    /// A home that cannot be placed is refused, and says which of its two
    /// directories could not be placed and what the filesystem said: an
    /// unresolved spelling compares as a stranger to the configuration
    /// directory, which is the failure this check exists to stop.
    #[test]
    fn a_home_that_cannot_be_placed_is_refused() {
        let scratch = setup(
            "unplaced",
            &["home/.config"],
            &[("gone", "nowhere"), ("home/.config/atomic", "nowhere")],
        );
        let home = "/gone/state";
        let vars = [("HOME", "/home"), ("ATOMIC_HOME", home)];
        // The home is read through a link to nowhere: where it leads cannot be
        // said, so neither can it be said that it is outside the configuration
        // directory.
        let outcome = check(&scratch, &vars, home, None, None);
        assert_eq!(
            words(&outcome),
            format!(
                "refused: cannot resolve atomic's state directory {s}: {g} cannot be read: \
                 No such file or directory (os error 2)",
                s = scratch.join("gone/state").display(),
                g = scratch.join("gone").display(),
            ),
            "the refusal of a home read through a link to nowhere"
        );
        // The configuration directory is the link to nowhere, so no spelling
        // of a home can be compared with it — and the refusal says which of the
        // two directories it could not place.
        let outcome = check(&scratch, &[("HOME", "/home")], "/home/state", None, None);
        assert!(
            words(&outcome).starts_with(&format!(
                "refused: cannot resolve your configuration directory {}: it cannot be read: ",
                scratch.join("home/.config/atomic").display()
            )),
            "{}",
            words(&outcome)
        );
        // A home below two links that point at each other: the same refusal,
        // with what the filesystem said about the cycle. Reading the name asked
        // for is already where the cycle stops, so that is the name the refusal
        // says cannot be looked up.
        let scratch = setup(
            "cycle",
            &["home/.config/atomic"],
            &[("there", "back"), ("back", "there")],
        );
        let outcome = check(&scratch, &[("HOME", "/home")], "/there/state", None, None);
        let why = words(&outcome);
        assert!(
            why.starts_with(&format!(
                "refused: cannot resolve atomic's state directory {}: ",
                scratch.join("there/state").display()
            )) && why.contains(&format!(
                "{} cannot be looked up: ",
                scratch.join("there/state").display()
            )),
            "{why}"
        );
        assert!(
            why.contains("symbolic link"),
            "the refusal does not say what the cycle did: {why}"
        );
        // A relative home, with no directory to read it from.
        let outcome = check(&scratch, &[("HOME", "/home")], "state", None, None);
        assert!(
            matches!(
                &outcome,
                Err(StateError::Resolve { side, path, .. })
                    if *side == ResolveSide::State && path == Path::new("state")
            ),
            "a relative home was not refused for want of a directory to read it from: {outcome:?}"
        );
    }

    /// A directory nobody may look in, holding a link into the configuration
    /// directory, and a home read through that link. Every lookup below the
    /// closed directory fails, which is not the same as those names not being
    /// there: taking it for "not there" and walking on to the closed directory
    /// itself places the home by its spelling, beside it and so outside the
    /// configuration directory, and the state gets made inside the very
    /// directory every atom's command is denied. Only `NotFound` may be read as
    /// missing, so this home is refused for the lookup that failed.
    ///
    /// Mode 000 is no bar to the superuser, so this does not stop when the
    /// tests run as root: it then asserts the refusal this shape exists to
    /// bring about — the home is the configuration directory's own, which the
    /// walk through the closed directory is what would have hidden — so a check
    /// that let a state inside the denied directory fails here whoever runs it.
    /// What only a process that cannot look in everything sees is *which*
    /// answer the walk gave: the lookup that failed, and the configuration
    /// directory as the side that cannot be placed. `scripts/controls-check.sh`
    /// runs as the operator, never as root, so that is what
    /// `scripts/controls/state-walk-notfound-only.row` is switched off and seen
    /// failing under; the same rule where the superuser cannot undo it is
    /// pinned by `a_home_that_walks_through_a_file_is_refused`, which the same
    /// row runs.
    #[test]
    fn a_home_behind_a_directory_nobody_may_look_in_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = setup("closed", &["closed", "settings/atomic"], &[]);
        std::os::unix::fs::symlink(
            scratch.join("settings/atomic"),
            scratch.join("closed/into-config"),
        )
        .expect("the link into the configuration directory");
        std::fs::set_permissions(
            scratch.join("closed"),
            std::fs::Permissions::from_mode(0o000),
        )
        .expect("the directory closed to its owner alone");
        // The home as its owner spelled it: `ATOMIC_HOME` names the link, which
        // is where the configuration directory really is.
        let home = scratch.join("closed/into-config/state");
        let config = scratch.join("settings/atomic");
        let outcome = check(
            &scratch,
            &[
                ("XDG_CONFIG_HOME", "/settings"),
                ("ATOMIC_HOME", "/closed/into-config/state"),
            ],
            "/closed/into-config/state",
            None,
            None,
        );
        if effective_uid() == 0 {
            // Nothing failed to be looked up: the walk went through the closed
            // directory, followed the link and read where the home really is,
            // which is inside the configuration directory. That is the refusal
            // this shape is for, so the comparison that refuses a directory
            // inside the configuration one — the half mode 000 cannot hide — is
            // seen failing for the superuser too, rather than passing because
            // nothing here could be made to fail.
            assert!(
                matches!(
                    &outcome,
                    Err(StateError::InsideConfig(outside))
                        if outside.state == config.join("state") && outside.config == config
                ),
                "as the superuser the home behind the closed directory is the \
                 configuration directory's own and was not refused for it: {:?}",
                words(&outcome)
            );
            return;
        }
        let why = words(&outcome);
        assert!(
            why.starts_with(&format!(
                "refused: cannot resolve atomic's state directory {}: ",
                home.display()
            )),
            "not the refusal of a home below a directory nobody may look in: {why}"
        );
        assert!(
            why.contains(&format!("{} cannot be looked up: ", home.display()))
                && why.contains("Permission denied"),
            "the refusal does not say which lookup failed and what it said: {why}"
        );
        // The same closed door on the other side: with the configuration
        // directory itself unreachable, no home can be compared with it, and
        // the refusal says which of the two directories it could not place. Not
        // reached as the superuser, for whom the door opens: the target the link
        // leads to is placed, `/elsewhere` is beside it, and there is nothing to
        // refuse. A directory that cannot be placed for anyone at all is the
        // shape `a_home_that_walks_through_a_file_is_refused` pins.
        let outcome = check(
            &scratch,
            &[("XDG_CONFIG_HOME", "/closed/into-config")],
            "/elsewhere",
            None,
            None,
        );
        assert!(
            matches!(
                &outcome,
                Err(StateError::Resolve { side, .. }) if *side == ResolveSide::Config
            ),
            "a configuration directory nobody may look in was not the side named: {outcome:?}"
        );
    }

    /// A name in the middle of a home's path that is a file and not a
    /// directory, with one name below it and two `..` back over both. Asking
    /// what is below a file is not the same as a name not being there: only
    /// `NotFound` may be read as missing. Reading what the filesystem says about
    /// a file as "missing" and walking on finds the file itself, and the home is
    /// then placed by folding its own spelling over it — out of the file's name,
    /// back out of the directory holding it, to the throwaway directory beside
    /// the configuration directory, and accepted — while a directory below a
    /// file cannot be anywhere at all. So the refusal is for the lookup that
    /// failed, and it says what the filesystem said. The superuser sees this
    /// shape too: nothing here needs a directory closed.
    #[test]
    fn a_home_that_walks_through_a_file_is_refused() {
        let scratch = setup("notdir", &["home/.config/atomic"], &[]);
        std::fs::write(scratch.join("gate"), "a file, not a directory").expect("the file");
        let spelled = "/gate/x/../..";
        let outcome = check(
            &scratch,
            &[("HOME", "/home"), ("ATOMIC_HOME", spelled)],
            spelled,
            None,
            None,
        );
        let why = words(&outcome);
        let home = scratch.join("gate/x/../..");
        assert!(
            why.starts_with(&format!(
                "refused: cannot resolve atomic's state directory {}: ",
                home.display()
            )),
            "not the refusal of a home below a file: {why}"
        );
        assert!(
            why.contains(&format!("{} cannot be looked up: ", home.display()))
                && why.contains("Not a directory"),
            "the refusal does not say which lookup failed and what it said: {why}"
        );
    }

    /// What a run of this binary really does when nothing in the environment
    /// names an absolute home: the configuration directory is the one under the
    /// home the password database gives, both for the check and for the denial
    /// built from [`user_config_dir`]. Not asking the password database at
    /// either entry point names no configuration directory at all, and then the
    /// state inside it is accepted while every atom's command is denied it.
    ///
    /// The environment and the password database are what the test says they
    /// are, so this needs neither this machine's user nor this process's own
    /// environment; see [`as_process`].
    #[test]
    fn a_process_with_no_home_in_its_environment_is_placed_by_the_password_home() {
        let scratch = setup("wired", &["real-home/.config/atomic"], &[]);
        let _process = as_process(&scratch, &[], Some("real-home"));
        let config = scratch.join("real-home/.config/atomic");
        assert_eq!(
            user_config_dir(),
            Some(config.clone()),
            "the denial is not made of the configuration directory under the home the \
             password database gives"
        );
        let state = config.join("state");
        let outcome = outside_user_config(&state);
        assert!(
            matches!(
                &outcome,
                Err(StateError::InsideConfig(outside))
                    if outside.config == config && outside.state == state
            ),
            "a state directory inside the password home was not refused by the check itself: \
             {outcome:?}"
        );
    }

    /// A relative state directory is placed — read from the directory the
    /// command was run in, the names below it that are not there yet kept as
    /// they were spelled — before it is compared with anything. This asks
    /// [`outside_user_config`], the one entry a command uses, because [`placed`]
    /// is what does that on that path. A command brings an absolute home here
    /// except with a relative `HOME` and no `ATOMIC_HOME` — see [`placed`] — so
    /// this is where the placing of a relative spelling is kept working for the
    /// caller that hands one over, which the command a person runs with a
    /// relative `HOME` shows end to end:
    /// `a_relative_home_gives_a_state_directory_placed_from_the_working_directory`
    /// in `crates/atomic/tests/state_cli.rs`. Given the spelling unresolved here
    /// there is nothing to compare, and the check refuses the run for a
    /// directory it cannot place.
    ///
    /// Nothing is created in the working directory: the name is one no
    /// directory holds, so placing it asks nothing of the filesystem beyond the
    /// working directory itself.
    #[test]
    fn a_relative_state_directory_is_placed_from_the_working_directory() {
        let scratch = setup("relative-home", &[], &[]);
        // The configuration directory is `.config/atomic` below `HOME`, which
        // the throwaway directory's parent gives — below the temporary
        // directory, so a state below this process's working directory is not
        // inside it and the placed case runs.
        let _process = as_process(&scratch, &[("HOME", "/.."), ("ATOMIC_HOME", "state")], None);
        let outcome = outside_user_config(Path::new("state"));
        assert_eq!(
            words(&outcome),
            "accepted",
            "a relative state directory was not placed from the working directory, and \
             unresolved there is nothing to compare it with: {outcome:?}"
        );
        // The same directory spelled out from the working directory answers
        // alike: placing is what makes the two spellings one directory.
        let spelled = std::env::current_dir()
            .expect("the working directory")
            .join("state");
        assert_eq!(
            words(&outside_user_config(&spelled)),
            words(&outcome),
            "{spelled:?} is not what the relative spelling names: {outcome:?}"
        );
    }
}
