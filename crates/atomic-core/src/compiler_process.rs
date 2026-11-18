// SPDX-License-Identifier: Apache-2.0

//! The bounded child process that runs the bundled Astro compiler.
//!
//! This is not an execution backend. Runners sit behind `atomic_exec::ExecutionBackend`;
//! the compiler is trusted host code that atomic starts itself, with the same
//! bounded process the local backend uses: a cleared environment, a time limit
//! and bounded input and output.

use atomic_exec::ExecError;
use atomic_exec::process::{self, Limits};
use std::path::Path;
use std::time::Duration;

/// The longest a compiler call may run.
pub(crate) const TIME_LIMIT: Duration = Duration::from_secs(30);
/// The largest request or response, in bytes.
pub(crate) const MESSAGE_LIMIT: usize = 8_388_608;

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
    let done = process::run(
        program,
        args,
        None,
        environment,
        request,
        Limits {
            time: time_limit,
            stdout: MESSAGE_LIMIT,
            stderr: process::STDERR_LIMIT,
        },
    )
    .map_err(|e| match e {
        ExecError::Start { program, message } => format!(
            "cannot start the Astro compiler with {program}: {message}; install Deno 2.9.8 or set ATOMIC_DENO"
        ),
        ExecError::TimedOut { seconds, .. } => {
            format!("the Astro compiler exceeded {seconds} seconds")
        }
        ExecError::TooLarge { .. } => "compiler response limit exceeded".into(),
        ExecError::Io { message, .. } => format!("running the Astro compiler: {message}"),
    })?;
    if !done.success() {
        let stderr = String::from_utf8_lossy(&done.stderr);
        let mut message = match done.code {
            Some(code) => format!("the Astro compiler crashed (exit status: {code})"),
            None => "the Astro compiler was stopped by a signal".to_owned(),
        };
        if !stderr.trim().is_empty() {
            message.push_str(": ");
            message.push_str(stderr.trim());
        }
        return Err(message);
    }
    Ok(done.stdout)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

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
