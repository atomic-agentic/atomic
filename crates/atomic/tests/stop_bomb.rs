// SPDX-License-Identifier: Apache-2.0

//! Stopping ends a command that is a fork bomb: inside a process namespace
//! of its own, as a sandbox's command runs, processes start others for as
//! long as the process count allows. When the stop returns, nothing of it
//! runs, its namespace's first process included, so the process, and any
//! lock it holds, outlasts it all.
//!
//! The bomb takes every process the container allows, so this runs only
//! when asked, alone, and needs unprivileged user namespaces, as the
//! sandbox does:
//!
//!   ATOMIC_DEV_SANDBOX=on scripts/dev.sh cargo test --locked -p atomic-agentic \
//!       --test stop_bomb -- --ignored
//!
//! In a test binary of its own: once stopped, this process starts nothing
//! more.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

mod support;

use atomic_exec::process::{Limits, run, stop_all};
use std::path::Path;
use std::time::{Duration, Instant};

/// Start others until the count allows no more, and go on trying.
const BOMB: &str =
    "import os\nwhile True:\n    try:\n        os.fork()\n    except OSError:\n        pass\n";

/// The live processes with `marker` on their command line, as far as
/// `/proc` can be listed by `deadline`: while a bomb runs, the listing
/// never ends by itself.
fn with_marker(marker: &str, deadline: Option<Instant>) -> Vec<String> {
    let me = std::process::id().to_string();
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .take_while(|_| deadline.is_none_or(|d| Instant::now() < d))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|pid| pid.bytes().all(|b| b.is_ascii_digit()) && *pid != me)
        .filter(|pid| {
            String::from_utf8_lossy(
                &std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default(),
            )
            .contains(marker)
        })
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/stat")).is_ok_and(|stat| {
                !matches!(
                    stat.rsplit(')')
                        .next()
                        .unwrap_or("")
                        .trim_start()
                        .chars()
                        .next(),
                    Some('Z' | 'X' | 'x') | None
                )
            }) || std::fs::read_dir(format!("/proc/{pid}/task")).is_ok_and(|t| t.count() > 1)
        })
        .collect()
}

#[test]
#[ignore = "takes every process the container allows: run alone, with --ignored"]
fn stopping_ends_a_fork_bomb_and_its_namespace() {
    support::require_sandbox();
    let marker = format!("atomic-stop-bomb-{}", std::process::id());
    // The namespace's first process reaps, as a sandbox's does, and starts
    // the bomb.
    let init = format!(
        "import os, subprocess, sys, time\n\
         subprocess.Popen(['python3', '-c', {BOMB:?}, {marker:?}])\n\
         while True:\n    \
             try:\n        os.wait()\n    \
             except ChildProcessError:\n        time.sleep(0.01)\n"
    );
    // The thread that stops is started first: once the bomb runs, no thread
    // can be.
    let (go, wait) = std::sync::mpsc::channel::<()>();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = wait.recv();
        let stopping = Instant::now();
        let mut reported = Vec::new();
        let stopped = stop_all(&mut |what| {
            eprintln!("stop: {what}");
            reported.push(what.to_owned());
        });
        let _ = sender.send((stopped, reported, stopping.elapsed()));
    });
    let args = [
        "--user",
        "--map-root-user",
        "--pid",
        "--fork",
        "/usr/bin/python3",
        "-c",
        &init,
        &marker,
    ]
    .map(str::to_owned);
    std::thread::spawn(move || {
        run(
            Path::new("/usr/bin/unshare"),
            &args,
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
    let begun = Instant::now();
    // The bomb runs: a starved look finds only some of it.
    while with_marker(&marker, Some(Instant::now() + Duration::from_secs(2))).len() < 50 {
        assert!(
            begun.elapsed() < Duration::from_secs(60),
            "the bomb never grew"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!(
        "the bomb runs, at least {} processes",
        with_marker(&marker, Some(Instant::now() + Duration::from_secs(2))).len()
    );
    std::thread::sleep(Duration::from_secs(1));
    eprintln!("stopping");
    go.send(()).unwrap();
    let (stopped, reported, took) = receiver
        .recv_timeout(Duration::from_secs(120))
        .unwrap_or_else(|_| panic!("stopping did not return in 120s"));
    // A complete look: nothing of the bomb is left to keep the listing going.
    let left = with_marker(&marker, None);
    assert!(
        left.is_empty(),
        "{} processes of the bomb outlived the stop ({took:?}): {:?}; {reported:?}",
        left.len(),
        &left[..left.len().min(10)]
    );
    assert_eq!(stopped, 1, "one command was running");
    eprintln!("stopped in {took:?}: {reported:?}");
}
