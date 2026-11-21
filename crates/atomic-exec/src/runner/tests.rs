// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::path::Path;

fn command(program: &str, args: &[&str], stdin: &[u8]) -> Command {
    Command {
        program: program.into(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        cwd: std::env::temp_dir(),
        env: vec![("ONLY".into(), "this".into())],
        stdin: stdin.to_vec(),
        time_limit: Duration::from_secs(10),
        output_limit: 1024,
        writable: Vec::new(),
        read_only: Vec::new(),
        network: Vec::new(),
        secret_env: Default::default(),
    }
}

fn scratch(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "atomic-runner-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A stand-in runner: a shell script that counts its starts in `dir/starts`,
/// then does what `script` says.
fn fake(dir: &Path, script: &str) -> ProcessRunner {
    let starts = dir.join("starts");
    ProcessRunner {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            format!("echo start >> '{}'; {script}", starts.display()),
        ],
        env: Vec::new(),
        retries: 2,
        lease_wait: Duration::from_millis(10),
        workspaces: LocalBackend::default(),
    }
}

fn starts(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("starts"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

#[test]
fn every_exit_code_has_one_class_and_an_unknown_code_is_a_crash() {
    let table = [
        (Some(0), Exit::Clean, Action::Accept),
        (Some(70), Exit::Crash, Action::Unknown),
        (Some(64), Exit::Usage, Action::Fail),
        (Some(78), Exit::Config, Action::FailUntilFixed),
        (Some(75), Exit::LeaseHeld, Action::Defer),
        (Some(1), Exit::Crash, Action::Unknown),
        (Some(2), Exit::Crash, Action::Unknown),
        (Some(101), Exit::Crash, Action::Unknown),
        (Some(-1), Exit::Crash, Action::Unknown),
        (None, Exit::Crash, Action::Unknown),
    ];
    for (code, class, action) in table {
        assert_eq!(Exit::classify(code), class, "{code:?}");
        assert_eq!(class.action(), action, "{code:?}");
    }
    for class in [
        Exit::Clean,
        Exit::Crash,
        Exit::Usage,
        Exit::Config,
        Exit::LeaseHeld,
    ] {
        assert_eq!(Exit::classify(Some(class.code())), class);
    }
}

#[test]
fn the_runner_side_runs_a_request_and_answers_it() {
    let request = serde_json::to_vec(&Request::new(&command(
        "/bin/sh",
        &[
            "-c",
            "cat; printf ' %s' \"$ONLY\"; printf 'why' >&2; exit 3",
        ],
        b"input",
    )))
    .expect("encode");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = serve(
        &mut request.as_slice(),
        &mut out,
        &mut err,
        &LocalBackend::default(),
    );
    assert_eq!(code, code::CLEAN, "{}", String::from_utf8_lossy(&err));
    let response: Response = serde_json::from_slice(&out).expect("a response");
    assert_eq!(
        response,
        Response::Completed {
            code: Some(3),
            stdout: hex(b"input this"),
            stderr: hex(b"why"),
        }
    );
}

#[test]
fn the_runner_side_refuses_what_it_cannot_read_as_a_usage_error() {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = serve(
        &mut &b"not json"[..],
        &mut out,
        &mut err,
        &LocalBackend::default(),
    );
    assert_eq!(code, code::USAGE);
    assert!(out.is_empty());

    let mut request = Request::new(&command("true", &[], b""));
    request.protocol = PROTOCOL + 1;
    let bytes = serde_json::to_vec(&request).expect("encode");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = serve(
        &mut bytes.as_slice(),
        &mut out,
        &mut err,
        &LocalBackend::default(),
    );
    assert_eq!(code, code::USAGE);
    assert!(String::from_utf8_lossy(&err).contains("protocol"));
}

#[test]
fn a_command_that_cannot_start_is_reported_as_the_runner_saw_it() {
    let request = serde_json::to_vec(&Request::new(&command("atomic-no-such-program", &[], b"")))
        .expect("encode");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(
        serve(
            &mut request.as_slice(),
            &mut out,
            &mut err,
            &LocalBackend::default()
        ),
        code::CLEAN
    );
    let response: Response = serde_json::from_slice(&out).expect("a response");
    assert!(
        matches!(
            response,
            Response::Failed {
                error: ExecError::Start { .. }
            }
        ),
        "{response:?}"
    );
}

#[test]
fn a_clean_exit_returns_the_commands_own_result() {
    let dir = scratch("clean");
    let response = serde_json::to_string(&Response::Completed {
        code: Some(0),
        stdout: hex(b"{\"ok\":true}"),
        stderr: String::new(),
    })
    .expect("encode");
    let runner = fake(&dir, &format!("cat > /dev/null; printf '%s' '{response}'"));
    let done = runner
        .run(&command("anything", &[], b"in"))
        .expect("completes");
    assert_eq!(done.stdout, b"{\"ok\":true}");
    assert_eq!(starts(&dir), 1);
}

#[test]
fn a_crash_is_an_unknown_outcome_and_the_command_never_runs_again() {
    // The command may have run before the runner died, so the runner never
    // runs it again: whether it may is the durable step's to decide.
    for script in ["exit 70", "exit 1", "kill -9 $$"] {
        let dir = scratch("crash");
        let error = fake(&dir, &format!("printf 'boom' >&2; {script}"))
            .run(&command("anything", &[], b""))
            .expect_err("fails");
        assert_eq!(starts(&dir), 1, "{script}: started once");
        assert!(error.outcome_unknown(), "{script}: {error}");
        let ExecError::Lost { stderr, .. } = &error else {
            panic!("{script}: {error}");
        };
        assert_eq!(stderr, "boom", "{script}: stderr is attached as it was");
    }
}

#[test]
fn a_crash_is_not_retried_even_when_a_retry_would_succeed() {
    let dir = scratch("flaky");
    let response = serde_json::to_string(&Response::Completed {
        code: Some(0),
        stdout: hex(b"done"),
        stderr: String::new(),
    })
    .expect("encode");
    let marker = dir.join("crashed");
    let script = format!(
        "cat > /dev/null; if [ -e '{m}' ]; then printf '%s' '{response}'; else : > '{m}'; exit 70; fi",
        m = marker.display()
    );
    let error = fake(&dir, &script)
        .run(&command("anything", &[], b""))
        .expect_err("the crash is reported");
    assert!(error.outcome_unknown(), "{error}");
    assert_eq!(starts(&dir), 1);
}

#[test]
fn a_usage_or_configuration_error_is_never_retried() {
    for (script, class) in [
        ("exit 64", "usage error"),
        ("exit 78", "configuration error"),
    ] {
        let dir = scratch("no-retry");
        // stderr that looks like a response changes nothing: it is never parsed.
        let error = fake(
            &dir,
            &format!("printf '{{\"outcome\":\"completed\"}}' >&2; {script}"),
        )
        .run(&command("anything", &[], b""))
        .expect_err("fails");
        assert_eq!(starts(&dir), 1, "{script}");
        assert!(!error.outcome_unknown(), "{script}: nothing ran");
        let ExecError::Runner { class: got, .. } = &error else {
            panic!("{script}: {error}");
        };
        assert_eq!(got, class);
    }
}

#[test]
fn a_held_lease_waits_and_tries_again() {
    let dir = scratch("lease");
    let error = fake(&dir, "exit 75")
        .run(&command("anything", &[], b""))
        .expect_err("still held");
    assert_eq!(
        starts(&dir),
        3,
        "a held lease means nothing ran, so it tries again"
    );
    assert!(!error.outcome_unknown(), "{error}");
    assert!(
        matches!(&error, ExecError::Runner { class, .. } if class == "lease held elsewhere"),
        "{error}"
    );
}

#[test]
fn a_clean_exit_with_no_readable_response_is_an_unknown_outcome() {
    for script in [
        "printf 'half a resp'",
        "printf '{\"outcome\":\"completed\",\"code\":0,\"stdout\":\"zz\",\"stderr\":\"\"}'",
    ] {
        let dir = scratch("garbled");
        let error = fake(&dir, script)
            .run(&command("anything", &[], b""))
            .expect_err("fails");
        assert_eq!(starts(&dir), 1, "{script}");
        assert!(
            matches!(&error, ExecError::Lost { .. }) && error.outcome_unknown(),
            "{script}: {error}"
        );
    }
}

#[test]
fn the_runner_process_gets_only_the_environment_it_is_given() {
    let dir = scratch("env");
    // The fake runner answers with its own HOME, which this process has.
    let mut runner = fake(
        &dir,
        "cat > /dev/null; v=\"${HOME-unset}\"; \
         printf '{\"outcome\":\"completed\",\"code\":0,\"stdout\":\"\",\"stderr\":\"%s\"}' \
         \"$(printf '%s' \"$v\" | od -An -tx1 | tr -d ' \\n')\"",
    );
    runner.env = vec![("PATH".into(), "/usr/bin:/bin".into())];
    let done = runner
        .run(&command("anything", &[], b""))
        .expect("completes");
    assert_eq!(done.stderr, b"unset");
}

#[test]
fn hex_round_trips() {
    let bytes: Vec<u8> = (0..=255).collect();
    assert_eq!(unhex(&hex(&bytes)), Some(bytes));
    assert_eq!(unhex("abc"), None);
    assert_eq!(unhex("zz"), None);
}

/// A runner the way the control plane starts one, from
/// [`ProcessRunner::atomic`], has only `PATH` from the control plane's
/// environment: a stand-in that prints its own environment shows it.
#[test]
fn a_runner_has_only_path_from_the_control_planes_environment() {
    assert!(std::env::var_os("PATH").is_some(), "this test needs PATH");
    let runner = ProcessRunner {
        args: Vec::new(),
        ..ProcessRunner::atomic(std::path::PathBuf::from("/usr/bin/env"))
    };
    let (_, done) = runner
        .attempt(&command("true", &[], b""), b"")
        .expect("the stand-in runs");
    let names: Vec<String> = String::from_utf8_lossy(&done.stdout)
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name.to_owned()))
        .collect();
    assert_eq!(names, ["PATH"]);
}

/// A request never carries its command's secret variables: they cross on
/// a socket of their own.
#[test]
fn a_request_never_carries_secret_variables() {
    let mut sent = command("true", &[], b"");
    sent.secret_env = crate::SecretEnv(vec![("K".into(), "the-secret-value".into())]);
    let request = Request::new(&sent);
    let json = serde_json::to_string(&request).expect("json");
    assert!(!json.contains("the-secret-value"), "{json}");
    assert!(!json.contains("secret_env"), "{json}");
    let back: Request = serde_json::from_str(&json).expect("read back");
    assert!(back.command().expect("command").secret_env.is_empty());
}

/// A runner gets its command's secret variables on a socket of their own,
/// never in the request on its stdin, a pipe another process could reopen
/// through /proc before the runner has hardened itself: a stand-in runner
/// that keeps what each brought shows it.
#[test]
fn a_runner_gets_secret_variables_on_a_socket_never_in_its_stdin() {
    const STAND_IN: &str = r#"import os, sys
out = sys.argv[1]
stdin = sys.stdin.buffer.read()
fd = int(os.environ['ATOMIC_SECRET_ENV_FD'])
secret = b''
while True:
    chunk = os.read(fd, 4096)
    if not chunk:
        break
    secret += chunk
open(os.path.join(out, 'stdin'), 'wb').write(stdin)
open(os.path.join(out, 'secret'), 'wb').write(secret)
"#;
    let dir = scratch("secret-socket");
    let runner = ProcessRunner {
        program: "python3".into(),
        args: vec!["-c".into(), STAND_IN.into(), dir.display().to_string()],
        env: std::env::var("PATH")
            .map(|path| vec![("PATH".to_owned(), path)])
            .unwrap_or_default(),
        retries: 0,
        lease_wait: Duration::from_millis(10),
        workspaces: LocalBackend::default(),
    };
    let mut sent = command("true", &[], b"");
    sent.secret_env = crate::SecretEnv(vec![("K".into(), "the-secret-value".into())]);
    let request = serde_json::to_vec(&Request::new(&sent)).expect("json");
    let (_, done) = runner.attempt(&sent, &request).expect("the stand-in runs");
    let stdin = std::fs::read(dir.join("stdin"))
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&done.stderr)));
    assert_eq!(stdin, request, "its stdin is the request");
    assert!(
        !String::from_utf8_lossy(&stdin).contains("the-secret-value"),
        "the secret crossed in its stdin"
    );
    let secret = crate::SecretEnv::decode(&std::fs::read(dir.join("secret")).expect("secret"))
        .expect("well formed");
    assert_eq!(
        secret.0,
        [
            ("K".to_owned(), "the-secret-value".to_owned()),
            (REQUEST_DIGEST.to_owned(), request_digest(&request)),
        ],
        "the secret, and the digest of the request it goes with"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The runner hands its command the secret only with the request it was
/// sent with: a request put in its stdin in place of that one, or a secret
/// with no digest or a second one, is refused before anything runs, and no
/// command gets the secret.
#[test]
fn a_runner_refuses_a_request_its_secret_was_not_sent_with() {
    let dir = scratch("forged-request");
    let marker = dir.join("ran");
    let forged = serde_json::to_vec(&Request::new(&command(
        "/bin/sh",
        &["-c", &format!("env > '{}'", marker.display())],
        b"",
    )))
    .expect("encode");
    let real = serde_json::to_vec(&Request::new(&command("true", &[], b""))).expect("encode");
    let secret = |entries: &[(&str, String)]| {
        crate::SecretEnv(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        )
    };
    let value = "the-secret-value".to_owned();
    for (case, handed) in [
        (
            "the real request's digest",
            secret(&[("K", value.clone()), (REQUEST_DIGEST, request_digest(&real))]),
        ),
        ("no digest", secret(&[("K", value.clone())])),
        (
            "two digests",
            secret(&[
                ("K", value.clone()),
                (REQUEST_DIGEST, request_digest(&forged)),
                (REQUEST_DIGEST, request_digest(&forged)),
            ]),
        ),
    ] {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = serve_with_secret(
            &mut forged.as_slice(),
            &mut out,
            &mut err,
            &LocalBackend::default(),
            handed,
        );
        assert_eq!(code, code::USAGE, "{case}: {}", String::from_utf8_lossy(&err));
        assert!(
            String::from_utf8_lossy(&err).contains("refused"),
            "{case}: {}",
            String::from_utf8_lossy(&err)
        );
        assert!(!marker.exists(), "{case}: the forged request ran");
    }
    // The request the secret was sent with runs, with the secret and
    // without the digest.
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = serve_with_secret(
        &mut forged.as_slice(),
        &mut out,
        &mut err,
        &LocalBackend::default(),
        secret(&[("K", value.clone()), (REQUEST_DIGEST, request_digest(&forged))]),
    );
    assert_eq!(code, code::CLEAN, "{}", String::from_utf8_lossy(&err));
    assert!(marker.exists(), "the request it was sent with ran");
    let _ = std::fs::remove_dir_all(&dir);
}
