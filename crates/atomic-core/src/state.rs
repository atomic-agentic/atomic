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
    /// `ATOMIC_HOME` is unset and the home directory is unknown: `HOME` and
    /// `XDG_CONFIG_HOME` name no absolute directory and the password
    /// database gives no absolute home, so there is no configuration
    /// directory to keep credentials in.
    #[error(
        "the home directory is unknown (HOME is unset or not an absolute path, and the user \
         has no home in the password database), so there is no configuration directory for \
         credentials; set HOME, or set ATOMIC_HOME to keep them under it"
    )]
    NoConfigDir,
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
        let legacy = legacy_layouts(&self.dir)?;
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
        #[cfg(test)]
        legacy_inventory_ready();
        lock.lock().map_err(fail(&lock_path))?;
        let locked_legacy = legacy_layouts(&self.dir)?;
        for dir in locked_legacy {
            // Both layouts at once: moving would lose one of them, and
            // ignoring the old one could hide the project's runs.
            for name in LEGACY {
                let (old, current) = (dir.join(name), self.private.join(name));
                if legacy_entry_present(&old)? && legacy_entry_present(&current)? {
                    return Err(StateError::Both { old, current });
                }
            }
            let database = dir.join("atomic.db");
            if database_in_use(&database)? {
                return Err(StateError::InUse {
                    path: database,
                    to: self.database(),
                });
            }
            // Validate all source entries before moving even the first member:
            // relocating a relative link can change where private state lives.
            for name in LEGACY {
                legacy_source_safe(&dir.join(name))?;
            }
            // An older atomic kept its execution lock in `<state>/private`: a
            // process holding it may still run the project's work. It is
            // held here until the state has moved, so no older atomic starts
            // on it meanwhile.
            let old_lock = dir.join("runtime.lock");
            let held = if dir != self.dir {
                let in_use = || StateError::InUse {
                    path: old_lock.clone(),
                    to: self.lock_file(),
                };
                match hold_lock(&old_lock) {
                    Ok(held) => held,
                    Err(std::fs::TryLockError::WouldBlock) => return Err(in_use()),
                    Err(std::fs::TryLockError::Error(source)) => {
                        return Err(fail(&old_lock)(source));
                    }
                }
            } else {
                None
            };
            for name in LEGACY {
                let (old, new) = (dir.join(name), self.private.join(name));
                if legacy_entry_present(&old)? {
                    std::fs::rename(&old, &new).map_err(fail(&new))?;
                }
            }
            if held.is_some() {
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
    /// An unknown database-lock outcome refuses instead of moving its unit.
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
                if database_in_use(&main)? {
                    crate::log_line!(
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
                    crate::log_line!(
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
    pub fn denied_scratch(&self) -> Vec<PathBuf> {
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

/// Only the filesystem saying an entry is absent counts as absent. A final
/// link is an entry too: an old entry or destination must not be lost merely
/// because the link's target is missing.
fn legacy_entry_present(path: &Path) -> Result<bool, StateError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(StateError::Create {
            path: path.to_owned(),
            source,
        }),
    }
}

fn legacy_source_safe(path: &Path) -> Result<(), StateError> {
    let fail = |source| StateError::Create {
        path: path.to_owned(),
        source,
    };
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(fail(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "legacy state entry is a symbolic link",
                )));
            }
            Ok(())
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(fail(source)),
    }
}

fn legacy_layouts(state: &Path) -> Result<Vec<PathBuf>, StateError> {
    let mut found = Vec::new();
    for dir in [state.join("private"), state.to_owned()] {
        let mut has_state = false;
        for name in LEGACY {
            has_state |= legacy_entry_present(&dir.join(name))?;
        }
        if has_state {
            found.push(dir);
        }
    }
    Ok(found)
}

#[cfg(test)]
type InventoryReadyHook = Box<dyn FnOnce()>;

#[cfg(test)]
thread_local! {
    static LEGACY_INVENTORY_READY: std::cell::RefCell<Option<InventoryReadyHook>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
fn legacy_inventory_ready() {
    let ready = LEGACY_INVENTORY_READY.with(|slot| slot.borrow_mut().take());
    if let Some(ready) = ready {
        ready();
    }
}

/// Hold an existing, regular legacy execution lock until the returned file
/// is dropped. Only a missing path is absent. Another holder, an unreadable
/// file or an unknown lock outcome stops migration.
fn hold_lock(path: &Path) -> Result<Option<std::fs::File>, std::fs::TryLockError> {
    let mut open = std::fs::OpenOptions::new();
    open.read(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut open, LEGACY_LOCK_OPEN_FLAGS);
    #[cfg(windows)]
    {
        // Open a final reparse point itself, so metadata rejects it instead
        // of following a missing target and treating the lock as absent.
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        std::os::windows::fs::OpenOptionsExt::custom_flags(&mut open, OPEN_REPARSE_POINT);
    }
    let file = match open.open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(std::fs::TryLockError::Error(source)),
    };
    let regular = legacy_lock_regular(&file).map_err(std::fs::TryLockError::Error)?;
    if !regular {
        return Err(std::fs::TryLockError::Error(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "legacy execution lock is not a regular file",
        )));
    }
    try_legacy_lock(&file)?;
    Ok(Some(file))
}

#[cfg(unix)]
const LEGACY_LOCK_OPEN_FLAGS: i32 = LINK_OPEN_FLAGS | NOT_THROUGH_A_LINK;

fn legacy_lock_regular(file: &std::fs::File) -> std::io::Result<bool> {
    #[cfg(test)]
    if let Some(error) = legacy_lock_error_at(LegacyLockCheck::Metadata) {
        return Err(error);
    }
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & REPARSE_POINT != 0 {
            return Ok(false);
        }
    }
    Ok(metadata.is_file())
}

fn try_legacy_lock(file: &std::fs::File) -> Result<(), std::fs::TryLockError> {
    #[cfg(test)]
    if let Some(error) = legacy_lock_error_at(LegacyLockCheck::Acquisition) {
        return Err(std::fs::TryLockError::Error(error));
    }
    file.try_lock()
}

#[cfg(test)]
thread_local! {
    static LEGACY_LOCK_ERROR: std::cell::Cell<Option<(LegacyLockCheck, std::io::ErrorKind)>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyLockCheck {
    Metadata,
    Acquisition,
}

#[cfg(test)]
fn legacy_lock_error_at(check: LegacyLockCheck) -> Option<std::io::Error> {
    LEGACY_LOCK_ERROR
        .get()
        .filter(|(stage, _)| *stage == check)
        .map(|(_, kind)| std::io::Error::new(kind, "injected legacy execution lock error"))
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
/// "DMS" byte (offset 128) for as long as it is open. Only a missing sidecar
/// or a successful, unlocked probe permits movement; inspection errors do not.
fn database_in_use(path: &Path) -> Result<bool, StateError> {
    let mut shm = path.as_os_str().to_owned();
    shm.push("-shm");
    let shm = PathBuf::from(shm);
    let fail = |source| StateError::Create {
        path: shm.clone(),
        source,
    };
    #[cfg(unix)]
    {
        let mut open = std::fs::OpenOptions::new();
        open.read(true).write(true);
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut open, DATABASE_SHM_OPEN_FLAGS);
        let file = match open.open(&shm) {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => return Err(fail(source)),
        };
        let shared_memory_regular = database_probe_regular(&file).map_err(fail)?;
        if !shared_memory_regular {
            return Err(fail(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "SQLite shared-memory lock is not a regular file",
            )));
        }
        // SAFETY: a zeroed flock is a valid value of a plain C struct.
        #[allow(unsafe_code)]
        let mut probe: libc::flock = unsafe { std::mem::zeroed() };
        probe.l_type = libc::F_WRLCK as _;
        probe.l_whence = libc::SEEK_SET as _;
        probe.l_start = 128;
        probe.l_len = 1;
        let (asked, error) = database_lock_query(&file, &mut probe);
        if asked != 0 {
            return Err(fail(error));
        }
        let sqlite_holder = [
            libc::F_RDLCK as libc::c_short,
            libc::F_WRLCK as libc::c_short,
        ]
        .contains(&probe.l_type);
        if sqlite_holder {
            return Ok(true); // Another process holds the SQLite DMS byte.
        }
        if probe.l_type == libc::F_UNLCK as libc::c_short {
            return Ok(false);
        }
        let unknown = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "SQLite shared-memory lock probe returned an unknown lock type",
        );
        Err(fail(unknown))
    }
    #[cfg(not(unix))]
    {
        if legacy_entry_present(&shm)? || legacy_entry_present(path)? {
            return Err(fail(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "SQLite database-in-use probing is unsupported on this platform",
            )));
        }
        Ok(false)
    }
}

