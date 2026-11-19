// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::path::Path;

fn shell(script: &str, stdin: &[u8], time: Duration) -> Result<Completed, ExecError> {
    LocalBackend.run(&Command {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: std::env::temp_dir(),
        env: vec![("ONLY".into(), "this".into())],
        stdin: stdin.to_vec(),
        time_limit: time,
        output_limit: 1024,
    })
}

#[test]
fn a_command_gets_its_input_and_exactly_its_environment() {
    let done = shell(
        "cat; printf ' %s %s' \"$ONLY\" \"${HOME-unset}\"",
        b"input",
        Duration::from_secs(10),
    )
    .expect("runs");
    assert!(done.success());
    assert_eq!(done.stdout, b"input this unset");
}

#[test]
fn a_nonzero_exit_completes_with_its_code_and_stderr() {
    let done = shell(
        "printf 'why' >&2; exit 3",
        &vec![b'x'; 1_048_576],
        Duration::from_secs(10),
    )
    .expect("completes");
    assert_eq!(done.code, Some(3));
    assert_eq!(done.stderr, b"why");
}

#[test]
fn a_command_past_its_time_limit_is_stopped() {
    let started = std::time::Instant::now();
    let error = shell("sleep 5", b"", Duration::from_millis(200)).expect_err("stopped");
    assert!(matches!(error, ExecError::TimedOut { .. }), "{error}");
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn output_past_its_limit_is_refused() {
    let error =
        shell("head -c 2048 /dev/zero", b"", Duration::from_secs(10)).expect_err("too large");
    assert!(matches!(error, ExecError::TooLarge { .. }), "{error}");
}

#[test]
fn no_shell_expands_the_arguments() {
    let done = LocalBackend
        .run(&Command {
            program: "printf".into(),
            args: vec!["%s".into(), "$HOME;`id`".into()],
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            stdin: Vec::new(),
            time_limit: Duration::from_secs(10),
            output_limit: 1024,
        })
        .expect("runs");
    assert_eq!(done.stdout, b"$HOME;`id`");
}

#[test]
fn a_missing_program_cannot_start() {
    let error = LocalBackend
        .run(&Command {
            program: "atomic-no-such-program".into(),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            stdin: Vec::new(),
            time_limit: Duration::from_secs(10),
            output_limit: 1024,
        })
        .expect_err("missing");
    assert!(matches!(error, ExecError::Start { .. }), "{error}");
}

fn source(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("atomic-exec-source-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("source");
    dir
}

#[test]
fn a_local_workspace_is_its_source_with_a_fresh_scratch_and_a_local_shell() {
    let src = source("open");
    let request = Open {
        label: "fix tests".into(),
        source: Some(src.clone()),
        sandbox: Sandbox::WorkspaceWrite,
        writable_roots: vec![std::env::temp_dir()],
        env: [("DECLARED".to_owned(), "yes".to_owned())].into(),
    };
    let first = LocalBackend.open(&request).expect("opens");
    let second = LocalBackend.open(&request).expect("opens again");
    assert_eq!(first.root.as_ref(), Some(&src));
    assert_eq!(first.shell, Shell::Local);
    assert_eq!(first.sandbox, Sandbox::WorkspaceWrite);
    assert_eq!(first.writable_roots, request.writable_roots);
    assert_eq!(first.env, request.env);
    let scratch = first.scratch.clone().expect("a scratch directory");
    assert!(scratch.is_dir());
    assert_eq!(std::fs::read_dir(&scratch).expect("read").count(), 0);
    // Each attempt gets a scratch directory of its own.
    assert_ne!(first.scratch, second.scratch);

    std::fs::write(scratch.join("left"), "behind").expect("write");
    LocalBackend.close(&first).expect("closes");
    assert!(!scratch.exists(), "the scratch directory goes");
    assert!(src.is_dir(), "the source stays");
    LocalBackend.close(&first).expect("closing twice succeeds");
    LocalBackend.close(&second).expect("closes");
    let _ = std::fs::remove_dir_all(&src);
}

#[test]
fn a_workspace_with_no_source_has_no_files_and_no_shell() {
    let ws = LocalBackend
        .open(&Open {
            label: "classify".into(),
            ..Open::default()
        })
        .expect("opens");
    assert_eq!(ws.root, None);
    assert_eq!(ws.scratch, None);
    assert_eq!(ws.shell, Shell::None);
    assert!(matches!(ws.require_root(), Err(ExecError::Workspace { .. })));
    LocalBackend.close(&ws).expect("closes");
}

#[test]
fn a_source_that_is_not_a_directory_is_refused() {
    let error = LocalBackend
        .open(&Open {
            label: "fix".into(),
            source: Some(std::env::temp_dir().join("atomic-exec-no-such-source")),
            ..Open::default()
        })
        .expect_err("refused");
    assert!(matches!(error, ExecError::Workspace { .. }), "{error}");
    assert!(error.to_string().contains("fix"), "{error}");
}

/// A command run in a local workspace ends exactly as the same command run
/// directly as a child process.
#[test]
fn a_command_in_a_local_workspace_runs_as_it_would_directly() {
    let src = source("same");
    std::fs::write(src.join("input.txt"), "from the source").expect("write");
    let ws = LocalBackend
        .open(&Open {
            label: "same".into(),
            source: Some(src.clone()),
            ..Open::default()
        })
        .expect("opens");
    let scratch = ws.scratch.clone().expect("scratch").display().to_string();
    let command = Command {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "cat input.txt; cat; printf ' %s' \"$HOME\"; exit 4".into(),
        ],
        cwd: ws.root.clone().expect("root"),
        env: vec![("HOME".into(), scratch)],
        stdin: b" and stdin".to_vec(),
        time_limit: Duration::from_secs(10),
        output_limit: 1024,
    };
    let through = LocalBackend.run(&command).expect("runs");
    let direct = process::run(
        Path::new("/bin/sh"),
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
    .expect("runs");
    assert_eq!(through, direct);
    assert_eq!(through.code, Some(4));
    LocalBackend.close(&ws).expect("closes");
    let _ = std::fs::remove_dir_all(&src);
}
