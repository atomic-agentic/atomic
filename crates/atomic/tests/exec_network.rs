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

/// The longest path a Unix socket may have: `sockaddr_un`'s `sun_path`,
/// which `bind()` copies the path into, holds 107 bytes and the NUL. Past
/// that `bind()` fails with "path must be shorter than SUN_LEN", so a test
/// whose scratch directory sits deep — a linked worktree's target directory
/// alone takes most of the budget — fails before it checks anything.
const SUN_PATH_LIMIT: usize = 107;

/// A short directory under `/tmp` that one test makes for its Unix socket
/// alone and removes when dropped, even when the test fails. The socket is
/// the only thing that lives there; the test's other scratch stays where it
/// was, however deep that is.
///
/// The socket stays a filesystem one, not an abstract one: a confined command
/// has a network namespace of its own, and an abstract socket is named inside
/// one, so it would be out of the command's reach whatever the sandbox let
/// sockets do, and the probe would prove nothing.
struct ShortSocketDir(PathBuf);

impl ShortSocketDir {
    /// Make the directory, its owner's alone, named with this process's pid
    /// and a nonce, so a run or a checkout sharing `/tmp` cannot collide with
    /// it; a name already taken is tried again.
    fn new() -> Self {
        let mut last = String::from("/tmp");
        for _ in 0..8 {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let dir = Path::new("/tmp").join(format!(
                "atomic-exec-network-{}-{nonce:08x}",
                std::process::id()
            ));
            let mut builder = std::fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(&dir) {
                Ok(()) => return Self(dir),
                Err(e) => last = format!("{}: {e}", dir.display()),
            }
        }
        panic!("could not make a short directory for the test's Unix socket: {last}");
    }

    /// Where a socket named `name` goes here, short enough for `bind()`.
    fn socket(&self, name: &str) -> PathBuf {
        let socket = self.0.join(name);
        assert!(
            socket.as_os_str().as_encoded_bytes().len() <= SUN_PATH_LIMIT,
            "{} is longer than a Unix socket path may be",
            socket.display()
        );
        socket
    }
}

impl Drop for ShortSocketDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
    // A Unix socket a confined command must not reach, such as a bus. Only
    // the socket moves, into a short directory of its own; the rest of the
    // scratch stays under the target directory, however deep that is.
    let short = ShortSocketDir::new();
    let socket = short.socket("bus.sock");
    let _bus = std::os::unix::net::UnixListener::bind(&socket).expect("unix socket");
    // The positive control: outside the sandbox the socket answers, so the
    // refusal the command meets below is the sandbox's, which the probe's
    // exact error shows.
    let _reached = std::os::unix::net::UnixStream::connect(&socket).expect("the socket answers");
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
    // The socket is refused by the sandbox, not missing from the command's
    // view: seccomp answers EPERM before any path lookup, whereas a path
    // that leads nowhere would be FileNotFoundError and a 0700 directory
    // EACCES, so the probe must say exactly this.
    const SOCKET_REFUSED: &str = "PermissionError: [Errno 1] Operation not permitted";

    // Given the first server's host, it fetches from it through the proxy,
    // and the proxy refuses the other.
    let given = run(vec![allowed_host]);
    assert_eq!(given["proxied"], true, "{given}");
    assert_eq!(given["allowed"], "from the allowed server", "{given}");
    assert_eq!(given["other"], "HTTP 403", "{given}");
    for probe in ["direct_allowed", "direct_other"] {
        assert!(refused(&given[probe]), "{probe}: {given}");
    }
    assert_eq!(given["unix"], SOCKET_REFUSED, "{given}");

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
    for probe in ["allowed", "other", "direct_allowed", "direct_other"] {
        assert!(refused(&none[probe]), "{probe}: {none}");
    }
    assert_eq!(none["unix"], SOCKET_REFUSED, "{none}");
    local.close(&workspace).expect("closes");
    let _ = std::fs::remove_dir_all(&dir);
}

/// What a command with both declared hosts and read-only inputs tries.
const BOTH: &str = r#"import json, os, sys, urllib.request
allowed, other, inputs = sys.argv[1:]
def attempt(f):
    try:
        return f()
    except urllib.error.HTTPError as e:
        return 'HTTP ' + str(e.code)
    except Exception as e:
        return type(e).__name__ + ': ' + str(e)
proxy = os.environ.get('HTTP_PROXY')
opener = urllib.request.build_opener(urllib.request.ProxyHandler({'http': proxy} if proxy else {}))
def fetch(url):
    return lambda: opener.open(url, timeout=10).read().decode()
