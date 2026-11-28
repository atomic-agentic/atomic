// SPDX-License-Identifier: Apache-2.0

//! An exec command's OS limits survive the local and runner paths.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use atomic_exec::{Command, ExecError, ExecutionBackend, LocalBackend, runner::ProcessRunner};
use std::{path::PathBuf, time::Duration};

fn backends() -> [Box<dyn ExecutionBackend>; 2] {
    [
        Box::new(LocalBackend::default()),
        Box::new(ProcessRunner::atomic(PathBuf::from(env!(
            "CARGO_BIN_EXE_atomic"
        )))),
    ]
}

fn python(script: &str) -> Command {
    Command {
        program: "/usr/bin/python3".into(),
        args: vec!["-c".into(), script.into()],
        cwd: std::env::temp_dir(),
        time_limit: Duration::from_secs(5),
        output_limit: 1024,
        ..Command::default()
    }
}

#[test]
fn cpu_limit_is_observed_and_exceeded_on_both_backends() {
    let script = "import resource\n\
        soft, hard = resource.getrlimit(resource.RLIMIT_CPU)\n\
        assert soft == 1 and hard <= 2, (soft, hard)\n\
        print('cpu limit observed', flush=True)\n\
        while True: pass\n";
    let backends = backends();
    for (name, backend) in [("local", &backends[0]), ("runner", &backends[1])] {
        let mut command = python(script);
        command.cpu_seconds = Some(1);
        let done = backend
            .run(&command)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            done.code,
            None,
            "{name}: {}",
            String::from_utf8_lossy(&done.stderr)
        );
        assert_eq!(done.stdout, b"cpu limit observed\n", "{name}: {done:?}");
    }
}

// macOS refuses an `RLIMIT_DATA` near any memory a command may hold; there
// atomic holds the limit from outside (the test below).
#[cfg(target_os = "linux")]
#[test]
fn writable_data_limit_is_observed_and_exceeded_on_both_backends() {
    let script = "import resource\n\
        soft, hard = resource.getrlimit(resource.RLIMIT_DATA)\n\
        assert soft == 160 * 1024 * 1024 and hard == soft, (soft, hard)\n\
        print('data limit observed', flush=True)\n\
        try: bytearray(256 * 1024 * 1024)\n\
        except MemoryError: print('data limit hit', flush=True)\n\
        else: raise AssertionError('allocation exceeded the cap')\n";
    let backends = backends();
    for (name, backend) in [("local", &backends[0]), ("runner", &backends[1])] {
        let mut command = python(script);
        command.data_bytes = Some(160 * 1024 * 1024);
        let done = backend
            .run(&command)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            done.code,
            Some(0),
            "{name}: {}",
            String::from_utf8_lossy(&done.stderr)
        );
        assert_eq!(
            done.stdout, b"data limit observed\ndata limit hit\n",
            "{name}: {done:?}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn writable_data_limit_is_held_from_outside_on_both_backends_on_macos() {
    // Each script writes to its memory, which a footprint counts, and then
    // holds it until it is stopped or its time is up.
    let hold = |mib: u32| {
        format!(
            "import time\n\
             data = b'x' * ({mib} * 1024 * 1024)\n\
             print('wrote', flush=True)\n\
             time.sleep(3)\n\
             print('kept', len(data) >= 1, flush=True)\n"
        )
    };
    let backends = backends();
    for (name, backend) in [("local", &backends[0]), ("runner", &backends[1])] {
        // Within its limit it runs to the end.
        let mut command = python(&hold(16));
        command.data_bytes = Some(160 * 1024 * 1024);
        let done = backend
            .run(&command)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            done.code,
            Some(0),
            "{name}: {}",
            String::from_utf8_lossy(&done.stderr)
        );
        assert_eq!(done.stdout, b"wrote\nkept True\n", "{name}: {done:?}");

        // Past it, it is killed before its sleep ends.
        let mut command = python(&hold(256));
        command.data_bytes = Some(160 * 1024 * 1024);
        let done = backend
            .run(&command)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let stderr = String::from_utf8_lossy(&done.stderr);
        assert_eq!(done.code, None, "{name}: {stderr}");
        assert!(
            stderr.contains("more than its memory limit of 167772160"),
            "{name}: {stderr}"
        );
        assert!(
            !String::from_utf8_lossy(&done.stdout).contains("kept"),
            "{name}: {done:?}"
        );
    }
}

#[test]
fn a_limit_that_means_unlimited_is_refused_on_both_backends() {
    let backends = backends();
    for (name, backend) in [("local", &backends[0]), ("runner", &backends[1])] {
        for field in ["cpu", "data"] {
            let mut command = python("print('must not run')");
            if field == "cpu" {
                command.cpu_seconds = Some(u64::MAX);
            } else {
                command.data_bytes = Some(u64::MAX);
            }
            let error = backend
                .run(&command)
                .expect_err("an unlimited value must be refused");
            assert!(
                matches!(error, ExecError::Start { .. }),
                "{name}/{field}: {error}"
            );
        }
    }
}
