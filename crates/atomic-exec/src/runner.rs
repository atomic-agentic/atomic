// SPDX-License-Identifier: Apache-2.0

//! The runner role, across a process boundary.
//!
//! `atomic runner` is the same binary as `atomic serve` in its other role: it
//! takes one [`Request`] on stdin, runs the command on its own machine, writes
//! one [`Response`] on stdout and exits with a code from a small, fixed set.
//! [`ProcessRunner`] is the control plane's side: an [`ExecutionBackend`]
//! that starts a runner process for each command and reads what it did from
//! the response and the exit code, never from stderr, which it only attaches
//! to a failure.
//!
//! The exit codes are separated by remedy, not severity ([`Exit`]):
//!
//! | Code | Class | Control-plane action |
//! | --- | --- | --- |
//! | 0 | clean | read the response |
//! | 64 | usage error | fail the attempt; do not retry |
//! | 78 | configuration or credential error | fail the attempt; do not retry until fixed |
//! | 75 | workspace lease held elsewhere | wait, then try again, a bounded number of times |
//! | any other, or a signal | crash | report the outcome as unknown; never run it again |
//!
//! A runner that crashes, or exits cleanly with no response it can read, may
//! have run the command, in part or in full, before it ended. The control
//! plane cannot know, so [`ProcessRunner`] never runs the command again: it
//! returns [`ExecError::Lost`], and the durable step, which knows what the
//! command declares about its effects, decides whether it may run again.
//! Only the codes that mean the command never started are retried.
//!
//! The runner process starts with only the environment the control plane
//! gives it, so it never sees the control plane's own credentials, and the
//! command it runs gets exactly the environment in its request.

use crate::process::{self, Limits};
use crate::{Command, Completed, ExecError, ExecutionBackend, LocalBackend};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

/// The version of the request and response format.
pub const PROTOCOL: u32 = 1;

/// The exit codes `atomic runner` uses.
pub mod code {
    /// It ran the request and wrote the response.
    pub const CLEAN: i32 = 0;
    /// It failed in a way that says nothing about the request, which may have
    /// run. Any code not listed here means the same.
    pub const CRASH: i32 = 70;
    /// The request or the command line was not one it accepts.
    pub const USAGE: i32 = 64;
    /// The workspace it was asked to use is leased by another runner.
    pub const LEASE_HELD: i32 = 75;
    /// Its configuration or a credential it needs is missing or wrong.
    pub const CONFIG: i32 = 78;
}

/// How a runner process ended, by remedy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// It ran the request; the response says how the command ended.
    Clean,
    /// It crashed, or ended with a code that is not in the contract.
    Crash,
    /// It was used wrongly.
    Usage,
    /// Its configuration or a credential is wrong.
    Config,
    /// Another runner holds the workspace's lease.
    LeaseHeld,
}

/// What the control plane does about an [`Exit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Read the response.
    Accept,
    /// Report that the command may have run and how it ended is unknown.
    Unknown,
    /// Fail the attempt; retrying cannot help.
    Fail,
    /// Fail the attempt; retrying cannot help until the configuration is fixed.
    FailUntilFixed,
    /// Wait for the lease, then try again, up to a bound.
    Defer,
}

impl Exit {
    /// The class of an exit code; a signal or an unknown code is a crash.
    pub fn classify(code: Option<i32>) -> Self {
        match code {
            Some(code::CLEAN) => Self::Clean,
            Some(code::USAGE) => Self::Usage,
            Some(code::CONFIG) => Self::Config,
            Some(code::LEASE_HELD) => Self::LeaseHeld,
            _ => Self::Crash,
        }
    }

    /// The code a runner exits with for this class.
    pub fn code(self) -> i32 {
        match self {
            Self::Clean => code::CLEAN,
            Self::Crash => code::CRASH,
            Self::Usage => code::USAGE,
            Self::Config => code::CONFIG,
            Self::LeaseHeld => code::LEASE_HELD,
        }
    }

    /// What the control plane does about it.
    pub fn action(self) -> Action {
        match self {
            Self::Clean => Action::Accept,
            Self::Crash => Action::Unknown,
            Self::Usage => Action::Fail,
            Self::Config => Action::FailUntilFixed,
            Self::LeaseHeld => Action::Defer,
        }
    }

    /// Its name, as failures report it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Crash => "crash",
            Self::Usage => "usage error",
            Self::Config => "configuration error",
            Self::LeaseHeld => "lease held elsewhere",
        }
    }
}

/// One command, as it crosses to a runner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// [`PROTOCOL`].
    pub protocol: u32,
    /// The program, found on the runner's `PATH` when it has no `/`.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// The directory it runs in, on the runner.
    pub cwd: PathBuf,
    /// Its whole environment.
    pub env: Vec<(String, String)>,
    /// Its stdin, hex-encoded.
    pub stdin: String,
    /// Its time limit, in milliseconds.
    pub time_limit_ms: u64,
    /// The most stdout it may write, in bytes.
    pub output_limit: usize,
}

