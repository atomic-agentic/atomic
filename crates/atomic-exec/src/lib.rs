// SPDX-License-Identifier: Apache-2.0

//! The `ExecutionBackend` trait between the control plane and its runners.
//!
//! The control plane drives runners only through this trait, even when both run
//! in one process, so moving runners elsewhere never changes workflow code.
//!
//! The trait starts with what an exec atom needs: run one command, with no
//! shell, in a given directory, with exactly the environment it is given, one
//! input on stdin and bounded output. [`LocalBackend`] runs it as a child
//! process on this machine. Workspaces and a model turn's shell and file work
//! join the trait as they move behind it.
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
}

/// Where commands run. The control plane reaches runners only through this.
pub trait ExecutionBackend: Send + Sync {
    /// Run `command` to completion. A nonzero exit is a [`Completed`] with
    /// that code, not an error: the caller decides what it means.
    fn run(&self, command: &Command) -> Result<Completed, ExecError>;
}

/// A backend shared by everything that runs commands, such as the runtimes
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
    fn run(&self, command: &Command) -> Result<Completed, ExecError> {
        self.0.run(command)
    }
}

/// Runs commands as child processes on this machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalBackend;

impl ExecutionBackend for LocalBackend {
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
