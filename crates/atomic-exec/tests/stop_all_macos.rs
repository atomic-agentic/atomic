// SPDX-License-Identifier: Apache-2.0

//! Stopping a process that ran commands ends, on macOS, every process those
//! commands started that still descends from it, before it returns: one
//! that left the command's process group and session and ignores SIGTERM
//! included, and one started while the stop runs. One of atomic's own
//! tools, such as its git, is let finish instead.
//!
//! macOS has no subreaper, so a process whose parent ended before the stop
//! found it has gone to launchd and is not found; the Linux suite
//! (`stop_all.rs`) covers that case there.
//!
//! In a test binary of its own: once stopped, this process starts nothing
//! more.
#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used)]

use atomic_exec::process::{Limits, run, spawn_own_tool, stop_all, stopping};
use std::path::Path;
use std::time::{Duration, Instant};

/// Whether `pid` runs: listed by `ps`, and not a zombie.
fn alive(pid: &str) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .is_ok_and(|out| {
            let stat = String::from_utf8_lossy(&out.stdout);
            let stat = stat.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        })
}

/// Wait for `file` to hold a process id, for at most ten seconds.
fn pid_in(file: &Path, what: &str) -> String {
    let started = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(file)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            return pid.to_string();
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{what} never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn stopping_ends_every_descendant_a_command_started_before_it_returns() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-stop-all-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pidfile = dir.join("grandchild");
    // The command leaves a grandchild in a session of its own, which a kill
    // of the command's process group does not reach, and which ignores
    // SIGTERM. macOS has no setsid(1), so Python leaves the session.
    let script = format!(
        "python3 -c 'import os, signal, time; os.setsid(); \
         signal.signal(signal.SIGTERM, signal.SIG_IGN); \
         open(\"{}\", \"w\").write(str(os.getpid())); time.sleep(300)' & sleep 300",
        pidfile.display(),
    );
    std::thread::spawn(move || {
        run(
            Path::new("/bin/sh"),
            &["-c".to_owned(), script],
            None,
            &[(
                "PATH".to_owned(),
                "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin".to_owned(),
            )],
            b"",
            Limits {
                time: Duration::from_secs(600),
                stdout: 1024,
                stderr: 1024,
                cpu_seconds: None,
                data_bytes: None,
            },
        )
    });
    let pid = pid_in(&pidfile, "the grandchild");
    assert!(alive(&pid), "the grandchild runs before the stop");
    // One of atomic's own tools, which finishes its work only once the late
    // process below has started, so that one starts while the stop waits.
    let done = dir.join("tool-done");
    let late_started = dir.join("late-started");
    let (mut tool, own) = spawn_own_tool(std::process::Command::new("/bin/sh").args([
        "-c",
        &format!(
            "trap '' TERM; while [ ! -e {} ]; do sleep 0.05; done; touch {}",
            late_started.display(),
            done.display()
        ),
    ]))
    .unwrap();
    // And a process started once the stop has begun, as a model turn's
    // command may be: killed on sight.
    let marker = late_started.clone();
    let late = std::thread::spawn(move || {
        while !stopping() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let child = std::process::Command::new("/bin/sleep")
            .arg("300")
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(&marker, "").unwrap();
        child
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