def write(path):
    def w():
        with open(path, 'w') as f:
            f.write('x')
        return 'written'
    return w
json.dump({
    'read': attempt(lambda: open(os.path.join(inputs, 'given')).read()),
    'plant': attempt(write(os.path.join(inputs, 'planted'))),
    'scratch': attempt(write(os.path.join(os.getcwd(), 'made'))),
    'allowed': attempt(fetch('http://' + allowed + '/')),
    'other': attempt(fetch('http://' + other + '/')),
}, sys.stdout)
"#;

/// A command given declared hosts and read-only inputs at once keeps both:
/// it reads its inputs and cannot write them, though they lie inside the
/// scratch directory it may write, and it still reaches only the hosts it
/// is given.
#[test]
fn a_confined_command_with_hosts_and_read_only_inputs_keeps_both() {
    support::require_sandbox();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-exec-network-ro-{}", std::process::id()));
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
            label: "network-ro".into(),
            source: Some(dir.join("project")),
            ..atomic_exec::Open::default()
        })
        .expect("opens");
    let allowed = serve("127.0.0.1", "from the allowed server");
    let other = serve("127.0.0.2", "from the other server");
    let allowed_host = allowed.split(':').next().expect("host").to_owned();

    let scratch = dir.join("cwd");
    let inputs = scratch.join("inputs");
    std::fs::create_dir_all(&inputs).expect("inputs");
    std::fs::write(inputs.join("given"), "an input").expect("input");
    let done = local
        .run(&Command {
            program: "python3".into(),
            args: vec![
                "-c".into(),
                BOTH.into(),
                allowed.clone(),
                other.clone(),
                inputs.display().to_string(),
            ],
            cwd: scratch.clone(),
            env: vec![
                ("HOME".into(), scratch.display().to_string()),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ],
            time_limit: Duration::from_secs(60),
            output_limit: 1 << 16,
            writable: vec![scratch.clone()],
            read_only: vec![inputs.clone()],
            network: vec![allowed_host],
            ..Command::default()
        })
        .expect("the command runs");
    assert!(done.success(), "{}", String::from_utf8_lossy(&done.stderr));
    let got: Value = serde_json::from_slice(&done.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {}{}",
            String::from_utf8_lossy(&done.stdout),
            String::from_utf8_lossy(&done.stderr)
        )
    });
    assert_eq!(got["read"], "an input", "{got}");
    assert!(
        got["plant"].as_str().is_some_and(|s| s.contains("Error")),
        "the read-only inputs were written: {got}"
    );
    assert!(!inputs.join("planted").exists(), "{got}");
    assert_eq!(got["scratch"], "written", "{got}");
    assert_eq!(got["allowed"], "from the allowed server", "{got}");
    assert_eq!(got["other"], "HTTP 403", "{got}");
    local.close(&workspace).expect("closes");
    let _ = std::fs::remove_dir_all(&dir);
}

/// What a command given a declared host checks about its proxy token.
const TOKEN: &str = r#"import json, os, sys, time, urllib.request
ready, done = sys.argv[1:]
key = 'CODEX_NETWORK_PROXY_ATTRIBUTION'
seen = []
for pid in os.listdir('/proc'):
    if pid.isdigit():
        try:
            if key.encode() in open('/proc/' + pid + '/environ', 'rb').read():
                seen.append(pid)
        except Exception:
            pass
opener = urllib.request.build_opener(urllib.request.ProxyHandler({'http': os.environ['HTTP_PROXY']}))
try:
    opener.open('http://allowed.invalid/', timeout=20).read()
    fetched = 'ok'
except urllib.error.HTTPError as e:
    fetched = 'HTTP ' + str(e.code) + ' ' + json.loads(e.read()).get('reason', '')
except Exception as e:
    fetched = type(e).__name__ + ': ' + str(e)
open(ready, 'w').close()
while not os.path.exists(done):
    time.sleep(0.1)
json.dump({
    'env': key in os.environ,
    'self': key.encode() in open('/proc/self/environ', 'rb').read(),
    'seen': seen,
    'fetched': fetched,
}, sys.stdout)
"#;

