// SPDX-License-Identifier: Apache-2.0

//! The bounded child process that runs the bundled Astro compiler.
//!
//! This is not an execution backend. Runners sit behind `atomic_exec::ExecutionBackend`;
//! the compiler is trusted host code that atomic starts itself (the atomic
//! binary as its own child, see `compiler_host`), with the bounds
//! the local backend applies: a cleared environment, a time limit and bounded
//! input and output. Unlike a runner, it talks to the host while it works: it
//! asks the host to run captured helpers, one line per call and reply.

use atomic_exec::process;
use std::path::Path;
use std::time::Duration;

/// The longest a compiler call may run.
pub(crate) const TIME_LIMIT: Duration = Duration::from_secs(30);
/// The largest request or response, in bytes.
pub(crate) const MESSAGE_LIMIT: usize = 8_388_608;

/// A request the worker makes while it works, and the host's reply. Each is
/// one line of JSON.
pub(crate) type CallHandler<'a> = dyn FnMut(&serde_json::Value) -> serde_json::Value + 'a;

/// Run `program` with `request` as one line on stdin, answering the `call`
/// lines it writes on stdout with `handler`, and return its final line.
///
/// stdin stays open until the final line, so the worker can read each reply;
/// the time limit covers the whole exchange, helper calls and writes to a
/// worker that stops reading included: writes happen on their own thread,
/// and the worker is killed at the limit. Output is
/// bounded per line and in total. A worker that exits successfully has
/// answered, even if it stopped reading its request early; one that exits
/// unsuccessfully has crashed, and the error carries its stderr.
pub(crate) fn run_interactive(
    program: &Path,
    args: &[String],
    environment: &[(String, String)],
    request: &[u8],
    time_limit: Duration,
    handler: &mut CallHandler<'_>,
) -> Result<Vec<u8>, String> {
    use std::io::{BufRead, Read, Write};
    use std::process::{Command, Stdio};
    let start = |message: String| {
        format!(
            "cannot start the Astro compiler with {}: {message}",
            program.display()
        )
    };
    let io = |e: std::io::Error| format!("running the Astro compiler: {e}");
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    new_session(&mut command);
    let mut child = command.spawn().map_err(|e| start(e.to_string()))?;
    let input = child.stdin.take().ok_or("no compiler stdin")?;
    let stdout = child.stdout.take().ok_or("no compiler stdout")?;
    let stderr = child.stderr.take().ok_or("no compiler stderr")?;
    let errors = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stderr = stderr;
        (&mut stderr)
            .take(process::STDERR_LIMIT as u64)
            .read_to_end(&mut bytes)?;
        std::io::copy(&mut stderr, &mut std::io::sink())?;
        Ok::<_, std::io::Error>(bytes)
    });
    // Lines from stdout, each bounded; `None` at end of output.
    let (lines, received) = std::sync::mpsc::channel::<Result<Option<Vec<u8>>, String>>();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut total = 0usize;
        loop {
            let mut line = Vec::new();
            let limit = (MESSAGE_LIMIT + 1) as u64;
            match (&mut reader).take(limit).read_until(b'\n', &mut line) {
                Ok(0) => {
                    let _ = lines.send(Ok(None));
                    return;
                }
                Ok(_) => {
                    total += line.len();
                    if line.len() > MESSAGE_LIMIT || total > 4 * MESSAGE_LIMIT {
                        let _ = lines.send(Err("compiler response limit exceeded".into()));
                        return;
                    }
                    if line.last() == Some(&b'\n') {
                        line.pop();
                    }
                    if lines.send(Ok(Some(line))).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = lines.send(Err(e.to_string()));
                    return;
                }
            }
        }
    });
    let started = std::time::Instant::now();
    let remaining = || time_limit.saturating_sub(started.elapsed());
    let kill = |child: &mut std::process::Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    // Lines to the worker, written in order on their own thread so a worker
    // that stops reading cannot hold the host past the limit. The thread
    // ends when a write fails (the worker exited or was killed) or when the
    // sender is dropped, which closes stdin.
    let (to_worker, outgoing) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut input = input;
        for bytes in outgoing {
            let written = input
                .write_all(&bytes)
                .and_then(|()| input.write_all(b"\n"))
                .and_then(|()| input.flush());
            if written.is_err() {
                return;
            }
        }
    });
    // A worker that exits without reading its request has still answered.
    let _ = to_worker.send(request.to_vec());
    let mut answer = None;
    loop {
        let line = match received.recv_timeout(remaining()) {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            Ok(Err(message)) => {
                kill(&mut child);
                return Err(message);
            }
            Err(_) => {
                kill(&mut child);
                return Err(format!(
                    "the Astro compiler exceeded {} seconds",
                    time_limit.as_secs()
                ));
            }
        };
        let call = serde_json::from_slice::<serde_json::Value>(&line)
            .ok()
            .and_then(|value| value.get("call").cloned());
        match call {
            Some(call) if answer.is_none() => {
                let reply = serde_json::to_vec(&handler(&call)).map_err(|e| e.to_string())?;
                if to_worker.send(reply).is_err() {
                    break;
                }
            }
            _ => {
                answer = Some(line);
                break;
            }
        }
    }
    drop(to_worker);
    let status = loop {
        match child.try_wait().map_err(io)? {
            Some(status) => break status,
            None if remaining().is_zero() => {
                kill(&mut child);
                return Err(format!(
                    "the Astro compiler exceeded {} seconds",
                    time_limit.as_secs()
                ));
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let stderr = errors
        .join()
        .map_err(|_| "stderr reader panicked".to_owned())?
        .map_err(io)?;
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        let mut message = match status.code() {
            Some(code) => format!("the Astro compiler crashed (exit status: {code})"),
            None if cpu_limit(&status) => {
                "the Astro compiler exceeded its CPU time limit".to_owned()
            }
            None => "the Astro compiler was stopped by a signal".to_owned(),
        };
        if !stderr.trim().is_empty() {
            message.push_str(": ");
            message.push_str(stderr.trim());
        }
        return Err(message);
    }
    answer.ok_or_else(|| "missing compiler result".to_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::time::Instant;

    fn shell(script: &str, request: &[u8], limit: Duration) -> Result<Vec<u8>, String> {
        run_interactive(
            Path::new("/bin/sh"),
            &["-c".into(), script.into()],
            &[],
            request,
            limit,
            &mut |_: &Value| panic!("no calls expected"),
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
            "read request; printf '{\"ok\":true}'; exit 2",
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

    fn interactive(script: &str, limit: Duration) -> (Result<Vec<u8>, String>, Vec<Value>) {
        let mut calls = Vec::new();
        let result = run_interactive(
            Path::new("/bin/sh"),
            &["-c".into(), script.into()],
            &[],
            b"{\"action\":\"render\"}",
            limit,
            &mut |call: &Value| {
                calls.push(call.clone());
                json!({ "ok": call["n"].as_i64().unwrap_or(0) * 2 })
            },
        );
        (result, calls)
    }

    #[test]
    fn calls_are_answered_line_by_line_before_the_final_answer() {
        let (result, calls) = interactive(
            r#"read request; printf '{"call":{"n":2}}\n'; read reply; printf '{"call":{"n":5}}\n'; read again; printf '{"ok":[%s,%s]}\n' "$reply" "$again""#,
            TIME_LIMIT,
        );
        assert_eq!(
            String::from_utf8(result.expect("answer")).expect("utf-8"),
            r#"{"ok":[{"ok":4},{"ok":10}]}"#
        );
        assert_eq!(calls, [json!({ "n": 2 }), json!({ "n": 5 })]);
    }

    #[test]
    fn a_worker_that_never_answers_is_killed_at_the_time_limit() {
        let started = Instant::now();
        let (result, _) = interactive("read request; sleep 5", Duration::from_millis(200));
        assert!(result.expect_err("killed").contains("exceeded"));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    // A worker that stops reading cannot hold the host on a blocked write:
    // neither a large request nor a large reply outlasts the limit.
    #[test]
    fn a_worker_that_stops_reading_is_killed_at_the_time_limit() {
        let big = vec![b'x'; 4 << 20];
        let started = Instant::now();
        let error = shell("sleep 4", &big, Duration::from_millis(200)).expect_err("killed");
        assert!(error.contains("exceeded"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let started = Instant::now();
        let result = run_interactive(
            Path::new("/bin/sh"),
            &[
                "-c".into(),
                r#"read request; printf '{"call":{}}\n'; sleep 4"#.into(),
            ],
            &[],
            b"{}",
            Duration::from_millis(300),
            &mut |_: &Value| json!({ "padding": "x".repeat(1 << 20) }),
        );
        assert!(result.expect_err("killed").contains("exceeded"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn an_interactive_crash_reports_its_stderr() {
        let (result, _) = interactive("read request; printf 'boom' >&2; exit 3", TIME_LIMIT);
        let error = result.expect_err("crash");
        assert!(error.contains("boom") && error.contains("3"), "{error}");
    }

    #[test]
    fn an_oversized_response_is_refused() {
        let error = shell("head -c 8388609 /dev/zero", b"", TIME_LIMIT).expect_err("too large");
        assert!(error.contains("response limit"), "{error}");
    }
}

/// Whether the worker was stopped by its CPU time limit (see `compiler_host`).
fn cpu_limit(status: &std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal() == Some(libc::SIGXCPU)
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        false
    }
}

/// Start `command` in a session of its own, with no controlling terminal,
/// so a process running project code cannot open atomic's `/dev/tty` and
/// type into a gate's prompt.
pub(crate) fn new_session(command: &mut std::process::Command) {
    #[cfg(unix)]
    // SAFETY: setsid is async-signal-safe and the closure allocates nothing.
    #[allow(unsafe_code)]
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(command, || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    #[cfg(not(unix))]
    let _ = command;
}
