// SPDX-License-Identifier: Apache-2.0

use super::*;

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

#[cfg(unix)]
#[test]
fn a_command_runs_in_a_process_group_of_its_own() {
    // Fields 1 and 5 of /proc/<pid>/stat: the process and its group.
    let done = shell(
        "read -r pid _ _ _ group _ < /proc/self/stat; printf '%s %s' \"$pid\" \"$group\"",
        b"",
        Duration::from_secs(10),
    )
    .expect("runs");
    let text = String::from_utf8_lossy(&done.stdout).into_owned();
    let (pid, group) = text.split_once(' ').expect("two fields");
    assert_eq!(pid, group, "{text}");
    assert_ne!(
        group,
        std::process::id().to_string(),
        "not this process's group"
    );
}

#[cfg(unix)]
#[test]
fn a_command_past_its_time_limit_is_stopped_with_everything_it_started() {
    let file = std::env::temp_dir().join(format!("atomic-exec-group-{}", std::process::id()));
    let script = format!("sleep 30 & printf '%s' \"$!\" > '{}'; wait", file.display());
    let error = shell(&script, b"", Duration::from_millis(500)).expect_err("times out");
    assert!(matches!(error, ExecError::TimedOut { .. }), "{error}");
    let pid = std::fs::read_to_string(&file).expect("the background pid");
    let _ = std::fs::remove_file(&file);
    let started = std::time::Instant::now();
    // Gone, or a zombie waiting for init to reap it.
    while std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        !stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .trim_start()
            .starts_with('Z')
    }) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the command's own child {pid} outlived it"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
