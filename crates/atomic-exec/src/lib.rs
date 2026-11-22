// SPDX-License-Identifier: Apache-2.0

//! The `ExecutionBackend` trait between the control plane and its runners.
//!
//! The control plane drives runners only through this trait, even when both run
//! in one process, so moving runners elsewhere never changes workflow code.
//!
//! Every atom run goes through it:
//!
//! 1. [`ExecutionBackend::open`] opens a [`Workspace`] for the attempt: the
//!    directory its work runs in, as the runner sees it, a scratch directory
//!    of its own, and where a model turn's shell and file work runs
//!    ([`Shell`]), under the sandbox and with the environment the request
//!    names (see [`workspace`]);
//! 2. a model turn runs its shell and file work there, and an exec atom's
//!    command runs through [`ExecutionBackend::run`]: no shell, a directory
//!    in the workspace, exactly the environment it is given, one input on
//!    stdin and bounded output, returning how it ended;
//! 3. [`ExecutionBackend::close`] releases the workspace, whether the run
//!    succeeded or not.
//!
//! Before any of that, the control plane connects: [`ExecutionBackend::connect`]
//! returns the backend's [`Statement`], which says once whether its
//! workspaces survive the runner process ([`Durability`]). The control plane
//! plans from that statement, never from which backend it is, and reads a
//! value it does not recognize as ephemeral.
//!
//! [`LocalBackend`] is the backend on this machine: the workspace is the
//! directory itself, a turn's shell runs under Codex's own sandbox in this
//! process, and commands run as child processes.
//!
//! [`runner`] moves the trait across a process boundary: `atomic runner`
//! runs commands in a process of its own, with an exit-code contract, and
//! [`runner::ProcessRunner`] drives it from the control plane.
//!
//! [`quiescence`] is the runner's side of a sandbox backend that suspends:
//! whether its sandbox is safe to suspend.

pub mod exchange;
pub mod process;
pub mod quiescence;
pub mod runner;
pub mod workspace;

/// Where this crate's log lines go: set once by the binary, to its
/// redacting log; until then they are dropped. This crate cannot depend on
/// the one that redacts.
static LOG: std::sync::OnceLock<fn(&str)> = std::sync::OnceLock::new();

/// Send this crate's log lines to `log`. The first call wins.
pub fn set_log(log: fn(&str)) {
    let _ = LOG.set(log);
}

/// Write one log line through the installed log.
pub(crate) fn log(line: &str) {
    if let Some(log) = LOG.get() {
        log(line);
    }
}

pub use workspace::{Open, Sandbox, Shell, Workspace, retire_scratch_root};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Whether a backend's workspaces survive its runner process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Durability {
    /// A workspace outlives the runner process: what an attempt left in it
    /// is still there after the runner stops or crashes.
    Durable,
    /// A workspace may go with the runner process, so nothing may depend on
    /// finding it again. A statement that is missing or not recognized
    /// means this.
    #[default]
    Ephemeral,
}

impl Durability {
    /// The durability a backend's statement names: `durable` is durable,
    /// and anything else, recognized or not, is ephemeral.
    pub fn from_statement(value: &str) -> Self {
        match value {
            "durable" => Self::Durable,
            _ => Self::Ephemeral,
        }
    }

    /// Its name in a statement.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Durable => "durable",
            Self::Ephemeral => "ephemeral",
        }
    }
}

impl<'de> Deserialize<'de> for Durability {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Any value at all: one that is not a string it recognizes is read
        // as ephemeral, never refused.
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(value.as_str().map_or(Self::Ephemeral, Self::from_statement))
    }
}

/// What a backend states when the control plane connects to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statement {
    /// Whether its workspaces survive its runner process.
    #[serde(default)]
    pub durability: Durability,
}