#[cfg(unix)]
const DATABASE_SHM_OPEN_FLAGS: i32 = LINK_OPEN_FLAGS | NOT_THROUGH_A_LINK;

#[cfg(unix)]
fn database_probe_regular(file: &std::fs::File) -> std::io::Result<bool> {
    #[cfg(test)]
    if let Some(DatabaseProbeFailure::Metadata(kind)) = DATABASE_PROBE_FAILURE.get() {
        return Err(std::io::Error::new(kind, "injected SQLite metadata error"));
    }
    file.metadata().map(|metadata| metadata.is_file())
}

/// Keep the native answer and its errno together: the caller must never
/// interpret the output flock after a failed query.
#[cfg(unix)]
fn database_lock_query(
    file: &std::fs::File,
    probe: &mut libc::flock,
) -> (libc::c_int, std::io::Error) {
    use std::os::fd::AsRawFd;
    // SAFETY: F_GETLK reads and writes only `probe`, a valid flock.
    #[allow(unsafe_code)]
    let asked = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, probe as *mut libc::flock) };
    let error = std::io::Error::last_os_error();
    #[cfg(test)]
    match DATABASE_PROBE_FAILURE.get() {
        Some(DatabaseProbeFailure::Query(kind)) => {
            probe.l_type = libc::F_UNLCK as libc::c_short;
            return (
                -1,
                std::io::Error::new(kind, "injected SQLite lock query error"),
            );
        }
        Some(DatabaseProbeFailure::LockType(kind)) => probe.l_type = kind,
        _ => {}
    }
    (asked, error)
}

#[cfg(all(test, unix))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DatabaseProbeFailure {
    Metadata(std::io::ErrorKind),
    Query(std::io::ErrorKind),
    LockType(libc::c_short),
}

#[cfg(all(test, unix))]
thread_local! {
    static DATABASE_PROBE_FAILURE: std::cell::Cell<Option<DatabaseProbeFailure>> = const {
        std::cell::Cell::new(None)
    };
}

/// Where atomic keeps every project's state: `$ATOMIC_HOME`, or `atomic` in
/// the user data directory.
pub fn base_dir() -> Result<PathBuf, StateError> {
    match atomic_home()? {
        Some(home) => Ok(home),
        None => Ok(process_data_dir()
            .ok_or(StateError::NoDataDir)?
            .join("atomic")),
    }
}

