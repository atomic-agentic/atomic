// SPDX-License-Identifier: Apache-2.0

//! A confined exec command reaches only the hosts it is given, through
//! atomic's proxy, and with none it has no network at all. Two HTTP servers
//! run on this machine: the command is given the first one's address, so it
//! fetches from that server and is refused the other, and it can open no
//! connection of its own, to either, nor a Unix socket. The atom-level
//! policy, which refuses addresses like these in an atom's declaration, is
//! tested where atoms load; this test gives the sandbox the hosts directly.
//!
//! Needs bubblewrap, unprivileged user namespaces and a network namespace
//! of the command's own, so it runs with the `sandbox-tests` feature; with
//! the feature on, a sandbox that cannot start fails the test.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![recursion_limit = "512"]

mod support;

use atomic_core::codex::{CodexPaths, ExecSandbox, StateGuard};
use atomic_exec::{Command, ExecutionBackend, LocalBackend};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// What the command tries, each reported as what it got or the error.
const PROBE: &str = r#"import json, os, socket, sys, urllib.request
allowed, other, path = sys.argv[1:]
def attempt(f):
    try:
        return f()
    except urllib.error.HTTPError as e:
        return 'HTTP ' + str(e.code)
    except Exception as e:
        return type(e).__name__ + ': ' + str(e)
# Through the proxy the command is given, whatever NO_PROXY says, or with
# no proxy at all.
proxy = os.environ.get('HTTP_PROXY')
opener = urllib.request.build_opener(urllib.request.ProxyHandler({'http': proxy} if proxy else {}))
def fetch(url):
    return lambda: opener.open(url, timeout=10).read().decode()
def direct(address):
    def connect():
        host, port = address.split(':')
        socket.create_connection((host, int(port)), timeout=5).close()
        return 'connected'
    return connect
def unix():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    return 'connected'
json.dump({
    'proxied': 'HTTP_PROXY' in os.environ,
    'allowed': attempt(fetch('http://' + allowed + '/')),
    'other': attempt(fetch('http://' + other + '/')),
    'direct_allowed': attempt(direct(allowed)),
    'direct_other': attempt(direct(other)),
    'unix': attempt(unix),
}, sys.stdout)
"#;

/// An HTTP server on `ip` that answers every request with `body`.
fn serve(ip: &str, body: &'static str) -> String {
    let listener = std::net::TcpListener::bind((ip, 0)).expect("listener");
    let address = listener.local_addr().expect("address").to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    address
}

#[test]
fn a_confined_command_reaches_only_the_hosts_it_is_given() {
    support::require_sandbox();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-exec-network-{}", std::process::id()));
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
    // The proxy reaches 127.0.0.1 because this test says so: atomic's policy
    // refuses any address, and a confined command naming another is refused.
    let sandbox = ExecSandbox::new(&paths, StateGuard::NoProjectState, dir.join("codex"))
        .expect("confinement")
        .reaching_loopback_for_tests(vec!["127.0.0.1".into()]);
    let local = LocalBackend::in_dir(dir.join("scratch")).confined(Arc::new(sandbox));
    local.connect().expect("connects");
    // Opening a workspace makes the backend's private temporary directory,
    // and checks that the sandbox starts.
    let workspace = local
        .open(&atomic_exec::Open {
            label: "network".into(),
            source: Some(dir.join("project")),
            ..atomic_exec::Open::default()
        })
        .expect("opens");

    let allowed = serve("127.0.0.1", "from the allowed server");
    let other = serve("127.0.0.2", "from the other server");
    let socket = dir.join("bus.sock");
    let _bus = std::os::unix::net::UnixListener::bind(&socket).expect("unix socket");
    let allowed_host = allowed.split(':').next().expect("host").to_owned();

    let command = |network: Vec<String>| -> Command {
        let scratch = dir.join("cwd");
        std::fs::create_dir_all(&scratch).expect("cwd");
        Command {
            program: "python3".into(),
            args: vec![
                "-c".into(),
                PROBE.into(),
                allowed.clone(),
                other.clone(),
                socket.display().to_string(),
            ],
            cwd: scratch.clone(),
            env: vec![
                ("HOME".into(), scratch.display().to_string()),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ],
            time_limit: Duration::from_secs(60),
            output_limit: 1 << 16,
            writable: vec![scratch],
            network,
            ..Command::default()
        }
    };
    let run = |network: Vec<String>| -> Value {
        let done = local.run(&command(network)).expect("the command runs");
        assert!(done.success(), "{}", String::from_utf8_lossy(&done.stderr));
        serde_json::from_slice(&done.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}: {}{}",
                String::from_utf8_lossy(&done.stdout),
                String::from_utf8_lossy(&done.stderr)
            )
        })
    };
    let refused = |v: &Value| v.as_str().is_some_and(|s| s.contains("Error"));

    // Given the first server's host, it fetches from it through the proxy,
    // and the proxy refuses the other.
    let given = run(vec![allowed_host]);
    assert_eq!(given["proxied"], true, "{given}");
    assert_eq!(given["allowed"], "from the allowed server", "{given}");
    assert_eq!(given["other"], "HTTP 403", "{given}");
    for probe in ["direct_allowed", "direct_other", "unix"] {
        assert!(refused(&given[probe]), "{probe}: {given}");
    }

    // A command naming an address the test did not allow, or any host
    // atomic's policy refuses, does not start.
    for refused in ["127.0.0.2", "localhost", "printer.local"] {
        let error = local
            .run(&command(vec![refused.into()]))
            .expect_err("refused");
        assert!(
            error.to_string().contains("its network"),
            "{refused}: {error}"
        );
    }

    // Given none, it has no network at all.
    let none = run(Vec::new());
    assert_eq!(none["proxied"], false, "{none}");
    for probe in ["allowed", "other", "direct_allowed", "direct_other", "unix"] {
        assert!(refused(&none[probe]), "{probe}: {none}");
    }
    local.close(&workspace).expect("closes");
    let _ = std::fs::remove_dir_all(&dir);
}