/// One command for a backend to run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Command {
    /// The program, found on the control plane's `PATH` when it has no `/`.
    pub program: String,
    /// Its arguments, passed as they are: no shell expands them.
    pub args: Vec<String>,
    /// The directory it runs in.
    pub cwd: PathBuf,
    /// The whole environment it receives. Nothing is inherited.
    pub env: Vec<(String, String)>,
    /// What it reads on stdin.
    pub stdin: Vec<u8>,
    /// How long it may run before it is killed.
    pub time_limit: Duration,
    /// The most stdout it may write, in bytes.
    pub output_limit: usize,
    /// The directories it may write, such as its workspace's scratch
    /// directory, when its backend confines it (see [`Confine`]). A backend
    /// that does not confine its commands ignores it.
    pub writable: Vec<PathBuf>,
    /// Directories inside [`writable`](Self::writable) ones that it may only
    /// read, such as the inputs atomic hands it, when its backend confines
    /// it. A backend that does not confine its commands ignores it.
    pub read_only: Vec<PathBuf>,
    /// The hosts it may reach, as its atom declares them, when its backend
    /// confines it; with none, it has no network at all. A backend that does
    /// not confine its commands ignores it.
    pub network: Vec<String>,
    /// Directories it may neither read nor write, apart from the
    /// [`writable`](Self::writable) ones inside them, when its backend
    /// confines it: its workspace's [`denied`](Workspace::denied), the
    /// backend's scratch directories, so it reads no other attempt's. A
    /// backend that does not confine its commands ignores it.
    pub denied: Vec<PathBuf>,
    /// Variables it gets on a socket instead of in its environment, which
    /// another process of the same user can read through /proc until the
    /// program has made itself non-dumpable: see [`SecretEnv`]. Only a
    /// program that reads the socket, such as atomic's sandbox helper, gets
    /// them. Unlike a pipe, a socket cannot be reopened through
    /// `/proc/<pid>/fd`, so in that window another process can take them
    /// only by attaching to the program (ptrace, `pidfd_getfd`), which Yama's
    /// `ptrace_scope` of 1 or more refuses to a process that is not its
    /// ancestor; at `ptrace_scope` 0 one can attach in that window and keep
    /// its access after, so the window is narrowed, not closed. A process
    /// runner forwards them to its runner the same way.
    pub secret_env: SecretEnv,
}

/// The variable that names the descriptor a command reads its
/// [`SecretEnv`] from.
pub const SECRET_ENV_FD: &str = "ATOMIC_SECRET_ENV_FD";

/// Variables handed to a command on a Unix socket rather than in its exec
/// environment: the command finds the socket's descriptor in
/// [`SECRET_ENV_FD`] and reads `KEY=VALUE` entries, each ended by a NUL
/// byte, until the other end closes. Its `Debug` form shows the names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretEnv(pub Vec<(String, String)>);

impl std::fmt::Debug for SecretEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(k, _)| format!("{k}=<hidden>")))
            .finish()
    }
}

impl SecretEnv {
    /// Whether it holds no variable.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Its entries as they cross the socket, or why one cannot: a name that
    /// is empty or holds `=` or NUL, or a value that holds NUL.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        for (key, value) in &self.0 {
            if key.is_empty() || key.contains(['=', '\0']) {
                return Err(format!("{key:?} cannot name a secret variable"));
            }
            if value.contains('\0') {
                return Err(format!("the secret variable {key} holds a NUL byte"));
            }
            bytes.extend_from_slice(key.as_bytes());
            bytes.push(b'=');
            bytes.extend_from_slice(value.as_bytes());
            bytes.push(0);
        }
        Ok(bytes)
    }

    /// The entries in `bytes`, as [`encode`](Self::encode) wrote them, each
    /// ended by its NUL, the last too.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let Some(entries) = bytes.strip_suffix(&[0]) else {
            return if bytes.is_empty() {
                Ok(Self::default())
            } else {
                Err("the secret variables end before their last NUL".to_owned())
            };
        };
        entries
            .split(|b| *b == 0)
            .map(|entry| {
                let entry = std::str::from_utf8(entry)
                    .map_err(|_| "a secret variable is not UTF-8".to_owned())?;
                let (key, value) = entry
                    .split_once('=')
                    .ok_or_else(|| "a secret variable has no `=`".to_owned())?;
                if key.is_empty() {
                    return Err("a secret variable has no name".to_owned());
                }
                Ok((key.to_owned(), value.to_owned()))
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }
}

/// Puts a command under a sandbox before a backend runs it: what it may
/// read, write and reach. atomic's runtime gives the local backend one that
/// keeps commands out of atomic's private state; the trait names no
/// sandbox, so a backend elsewhere is free to confine its own way.
pub trait Confine: Send + Sync {
    /// The command that runs `command` confined. It must run `command` with
    /// exactly its environment and in its directory, let it write only
    /// [`Command::writable`], except [`Command::read_only`] inside them,
    /// which it may only read, and keep it out of [`Command::denied`] apart
    /// from the writable directories inside them. `tmp` is the backend's private temporary
    /// directory, where the sandbox may keep records of its own.
    fn confine(&self, command: &Command, tmp: &std::path::Path) -> Result<Command, ExecError>;

