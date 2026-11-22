// SPDX-License-Identifier: Apache-2.0

//! A confined command that opens tunnels as fast as it can to a port that
//! drops every SYN, and abandons each, keeps atomic's file descriptors
//! within its budget for the whole run. The proxy answers each `CONNECT`
//! before it dials, and its dial runs until the kernel gives up, for about
//! two minutes; the relay in front of it counts each of the command's
//! connections until the proxy lets go, so the command holds at most its
//! limit of them, and every one past that is closed at once. While it
//! floods, a second command keeps its own connections and reaches a server
//! that answers. atomic's descriptors are sampled throughout.
//!
//! The port is on this machine's loopback interface: a listener whose
//! queue has room for one connection and holds one, so the kernel drops
//! every SYN after it. Needs bubblewrap, unprivileged user namespaces and a
//! network namespace of the command's own, so it runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot
//! start fails the test.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]

mod support;

use atomic_core::codex::{CodexPaths, ExecSandbox, StateGuard};
use atomic_core::network::{DEFAULT_CONNECTIONS_PER_COMMAND, DESCRIPTORS_PER_COMMAND};
use atomic_exec::{Command, ExecutionBackend, LocalBackend};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// For `seconds`, opens a tunnel through the proxy to `target`, waits for
/// the proxy's answer, and abandons it, as fast as it can; writes
/// `flooding` once it has opened its limit; stops early when `stop`
/// exists. Then opens five more and reports how long each took to be
/// closed and what it got.
const FLOOD: &str = r#"import json, os, socket, sys, time
target, seconds, limit, flooding, stop = sys.argv[1:]
host, port = os.environ['HTTP_PROXY'].split('//')[1].rstrip('/').rsplit(':', 1)
request = ('CONNECT ' + target + ' HTTP/1.1\r\nHost: ' + target + '\r\n\r\n').encode()
def attempt():
    s = socket.create_connection((host, int(port)), timeout=5)
    s.settimeout(10)
    s.sendall(request)
    started = time.time()
    try:
        got = s.recv(64)
    except Exception as e:
        got = type(e).__name__.encode()
    s.close()
    return time.time() - started, got[:12].decode(errors='replace')
answered = 0
opened = 0
deadline = time.time() + float(seconds)
while time.time() < deadline and not os.path.exists(stop):
    try:
        _, got = attempt()
        opened += 1
        if got.startswith('HTTP/1.1 200'):
            answered += 1
    except Exception:
        pass
    if answered >= int(limit) and not os.path.exists(flooding):
        open(flooding, 'w').close()
closes = [attempt() for _ in range(5)]
print(json.dumps({'opened': opened, 'answered': answered, 'closes': closes}))
"#;

/// Once `flooding` exists, opens a tunnel through the proxy to `target`
/// and prints what came back through it.
const OTHER: &str = r#"import os, socket, sys, time
target, flooding = sys.argv[1:]
host, port = os.environ['HTTP_PROXY'].split('//')[1].rstrip('/').rsplit(':', 1)
started = time.time()
while not os.path.exists(flooding) and time.time() - started < 60:
    time.sleep(0.05)
s = socket.create_connection((host, int(port)), timeout=5)
s.settimeout(10)
s.sendall(('CONNECT ' + target + ' HTTP/1.1\r\nHost: ' + target + '\r\n\r\n').encode())
got = b''
while b'hello' not in got:
    chunk = s.recv(256)
    if not chunk:
        break
    got += chunk
print(got.decode(errors='replace'))
"#;

/// The file descriptors this process has open.
fn open_descriptors() -> u64 {
    std::fs::read_dir("/proc/self/fd").expect("fds").count() as u64
}

/// A listener on this machine's loopback interface that drops every SYN:
/// its queue has room for one connection, which this fills.
#[allow(unsafe_code)]
fn dropping_syns() -> (TcpListener, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    // SAFETY: listen again, with no backlog, on a socket this test owns.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let queued = TcpStream::connect(listener.local_addr().expect("address")).expect("queued");
    // A second connection does not complete: the kernel drops its SYN.
    let probe = TcpStream::connect_timeout(
        &listener.local_addr().expect("address"),
        Duration::from_millis(1500),
    );
    assert!(probe.is_err(), "the port answers SYNs: {probe:?}");
    (listener, queued)
}