/// `$ATOMIC_HOME`, if it is set, as an absolute path: one given relative
/// is taken from the working directory atomic started in, so it names the
/// same directory wherever it is used, such as by a helper Codex starts in
/// another directory, with its `..` resolved as far as it exists. The
/// variable is read as [`user_config_dir`] reads its own, so the home and
/// the configuration directory beside it come from one reading of the
/// environment.
pub(crate) fn atomic_home() -> Result<Option<PathBuf>, StateError> {
    let Some(home) = process_env("ATOMIC_HOME").filter(|v| !v.is_empty()) else {
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
/// repository, and for a `commondir` whose target is not there. A target
/// that is there and cannot be looked at, or whose path loops or walks
/// through a file, is refused: what cannot be seen is never taken for
/// absent.
fn main_checkout(git: &Path, through_links: bool) -> Result<Option<PathBuf>, String> {
    if std::fs::metadata(git).is_ok_and(|m| m.is_dir()) {
        return Ok(None);
    }
    let Some(text) = link_file(git, through_links)? else {
        return Ok(None);
    };
    let Some(gitdir) = text
        .split(|b| *b == b'\n')
        .find_map(|line| line.strip_prefix(b"gitdir:".as_slice()))
    else {
        return Ok(None);
    };
    let Some(parent) = git.parent() else {
        return Ok(None);
    };
    let gitdir = parent.join(link_path(gitdir));
    let Some(common) = link_file(&gitdir.join("commondir"), through_links)? else {
        return Ok(None);
    };
    let commondir = gitdir.join(link_path(&common));
    let common = match std::fs::canonicalize(&commondir) {
        Ok(common) => common,
        Err(e) if not_there(&e) => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", commondir.display())),
    };
    if common.file_name().is_none_or(|n| n != ".git") {
        return Ok(None);
    }
    Ok(common.parent().map(Path::to_owned))
}

/// Whether a failed lookup found nothing there, which is all that counts as
/// absent: what a user cannot look at, a path that loops, one that walks
/// through a file and every other error say nothing about what is there, so
/// they refuse rather than let a checkout go uncounted.
fn not_there(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

/// The path a link file names, as its bytes and not as text: a Unix file
/// name is any bytes but a separator and a NUL, so a checkout under a name
/// that is not text is still found. Text trims as text does; bytes that are
/// not text lose only ASCII whitespace, which is all git writes around one.
fn link_path(bytes: &[u8]) -> PathBuf {
    fn not_space(b: &u8) -> bool {
        !b.is_ascii_whitespace()
    }
    /// The bytes with ASCII whitespace removed from both ends.
    fn ascii_trim(bytes: &[u8]) -> &[u8] {
        let start = bytes.iter().position(not_space).unwrap_or(bytes.len());
        let end = bytes.iter().rposition(not_space).map_or(start, |i| i + 1);
        &bytes[start..end]
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => PathBuf::from(text.trim()),
        #[cfg(unix)]
        Err(_) => {
            use std::os::unix::ffi::OsStrExt;
            PathBuf::from(std::ffi::OsStr::from_bytes(ascii_trim(bytes)))
        }
        #[cfg(not(unix))]
        Err(_) => PathBuf::from(String::from_utf8_lossy(ascii_trim(bytes)).as_ref()),
    }
}

/// The bytes of `path`, a small regular file, or `None` when there is none:
/// opened without blocking, so a pipe there cannot hang the read, and
/// refused when it is anything but a regular file of at most
/// [`LINK_FILE_MAX`] bytes. Bytes and not text, because what git writes
/// there is a path and a path is any bytes.
fn link_file(path: &Path, through_links: bool) -> Result<Option<Vec<u8>>, String> {
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
    let mut bytes = Vec::new();
    file.take(LINK_FILE_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > LINK_FILE_MAX {
        return refused("is larger than a link file");
    }
    Ok(Some(bytes))
}

/// The directory an older atomic kept credentials and the checkpoint key
/// in when `ATOMIC_HOME` was unset: `atomic` in the configuration directory
/// `dirs` names. On macOS that is `~/Library/Application Support/atomic`
/// (on Windows, under `%APPDATA%`), never [`user_config_dir`]; on Linux it
/// is the same directory unless `HOME` is relative.
pub fn legacy_config_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(said) = PROCESS.with(|p| p.borrow().as_ref().map(|process| process.config.clone()))
    {
        return said.map(|d| d.join("atomic"));
    }
    dirs::config_dir().map(|d| d.join("atomic"))
}

/// The places under `legacy`, an older atomic's configuration directory
/// (see [`legacy_config_dir`]), that held its credentials and checkpoint
/// key, for denying to atoms' commands, `ATOMIC_HOME` set or not, as
/// absolute paths (a relative `HOME` makes `legacy` relative, read from the
/// working directory). Both are listed whether they exist or not: an older
/// atomic can still make one after the list is made, and a place that is
/// missing when a command starts is denied once it exists (see
/// [`crate::codex::PrivateState::also`]).
pub fn legacy_secret_dirs(legacy: Option<&Path>) -> Vec<PathBuf> {
    let Some(legacy) = legacy else {
        return Vec::new();
    };
    let mut places = Vec::new();
    for name in ["credentials", "keys"] {
        let dir = legacy.join(name);
        places.push(std::path::absolute(&dir).unwrap_or(dir));
    }
    places
}

/// Whether `place` holds what atomic keeps in it: with `file`, a file of
/// that name in it; without, any entry at all. `Ok(false)` comes only from
/// the operating system saying the place, or the file, is not there, or
/// from a directory it lists with nothing in it; every other answer (a
/// place that cannot be entered or is not a directory, an entry that cannot
/// be read) is the error itself, for the caller to refuse on.
fn holds(place: &Path, file: Option<&str>) -> std::io::Result<bool> {
    let absent = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    match file {
        Some(name) => match std::fs::symlink_metadata(place.join(name)) {
            Ok(_) => Ok(true),
            Err(e) if absent(&e) => Ok(false),
            Err(e) => Err(e),
        },
        None => match std::fs::read_dir(place) {
            Ok(mut entries) => entries.next().transpose().map(|entry| entry.is_some()),
            Err(e) if absent(&e) => Ok(false),
            Err(e) => Err(e),
        },
    }
}

/// Refuses to use `current`, where atomic keeps `what` now, while it holds
/// none of them and `legacy`, where an older atomic kept them, still does:
/// using `current` would hide what is kept there (a credential would read
/// as missing, a new checkpoint key would be made and the runs signed with
/// the old one would no longer resume). What a place holds is judged by
/// its contents (see [`holds`]), `file` naming the one file that counts
/// (the checkpoint key's) or none for any entry (the credential store's),
/// not by whether the directory exists: atomic itself makes `current`,
/// empty, to deny it to atoms' commands. An old place whose contents cannot
/// be read counts as holding them, and a new one whose contents cannot be
/// read as holding none, so an error either way refuses. The person moves
/// them, which keeps what they are, rather than atomic moving them unasked.
pub(crate) fn refuse_stranded(
    what: &str,
    current: &Path,
    legacy: Option<&Path>,
    file: Option<&str>,
) -> Result<(), String> {
    let Some(legacy) = legacy.filter(|l| *l != current) else {
        return Ok(());
    };
    let old = match holds(legacy, file) {
        Ok(false) => return Ok(()),
        Ok(true) => "which still holds them".to_owned(),
        Err(e) => format!("which cannot be read ({e}), so it may still hold them"),
    };
    let (new, readable) = match holds(current, file) {
        Ok(true) => return Ok(()),
        Ok(false) => ("which holds none of them".to_owned(), true),
        Err(e) => (format!("which cannot be read ({e})"), false),
    };
    let current_text = current.display();
    let legacy_text = legacy.display();
    let fix = if readable {
        format!(
            "move everything in {legacy_text} into {current_text}, making {current_text} first \
             if it does not exist"
        )
    } else {
        format!(
            "change the permissions of {current_text} so that atomic can read it, then move \
             everything in {legacy_text} into it"
        )
    };
    let leaf = current
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Err(format!(
        "{what} are kept in {current_text} now, {new}, and an older atomic kept them in \
         {legacy_text}, {old}: {fix}, to keep using them, or set ATOMIC_HOME to a directory \
         that holds them in its `{leaf}` directory"
    ))
}

/// [`credentials_dir`], for reading or writing what it holds: refused while
/// an older atomic's credentials are stranded at its old place (see
/// [`refuse_stranded`]), whatever the new place holds being any entry at
/// all. With `ATOMIC_HOME` set, the place has not moved.
pub fn credentials_dir_in_use() -> Result<PathBuf, String> {
    let dir = credentials_dir().map_err(|e| e.to_string())?;
    if atomic_home().map_err(|e| e.to_string())?.is_none() {
        let legacy = legacy_config_dir().map(|d| d.join("credentials"));
        refuse_stranded("credentials", &dir, legacy.as_deref(), None)?;
    }
    Ok(dir)
}

/// Where the opt-in file store keeps operator credentials (see
/// [`crate::credential::FileSecrets`]): `$ATOMIC_HOME/credentials`, or
/// `credentials` under [`user_config_dir`] — the same directory the
/// person's own configuration resolves under and the denial is made of, so
/// one spelling of `HOME`, `XDG_CONFIG_HOME` or the password database
/// places settings, credentials, keys and the denial together, and never a
/// place a working directory would complete. Outside every project and
/// every state directory.
pub fn credentials_dir() -> Result<PathBuf, StateError> {
    match atomic_home()? {
        Some(home) => Ok(home.join("credentials")),
        None => Ok(user_config_dir()
            .ok_or(StateError::NoConfigDir)?
            .join("credentials")),
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
pub(crate) fn process_env(name: &str) -> Option<std::ffi::OsString> {
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

/// The data directory `dirs` names for this process; for the thread a test
/// runs on, what `as_process` said it is instead.
pub(crate) fn process_data_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(said) = PROCESS.with(|p| p.borrow().as_ref().map(|process| process.data.clone())) {
        return said;
    }
    dirs::data_dir()
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

/// What a test says this process's environment and password database are,
/// and the configuration and data directories `dirs` would name for it.
#[cfg(test)]
struct TestProcess {
    env: std::collections::BTreeMap<String, std::ffi::OsString>,
    passwd: Option<PathBuf>,
    config: Option<PathBuf>,
    data: Option<PathBuf>,
}

/// Say, for this test thread, that the environment holds exactly `vars` — a
/// `name` whose value starting with `/` names a directory below `scratch` —
/// and that the password database gives `passwd`, below `scratch` with a
/// leading `/`. The answer comes back as [`process_env`] and
/// [`process_passwd_home`] until the returned guard is dropped, which puts
/// this process's own back however the thread ends.
///
/// `dirs` names no configuration or data directory for it: no older
/// atomic's place ([`legacy_config_dir`]), and no state home without
/// `ATOMIC_HOME` ([`base_dir`]), so nothing a test does lands in this
/// machine's own; [`as_process_with_dirs`] names them.
#[cfg(test)]
pub(crate) fn as_process(
    scratch: &Path,
    vars: &[(&str, &str)],
    passwd: Option<&str>,
) -> TestProcessGuard {
    as_process_with_dirs(scratch, vars, passwd, None, None)
}

/// [`as_process`], with `config` and `data`, below `scratch`, as the
/// configuration and data directories `dirs` names: an older atomic kept
/// its credentials and key in `atomic` in the first, and the state home
/// without `ATOMIC_HOME` is `atomic` in the second.
#[cfg(test)]
pub(crate) fn as_process_with_dirs(
    scratch: &Path,
    vars: &[(&str, &str)],
    passwd: Option<&str>,
    config: Option<&str>,
    data: Option<&str>,
) -> TestProcessGuard {
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
            config: config.map(|dir| scratch.join(dir)),
            data: data.map(|dir| scratch.join(dir)),
        });
    });
    TestProcessGuard(())
}

/// A throwaway directory for a test, made empty and named after `tag` and
/// this process, that goes with everything below it when the value is
/// dropped, however the test ends.
#[cfg(test)]
pub(crate) struct TestScratch(PathBuf);

#[cfg(test)]
impl TestScratch {
    pub(crate) fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let made = std::env::temp_dir().join(format!(
            "atomic-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&made);
        std::fs::create_dir_all(&made).expect("the throwaway directory");
        Self(std::fs::canonicalize(&made).expect("the throwaway directory placed"))
    }
}

#[cfg(test)]
impl std::ops::Deref for TestScratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TestScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Puts this process's own environment and password database back for its
/// thread; see [`as_process`].
#[cfg(test)]
pub(crate) struct TestProcessGuard(());

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

    /// A `commondir` whose target is there but cannot be looked in, whose
    /// path walks through a file, or whose path loops refuses the project,
    /// and says which error it met. Only a target that is not there at all
    /// is taken for absent, and then no main checkout is counted.
    #[cfg(unix)]
    #[test]
    fn a_commondir_target_that_cannot_be_seen_refuses_the_project() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("atomic-commondir-seen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // The `commondir` names `<main>/shared/.git`, whose parent is the
        // main checkout. `shared` is on the way to what is looked for: it
        // can be made unsearchable, be a file, or be a link that loops.
        let gitdir = dir.join("main").join("wtgit");
        let shared = dir.join("main").join("shared");
        std::fs::create_dir_all(&gitdir).expect("gitdir");
        std::fs::create_dir_all(shared.join(".git")).expect("common dir");
        std::fs::write(gitdir.join("commondir"), "../shared/.git\n").expect("commondir");
        std::fs::create_dir_all(dir.join("wt/project")).expect("worktree");
        std::fs::write(
            dir.join("wt/.git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .expect(".git");
        let project = dir.join("wt/project");
        // Where the `commondir` says to look, with its `..` still in it as
        // the refusal names it.
        let target = gitdir.join("../shared/.git");
        let held = checkouts_holding(&project).expect("read");
        assert!(
            held.contains(&std::fs::canonicalize(&shared).expect("shared")),
            "the well-formed pair found no main checkout: {held:?}"
        );

        // A directory on the way this user may not look in. Its `.git` is
        // there, so nothing is absent: the lookup is refused.
        let mode = |m: u32| {
            std::fs::set_permissions(&shared, PermissionsExt::from_mode(m)).expect("chmod")
        };
        // Opened again however the test ends, so that a failure here leaves
        // no directory the next run cannot remove.
        struct Reopened(PathBuf);
        impl Drop for Reopened {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(
                    &self.0,
                    std::os::unix::fs::PermissionsExt::from_mode(0o700),
                );
            }
        }
        mode(0o000);
        let _reopened = Reopened(shared.clone());
        assert!(
            std::fs::read_dir(&shared).is_err(),
            "mode 000 did not keep this user out (root?): run the test as a user"
        );
        let refused = checkouts_holding(&project).expect_err("a target out of reach");
        assert!(
            refused.contains(&target.display().to_string()),
            "which target was refused: {refused}"
        );
        assert!(
            refused.contains(&format!("os error {}", libc::EACCES)),
            "not the permission denial: {refused}"
        );
        drop(_reopened);
        mode(0o700);

        // A file where the directory on the way should be.
        std::fs::remove_dir_all(&shared).expect("remove");
        std::fs::write(&shared, "not a directory").expect("file");
        let refused = checkouts_holding(&project).expect_err("a path through a file");
        assert!(
            refused.contains(&format!("os error {}", libc::ENOTDIR)),
            "not the walk through a file: {refused}"
        );

        // A link that loops back on itself.
        std::fs::remove_file(&shared).expect("remove");
        std::os::unix::fs::symlink(&shared, &shared).expect("a loop");
        let refused = checkouts_holding(&project).expect_err("a link loop");
        assert!(
            refused.contains(&format!("os error {}", libc::ELOOP)),
            "not the link loop: {refused}"
        );

        // Nothing there at all, which is absent: the project is read, and no
        // main checkout is counted beside its own worktree.
        std::fs::remove_file(&shared).expect("remove");
        let held = checkouts_holding(&project)
            .expect("a target that is not there is absent, not an error");
        assert_eq!(held, vec![dir.join("wt")], "{held:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A worktree's `.git` whose `gitdir:` names a path that is no text, and
    /// a `commondir` that names one too, are read as the paths they name, so
    /// the main checkout there is still counted.
    // The on-disk fixture needs a filesystem that accepts non-UTF-8 filenames.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_link_file_naming_a_path_that_is_no_text_is_still_read() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let dir = std::env::temp_dir().join(format!("atomic-link-bytes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A name containing a byte that no UTF-8 sequence starts with.
        let odd = |name: &str| {
            let mut name = name.as_bytes().to_vec();
            name.push(0xff);
            std::ffi::OsString::from_vec(name)
        };
        // What a link file holds, as the raw bytes git writes there. Going
        // through `display()` would replace the byte that is no text with a
        // marker, which names no directory.
        let written = |head: &str, to: &Path, tail: &str| {
            let mut body = head.as_bytes().to_vec();
            body.extend_from_slice(to.as_os_str().as_bytes());
            body.extend_from_slice(tail.as_bytes());
            body
        };
        let main = dir.join(odd("repo"));
        std::fs::create_dir_all(main.join(".git")).expect("main");
        // One worktree whose `.git` names its git directory under the name
        // that is no text.
        let by_bytes = main.join(".git").join("worktrees").join("wt");
        std::fs::create_dir_all(&by_bytes).expect("gitdir");
        std::fs::write(by_bytes.join("commondir"), "../..\n").expect("commondir");
        std::fs::create_dir_all(dir.join("by-bytes/project")).expect("worktree");
        std::fs::write(
            dir.join("by-bytes/.git"),
            written("gitdir: ", &by_bytes, "\n"),
        )
        .expect(".git");
        // Another whose git directory sits under names that are text, but
        // whose `commondir` names the main checkout through one that is not:
        // four levels up from `<dir>/plain/.git/worktrees/wt` is `<dir>`.
        let by_name = dir.join("plain/.git/worktrees/wt");
        std::fs::create_dir_all(&by_name).expect("gitdir");
        let mut commondir = b"../../../../".to_vec();
        commondir.extend_from_slice(odd("repo").as_bytes());
        commondir.extend_from_slice(b"/.git\n");
        std::fs::write(by_name.join("commondir"), &commondir).expect("commondir");
        std::fs::create_dir_all(dir.join("by-name/project")).expect("worktree");
        std::fs::write(
            dir.join("by-name/.git"),
            written("gitdir: ", &by_name, "\n"),
        )
        .expect(".git");
        let main = std::fs::canonicalize(&main).expect("main");
        let held = checkouts_holding(&dir.join("by-bytes/project")).expect("read");
        assert!(
            held.contains(&main),
            "a `.git` naming a path that is no text lost the main checkout: {held:?}"
        );
        let held = checkouts_holding(&dir.join("by-name/project")).expect("read");
        assert!(
            held.contains(&main),
            "a `commondir` naming a path that is no text lost the main checkout: {held:?}"
        );
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

    fn legacy_lock_fixture(tag: &str) -> (TestScratch, ProjectState, PathBuf) {
        let home = TestScratch::new(tag);
        let state = ProjectState {
            root: home.join("project"),
            dir: home.join("home/projects/p"),
            private: home.join("home/private/p"),
            scratch: home.join("home/scratch/p"),
        };
        let old = state.dir().join("private");
        std::fs::create_dir_all(old.join("context")).expect("old context");
        std::fs::write(old.join("context/kept"), "unmoved").expect("old state");
        (home, state, old)
    }

    #[test]
    fn legacy_inventory_lookup_errors_refuse_creation() {
        let (_home, state, old) = legacy_lock_fixture("legacy-inventory-directory");
        std::fs::remove_dir_all(&old).expect("remove old fixture");
        std::fs::write(&old, "not a directory").expect("opaque old layout");
        assert_eq!(
            std::fs::symlink_metadata(old.join("context"))
                .expect_err("the lookup really fails")
                .kind(),
            std::io::ErrorKind::NotADirectory
        );
        match state.create().expect_err("an unknown layout must refuse") {
            StateError::Create { path, source } => {
                assert_eq!(path, old.join("atomic.db"));
                assert_eq!(source.kind(), std::io::ErrorKind::NotADirectory);
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&old).expect("unchanged"),
            "not a directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_inventory_loop_errors_refuse_creation() {
        let (_home, state, old) = legacy_lock_fixture("legacy-inventory-loop");
        std::fs::remove_dir_all(&old).expect("remove old fixture");
        std::os::unix::fs::symlink("private", &old).expect("old layout loop");
        assert_eq!(
            std::fs::symlink_metadata(old.join("atomic.db"))
                .expect_err("the parent loop really fails")
                .raw_os_error(),
            Some(libc::ELOOP)
        );
        match state.create().expect_err("an unknown layout must refuse") {
            StateError::Create { path, source } => {
                assert_eq!(path, old.join("atomic.db"));
                assert_eq!(source.raw_os_error(), Some(libc::ELOOP));
            }
            other => panic!("wrong refusal: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_inventory_keeps_an_existing_dangling_destination() {
        let (_home, state, old) = legacy_lock_fixture("legacy-inventory-destination");
        std::fs::write(old.join("atomic.db"), "old database").expect("old database");
        std::fs::create_dir_all(state.private_dir()).expect("new private directory");
        std::os::unix::fs::symlink("missing", state.database()).expect("existing destination");
        match state
            .create()
            .expect_err("an existing destination is not absent")
        {
            StateError::Both { old: from, current } => {
                assert_eq!(from, old.join("atomic.db"));
                assert_eq!(current, state.database());
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        assert_eq!(
            std::fs::read_link(state.database()).expect("link remains"),
            Path::new("missing")
        );
        assert_eq!(
            std::fs::read_to_string(old.join("atomic.db")).expect("source remains"),
            "old database"
        );
        legacy_context_is_unmoved(&state, &old);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_inventory_refuses_source_links_before_moving_members() {
        for target in ["missing", "../../outside", "live"] {
            let (home, state, old) = legacy_lock_fixture("legacy-inventory-entry");
            std::fs::write(old.join("atomic.db"), "old database").expect("old database");
            std::fs::write(old.join("atomic.db-wal"), "old WAL").expect("old WAL");
            std::fs::remove_dir_all(old.join("context")).expect("replace old context");
            std::os::unix::fs::symlink(target, old.join("context")).expect("old entry");
            if target == "../../outside" {
                std::fs::create_dir(home.join("home/outside"))
                    .expect("destination-relative target");
            }
            if target == "live" {
                std::fs::create_dir(old.join("live")).expect("existing link target");
                assert!(
                    std::fs::metadata(old.join("context"))
                        .expect("live link")
                        .is_dir()
                );
            } else {
                assert_eq!(
                    std::fs::metadata(old.join("context"))
                        .expect_err("old link is dangling")
                        .kind(),
                    std::io::ErrorKind::NotFound
                );
            }
            match state
                .create()
                .expect_err("source links must refuse before any member moves")
            {
                StateError::Create { path, source } => {
                    assert_eq!(path, old.join("context"));
                    assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
                    assert_eq!(source.to_string(), "legacy state entry is a symbolic link");
                }
                other => panic!("wrong refusal: {other:?}"),
            }
            assert_eq!(
                std::fs::read_link(old.join("context")).expect("original link remains"),
                Path::new(target)
            );
            for (name, bytes) in [("atomic.db", "old database"), ("atomic.db-wal", "old WAL")] {
                assert_eq!(
                    std::fs::read_to_string(old.join(name)).expect("source remains"),
                    bytes
                );
                assert_eq!(
                    std::fs::symlink_metadata(state.private_dir().join(name))
                        .expect_err("destination untouched")
                        .kind(),
                    std::io::ErrorKind::NotFound
                );
            }
            assert_eq!(
                std::fs::symlink_metadata(state.context_dir())
                    .expect_err("no context installed")
                    .kind(),
                std::io::ErrorKind::NotFound
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_inventory_keeps_links_inside_an_ordinary_context_directory() {
        let (_home, state, old) = legacy_lock_fixture("legacy-context-directory");
        std::os::unix::fs::symlink("kept", old.join("context/nested-link")).expect("nested link");
        state.create().expect("ordinary context directory migrates");
        assert_eq!(
            std::fs::read_to_string(state.context_dir().join("kept")).expect("context bytes"),
            "unmoved"
        );
        assert_eq!(
            std::fs::read_link(state.context_dir().join("nested-link"))
                .expect("nested link retained"),
            Path::new("kept")
        );
        assert_eq!(
            std::fs::symlink_metadata(old.join("context"))
                .expect_err("old context moved")
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn legacy_inventory_is_rechecked_under_the_migration_lock() {
        let (_home, state, _old) = legacy_lock_fixture("legacy-inventory-locked");
        let held = std::fs::File::create(state.dir().join("migrate.lock")).expect("migration lock");
        held.lock().expect("another migration holds the lock");
        let (ready, before_lock) = std::sync::mpsc::sync_channel(0);
        let work = state.clone();
        let worker = std::thread::spawn(move || {
            LEGACY_INVENTORY_READY.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    ready.send(()).expect("preflight observed");
                }));
            });
            work.create()
        });
        let reached = before_lock.recv_timeout(std::time::Duration::from_secs(10));
        let arrived = std::fs::write(
            state.dir().join("atomic.db"),
            "arrived during another migration",
        );
        let released = held.unlock();
        let migrated = worker.join();
        reached.expect("the first inventory completed before waiting on the lock");
        arrived.expect("another process left a legacy database");
        released.expect("migration lock released");
        migrated
            .expect("migration thread")
            .expect("migration succeeded");
        assert_eq!(
            std::fs::read_to_string(state.database()).expect("newly discovered database moved"),
            "arrived during another migration"
        );
    }

    fn database_probe_fixture(tag: &str) -> (TestScratch, ProjectState, PathBuf) {
        let (home, state, old) = legacy_lock_fixture(tag);
        std::fs::write(old.join("atomic.db"), "old database").expect("old database");
        (home, state, old)
    }

    #[cfg(unix)]
    #[test]
    fn database_probe_loop_errors_refuse_legacy_migration() {
        let (_home, state, old) = database_probe_fixture("database-probe-loop");
        let shm = old.join("atomic.db-shm");
        std::os::unix::fs::symlink("atomic.db-shm", &shm).expect("shared-memory loop");
        assert_eq!(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&shm)
                .expect_err("the open really fails")
                .raw_os_error(),
            Some(libc::ELOOP)
        );
        match state
            .create()
            .expect_err("an unknown database lock refuses migration")
        {
            StateError::Create { path, source } => {
                assert_eq!(path, shm);
                assert_eq!(source.raw_os_error(), Some(libc::ELOOP));
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        legacy_context_is_unmoved(&state, &old);
    }

    #[cfg(unix)]
    #[test]
    fn database_probe_open_errors_refuse_legacy_migration() {
        let (_home, state, old) = database_probe_fixture("database-probe-directory");
        let shm = old.join("atomic.db-shm");
        std::fs::create_dir(&shm).expect("a directory instead of shared memory");
        match state
            .create()
            .expect_err("an unknown database lock refuses migration")
        {
            StateError::Create { path, source } => {
                assert_eq!(path, shm);
                assert_eq!(source.raw_os_error(), Some(libc::EISDIR));
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        legacy_context_is_unmoved(&state, &old);
    }

    #[cfg(unix)]
    #[test]
    fn database_probe_errors_keep_codex_records() {
        let home = TestScratch::new("codex-database-probe-loop");
        let root = home.join("project");
        std::fs::create_dir(&root).expect("project");
        let state = ProjectState::under(&home, &root);
        let codex = state.codex_home();
        std::fs::create_dir_all(&codex).expect("Codex home");
        let database = codex.join("state_5.sqlite");
        std::fs::write(&database, "old Codex record").expect("old database");
        let shm = codex.join("state_5.sqlite-shm");
        std::os::unix::fs::symlink("state_5.sqlite-shm", &shm).expect("shared-memory loop");
        match state
            .create()
            .expect_err("unknown records must stay in place")
        {
            StateError::Create { path, source } => {
                assert_eq!(path, shm);
                assert_eq!(source.raw_os_error(), Some(libc::ELOOP));
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(database).expect("old record remains"),
            "old Codex record"
        );
        assert_eq!(
            std::fs::symlink_metadata(state.codex_records().join("state_5.sqlite"))
                .expect_err("no record moved")
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn database_probe_links_are_not_followed() {
        for target in ["missing-shm", "other-shm"] {
            let (_home, state, old) = database_probe_fixture("database-probe-link");
            let shm = old.join("atomic.db-shm");
            if target == "other-shm" {
                std::fs::write(old.join(target), "unlocked shared memory").expect("target");
            }
            std::os::unix::fs::symlink(target, &shm).expect("shared-memory link");
            match state
                .create()
                .expect_err("a linked shared-memory lock refuses migration")
            {
                StateError::Create { path, source } => {
                    assert_eq!(path, shm);
                    assert_eq!(source.raw_os_error(), Some(libc::ELOOP));
                }
                other => panic!("wrong refusal: {other:?}"),
            }
            legacy_context_is_unmoved(&state, &old);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn database_probe_nonregular_files_refuse_migration() {
        let (_home, state, old) = database_probe_fixture("database-probe-fifo");
        let shm = old.join("atomic.db-shm");
        let name = std::ffi::CString::new(shm.as_os_str().as_encoded_bytes()).expect("path");
        // SAFETY: `name` is a NUL-terminated path that outlives this call.
        #[allow(unsafe_code)]
        let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo");
        match state
            .create()
            .expect_err("a pipe is not SQLite shared memory")
        {
            StateError::Create { path, source } => {
                assert_eq!(path, shm);
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
                assert_eq!(
                    source.to_string(),
                    "SQLite shared-memory lock is not a regular file"
                );
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        legacy_context_is_unmoved(&state, &old);
    }

    #[cfg(unix)]
    struct RestoreDatabaseProbe;

    #[cfg(unix)]
    impl Drop for RestoreDatabaseProbe {
        fn drop(&mut self) {
            DATABASE_PROBE_FAILURE.set(None);
        }
    }

    #[cfg(unix)]
    fn database_probe_errors_refuse(failure: fn(std::io::ErrorKind) -> DatabaseProbeFailure) {
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotADirectory,
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::Unsupported,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::Other,
        ] {
            let (_home, state, old) = database_probe_fixture("database-probe-error");
            let shm = old.join("atomic.db-shm");
            std::fs::write(&shm, "shared memory").expect("shared-memory file");
            let injected = failure(kind);
            assert_eq!(DATABASE_PROBE_FAILURE.replace(Some(injected)), None);
            let restore = RestoreDatabaseProbe;
            match state
                .create()
                .expect_err("an unknown probe outcome must refuse")
            {
                StateError::Create { path, source } => {
                    assert_eq!(path, shm);
                    assert_eq!(source.kind(), kind);
                    let message = match injected {
                        DatabaseProbeFailure::Metadata(_) => "injected SQLite metadata error",
                        DatabaseProbeFailure::Query(_) => "injected SQLite lock query error",
                        other => panic!("unexpected fixture: {other:?}"),
                    };
                    assert_eq!(source.to_string(), message);
                }
                other => panic!("wrong refusal: {other:?}"),
            }
            legacy_context_is_unmoved(&state, &old);
            drop(restore);
            state.create().expect("a known unlocked database can move");
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_probe_metadata_errors_refuse_migration() {
        database_probe_errors_refuse(DatabaseProbeFailure::Metadata);
    }

    #[cfg(unix)]
    #[test]
    fn database_probe_query_errors_refuse_migration() {
        database_probe_errors_refuse(DatabaseProbeFailure::Query);
    }

    #[cfg(unix)]
    #[test]
    fn database_probe_unknown_lock_types_refuse_migration() {
        let (_home, state, old) = database_probe_fixture("database-probe-unknown");
        let shm = old.join("atomic.db-shm");
        std::fs::write(&shm, "shared memory").expect("shared-memory file");
        assert_eq!(
            DATABASE_PROBE_FAILURE
                .replace(Some(DatabaseProbeFailure::LockType(libc::c_short::MAX))),
            None
        );
        let _restore = RestoreDatabaseProbe;
        match state
            .create()
            .expect_err("an unknown lock type must refuse")
        {
            StateError::Create { path, source } => {
                assert_eq!(path, shm);
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidData);
                assert_eq!(
                    source.to_string(),
                    "SQLite shared-memory lock probe returned an unknown lock type"
                );
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        legacy_context_is_unmoved(&state, &old);
    }

    #[cfg(unix)]
    #[test]
    fn database_probe_known_holders_refuse_migration() {
        for kind in [
            libc::F_RDLCK as libc::c_short,
            libc::F_WRLCK as libc::c_short,
        ] {
            let (_home, state, old) = database_probe_fixture("database-probe-held");
            std::fs::write(old.join("atomic.db-shm"), "shared memory").expect("shared-memory file");
            assert_eq!(
                DATABASE_PROBE_FAILURE.replace(Some(DatabaseProbeFailure::LockType(kind))),
                None
            );
            let restore = RestoreDatabaseProbe;
            match state.create().expect_err("a held database must refuse") {
                StateError::InUse { path, to } => {
                    assert_eq!(path, old.join("atomic.db"));
                    assert_eq!(to, state.database());
                }
                other => panic!("wrong refusal: {other:?}"),
            }
            legacy_context_is_unmoved(&state, &old);
            drop(restore);
            state.create().expect("an unlocked database can move");
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_probe_known_holders_keep_codex_records() {
        for kind in [
            libc::F_RDLCK as libc::c_short,
            libc::F_WRLCK as libc::c_short,
        ] {
            let home = TestScratch::new("codex-probe-held");
            let root = home.join("project");
            std::fs::create_dir(&root).expect("project");
            let state = ProjectState::under(&home, &root);
            let codex = state.codex_home();
            std::fs::create_dir_all(&codex).expect("Codex home");
            let database = codex.join("state_5.sqlite");
            std::fs::write(&database, "old Codex record").expect("old database");
            std::fs::write(codex.join("state_5.sqlite-shm"), "shared memory")
                .expect("shared-memory file");
            assert_eq!(
                DATABASE_PROBE_FAILURE.replace(Some(DatabaseProbeFailure::LockType(kind))),
                None
            );
            let restore = RestoreDatabaseProbe;
            state
                .create()
                .expect("held records stay for the old writer");
            assert_eq!(
                std::fs::read_to_string(&database).expect("held record remains"),
                "old Codex record"
            );
            assert_eq!(
                std::fs::symlink_metadata(state.codex_records().join("state_5.sqlite"))
                    .expect_err("no held record moved")
                    .kind(),
                std::io::ErrorKind::NotFound
            );
            drop(restore);
            state.create().expect("unlocked records can move");
            assert_eq!(
                std::fs::symlink_metadata(&database)
                    .expect_err("old record moved")
                    .kind(),
                std::io::ErrorKind::NotFound
            );
            assert_eq!(
                std::fs::read_to_string(state.codex_records().join("state_5.sqlite"))
                    .expect("record moved"),
                "old Codex record"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_probe_unlocked_shared_memory_allows_migration() {
        let (_home, state, old) = database_probe_fixture("database-probe-unlocked");
        std::fs::write(old.join("atomic.db-shm"), "shared memory").expect("shared-memory file");
        state.create().expect("unlocked state migrates");
        assert_eq!(
            std::fs::read_to_string(state.database()).expect("database moved"),
            "old database"
        );
        state.create().expect("migration is idempotent");
    }

    fn legacy_context_is_unmoved(state: &ProjectState, old: &Path) {
        assert_eq!(
            std::fs::read_to_string(old.join("context/kept")).expect("old state remains"),
            "unmoved"
        );
        assert_eq!(
            std::fs::symlink_metadata(state.context_dir())
                .expect_err("no context moved")
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }

    /// A link loop is not absence, even when the test runs as root.
    #[cfg(unix)]
    #[test]
    fn legacy_execution_lock_link_errors_refuse_migration() {
        let (_home, state, old) = legacy_lock_fixture("legacy-lock-loop");
        let old_lock = old.join("runtime.lock");
        std::os::unix::fs::symlink("runtime.lock", &old_lock).expect("loop");
        assert_eq!(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&old_lock)
                .expect_err("the loop really refuses an open")
                .raw_os_error(),
            Some(libc::ELOOP)
        );
        match state
            .create()
            .expect_err("an unseen lock refuses migration")
        {
            StateError::Create { path, source } => {
                assert_eq!(path, old_lock);
                assert_eq!(source.raw_os_error(), Some(libc::ELOOP));
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        legacy_context_is_unmoved(&state, &old);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_execution_lock_links_are_not_followed() {
        for target in ["missing-lock", "other-lock"] {
            let (_home, state, old) = legacy_lock_fixture("legacy-lock-link");
            let old_lock = old.join("runtime.lock");
            if target == "other-lock" {
                std::fs::write(old.join(target), "untouched").expect("target");
            }
            std::os::unix::fs::symlink(target, &old_lock).expect("lock link");
            assert!(
                std::fs::symlink_metadata(&old_lock)
                    .expect("link exists even if its target does not")
                    .file_type()
                    .is_symlink()
            );
            match state.create().expect_err("a linked lock refuses migration") {
                StateError::Create { path, source } => {
                    assert_eq!(path, old_lock);
                    assert_eq!(source.raw_os_error(), Some(libc::ELOOP));
                }
                other => panic!("wrong refusal: {other:?}"),
            }
            legacy_context_is_unmoved(&state, &old);
        }
    }

    #[cfg(unix)]
    #[test]
    fn legacy_execution_lock_open_errors_refuse_migration() {
        let (_home, state, old) = legacy_lock_fixture("legacy-lock-directory");
        let old_lock = old.join("runtime.lock");
        std::fs::create_dir(&old_lock).expect("a directory instead of a lock");
        match state
            .create()
            .expect_err("an unopenable lock refuses migration")
        {
            StateError::Create { path, source } => {
                assert_eq!(path, old_lock);
                assert_eq!(source.raw_os_error(), Some(libc::EISDIR));
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        legacy_context_is_unmoved(&state, &old);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_execution_lock_nonregular_files_refuse_migration() {
        let (_home, state, old) = legacy_lock_fixture("legacy-lock-fifo");
        let old_lock = old.join("runtime.lock");
        let name = std::ffi::CString::new(old_lock.as_os_str().as_encoded_bytes()).expect("path");
        // SAFETY: `name` is a NUL-terminated path that outlives this call.
        #[allow(unsafe_code)]
        let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo");
        match state
            .create()
            .expect_err("a pipe cannot be an execution lock")
        {
            StateError::Create { path, source } => {
                assert_eq!(path, old_lock);
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
                assert_eq!(
                    source.to_string(),
                    "legacy execution lock is not a regular file"
                );
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        legacy_context_is_unmoved(&state, &old);
    }

    fn legacy_lock_errors_refuse_migration(check: LegacyLockCheck) {
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                LEGACY_LOCK_ERROR.set(None);
            }
        }
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotADirectory,
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::Unsupported,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::Other,
        ] {
            let (_home, state, old) = legacy_lock_fixture("legacy-lock-error");
            let old_lock = old.join("runtime.lock");
            std::fs::write(&old_lock, "legacy owner").expect("old lock");
            assert_eq!(LEGACY_LOCK_ERROR.replace(Some((check, kind))), None);
            let restore = Restore;
            match state
                .create()
                .expect_err("an unknown lock outcome refuses migration")
            {
                StateError::Create { path, source } => {
                    assert_eq!(path, old_lock);
                    assert_eq!(source.kind(), kind);
                    assert_eq!(source.to_string(), "injected legacy execution lock error");
                }
                other => panic!("wrong refusal: {other:?}"),
            }
            legacy_context_is_unmoved(&state, &old);
            drop(restore);
            state.create().expect("moves after a successful lock");
            assert_eq!(
                std::fs::read_to_string(state.context_dir().join("kept")).expect("moved"),
                "unmoved"
            );
        }
    }

    #[test]
    fn legacy_execution_lock_acquisition_errors_refuse_migration() {
        legacy_lock_errors_refuse_migration(LegacyLockCheck::Acquisition);
    }

    #[test]
    fn legacy_execution_lock_metadata_errors_refuse_migration() {
        legacy_lock_errors_refuse_migration(LegacyLockCheck::Metadata);
    }

    #[test]
    fn legacy_execution_lock_absence_allows_migration() {
        let (_home, state, old) = legacy_lock_fixture("legacy-lock-absent");
        assert_eq!(
            std::fs::symlink_metadata(old.join("runtime.lock"))
                .expect_err("no old execution lock")
                .kind(),
            std::io::ErrorKind::NotFound
        );
        state.create().expect("an absent lock holds nothing back");
        assert_eq!(
            std::fs::read_to_string(state.context_dir().join("kept")).expect("moved"),
            "unmoved"
        );
        state.create().expect("already migrated");
    }

    #[test]
    fn legacy_execution_lock_holders_refuse_migration() {
        let (_home, state, old) = legacy_lock_fixture("legacy-lock-held");
        let old_lock = old.join("runtime.lock");
        let held = std::fs::File::create(&old_lock).expect("old lock");
        held.lock().expect("another executor holds the old lock");
        match state.create().expect_err("a held lock refuses migration") {
            StateError::InUse { path, to } => {
                assert_eq!(path, old_lock);
                assert_eq!(to, state.lock_file());
            }
            other => panic!("wrong refusal: {other:?}"),
        }
        legacy_context_is_unmoved(&state, &old);
        held.unlock().expect("executor releases its lock");
        state.create().expect("moves after release");
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
        // Another test can fork while this descriptor is open. Keep a
        // duplicate alive to model the inherited reference deterministically.
        let inherited = held.try_clone().expect("duplicate the held descriptor");
        match state.create() {
            Err(StateError::InUse { path, to }) => {
                assert_eq!(path, old_lock);
                assert_eq!(to, state.lock_file());
            }
            other => panic!("moved under a held execution lock: {other:?}"),
        }
        assert!(old_db.exists(), "nothing moved");
        // Closing one descriptor does not release a lock while an inherited
        // reference remains open. Release this fixture's lock explicitly.
        held.unlock().expect("release the old execution lock");
        drop(held);
        state.create().expect("moves once closed");
        drop(inherited);
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

    /// An older atomic's credentials or key at their old place, with none
    /// at the new one, refuse the new place, naming both and saying to move
    /// what the old one holds into the new one. What a place holds is judged
    /// by its contents, not by whether it exists: the new directory made
    /// empty, as the denial makes it, still refuses, and an old directory
    /// with nothing in it (or, for keys, no key) refuses nothing. Once the
    /// new place holds them, or where the places are one, or with nothing
    /// at the old one, the new place is used. A place whose contents cannot
    /// be read refuses: an old one counts as holding them, a new one as
    /// holding none.
    #[test]
    fn credentials_left_at_an_older_atomics_place_are_not_hidden() {
        let scratch = setup(
            "stranded",
            &["legacy/credentials", "legacy/keys", "file-parent"],
            &[],
        );
        let current = scratch.join("home/.config/atomic/credentials");
        let legacy = scratch.join("legacy/credentials");
        let refusal = |current: &Path, legacy: &Path, new: &str, old: &str, leaf: &str| {
            format!(
                "credentials are kept in {current} now, {new}, and an older atomic kept them \
                 in {legacy}, {old}: move everything in {legacy} into {current}, making \
                 {current} first if it does not exist, to keep using them, or set ATOMIC_HOME \
                 to a directory that holds them in its `{leaf}` directory",
                current = current.display(),
                legacy = legacy.display(),
            )
        };
        // An old directory with nothing in it holds no credentials.
        assert_eq!(
            refuse_stranded("credentials", &current, Some(&legacy), None),
            Ok(())
        );
        std::fs::write(legacy.join("6f70"), "secret").expect("an older atomic's credential");
        let stranded = refusal(
            &current,
            &legacy,
            "which holds none of them",
            "which still holds them",
            "credentials",
        );
        assert_eq!(
            refuse_stranded("credentials", &current, Some(&legacy), None),
            Err(stranded.clone()),
            "the new place does not exist"
        );
        std::fs::create_dir_all(&current).expect("the new place, empty");
        assert_eq!(
            refuse_stranded("credentials", &current, Some(&legacy), None),
            Err(stranded),
            "the new place exists, empty, as the denial makes it"
        );
        assert_eq!(
            refuse_stranded("credentials", &legacy, Some(&legacy), None),
            Ok(())
        );
        let absent = scratch.join("legacy/none");
        assert_eq!(
            refuse_stranded("credentials", &current, Some(&absent), None),
            Ok(())
        );
        assert_eq!(refuse_stranded("credentials", &current, None, None), Ok(()));

        // An old place under a file ("not a directory") cannot be read, and
        // counts as holding them.
        let file = scratch.join("file-parent/plain");
        std::fs::write(&file, "").expect("a file");
        let under_file = file.join("credentials");
        let why = refuse_stranded("credentials", &current, Some(&under_file), None)
            .expect_err("an old place that cannot be read");
        assert!(
            why.contains("which cannot be read (Not a directory")
                && why.contains("so it may still hold them"),
            "{why}"
        );

        // A directory nobody may list: empty, so only its being unreadable
        // can refuse. Under root it can be listed, and these fail rather
        // than pass for the wrong reason.
        /// Gives a directory of the case its permissions back, however the
        /// case ends, so the throwaway directory can go.
        struct Listable(PathBuf);
        impl Drop for Listable {
            fn drop(&mut self) {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
        use std::os::unix::fs::PermissionsExt;
        let sealed = scratch.join("sealed");
        std::fs::create_dir_all(&sealed).expect("a directory to seal");
        let _listable = Listable(sealed.clone());
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).expect("sealed");
        let why = refuse_stranded("credentials", &current, Some(&sealed), None)
            .expect_err("an old place that cannot be listed");
        assert!(
            why.contains("which cannot be read (Permission denied")
                && why.contains("so it may still hold them"),
            "{why}"
        );
        assert_eq!(
            refuse_stranded("credentials", &sealed, Some(&legacy), None),
            Err(format!(
                "credentials are kept in {sealed} now, which cannot be read (Permission denied \
                 (os error 13)), and an older atomic kept them in {legacy}, which still holds \
                 them: change the permissions of {sealed} so that atomic can read it, then move \
                 everything in {legacy} into it, to keep using them, or set ATOMIC_HOME to a \
                 directory that holds them in its `sealed` directory",
                sealed = sealed.display(),
                legacy = legacy.display(),
            )),
            "a new place that cannot be listed was taken to hold them, or the remedy does not \
             say to make it readable"
        );

        // Once the new place holds something, it is used.
        std::fs::write(current.join("6f70"), "secret").expect("moved");
        assert_eq!(
            refuse_stranded("credentials", &current, Some(&legacy), None),
            Ok(())
        );

        // The key: only the key file counts, at either place.
        let keys = scratch.join("home/.config/atomic/keys");
        let old_keys = scratch.join("legacy/keys");
        std::fs::create_dir_all(&keys).expect("the new key directory, empty");
        std::fs::write(old_keys.join("other"), "").expect("not a key");
        assert_eq!(
            refuse_stranded("keys", &keys, Some(&old_keys), Some("context.key")),
            Ok(()),
            "an old key directory without a key"
        );
        std::fs::write(old_keys.join("context.key"), [7u8; 32]).expect("the old key");
        std::fs::write(keys.join("other"), "").expect("not a key");
        let why = refuse_stranded("keys", &keys, Some(&old_keys), Some("context.key"))
            .expect_err("a new key directory without a key");
        assert!(why.contains("which holds none of them"), "{why}");
        std::fs::write(keys.join("context.key"), [8u8; 32]).expect("a new key");
        assert_eq!(
            refuse_stranded("keys", &keys, Some(&old_keys), Some("context.key")),
            Ok(())
        );

        // Both old places are listed for the denial whether they exist or
        // not: an older atomic can still make one later.
        assert_eq!(
            legacy_secret_dirs(Some(&scratch.join("legacy"))),
            [
                scratch.join("legacy/credentials"),
                scratch.join("legacy/keys")
            ]
        );
        assert_eq!(
            legacy_secret_dirs(Some(&scratch.join("nowhere"))),
            [
                scratch.join("nowhere/credentials"),
                scratch.join("nowhere/keys")
            ]
        );
        assert_eq!(legacy_secret_dirs(None), Vec::<PathBuf>::new());

        // The credential store's place is refused only without
        // `ATOMIC_HOME`, whose place never moved.
        std::fs::remove_file(current.join("6f70")).expect("the new place emptied");
        std::fs::create_dir_all(scratch.join("old")).expect("the older configuration directory");
        std::fs::rename(scratch.join("legacy"), scratch.join("old/atomic")).expect("placed");
        let _home = as_process_with_dirs(&scratch, &[("HOME", "/home")], None, Some("old"), None);
        assert_eq!(
            credentials_dir_in_use(),
            Err(refusal(
                &current,
                &scratch.join("old/atomic/credentials"),
                "which holds none of them",
                "which still holds them",
                "credentials",
            ))
        );
        drop(_home);
        let _atomic = as_process_with_dirs(
            &scratch,
            &[("HOME", "/home"), ("ATOMIC_HOME", "/home")],
            None,
            Some("old"),
            None,
        );
        assert_eq!(
            credentials_dir_in_use(),
            Ok(scratch.join("home/credentials"))
        );
    }

    /// Settings, the denial and the credential store name one configuration
    /// directory for every spelling of the home: `$XDG_CONFIG_HOME/atomic`,
    /// else `~/.config/atomic`, else the home the password database gives.
    /// A *relative* `HOME` is no home and gives way to the password
    /// database, so credentials are never placed at a spelling a working
    /// directory would complete; no absolute spelling at all is refused
    /// saying the home is unknown, rather than placing credentials
    /// nowhere-different.
    #[test]
    fn credentials_are_kept_in_the_one_configuration_directory_of_every_spelling() {
        let scratch = setup(
            "credentials-place",
            &["home", "settings", "real-home/.config/atomic"],
            &[],
        );
        // A relative `HOME`: no configuration directory, and no place for
        // credentials that `<cwd>/rel-home/.config` would name.
        let _relative_home = as_process(&scratch, &[("HOME", "rel-home")], Some("real-home"));
        let config = scratch.join("real-home/.config/atomic");
        assert_eq!(
            credentials_dir().expect("credentials"),
            config.join("credentials"),
            "credentials are not kept under the home the password database gives"
        );
        assert_eq!(
            credentials_dir().expect("credentials"),
            user_config_dir()
                .expect("the configuration directory")
                .join("credentials"),
            "credentials are not kept where settings and the denial are made"
        );
        drop(_relative_home);
        let _home = as_process(&scratch, &[("HOME", "/home")], None);
        assert_eq!(
            credentials_dir().expect("credentials"),
            scratch.join("home/.config/atomic/credentials"),
            "credentials are not under `HOME`"
        );
        drop(_home);
        let _xdg = as_process(
            &scratch,
            &[("HOME", "/home"), ("XDG_CONFIG_HOME", "/settings")],
            None,
        );
        assert_eq!(
            credentials_dir().expect("credentials"),
            scratch.join("settings/atomic/credentials"),
            "`XDG_CONFIG_HOME` did not move the credentials"
        );
        drop(_xdg);
        let _relative_xdg = as_process(
            &scratch,
            &[("HOME", "/home"), ("XDG_CONFIG_HOME", "settings")],
            None,
        );
        assert_eq!(
            credentials_dir().expect("credentials"),
            scratch.join("home/.config/atomic/credentials"),
            "a relative `XDG_CONFIG_HOME` did not give way to `HOME`"
        );
        drop(_relative_xdg);
        // `ATOMIC_HOME` keeps credentials inside the home, and is read from
        // the same environment as the configuration directory beside it.
        let _atomic = as_process(
            &scratch,
            &[("HOME", "/home"), ("ATOMIC_HOME", "/home")],
            None,
        );
        assert_eq!(
            credentials_dir().expect("credentials"),
            scratch.join("home/credentials"),
            "`ATOMIC_HOME` did not keep the credentials inside the home"
        );
        drop(_atomic);
        let _none = as_process(&scratch, &[("HOME", "rel-home")], None);
        let why = credentials_dir().expect_err("no absolute spelling of a home");
        assert!(
            matches!(why, StateError::NoConfigDir),
            "not the unknown-home refusal for credentials: {why}"
        );
        assert!(
            why.to_string().contains("the home directory is unknown")
                && why.to_string().contains("set ATOMIC_HOME"),
            "the refusal does not say which home is missing and what to set: {why}"
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