    /// As [`confine`](Self::confine), with what the confined command needs
    /// while it runs, such as a proxy that carries its network traffic: the
    /// backend holds it until the command has ended. A confinement that
    /// needs nothing gives none.
    fn confine_held(
        &self,
        command: &Command,
        tmp: &std::path::Path,
    ) -> Result<(Command, Option<Held>), ExecError> {
        Ok((self.confine(command, tmp)?, None))
    }

    /// Check, once, that commands can be confined here at all, so a machine
    /// that cannot confine them fails at connect with a reason rather than
    /// in every command.
    fn check(&self, tmp: &std::path::Path) -> Result<(), ExecError> {
        let _ = tmp;
        Ok(())
    }
}

/// What a confined command needs while it runs (see
/// [`Confine::confine_held`]): kept until it has ended, then dropped.
pub type Held = std::sync::Arc<dyn std::any::Any + Send + Sync>;

/// How a command ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completed {
    /// Its exit code, or `None` if a signal ended it.
    pub code: Option<i32>,
    /// What it wrote to stdout.
    pub stdout: Vec<u8>,
    /// What it wrote to stderr, up to a limit.
    pub stderr: Vec<u8>,
}

impl Completed {
    /// Whether it exited with code 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Why a command could not be run to completion.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecError {
    /// It could not be started.
    #[error("cannot start {program}: {message}")]
    Start {
        /// The program.
        program: String,
        /// Why.
        message: String,
    },
    /// It ran past its time limit and was killed.
    #[error("{program} exceeded its time limit of {seconds} seconds and was stopped")]
    TimedOut {
        /// The program.
        program: String,
        /// The limit.
        seconds: u64,
    },
    /// It wrote more than its output limit.
    #[error("{program} wrote more than {limit} bytes")]
    TooLarge {
        /// The program.
        program: String,
        /// The limit.
        limit: usize,
    },
    /// Running it failed in some other way.
    #[error("running {program}: {message}")]
    Io {
        /// The program.
        program: String,
        /// What failed.
        message: String,
    },
    /// The runner process it was sent to ended without running it: see
    /// [`runner`] for the classes and what each one means.
    #[error("the runner for {program} ended with a {class}{}: {message}{}",
        code.map(|c| format!(" (exit code {c})")).unwrap_or_default(),
        if stderr.is_empty() { String::new() } else { format!("; its stderr: {stderr}") })]
    Runner {
        /// The program.
        program: String,
        /// The runner's exit class.
        class: String,
        /// Its exit code, or `None` if a signal ended it.
        code: Option<i32>,
        /// What the control plane concluded.
        message: String,
        /// What the runner wrote to stderr, as it was: captured, never parsed.
        stderr: String,
    },
    /// A workspace could not be opened or closed.
    #[error("the workspace for {label}: {message}")]
    Workspace {
        /// The workspace's label.
        label: String,
        /// What failed.
        message: String,
    },
    /// Commands cannot be confined on this machine at all, such as where
    /// user namespaces are not allowed: nothing was started, and nothing will
    /// start until the machine is fixed.
    #[error("{message}")]
    Unconfinable {
        /// What the sandbox needs.
        message: String,
    },
    /// The backend cannot open a workspace now, for a reason that may pass,
    /// such as a full disk or no room for another runner: trying again later
    /// may succeed, and nothing was started.
    #[error("the workspace for {label} is not available now: {message}")]
    Unavailable {
        /// The workspace's label.
        label: String,
        /// Why.
        message: String,
    },
    /// The runner process it was sent to ended without saying how the
    /// command ended. The command may have run, in part or in full, so what
    /// it did is unknown, and the backend never runs it again on its own.
    #[error("the runner for {program} ended without its result{}: {message}{}; the command may have run",
        code.map(|c| format!(" (exit code {c})")).unwrap_or_default(),
        if stderr.is_empty() { String::new() } else { format!("; its stderr: {stderr}") })]
    Lost {
        /// The program.
        program: String,
        /// The runner's exit code, or `None` if a signal ended it.
        code: Option<i32>,
        /// What the control plane concluded.
        message: String,
        /// What the runner wrote to stderr, as it was: captured, never parsed.
        stderr: String,
    },
}

