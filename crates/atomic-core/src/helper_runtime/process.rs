// SPDX-License-Identifier: Apache-2.0

//! Each helper call runs in a child process of its own.
//!
//! A child runs under memory, CPU and wall-time limits. The host reports
//! allocation failures and termination without sharing guest state. On macOS
//! the host holds the process memory limit (see `child_memory`).
//! The child is the atomic binary itself, started with [`CHILD_ARG`] and a
//! cleared environment, so the host's timezone and locale never reach it.
//! It reads one request on stdin and writes one result line on stdout.

#![allow(unsafe_code)]

use super::{Captured, Clock, Controls, Failure, Limits};
use crate::child_memory::{Breach, INTERVAL, Watch};
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

/// Writable memory the child needs besides the call's own limits: the engine,
/// its runtime threads and the binary's own data.
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

/// The process memory budget and the maximum bytes read from stdout.
fn byte_limits(limits: &Limits) -> Result<(u64, u64), Failure> {
    let compute = || {
        let heap = u64::try_from(limits.heap_bytes).ok()?.checked_mul(2)?;
        let buffers = u64::try_from(limits.array_buffer_bytes).ok()?;
        let result = u64::try_from(limits.output_bytes.checked_add(limits.error_bytes)?).ok()?;
        // The heap may grow while a call unwinds, and the result and error
        // bytes can be held several times over.
        let memory = BASE_MEMORY
            .checked_add(heap)?
            .checked_add(buffers)?
            .checked_add(result.checked_mul(4)?)?;
        let stdout = result.checked_add(1024 * 1024)?;
        Some((memory, stdout))
    };
    compute().ok_or_else(|| Failure::Host("helper byte limits are too large".into()))
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
    let (memory_limit, output_limit) = byte_limits(&limits)?;
    let allowed_time = STARTUP
        .checked_add(limits.deadline)
        .filter(|time| Instant::now().checked_add(*time).is_some())
        .ok_or_else(|| host("helper deadline is too large".into()))?;
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
    #[cfg(target_os = "macos")]
    let mut supervisor = crate::worker_lifetime::Supervisor::prepare(&mut process)
        .map_err(|e| host(format!("cannot supervise the helper process: {e}")))?;
    #[cfg(windows)]
    let (mut child, _job) =
        crate::windows_worker::spawn(&mut process, memory_limit, cpu_seconds(&limits)?)
            .map_err(|e| host(format!("cannot start the helper in its job: {e}")))?;
    #[cfg(not(windows))]
    let mut child = process.spawn().map_err(|e| {
        host(format!(
            "cannot start the helper process {}: {e}",
            command.program.display()
        ))
    })?;
    #[cfg(target_os = "macos")]
    supervisor.child_spawned();
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
    let out = std::thread::spawn(move || bounded(stdout, output_limit));
    let err = std::thread::spawn(move || bounded(stderr, 64 * 1024));
    let mut watch = Watch::for_child(memory_limit);
    // The startup and call allowance is for the child. Parent-side request
    // serialization and spawn setup are outside it.
    let Some(deadline) = Instant::now().checked_add(allowed_time) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(host("helper deadline is too large".into()));
    };
    let status = loop {
        match child.try_wait().map_err(|e| host(e.to_string()))? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Deadline);
            }
            None => {}
        }
        // Where the host holds the child's memory limit, a child found past
        // it has exhausted its memory, as one the kernel refused has.
        let read = match watch.as_mut() {
            Some(watch) => watch.check(&mut child),
            None => Ok(()),
        };
        if let Err(breach) = read {
            let _ = child.kill();
            let _ = child.wait();
            return Err(match breach {
                Breach::Exceeded => Failure::Heap,
                Breach::Unread(error) => host(format!(
                    "cannot read the helper process's memory to limit it: {error}"
                )),
            });
        }
        std::thread::sleep(INTERVAL);
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
    match status.code() {
        Some(crate::js_budget::HEAP_EXIT) => return Err(Failure::Heap),
        Some(crate::js_budget::BUFFER_EXIT) => return Err(Failure::ArrayBufferLimit),
        Some(crate::js_budget::DEADLINE_EXIT) => return Err(Failure::Deadline),
        _ => {}
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
    // Rust reports unsatisfied allocations on stderr; the operating
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
    #[cfg(windows)]
    if let Err(error) = crate::windows_worker::wait_for_job() {
        crate::log_line!("cannot start helper: {error}");
        return 2;
    }
    #[cfg(target_os = "macos")]
    if let Err(error) = crate::worker_lifetime::watch_supervisor() {
        crate::log_line!("cannot watch the helper's supervisor: {error}");
        return 2;
    }
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
                supervised(
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

// This function runs exclusively in the disposable helper child.
pub(super) fn supervised(
    captured: &Captured,
    export: &str,
    args: &Value,
    limits: Limits,
    clock: Clock,
    controls: Controls,
) -> Result<Value, Failure> {
    let (done, finished) = std::sync::mpsc::channel();
    let deadline = limits.deadline;
    let _watchdog = controls.watchdog.then(|| {
        std::thread::spawn(move || {
            if finished.recv_timeout(deadline) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                std::process::exit(crate::js_budget::DEADLINE_EXIT);
            }
        })
    });
    crate::js_budget::enter(
        if controls.heap_callback {
            limits.heap_bytes
        } else {
            usize::MAX
        },
        if controls.bounded_array_buffers {
            limits.array_buffer_bytes
        } else {
            usize::MAX
        },
    );
    let result = super::invoke_with(captured, export, args, limits, clock, controls);
    let _ = done.send(());
    result
}

/// Limits this process: its CPU time and core files, and its writable memory
/// except on macOS. macOS refuses a data limit below its address map, so the
/// host holds the memory limit there (see `child_memory`).
#[cfg(unix)]
fn limit_self(limits: &Limits) -> Result<(), Failure> {
    let memory = byte_limits(limits)?.0;
    #[cfg(target_os = "macos")]
    let _ = memory;
    let cpu = cpu_seconds(limits)?;
    for (resource, value) in [
        #[cfg(not(target_os = "macos"))]
        (libc::RLIMIT_DATA, memory),
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
            return Err(Failure::Host(format!(
                "cannot limit the helper process: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

fn cpu_seconds(limits: &Limits) -> Result<u64, Failure> {
    limits
        .deadline
        .as_secs()
        .checked_add(2)
        .ok_or_else(|| Failure::Host("helper CPU limit is too large".into()))
}

#[cfg(windows)]
fn limit_self(limits: &Limits) -> Result<(), Failure> {
    crate::windows_worker::verify(byte_limits(limits)?.0, cpu_seconds(limits)?)
        .map_err(|error| Failure::Host(format!("cannot verify helper job: {error}")))
}

#[cfg(test)]
mod tests;
