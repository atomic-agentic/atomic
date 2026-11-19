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

pub mod process;
pub mod quiescence;
pub mod runner;
pub mod workspace;

pub use workspace::{Open, Sandbox, Shell, Workspace};

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
#[derive(Clone, Debug, PartialEq, Eq)]
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
}

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
}

/// Where atoms run. The control plane reaches runners only through this.
///
/// Every method may block: callers on an async executor call it on the
/// blocking pool.
pub trait ExecutionBackend: Send + Sync {
    /// Connect, and state what the control plane may plan on.
    fn connect(&self) -> Result<Statement, ExecError>;

    /// Open a workspace for one attempt of one atom run. Each call opens a
    /// workspace of its own, even for a request opened before: an attempt
    /// that runs again starts in a new one.
    fn open(&self, request: &Open) -> Result<Workspace, ExecError>;

    /// Run `command` to completion in a workspace this backend opened. A
    /// nonzero exit is a [`Completed`] with that code, not an error: the
    /// caller decides what it means.
    fn run(&self, command: &Command) -> Result<Completed, ExecError>;

    /// Release `workspace`. Closing one that is closed already succeeds.
    fn close(&self, workspace: &Workspace) -> Result<(), ExecError>;
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
            inner: Arc::new(LocalBackend),
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
}

/// The backend on this machine, in the control plane's process: a
/// workspace is the source directory itself, a model turn's shell and file
/// work runs under Codex's own sandbox, and commands run as child processes.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalBackend;

impl LocalBackend {
    /// What it states: a workspace is a directory on this machine's disk,
    /// which outlives any process, so it is durable.
    pub const STATEMENT: Statement = Statement {
        durability: Durability::Durable,
    };
}

impl ExecutionBackend for LocalBackend {
    fn connect(&self) -> Result<Statement, ExecError> {
        Ok(Self::STATEMENT)
    }

    fn open(&self, request: &Open) -> Result<Workspace, ExecError> {
        workspace::open_local(request)
    }

    fn close(&self, workspace: &Workspace) -> Result<(), ExecError> {
        workspace::close_local(workspace)
    }

    fn run(&self, command: &Command) -> Result<Completed, ExecError> {
        let program = process::resolve(&command.program).map_err(|message| ExecError::Start {
            program: command.program.clone(),
            message,
        })?;
        process::run(
            &program,
            &command.args,
            Some(&command.cwd),
            &command.env,
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