impl ExecError {
    /// Whether the command may have run although no result came back, so
    /// running it again could repeat what it did. Only the caller, which
    /// knows what the command declares about its effects, may decide to.
    pub fn outcome_unknown(&self) -> bool {
        matches!(self, Self::Lost { .. })
    }
}

/// Where atoms run. The control plane reaches runners only through this.
///
/// Every method may block: callers on an async executor call it on the
/// blocking pool.
pub trait ExecutionBackend: Send + Sync {
    /// Connect, and state what the control plane may plan on.
    fn connect(&self) -> Result<Statement, ExecError>;

    /// Open a workspace for one attempt of one atom run. Each attempt opens
    /// a workspace of its own: one that runs again, as a new attempt,
    /// starts in a new one, and opening an attempt that is open already is
    /// refused.
    fn open(&self, request: &Open) -> Result<Workspace, ExecError>;

    /// Run `command` to completion in a workspace this backend opened. A
    /// nonzero exit is a [`Completed`] with that code, not an error: the
    /// caller decides what it means.
    fn run(&self, command: &Command) -> Result<Completed, ExecError>;

    /// Release `workspace`. Closing one that is closed already succeeds.
    fn close(&self, workspace: &Workspace) -> Result<(), ExecError>;

    /// The exchange of `workspace`, which this backend opened: the
    /// directories an exec atom's command is given artifacts in and hands
    /// files over in (see [`exchange`]), made new before the command runs.
    /// The control plane moves those files only through it. A backend that
    /// hands no files over gives none, the default: its commands are given
    /// no artifacts, and an output that names a file is refused.
    fn exchange(
        &self,
        workspace: &Workspace,
    ) -> Result<Option<Box<dyn exchange::Exchange>>, ExecError> {
        let _ = workspace;
        Ok(None)
    }
}

/// A connected backend, shared by everything that runs atoms, such as the
/// runtimes atoms run on, with the statement it made when it connected. The
/// default is [`LocalBackend`].
#[derive(Clone)]
pub struct Backend {
    inner: Arc<dyn ExecutionBackend>,
    statement: Statement,
}

impl Backend {
    /// Connect to `backend` once and share it.
    pub fn connect(backend: impl ExecutionBackend + 'static) -> Result<Self, ExecError> {
        let statement = backend.connect()?;
        Ok(Self {
            inner: Arc::new(backend),
            statement,
        })
    }

    /// The shared backend itself.
    pub fn shared(&self) -> Arc<dyn ExecutionBackend> {
        self.inner.clone()
    }

    /// What it stated when it connected.
    pub fn statement(&self) -> Statement {
        self.statement
    }

    /// Whether its workspaces survive its runner process, as it stated.
    pub fn durability(&self) -> Durability {
        self.statement.durability
    }
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            inner: Arc::new(LocalBackend::default()),
            statement: LocalBackend::STATEMENT,
        }
    }
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("statement", &self.statement)
            .finish_non_exhaustive()
    }
}

impl ExecutionBackend for Backend {
    /// The statement it made when it connected: a backend is connected once.
    fn connect(&self) -> Result<Statement, ExecError> {
        Ok(self.statement)
    }

    fn open(&self, request: &Open) -> Result<Workspace, ExecError> {
        self.inner.open(request)
    }

    fn run(&self, command: &Command) -> Result<Completed, ExecError> {
        self.inner.run(command)
    }

    fn close(&self, workspace: &Workspace) -> Result<(), ExecError> {
        self.inner.close(workspace)
    }

    fn exchange(
        &self,
        workspace: &Workspace,
    ) -> Result<Option<Box<dyn exchange::Exchange>>, ExecError> {
        self.inner.exchange(workspace)
    }
}

