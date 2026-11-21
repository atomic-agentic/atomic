// SPDX-License-Identifier: Apache-2.0

//! A confined command that holds many tunnels through its proxy to a server
//! that answers stays within its share of atomic's file descriptors: the
//! admission budget holds for an upstream that behaves (a command that
//! abandons connections to one that never answers can pass it; that is not
//! enforced). With atomic's limit on open files held at 1024, soft and hard,
//! a command opens 300 tunnels through the proxy to a server on this
//! machine that keeps every one open. Its sandbox's bridge
//! lets at most 128 through at once, each costing atomic two descriptors,
//! so the proxy holds no more than its per-command allowance, and that is
//! within half of the limit. While it runs, a second command that declares
//! hosts would pass that half, and does not start.
//!
//! Needs bubblewrap, unprivileged user namespaces and a network namespace
//! of the command's own, so it runs with the `sandbox-tests` feature; with
//! the feature on, a sandbox that cannot start fails the test. It lowers
//! this test process's limit on open files, so it has a binary of its own.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]

mod support;

use atomic_core::codex::{CodexPaths, ExecSandbox, StateGuard};
use atomic_core::network::DESCRIPTORS_PER_COMMAND;
use atomic_exec::{Command, ExecutionBackend, LocalBackend};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Opens 300 tunnels through the proxy to `target`, counts those the proxy
/// opened, writes the count to `ready`, and keeps them all until `done`
/// exists.
const OPENER: &str = r#"import os, socket, sys, time
target, ready, done = sys.argv[1:]
host, port = os.environ['HTTP_PROXY'].split('//')[1].rstrip('/').rsplit(':', 1)
socks = []
for _ in range(300):
    try:
        s = socket.create_connection((host, int(port)), timeout=5)
        s.sendall(('CONNECT ' + target + ' HTTP/1.1\r\nHost: ' + target + '\r\n\r\n').encode())
        socks.append(s)
    except Exception:
        pass
tunnels = 0
for s in socks:
    try:
        s.settimeout(10)
        if s.recv(64).startswith(b'HTTP/1.1 200'):
            tunnels += 1
    except Exception:
        pass
with open(ready + '.part', 'w') as f:
    f.write(str(tunnels))
os.rename(ready + '.part', ready)
while not os.path.exists(done):
    time.sleep(0.1)
print(tunnels)
"#;

/// The file descriptors this process has open.
fn open_descriptors() -> u64 {
    std::fs::read_dir("/proc/self/fd").expect("fds").count() as u64
}

/// Hold this process to `soft` open files, soft and hard, so that atomic
/// cannot raise it.
#[allow(unsafe_code)]
fn limit_open_files(soft: u64) {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit and setrlimit read and write one rlimit.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        limit.rlim_cur = soft.min(limit.rlim_max);
        limit.rlim_max = limit.rlim_cur;
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
    }
}

#[test]
fn a_command_holding_many_tunnels_to_a_live_server_stays_within_its_budget() {
    support::require_sandbox();
    const LIMIT: u64 = 1024;
    limit_open_files(LIMIT);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-exec-network-fds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("scratch");
    std::fs::create_dir_all(dir.join("project")).expect("project");
    let dir = std::fs::canonicalize(dir).expect("canonical");
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_atomic"),
        dir.join("bin/codex-linux-sandbox"),
    )
    .expect("sandbox helper");
    let paths = CodexPaths::with_helpers(
        PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
        dir.join("bin/codex-linux-sandbox"),
    );
    let sandbox = ExecSandbox::new(&paths, StateGuard::NoProjectState, dir.join("codex"))
        .expect("confinement")
        .reaching_loopback_for_tests(vec!["127.0.0.1".into()]);
    let local = LocalBackend::in_dir(dir.join("scratch")).confined(Arc::new(sandbox));
    local.connect().expect("connects");
    let workspace = local
        .open(&atomic_exec::Open {
            label: "network-fds".into(),
            source: Some(dir.join("project")),
            ..atomic_exec::Open::default()
        })
        .expect("opens");

    // A server that keeps every connection it gets.
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let target = listener.local_addr().expect("address").to_string();
    let held: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
    {
        let held = held.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                held.lock().expect("held").push(stream);
            }
        });
    }

    let scratch = dir.join("cwd");
    std::fs::create_dir_all(&scratch).expect("cwd");
    let ready = scratch.join("ready");
    let done = scratch.join("done");
    let command = |done: &Path| Command {
        program: "python3".into(),
        args: vec![
            "-c".into(),
            OPENER.into(),
            target.clone(),
            ready.display().to_string(),
            done.display().to_string(),
        ],
        cwd: scratch.clone(),
        env: vec![
            ("HOME".into(), scratch.display().to_string()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ],
        time_limit: Duration::from_secs(120),
        output_limit: 1 << 16,
        writable: vec![scratch.clone()],
        network: vec!["127.0.0.1".into()],
        ..Command::default()
    };

    let before = open_descriptors();
    std::thread::scope(|s| {
        let first = s.spawn(|| local.run(&command(&done)));
        let started = Instant::now();
        while !ready.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(90) && !first.is_finished(),
                "the command did not open its connections"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        let tunnels: u64 = std::fs::read_to_string(&ready)
            .expect("ready")
            .parse()
            .expect("a count");
        // Let the proxy's last connections settle before counting.
        std::thread::sleep(Duration::from_secs(1));
        let servers = held.lock().expect("held").len() as u64;
        let proxy = open_descriptors()
            .saturating_sub(before)
            .saturating_sub(servers);
        assert!(tunnels >= 64, "the command opened only {tunnels} tunnels");
        // The running command's own pipes are counted too: a few more.
        assert!(
            proxy <= DESCRIPTORS_PER_COMMAND + 8,
            "{tunnels} tunnels cost the proxy {proxy} descriptors, over {DESCRIPTORS_PER_COMMAND}"
        );
        assert!(proxy <= LIMIT / 2, "{proxy} of {LIMIT}");

        // A second command that declares hosts would pass half the limit,
        // and does not start. Its `done` already exists, so were it to
        // start it would end at once.
        let other_done = scratch.join("other-done");
        std::fs::write(&other_done, "").expect("done");
        let second = local.run(&command(&other_done));
        std::fs::write(&done, "").expect("done");
        let error = second.expect_err("a second command started past the share");
        assert!(
            error
                .to_string()
                .contains("commands that declare hosts are running, and atomic's limit of 1024"),
            "{error}"
        );

        let first = first.join().expect("the first command").expect("it runs");
        assert!(
            first.success(),
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
    });

    // Once it has ended, its room is free again.
    std::fs::remove_file(&ready).expect("ready");
    let again = local
        .run(&command(&done))
        .expect("a command starts once the first has ended");
    assert!(
        again.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    local.close(&workspace).expect("closes");
    let _ = std::fs::remove_dir_all(&dir);
}
