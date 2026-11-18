// SPDX-License-Identifier: Apache-2.0

//! The bounded child process that runs the bundled Astro compiler.
//!
//! This is not an execution backend. Runners sit behind `atomic_exec::ExecutionBackend`;
//! the compiler is trusted host code that atomic starts itself, with a cleared
//! environment, a time limit and bounded input and output.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The longest a compiler call may run.
pub(crate) const TIME_LIMIT: Duration = Duration::from_secs(30);
/// The largest request or response, in bytes.
pub(crate) const MESSAGE_LIMIT: usize = 8_388_608;
const STDERR_LIMIT: usize = 65_536;

/// Run `program` with `request` on stdin and return its stdout.
///
/// A worker that exits successfully has answered, even if it stopped reading
/// its request early. A worker that exits unsuccessfully has crashed, and the
/// error carries its stderr.
pub(crate) fn run(
    program: &Path,
    args: &[String],
    environment: &[(String, String)],
    request: &[u8],
    time_limit: Duration,
) -> Result<Vec<u8>, String> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "cannot start the Astro compiler with {}: {e}; install Deno 2.9.8 or set ATOMIC_DENO",
                program.display()
            )
        })?;
    let mut stdin = child.stdin.take().ok_or("compiler stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("compiler stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("compiler stderr unavailable")?;
    let request = request.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&request));
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(MESSAGE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let errors = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .take(STDERR_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if started.elapsed() > time_limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "the Astro compiler exceeded {} seconds",
                    time_limit.as_secs()
                ));
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let output = reader
        .join()
        .map_err(|_| "compiler reader panicked")?
        .map_err(|e| e.to_string())?;
    let error = errors
        .join()
        .map_err(|_| "compiler stderr reader panicked")?
        .map_err(|e| e.to_string())?;
    let written = writer.join().map_err(|_| "compiler writer panicked")?;
    if output.len() > MESSAGE_LIMIT || error.len() > STDERR_LIMIT {
        return Err("compiler response limit exceeded".into());
    }
    if !status.success() {
        let stderr = String::from_utf8_lossy(&error);
        let mut message = format!("the Astro compiler crashed ({status})");
        if !stderr.trim().is_empty() {
            message.push_str(": ");
            message.push_str(stderr.trim());
        }
        // A broken pipe only says the worker stopped reading; stderr says why.
        if let Err(write) = written
            && write.kind() != std::io::ErrorKind::BrokenPipe
        {
            message.push_str(&format!("; writing its request failed: {write}"));
        }
        return Err(message);
    }
    Ok(output)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn shell(script: &str, request: &[u8], limit: Duration) -> Result<Vec<u8>, String> {
        run(
            Path::new("/bin/sh"),
            &["-c".into(), script.into()],
            &[],
            request,
            limit,
        )
    }

    #[test]
    fn a_crash_reports_its_stderr_even_with_a_broken_pipe() {
        let error = shell(
            "printf 'distinct-worker-error' >&2; exit 23",
            &vec![b'x'; 4_194_304],
            TIME_LIMIT,
        )
        .expect_err("early exit");
        assert!(error.contains("distinct-worker-error"), "{error}");
        assert!(error.contains("23"), "{error}");
        assert!(!error.contains("Broken pipe"), "{error}");
    }

    #[test]
    fn nonzero_exit_cannot_return_a_success_response() {
        let error = shell(
            "cat >/dev/null; printf '{\"ok\":true}'; exit 2",
            b"{}",
            TIME_LIMIT,
        )
        .expect_err("nonzero response");
        assert!(error.contains("exit status: 2"), "{error}");
    }

    #[test]
    fn an_answer_given_before_reading_the_request_is_kept() {
        let output =
            shell("printf '{\"ok\":1}'", &vec![b'x'; 4_194_304], TIME_LIMIT).expect("answer");
        assert_eq!(output, b"{\"ok\":1}");
    }

    #[test]
    fn the_environment_is_cleared() {
        let output = shell("printf '%s' \"${HOME-unset}\"", b"", TIME_LIMIT).expect("run");
        assert_eq!(output, b"unset");
    }

    #[test]
    fn a_slow_worker_is_killed_at_the_time_limit() {
        let started = Instant::now();
        let error = shell("sleep 5", b"", Duration::from_millis(200)).expect_err("killed");
        assert!(error.contains("exceeded"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn an_oversized_response_is_refused() {
        let error = shell("head -c 8388609 /dev/zero", b"", TIME_LIMIT).expect_err("too large");
        assert!(error.contains("response limit"), "{error}");
    }
}