/// The backend on this machine, in the control plane's process: a
/// workspace is the source directory itself, a model turn's shell and file
/// work runs under Codex's own sandbox, and commands run as child processes.
///
/// Each attempt's scratch directory is a private one under
/// [`scratch_root`](Self::scratch_root), named by the attempt's run and step
/// (see [`workspace`]). Give it a directory no sandboxed turn may write, such
/// as one in the atomic home's `scratch/`: [`LocalBackend::in_dir`].
#[derive(Clone)]
pub struct LocalBackend {
    scratch_root: PathBuf,
    tmp: PathBuf,
    confine: Option<Arc<dyn Confine>>,
    /// Directories its workspaces' commands are kept out of besides its own
    /// scratch root, such as the directory that holds every backend's
    /// scratch roots.
    denying: Vec<PathBuf>,
    /// Set once its confinement has been seen to work here; a check that
    /// fails is made again the next time.
    checked: Arc<std::sync::OnceLock<()>>,
}

impl std::fmt::Debug for LocalBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalBackend")
            .field("scratch_root", &self.scratch_root)
            .field("tmp", &self.tmp)
            .field("confined", &self.confine.is_some())
            .field("denying", &self.denying)
            .finish()
    }
}

impl PartialEq for LocalBackend {
    fn eq(&self, other: &Self) -> bool {
        self.scratch_root == other.scratch_root
            && self.tmp == other.tmp
            && match (&self.confine, &other.confine) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Eq for LocalBackend {}

impl LocalBackend {
    /// What it states: a workspace is a directory on this machine's disk,
    /// which outlives any process, so it is durable.
    pub const STATEMENT: Statement = Statement {
        durability: Durability::Durable,
    };

    /// A backend keeping its scratch directories under `scratch_root`,
    /// which it creates, private, when it first needs it.
    pub fn in_dir(scratch_root: PathBuf) -> Self {
        Self {
            tmp: scratch_root.join(".tmp"),
            scratch_root,
            confine: None,
            denying: Vec::new(),
            checked: Arc::default(),
        }
    }

    /// The same backend, keeping its workspaces' commands out of `dirs` as
    /// well as its own scratch root, such as the directory that holds every
    /// backend's scratch roots, of every project, so no command of one reads
    /// another's attempts. Each is created, if missing, when a workspace
    /// opens, so it cannot appear undenied while a command runs.
    pub fn denying(mut self, dirs: impl IntoIterator<Item = PathBuf>) -> Self {
        self.denying.extend(dirs);
        self
    }

    /// The same backend, running every command under `confine`.
    pub fn confined(mut self, confine: Arc<dyn Confine>) -> Self {
        self.confine = Some(confine);
        self
    }

    /// `command` as this backend runs it: under its confinement, if it has
    /// one, with what the confined command needs while it runs, to hold
    /// until it has ended.
    pub fn prepare(&self, command: &Command) -> Result<(Command, Option<Held>), ExecError> {
        match &self.confine {
            Some(confine) => confine.confine_held(command, &self.tmp),
            None => Ok((command.clone(), None)),
        }
    }

    /// Where it keeps its scratch directories.
    pub fn scratch_root(&self) -> &std::path::Path {
        &self.scratch_root
    }

    /// The temporary directory every turn on this backend shares, created
    /// private to this user if it is not there: the `TMPDIR` of Codex's
    /// sandbox helper outside the sandbox, where it keeps its registry of
    /// the mount points it makes in a turn's writable roots.
    pub fn shared_tmp(&self) -> std::io::Result<&std::path::Path> {
        workspace::ensure_private(&self.tmp)?;
        Ok(&self.tmp)
    }

    /// Refuse a scratch root inside the system's temporary directories,
    /// which sandboxed turns may write, as
    /// [`connect`](ExecutionBackend::connect) does.
    pub fn check_scratch_root(&self) -> Result<(), ExecError> {
        workspace::check_outside(&self.scratch_root, &workspace::temporary_roots()).map_err(
            |message| ExecError::Workspace {
                label: "connect".into(),
                message,
            },
        )
    }
}

impl Default for LocalBackend {
    /// Scratch directories of this backend's own under a private directory
    /// of this user's in the system's temporary directory, and a temporary
    /// directory every such backend of this user shares. Sandboxed turns may
    /// write there, so [`connect`](ExecutionBackend::connect) refuses it:
    /// atomic itself always uses [`LocalBackend::in_dir`] with a directory
    /// in the atomic home's `scratch/`, and this one is for tests that
    /// open workspaces without connecting. Each has its own scratch root,
    /// since runs of different stores may share a run and step name.
    fn default() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        #[cfg(unix)]
        // SAFETY: getuid cannot fail and touches no memory.
        #[allow(unsafe_code)]
        let user = unsafe { libc::getuid() }.to_string();
        #[cfg(not(unix))]
        let user = std::env::var("USERNAME").unwrap_or_default();
        let base = std::env::temp_dir().join(format!("atomic-scratch-{user}"));
        Self {
            scratch_root: base.join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )),
            tmp: base.join(".tmp"),
            confine: None,
            denying: Vec::new(),
            checked: Arc::default(),
        }
    }
}