/// This process's children that are Codex's sandbox helper (by the name
/// the kernel keeps, its first 15 bytes) for a command run in `dir`, which
/// its command line names, with the helper processes they forked, its
/// bridges among them.
fn sandbox_helpers_in(dir: &Path) -> Vec<u32> {
    let comm = "(codex-linux-san";
    let dir = dir.display().to_string();
    // pid, parent, name, for every process.
    let all: Vec<(u32, u32, String)> = std::fs::read_dir("/proc")
        .expect("proc")
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // pid (comm) state ppid ...
            let (head, rest) = stat.rsplit_once(") ")?;
            let ppid = rest.split(' ').nth(1)?.parse().ok()?;
            Some((pid, ppid, head.to_owned()))
        })
        .collect();
    let mut found: Vec<u32> = all
        .iter()
        .filter(|(pid, ppid, head)| {
            *ppid == std::process::id()
                && head.ends_with(comm)
                && std::fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|c| String::from_utf8_lossy(&c).contains(&dir))
        })
        .map(|(pid, _, _)| *pid)
        .collect();
    let forked: Vec<u32> = all
        .iter()
        .filter(|(_, ppid, head)| found.contains(ppid) && head.ends_with(comm))
        .map(|(pid, _, _)| *pid)
        .collect();
    found.extend(forked);
    found
}

/// A confined command's proxy token is its bridge's, never the command's:
/// not in its environment, nor in that of any process it can see, and the
/// helper outside the sandbox that started with it cannot be read through
/// /proc by another process of the same user. The bridge's token still
/// works: the command's request for its declared host, which never
/// resolves, passes the proxy's token and host checks and is refused only
/// by the address check (`not_allowed_local`). No loopback address is on
/// the proxy's list here, so every request goes through the token check.
#[test]
fn a_confined_command_never_holds_its_proxy_token() {
    support::require_sandbox();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-exec-network-token-{}", std::process::id()));
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
        .expect("confinement");
    let local = LocalBackend::in_dir(dir.join("scratch")).confined(Arc::new(sandbox));
    local.connect().expect("connects");
    let workspace = local
        .open(&atomic_exec::Open {
            label: "network-token".into(),
            source: Some(dir.join("project")),
            ..atomic_exec::Open::default()
        })
        .expect("opens");
    let scratch = dir.join("cwd");
    std::fs::create_dir_all(&scratch).expect("cwd");
    let ready = scratch.join("ready");
    let done = scratch.join("done");
    let command = Command {
        program: "python3".into(),
        args: vec![
            "-c".into(),
            TOKEN.into(),
            ready.display().to_string(),
            done.display().to_string(),
        ],
        cwd: scratch.clone(),
        env: vec![
            ("HOME".into(), scratch.display().to_string()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ],
        time_limit: Duration::from_secs(60),
        output_limit: 1 << 16,
        writable: vec![scratch.clone()],
        network: vec!["allowed.invalid".into()],
        ..Command::default()
    };
    let (done_run, helpers) = std::thread::scope(|s| {
        let run = s.spawn(|| local.run(&command));
        let started = std::time::Instant::now();
        while !ready.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(50) && !run.is_finished(),
                "the command did not start"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        let helpers: Vec<(u32, std::io::Result<Vec<u8>>)> = sandbox_helpers_in(&scratch)
            .into_iter()
            .map(|pid| (pid, std::fs::read(format!("/proc/{pid}/environ"))))
            .collect();
        std::fs::write(&done, "").expect("done");
        (run.join().expect("the command"), helpers)
    });
    let done_run = done_run.expect("the command runs");
    assert!(
        done_run.success(),
        "{}",
        String::from_utf8_lossy(&done_run.stderr)
    );
    let got: Value = serde_json::from_slice(&done_run.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {}{}",
            String::from_utf8_lossy(&done_run.stdout),
            String::from_utf8_lossy(&done_run.stderr)
        )
    });
    assert_eq!(got["env"], false, "{got}");
    assert_eq!(got["self"], false, "{got}");
    assert_eq!(got["seen"], serde_json::json!([]), "{got}");
    assert_eq!(got["fetched"], "HTTP 403 not_allowed_local", "{got}");
    assert!(
        !helpers.is_empty(),
        "found no sandbox helper of this process"
    );
    for (pid, read) in helpers {
        match read {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{pid}: {e}"),
            Ok(env) => panic!(
                "read the sandbox helper {pid}'s environment ({} bytes, {} the token)",
                env.len(),
                if String::from_utf8_lossy(&env).contains("CODEX_NETWORK_PROXY_ATTRIBUTION") {
                    "with"
                } else {
                    "without"
                }
            ),
        }
    }
    local.close(&workspace).expect("closes");
    let _ = std::fs::remove_dir_all(&dir);
}
