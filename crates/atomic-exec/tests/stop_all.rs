// SPDX-License-Identifier: Apache-2.0

//! Stopping a process that ran commands ends every process those commands
//! started before it returns, one that left the command's process group
//! and session and ignores SIGTERM included, so nothing a stopped command
//! started outlives the process, or the lock it holds. One of atomic's own
//! tools, such as its git, is let finish instead.
//!
//! In a test binary of its own: once stopped, this process starts nothing
//! more.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use atomic_exec::process::{Limits, run, spawn_own_tool, stop_all};
use std::path::Path;
use std::time::{Duration, Instant};

/// Whether `pid` runs: there, and neither a zombie nor dead.
fn alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        !matches!(
            stat.rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .chars()
                .next(),
            Some('Z' | 'X' | 'x') | None
        )
    })
}

#[test]
fn stopping_ends_every_process_a_command_started_before_it_returns() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-stop-all-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pidfile = dir.join("grandchild");
    // The command leaves a grandchild in a session of its own, which a kill
    // of the command's process group does not reach, and which ignores
    // SIGTERM.
    let script = format!(
        "setsid sh -c 'trap \"\" TERM; echo $$ > {}; exec sleep 300' & sleep 300",
        pidfile.display()
    );
    std::thread::spawn(move || {
        run(
            Path::new("/bin/sh"),
            &["-c".to_owned(), script],
            None,
            &[("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
            b"",
            Limits {
                time: Duration::from_secs(600),
                stdout: 1024,
                stderr: 1024,
            },
        )
    });
    let started = Instant::now();
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pidfile)
            && let Some(pid) = text.trim().parse::<u32>().ok()
        {
            break pid.to_string();
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the grandchild never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(alive(&pid), "the grandchild runs before the stop");
    // One of atomic's own tools, which takes a moment to finish its work.
    let done = dir.join("tool-done");
    let (mut tool, own) = spawn_own_tool(std::process::Command::new("/bin/sh").args([
        "-c",
        &format!("trap '' TERM; sleep 1; touch {}", done.display()),
    ]))
    .unwrap();
    // And a process started while the stop waits for the tool, as a model
    // turn's command may be: killed on sight.
    let late = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(400));
        std::process::Command::new("/bin/sleep")
            .arg("300")
            .spawn()
            .unwrap()
    });
    let mut reported = Vec::new();
    let stopped = stop_all(&mut |what| reported.push(what.to_owned()));
    assert!(done.exists(), "the tool was killed, not let finish");
    let mut late = late.join().unwrap();
    let late_pid = late.id().to_string();
    assert!(
        !alive(&late_pid) || late.try_wait().unwrap().is_some(),
        "process {late_pid}, started during the stop, outlived it"
    );
    assert_eq!(stopped, 1, "one command was running");
    assert!(!alive(&pid), "process {pid} outlived the stop");
    assert!(tool.wait().unwrap().success());
    drop(own);
    assert!(
        reported.is_empty(),
        "nothing outlived its kill: {reported:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