impl ExecutionBackend for LocalBackend {
    /// States durable workspaces, after checking that its scratch root is
    /// outside the system's temporary directories, which sandboxed turns may
    /// write, and removing what attempts of processes that no longer run left
    /// there. Each workspace it opens is also checked against the turn's own
    /// writable directories.
    fn connect(&self) -> Result<Statement, ExecError> {
        self.check_scratch_root()?;
        workspace::sweep_local(&self.scratch_root);
        // No turn of this backend runs yet: what earlier turns left in the
        // shared temporary directory goes.
        workspace::empty_dir(&self.tmp);
        Ok(Self::STATEMENT)
    }

    /// A workspace with files is where confined commands, or a turn's
    /// commands, will run: until its confinement has been seen to work here
    /// ([`Confine::check`]), each such open checks it first and is refused,
    /// before anything runs, if it does not: with
    /// [`ExecError::Unavailable`] when the check may pass later (no room, no
    /// process to spare), or [`ExecError::Unconfinable`] when the machine
    /// cannot confine commands at all. Only a check that passed is kept. A
    /// workspace with no files, such as a lite atom's, needs no check.
    fn open(&self, request: &Open) -> Result<Workspace, ExecError> {
        if let (Some(confine), Some(_)) = (&self.confine, &request.source)
            && self.checked.get().is_none()
        {
            workspace::ensure_private(&self.tmp).map_err(|e| {
                let message = format!("create {}: {e}", self.tmp.display());
                if workspace::transient(&e) {
                    ExecError::Unavailable {
                        label: request.label.clone(),
                        message,
                    }
                } else {
                    ExecError::Workspace {
                        label: request.label.clone(),
                        message,
                    }
                }
            })?;
            confine.check(&self.tmp)?;
            let _ = self.checked.set(());
        }
        workspace::open_local(&self.scratch_root, &self.tmp, &self.denying, request)
    }

    fn close(&self, workspace: &Workspace) -> Result<(), ExecError> {
        workspace::close_local(workspace)
    }

    /// The exchange in the workspace's scratch directory, on Unix
    /// ([`exchange::LocalExchange`]); none for a workspace with no scratch
    /// directory, or elsewhere.
    fn exchange(
        &self,
        workspace: &Workspace,
    ) -> Result<Option<Box<dyn exchange::Exchange>>, ExecError> {
        #[cfg(unix)]
        if let Some(scratch) = &workspace.scratch {
            let made =
                exchange::LocalExchange::create(scratch).map_err(|e| ExecError::Workspace {
                    label: workspace.label.clone(),
                    message: format!("make its exchange in {}: {e}", scratch.display()),
                })?;
            return Ok(Some(Box::new(made)));
        }
        let _ = workspace;
        Ok(None)
    }

    /// Runs `command` under this backend's confinement, if it has one, in a
    /// process group of its own.
    fn run(&self, command: &Command) -> Result<Completed, ExecError> {
        let (command, _held) = self.prepare(command)?;
        Self::run_in(process::Group::Own, &command)
    }
}

impl LocalBackend {
    /// Run `command` as it is, unconfined, in `group`: what
    /// [`ExecutionBackend::run`] does once the command is confined.
    pub fn run_in(group: process::Group, command: &Command) -> Result<Completed, ExecError> {
        let program = process::resolve(&command.program).map_err(|message| ExecError::Start {
            program: command.program.clone(),
            message,
        })?;
        process::run_in_with_secret(
            group,
            &program,
            &command.args,
            Some(&command.cwd),
            &command.env,
            &command.secret_env,
            &command.stdin,
            process::Limits {
                time: command.time_limit,
                stdout: command.output_limit,
                stderr: process::STDERR_LIMIT,
            },
        )
    }
}

#[cfg(all(test, unix))]
mod tests;
