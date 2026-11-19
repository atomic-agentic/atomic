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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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

/// A backend shared by everything that runs atoms, such as the runtimes
/// atoms run on. The default is [`LocalBackend`].
#[derive(Clone)]
pub struct Backend(Arc<dyn ExecutionBackend>);

impl Backend {
    /// Share `backend`.
    pub fn new(backend: impl ExecutionBackend + 'static) -> Self {
        Self(Arc::new(backend))
    }

    /// The shared backend itself.
    pub fn shared(&self) -> Arc<dyn ExecutionBackend> {
        self.0.clone()
    }
}

impl Default for Backend {
    fn default() -> Self {
        Self::new(LocalBackend)
    }
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Backend(..)")
    }
}

impl ExecutionBackend for Backend {
    fn open(&self, request: &Open) -> Result<Workspace, ExecError> {
        self.0.open(request)
    }

    fn run(&self, command: &Command) -> Result<Completed, ExecError> {
        self.0.run(command)
    }

    fn close(&self, workspace: &Workspace) -> Result<(), ExecError> {
        self.0.close(workspace)
    }
}

/// The backend on this machine, in the control plane's process: a
/// workspace is the source directory itself, a model turn's shell and file
/// work runs under Codex's own sandbox, and commands run as child processes.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalBackend;

impl ExecutionBackend for LocalBackend {
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
