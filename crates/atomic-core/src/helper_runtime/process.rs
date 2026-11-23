// SPDX-License-Identifier: Apache-2.0

//! Each helper call runs in a child process of its own.
//!
//! Some failures inside V8 cannot be caught: an "invalid size" error or an
//! allocation V8 cannot satisfy aborts the process it runs in, whatever the
//! isolate's limits. Memory V8 allocates outside its heap (flattened
//! strings, compiled regular expressions, parser buffers) is also outside
//! the heap limit. A process boundary contains both: the child runs one
//! call under an operating-system limit on its writable memory, the host
//! kills it at the call's deadline, and a child that dies takes nothing
//! with it.
//!
//! The child is the atomic binary itself, started with [`CHILD_ARG`] and a
//! cleared environment, so the host's timezone and locale never reach it.
//! It reads one request on stdin and writes one result line on stdout.

#![allow(unsafe_code)]

use super::{Captured, Clock, Controls, Failure, Limits};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The argument that makes the atomic binary serve one helper call.
pub const CHILD_ARG: &str = "__atomic-helper";

/// The prefix of the child's result line, so other output on stdout (a test
/// harness's) is ignored.
const RESULT: &str = "atomic-helper-result ";

/// Writable memory the child needs besides the call's own limits: V8's
/// code, its platform threads and the binary's own data.
const BASE_MEMORY: u64 = 384 * 1024 * 1024;

/// Time the child may take to start before its deadline begins.
const STARTUP: Duration = Duration::from_secs(2);

/// The largest request the child reads.
const REQUEST_LIMIT: u64 = 64 * 1024 * 1024;

/// How the host starts a helper child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCommand {
    /// The executable: an atomic binary.
    pub program: PathBuf,
    /// Its arguments, [`CHILD_ARG`] for an atomic binary.
    pub args: Vec<String>,
    /// The child's whole environment.
    pub env: Vec<(String, String)>,
}

static HOST: OnceLock<HostCommand> = OnceLock::new();

/// Sets how helper children start, for a process that is not the atomic
/// binary (an integration test, say). The first setting stays.
pub fn set_host_command(command: HostCommand) {
    let _ = HOST.set(command);
}

#[cfg(not(test))]
fn default_host() -> Result<HostCommand, Failure> {
    Ok(HostCommand {
        program: std::env::current_exe()
            .map_err(|e| Failure::Host(format!("cannot find the atomic binary: {e}")))?,
        args: vec![CHILD_ARG.to_owned()],
        env: Vec::new(),
    })
}

