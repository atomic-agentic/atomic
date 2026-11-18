// SPDX-License-Identifier: Apache-2.0

//! A bounded child process: no shell, a cleared environment, a time limit,
//! and capped output. [`crate::LocalBackend`] runs commands with it, and so
//! does atomic's own trusted tooling, such as the Astro compiler, which is
//! not a runner and does not go through the backend trait.

use crate::{Completed, ExecError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// The most stderr kept, in bytes; anything beyond it is dropped.
pub const STDERR_LIMIT: usize = 65_536;

/// How long a process may run and how much it may write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Wall-clock time before it is killed.
    pub time: Duration,
    /// The most stdout, in bytes; more is an error.
    pub stdout: usize,
    /// The most stderr kept, in bytes; more is dropped.
    pub stderr: usize,
}

/// `program` as a path: as given when it names a path, else the first match
/// on this process's `PATH`. The child's own environment never has `PATH`
/// unless the caller passes one, so the lookup happens here.
pub fn resolve(program: &str) -> Result<PathBuf, String> {
    if program.contains('/') {
        return Ok(PathBuf::from(program));
    }
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(program))
                .find(|file| file.is_file())
        })
        .ok_or_else(|| format!("{program} is not on PATH"))
}

/// Run `program` with `args`, in `cwd` (or the current directory), with
/// exactly `env`, `stdin` on its standard input, within `limits`.
///
/// A process that exits, with any code, has completed. A broken pipe on its
/// stdin only means it stopped reading; its exit code and stderr say why.
pub fn run(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &[(String, String)],
    stdin: &[u8],
    limits: Limits,
) -> Result<Completed, ExecError> {
    let name = program.display().to_string();
    let io = |e: std::io::Error| ExecError::Io {
        program: name.clone(),
        message: e.to_string(),
    };
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|e| ExecError::Start {
        program: name.clone(),
        message: e.to_string(),
    })?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| io(std::io::Error::other("no stdin")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io(std::io::Error::other("no stdout")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io(std::io::Error::other("no stderr")))?;
    let request = stdin.to_vec();
    let writer = std::thread::spawn(move || input.write_all(&request));
    let stdout_limit = limits.stdout as u64 + 1;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(stdout_limit)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let stderr_limit = limits.stderr as u64;
    let errors = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stderr = stderr;
        (&mut stderr).take(stderr_limit).read_to_end(&mut bytes)?;
        // Drain the rest so the process never blocks on a full pipe.
        std::io::copy(&mut stderr, &mut std::io::sink())?;
        Ok::<_, std::io::Error>(bytes)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait().map_err(io)? {
            Some(status) => break status,
            None if started.elapsed() > limits.time => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ExecError::TimedOut {
                    program: name,
                    seconds: limits.time.as_secs(),
                });
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let stdout = reader
        .join()
        .map_err(|_| io(std::io::Error::other("stdout reader panicked")))?
        .map_err(io)?;
    let stderr = errors
        .join()
        .map_err(|_| io(std::io::Error::other("stderr reader panicked")))?
        .map_err(io)?;
    let written = writer
        .join()
        .map_err(|_| io(std::io::Error::other("stdin writer panicked")))?;
    if stdout.len() > limits.stdout {
        return Err(ExecError::TooLarge {
            program: name,
            limit: limits.stdout,
        });
    }
    if let Err(e) = written
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(io(e));
    }
    Ok(Completed {
        code: status.code(),
        stdout,
        stderr,
    })
}