impl Request {
    /// The request for `command`.
    pub fn new(command: &Command) -> Self {
        Self {
            protocol: PROTOCOL,
            program: command.program.clone(),
            args: command.args.clone(),
            cwd: command.cwd.clone(),
            env: command.env.clone(),
            stdin: hex(&command.stdin),
            time_limit_ms: u64::try_from(command.time_limit.as_millis()).unwrap_or(u64::MAX),
            output_limit: command.output_limit,
        }
    }

    /// The command it carries.
    pub fn command(&self) -> Result<Command, String> {
        Ok(Command {
            program: self.program.clone(),
            args: self.args.clone(),
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            stdin: unhex(&self.stdin).ok_or("stdin is not hex")?,
            time_limit: Duration::from_millis(self.time_limit_ms),
            output_limit: self.output_limit,
            // Confined, if at all, before it was sent.
            writable: Vec::new(),
            network: Vec::new(),
        })
    }
}

/// What a runner did with a [`Request`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Response {
    /// The command ran to an exit.
    Completed {
        /// Its exit code, or `None` if a signal ended it.
        code: Option<i32>,
        /// Its stdout, hex-encoded.
        stdout: String,
        /// Its stderr, hex-encoded.
        stderr: String,
    },
    /// The command could not be run to completion.
    Failed {
        /// Why.
        error: ExecError,
    },
}

/// The runner's side: read one request from `input`, run it on `backend`,
/// write the response to `output`, and return the code to exit with. Why a
/// request was refused goes to `errors`, which the control plane captures and
/// never parses.
pub fn serve(
    input: &mut dyn Read,
    output: &mut dyn Write,
    errors: &mut dyn Write,
    backend: &dyn ExecutionBackend,
) -> i32 {
    let mut bytes = Vec::new();
    if let Err(e) = input.read_to_end(&mut bytes) {
        let _ = writeln!(errors, "atomic runner: cannot read the request: {e}");
        return code::CRASH;
    }
    let request: Request = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(errors, "atomic runner: the request is not one I read: {e}");
            return code::USAGE;
        }
    };
    if request.protocol != PROTOCOL {
        let _ = writeln!(
            errors,
            "atomic runner: request protocol {} is not {PROTOCOL}; run a runner from the same atomic release",
            request.protocol
        );
        return code::USAGE;
    }
    let command = match request.command() {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(errors, "atomic runner: {e}");
            return code::USAGE;
        }
    };
    let response = match backend.run(&command) {
        Ok(done) => Response::Completed {
            code: done.code,
            stdout: hex(&done.stdout),
            stderr: hex(&done.stderr),
        },
        Err(error) => Response::Failed { error },
    };
    let written = serde_json::to_writer(&mut *output, &response)
        .map_err(std::io::Error::other)
        .and_then(|()| output.flush());
    match written {
        Ok(()) => code::CLEAN,
        Err(e) => {
            let _ = writeln!(errors, "atomic runner: cannot write the response: {e}");
            code::CRASH
        }
    }
}

/// The runner's side on this process's stdin, stdout and stderr, running
/// commands as child processes in the runner's own process group, so that
/// the control plane stops a command, with everything it started, by
/// stopping its runner's group.
pub fn serve_stdio() -> i32 {
    process::ignore_stop_signals();
    serve(
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
        &InRunnerGroup(LocalBackend::default()),
    )
}

/// [`LocalBackend`], with each command in the runner's process group. The
/// command comes already confined: the control plane confines it before it
/// crosses to the runner.
struct InRunnerGroup(LocalBackend);

impl ExecutionBackend for InRunnerGroup {
    fn connect(&self) -> Result<crate::Statement, ExecError> {
        self.0.connect()
    }
    fn open(&self, request: &crate::Open) -> Result<crate::Workspace, ExecError> {
        self.0.open(request)
    }
    fn run(&self, command: &Command) -> Result<Completed, ExecError> {
        LocalBackend::run_in(process::Group::Inherit, command)
    }
    fn close(&self, workspace: &crate::Workspace) -> Result<(), ExecError> {
        self.0.close(workspace)
    }
}

/// The control plane's side: an [`ExecutionBackend`] that runs each command
/// in a runner process of its own, started as `program args…`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessRunner {
    /// The runner executable, such as atomic itself.
    pub program: PathBuf,
    /// Its arguments, such as `["runner"]`.
    pub args: Vec<String>,
    /// The runner process's whole environment. Nothing else is inherited.
    pub env: Vec<(String, String)>,
    /// How many more times a command is tried after a held lease, which
    /// means it never started. A crash is never retried.
    pub retries: u32,
    /// How long to wait for a held lease before trying again.
    pub lease_wait: Duration,
    /// Where its workspaces are opened: its runners run on this machine.
    pub workspaces: LocalBackend,
}

/// How much longer than its command a runner may take before it is stopped.
const RUNNER_GRACE: Duration = Duration::from_secs(30);