#[test]
fn a_command_abandoning_connections_to_a_port_that_drops_syns_stays_within_its_budget() {
    support::require_sandbox();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-exec-syn-drop-{}", std::process::id()));
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
            label: "syn-drop".into(),
            source: Some(dir.join("project")),
            ..atomic_exec::Open::default()
        })
        .expect("opens");

    let (dropping, _queued) = dropping_syns();
    let dead = dropping.local_addr().expect("address").to_string();
    // A server that greets every connection and keeps it.
    let live = TcpListener::bind("127.0.0.1:0").expect("listener");
    let live_addr = live.local_addr().expect("address").to_string();
    let greeted: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
    {
        let greeted = greeted.clone();
        std::thread::spawn(move || {
            for mut stream in live.incoming().flatten() {
                let _ = stream.write_all(b"hello");
                greeted.lock().expect("greeted").push(stream);
            }
        });
    }

    let scratch = dir.join("cwd");
    std::fs::create_dir_all(&scratch).expect("cwd");
    let flooding = scratch.join("flooding");
    let stop = scratch.join("stop");
    let command = |args: Vec<String>| Command {
        program: "python3".into(),
        args,
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
    let flood = command(vec![
        "-c".into(),
        FLOOD.into(),
        dead.clone(),
        "20".into(),
        DEFAULT_CONNECTIONS_PER_COMMAND.to_string(),
        flooding.display().to_string(),
        stop.display().to_string(),
    ]);
    let other = command(vec![
        "-c".into(),
        OTHER.into(),
        live_addr,
        flooding.display().to_string(),
    ]);

    // The budget: the flooding command's, the other's few connections and
    // the test's own threads and pipes.
    let before = open_descriptors();
    let bound = DESCRIPTORS_PER_COMMAND + 64;
    let peak = AtomicU64::new(0);
    let running = AtomicBool::new(true);
    let (flooded, reached) = std::thread::scope(|s| {
        s.spawn(|| {
            while running.load(Ordering::Relaxed) {
                let now = open_descriptors().saturating_sub(before);
                peak.fetch_max(now, Ordering::Relaxed);
                // Past the bound the control has failed: stop the flood
                // before atomic runs out of descriptors.
                if now > bound {
                    let _ = std::fs::write(&stop, "");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let flooded = s.spawn(|| local.run(&flood));
        let reached = s.spawn(|| local.run(&other));
        let reached = reached.join().expect("the other command");
        let flooded = flooded.join().expect("the flooding command");
        running.store(false, Ordering::Relaxed);
        (flooded, reached)
    });

    let flooded = flooded.expect("the flood runs");
    let stdout = String::from_utf8_lossy(&flooded.stdout);
    assert!(
        flooded.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&flooded.stderr)
    );
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).expect("a report");
    let peak = peak.load(Ordering::Relaxed);
    assert!(
        peak <= bound,
        "opening and abandoning {} tunnels took atomic {peak} descriptors, over {bound}: {report}",
        report["opened"]
    );
    let opened = report["opened"].as_u64().expect("opened");
    let answered = report["answered"].as_u64().expect("answered");
    assert!(
        opened > 4 * u64::from(DEFAULT_CONNECTIONS_PER_COMMAND),
        "it opened only {opened}: {report}"
    );
    assert_eq!(
        answered,
        u64::from(DEFAULT_CONNECTIONS_PER_COMMAND),
        "the proxy tunnelled the limit and no more: {report}"
    );
    // Over its limit, a connection is closed at once, unanswered.
    for close in report["closes"].as_array().expect("closes") {
        let took = close[0].as_f64().expect("seconds");
        let got = close[1].as_str().expect("what it got");
        assert!(
            took < 2.0 && !got.starts_with("HTTP/1.1 200"),
            "a connection over the limit: {close}"
        );
    }
    // The other command kept its own budget.
    let reached = reached.expect("the other command runs");
    assert!(
        reached.success() && String::from_utf8_lossy(&reached.stdout).contains("hello"),
        "the other command: {}{}",
        String::from_utf8_lossy(&reached.stdout),
        String::from_utf8_lossy(&reached.stderr)
    );
    assert!(
        !stop.exists(),
        "the flood was stopped at the bound: {peak} descriptors"
    );
    local.close(&workspace).expect("closes");
    let _ = std::fs::remove_dir_all(&dir);
}