// A unit-test binary is not the atomic binary: it serves calls from an
// ignored test instead.
#[cfg(test)]
fn default_host() -> Result<HostCommand, Failure> {
    Ok(HostCommand {
        program: std::env::current_exe().map_err(|e| Failure::Host(e.to_string()))?,
        args: [
            "--exact",
            "helper_runtime::process::tests::child",
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ]
        .map(str::to_owned)
        .to_vec(),
        env: vec![(tests::CHILD_ENV.to_owned(), "1".to_owned())],
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Request {
    captured: Captured,
    export: String,
    args: Value,
    limits: Limits,
    clock: Clock,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Response {
    Ok(Value),
    Failure(Failure),
}

/// The writable memory a child may hold for a call with `limits`.
fn memory_limit(limits: &Limits) -> u64 {
    // The heap may grow to twice its limit while a call unwinds (see the
    // heap callback); results and errors are held a few times over.
    BASE_MEMORY
        + 2 * limits.heap_bytes as u64
        + limits.array_buffer_bytes as u64
        + 4 * (limits.output_bytes + limits.error_bytes) as u64
}

/// Runs one call in a child process.
pub(super) fn invoke(
    captured: &Captured,
    export: &str,
    args: &Value,
    limits: Limits,
    clock: Clock,
) -> Result<Value, Failure> {
    let command = match HOST.get() {
        Some(command) => command.clone(),
        None => default_host()?,
    };
    run(&command, captured, export, args, limits, clock)
}

fn run(
    command: &HostCommand,
    captured: &Captured,
    export: &str,
    args: &Value,
    limits: Limits,
    clock: Clock,
) -> Result<Value, Failure> {
    let host = |message: String| Failure::Host(message);
    let request = serde_json::to_vec(&Request {
        captured: captured.clone(),
        export: export.to_owned(),
        args: args.clone(),
        limits,
        clock,
    })
    .map_err(|e| host(e.to_string()))?;
    let mut process = std::process::Command::new(&command.program);
    process
        .args(&command.args)
        .env_clear()
        .envs(command.env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::compiler_process::new_session(&mut process);
    let mut child = process.spawn().map_err(|e| {
        host(format!(
            "cannot start the helper process {}: {e}",
            command.program.display()
        ))
    })?;
    let mut stdin = child.stdin.take().ok_or_else(|| host("no stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| host("no stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| host("no stderr".into()))?;
    // Writes and reads happen on their own threads, so a child that stops
    // reading or floods its output cannot hold the host past the deadline.
    std::thread::spawn(move || {
        let _ = stdin.write_all(&request);
    });
    let output_limit = (limits.output_bytes + limits.error_bytes) as u64 + 1024 * 1024;
    let out = std::thread::spawn(move || bounded(stdout, output_limit));
    let err = std::thread::spawn(move || bounded(stderr, 64 * 1024));
    let deadline = Instant::now() + STARTUP + limits.deadline;
    let status = loop {
        match child.try_wait().map_err(|e| host(e.to_string()))? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Deadline);
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        // The child's CPU limit: a call that kept the processor past its
        // deadline.
        if status.signal() == Some(libc::SIGXCPU) {
            return Err(Failure::Deadline);
        }
    }
    let text = String::from_utf8_lossy(&out);
    if let Some(line) = text.lines().find_map(|line| line.strip_prefix(RESULT)) {
        return match serde_json::from_str(line) {
            Ok(Response::Ok(value)) => Ok(value),
            Ok(Response::Failure(failure)) => Err(failure),
            Err(e) => Err(host(format!("the helper process answered badly: {e}"))),
        };
    }
    let stderr = String::from_utf8_lossy(&err);
    // V8 reports what it could not allocate and aborts; the operating
    // system's limit on the child's memory makes allocations fail the same
    // way. Either is the call exhausting its memory.
    let exhausted = [
        "out of memory",
        "invalid size",
        "allocation failed",
        "Reached heap limit",
        "memory allocation of",
    ]
    .iter()
    .any(|sign| stderr.contains(sign));
    if exhausted {
        return Err(Failure::Heap);
    }
    let tail: String = stderr
        .trim()
        .chars()
        .rev()
        .take(2000)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Err(host(format!(
        "the helper process ended without a result ({status}){}",
        if tail.is_empty() {
            String::new()
        } else {
            format!(": {tail}")
        }
    )))
}

fn bounded(mut reader: impl Read, limit: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = (&mut reader).take(limit).read_to_end(&mut bytes);
    let _ = std::io::copy(&mut reader, &mut std::io::sink());
    bytes
}

/// Serves one helper call: reads the request on stdin, runs it under the
/// child's limits and writes the result line. Returns the exit status.
pub fn serve() -> i32 {
    let mut request = Vec::new();
    if std::io::stdin()
        .take(REQUEST_LIMIT)
        .read_to_end(&mut request)
        .is_err()
    {
        return 2;
    }
    let response = match serde_json::from_slice::<Request>(&request) {
        Ok(request) => {
            let result = limit_self(&request.limits).and_then(|()| {
                super::invoke_with(
                    &request.captured,
                    &request.export,
                    &request.args,
                    request.limits,
                    request.clock,
                    Controls::default(),
                )
            });
            match result {
                Ok(value) => Response::Ok(value),
                Err(failure) => Response::Failure(failure),
            }
        }
        Err(e) => Response::Failure(Failure::Host(format!("bad helper request: {e}"))),
    };
    let Ok(line) = serde_json::to_string(&response) else {
        return 2;
    };
    let mut stdout = std::io::stdout().lock();
    if writeln!(stdout, "{RESULT}{line}")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return 2;
    }
    0
}

/// Limits this process: its writable memory, its CPU time, and no core
/// file when V8 aborts.
fn limit_self(limits: &Limits) -> Result<(), Failure> {
    let cpu = limits.deadline.as_secs() + 2;
    for (resource, value) in [
        (libc::RLIMIT_DATA, memory_limit(limits)),
        (libc::RLIMIT_CPU, cpu),
        (libc::RLIMIT_CORE, 0),
    ] {
        let limit = libc::rlimit {
            rlim_cur: value as libc::rlim_t,
            rlim_max: value as libc::rlim_t,
        };
        // SAFETY: `limit` is a valid rlimit that outlives the call; lowering
        // this process's own limits has no other effect on memory safety.
        if unsafe { libc::setrlimit(resource, &limit) } != 0 {
            let error = std::io::Error::last_os_error();
            #[cfg(target_os = "macos")]
            if resource == libc::RLIMIT_DATA {
                let details = crate::darwin_memory::rejected_data_limit(value, &error);
                return Err(Failure::Host(format!(
                    "cannot limit the helper process: {details}"
                )));
            }
            return Err(Failure::Host(format!(
                "cannot limit the helper process: {error}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