impl ProcessRunner {
    /// Runners started as `program runner`, with only `PATH` from this
    /// process, so a command's program resolves as it would locally.
    pub fn atomic(program: PathBuf) -> Self {
        Self {
            program,
            args: vec!["runner".into()],
            env: std::env::var("PATH")
                .map(|path| vec![("PATH".to_owned(), path)])
                .unwrap_or_default(),
            retries: 2,
            lease_wait: Duration::from_secs(1),
            workspaces: LocalBackend::default(),
        }
    }

    /// Start one runner on `request` and classify how it ended.
    fn attempt(&self, command: &Command, request: &[u8]) -> Result<(Exit, Completed), ExecError> {
        // The runner's group holds its command too: whatever is left in it
        // when the runner ends goes with it.
        let done = process::run_in(
            process::Group::Own,
            &self.program,
            &self.args,
            None,
            &self.env,
            request,
            Limits {
                time: command.time_limit.saturating_add(RUNNER_GRACE),
                // Hex doubles the command's output; the rest is the envelope
                // and its stderr.
                stdout: command
                    .output_limit
                    .saturating_mul(2)
                    .saturating_add(4 * process::STDERR_LIMIT),
                stderr: process::STDERR_LIMIT,
            },
        )?;
        Ok((Exit::classify(done.code), done))
    }

    fn lost(&self, command: &Command, runner: &Completed, why: &str) -> ExecError {
        ExecError::Lost {
            program: command.program.clone(),
            code: runner.code,
            message: why.to_owned(),
            stderr: String::from_utf8_lossy(&runner.stderr).trim().to_owned(),
        }
    }

    fn failure(&self, command: &Command, exit: Exit, runner: &Completed, why: &str) -> ExecError {
        ExecError::Runner {
            program: command.program.clone(),
            class: exit.name().to_owned(),
            code: runner.code,
            message: why.to_owned(),
            stderr: String::from_utf8_lossy(&runner.stderr).trim().to_owned(),
        }
    }
}

impl ExecutionBackend for ProcessRunner {
    /// Its workspaces are [`LocalBackend`]'s, so it states what that does.
    fn connect(&self) -> Result<crate::Statement, ExecError> {
        self.workspaces.connect()
    }

    /// Its runners run on this machine, so a workspace is opened here, as
    /// [`LocalBackend`] opens one, and each command a runner runs finds it.
    fn open(&self, request: &crate::Open) -> Result<crate::Workspace, ExecError> {
        self.workspaces.open(request)
    }

    fn close(&self, workspace: &crate::Workspace) -> Result<(), ExecError> {
        self.workspaces.close(workspace)
    }

    /// The command is confined here, before it crosses, as its workspaces'
    /// backend confines it: the runner runs what it is sent.
    fn run(&self, command: &Command) -> Result<Completed, ExecError> {
        let command = &self.workspaces.prepare(command)?;
        let request = serde_json::to_vec(&Request::new(command)).map_err(|e| ExecError::Io {
            program: command.program.clone(),
            message: format!("encode the runner request: {e}"),
        })?;
        let mut tries = 0;
        loop {
            let (exit, runner) = match self.attempt(command, &request) {
                Ok(ended) => ended,
                // A runner that never started ran nothing.
                Err(e @ ExecError::Start { .. }) => return Err(e),
                // One stopped or unreadable after it started may have run it.
                Err(e) => {
                    return Err(ExecError::Lost {
                        program: command.program.clone(),
                        code: None,
                        message: e.to_string(),
                        stderr: String::new(),
                    });
                }
            };
            let retry = tries < self.retries;
            tries += 1;
            match exit.action() {
                Action::Accept => {
                    // A clean exit with no response it can read is a runner
                    // fault after the command may have run.
                    return match serde_json::from_slice::<Response>(&runner.stdout) {
                        Ok(Response::Completed {
                            code,
                            stdout,
                            stderr,
                        }) => match (unhex(&stdout), unhex(&stderr)) {
                            (Some(stdout), Some(stderr)) => Ok(Completed {
                                code,
                                stdout,
                                stderr,
                            }),
                            _ => Err(self.lost(command, &runner, "its response is not hex")),
                        },
                        Ok(Response::Failed { error }) => Err(error),
                        Err(e) => Err(self.lost(
                            command,
                            &runner,
                            &format!("it exited cleanly with no response it could read: {e}"),
                        )),
                    };
                }
                Action::Unknown => {
                    return Err(self.lost(command, &runner, "it crashed"));
                }
                Action::Defer if retry => std::thread::sleep(self.lease_wait),
                Action::Defer => {
                    return Err(self.failure(
                        command,
                        exit,
                        &runner,
                        &format!("the lease was still held after {tries} tries"),
                    ));
                }
                Action::Fail => {
                    return Err(self.failure(command, exit, &runner, "retrying cannot help"));
                }
                Action::FailUntilFixed => {
                    return Err(self.failure(
                        command,
                        exit,
                        &runner,
                        "fix the runner's configuration, then run the step again",
                    ));
                }
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0xf)]));
    }
    out
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

#[cfg(all(test, unix))]
mod tests;
