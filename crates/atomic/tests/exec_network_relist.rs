// SPDX-License-Identifier: Apache-2.0

//! A confined command that declares a host, whose sandbox helper starts
//! again because a link was made in a denied place after its deny list was
//! built, still never holds its proxy token: not in its environment, nor in
//! that of any process it can see, and the helper, started again, cannot be
//! read through /proc by another process of the same user. The helper walks
//! for late links before it reads the token from its socket, so the token
//! never passes through the start again; read first, it would reach the
//! helper started again in its environment, which the helper refuses. The
//! bridge's token still works: the request for the declared host, which
//! never resolves, passes the proxy's token and host checks and is refused
//! only by the address check (`not_allowed_local`).
//!
//! Needs bubblewrap, unprivileged user namespaces and a network namespace
//! of the command's own, so it runs with the `sandbox-tests` feature; with
//! the feature on, a sandbox that cannot start fails the test. It sets HOME
//! and ATOMIC_HOME for its whole process, so it has a binary of its own.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]

mod support;

use atomic_core::codex::{CodexPaths, ExecSandbox, StateGuard};
use atomic_exec::Confine;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What the command checks about its proxy token, and whether it can read
/// the file a late link leads to.
const TOKEN: &str = r#"import json, os, sys, time, urllib.request
ready, done, kept = sys.argv[1:]
key = 'CODEX_NETWORK_PROXY_ATTRIBUTION'
seen = []
for pid in os.listdir('/proc'):
    if pid.isdigit():
        try:
            if key.encode() in open('/proc/' + pid + '/environ', 'rb').read():
                seen.append(pid)
        except Exception:
            pass
try:
    open(kept).read()
    read = 'ok'
except Exception as e:
    read = type(e).__name__
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
    'secret_fd_var': 'ATOMIC_SECRET_ENV_FD' in os.environ,
    'kept': read,
    'fetched': fetched,
}, sys.stdout)
"#;

/// This process's children that are the sandbox helper, by the name the
/// kernel keeps (`atomic` once it has started again from atomic's own
/// executable, its first 15 bytes as the helper's name before), with the
/// helper processes they forked, its bridges among them: pid and name.
fn sandbox_helpers() -> Vec<(u32, String)> {
    let helper = |name: &str| name == "atomic" || name == "codex-linux-san";
    // pid, parent, name, for every process.
    let all: Vec<(u32, u32, String)> = std::fs::read_dir("/proc")
        .expect("proc")
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // pid (comm) state ppid ...
            let (head, rest) = stat.rsplit_once(") ")?;
            let (_, name) = head.split_once(" (")?;
            let ppid = rest.split(' ').nth(1)?.parse().ok()?;
            Some((pid, ppid, name.to_owned()))
        })
        .collect();
    let mut found: Vec<(u32, String)> = all
        .iter()
        .filter(|(_, ppid, name)| *ppid == std::process::id() && helper(name))
        .map(|(pid, _, name)| (*pid, name.clone()))
        .collect();
    let parents: Vec<u32> = found.iter().map(|(pid, _)| *pid).collect();
    found.extend(
        all.iter()
            .filter(|(_, ppid, name)| parents.contains(ppid) && helper(name))
            .map(|(pid, _, name)| (*pid, name.clone())),
    );
    found
}

#[test]
fn a_command_whose_helper_starts_again_for_a_late_link_never_holds_its_proxy_token() {
    support::require_sandbox();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-exec-network-relist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in [
        "bin",
        "person/.aws",
        "person/elsewhere",
        "project",
        "cwd/.tmp",
    ] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
    let dir = std::fs::canonicalize(dir).expect("canonical");
    // The credential places follow HOME; this test is alone in its binary,
    // and sets it before any thread starts.
    // SAFETY: the process has one thread here.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("ATOMIC_HOME", dir.join("home"));
        std::env::set_var("HOME", dir.join("person"));
    }
    let (link, kept) = (
        dir.join("person/.aws/credentials"),
        dir.join("person/elsewhere/creds"),
    );
    std::fs::write(&kept, "the person's key").expect("key");
    let state = dir.join("home/projects/p");
    std::fs::create_dir_all(&state).expect("state");
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_atomic"),
        dir.join("bin/codex-linux-sandbox"),
    )
    .expect("sandbox helper");
    let paths = CodexPaths::with_helpers(
        PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
        dir.join("bin/codex-linux-sandbox"),
    );
    let sandbox = ExecSandbox::new(
        &paths,
        StateGuard::deny_state_dir(&state).expect("guard"),
        state.join("codex"),
    )
    .expect("confinement");
    let scratch = dir.join("cwd");
    let (ready, done) = (scratch.join("ready"), scratch.join("done"));
    let text = |p: &Path| p.display().to_string();
    // The command's list is built now, before the link exists, as a
    // thread's is when it begins; its proxy is held while it runs.
    let (confined, _held) = sandbox
        .confine_held(
            &atomic_exec::Command {
                program: "python3".into(),
                args: vec![
                    "-c".into(),
                    TOKEN.into(),
                    text(&ready),
                    text(&done),
                    text(&kept),
                ],
                cwd: scratch.clone(),
                env: vec![
                    ("HOME".into(), text(&scratch)),
                    ("PATH".into(), "/usr/bin:/bin".into()),
                ],
                writable: vec![scratch.clone()],
                network: vec!["allowed.invalid".into()],
                time_limit: Duration::from_secs(60),
                output_limit: 1 << 16,
                ..Default::default()
            },
            &scratch.join(".tmp"),
        )
        .expect("confined");
    // Another process links the key in before the command starts, so its
    // helper finds the link and starts again with it denied.
    std::os::unix::fs::symlink(&kept, &link).expect("link");
    let (finished, helpers) = std::thread::scope(|s| {
        let run = s.spawn(|| {
            atomic_exec::LocalBackend::run_in(atomic_exec::process::Group::Own, &confined)
        });
        let started = std::time::Instant::now();
        while !ready.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(50) && !run.is_finished(),
                "the command did not start"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        let helpers: Vec<(u32, String, std::io::Result<Vec<u8>>)> = sandbox_helpers()
            .into_iter()
            .map(|(pid, name)| {
                let read = std::fs::read(format!("/proc/{pid}/environ"));
                (pid, name, read)
            })
            .collect();
        std::fs::write(&done, "").expect("done");
        (run.join().expect("the command"), helpers)
    });
    let finished = finished.expect("the command runs");
    assert!(
        finished.success(),
        "{}",
        String::from_utf8_lossy(&finished.stderr)
    );
    let got: Value = serde_json::from_slice(&finished.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {}{}",
            String::from_utf8_lossy(&finished.stdout),
            String::from_utf8_lossy(&finished.stderr)
        )
    });
    // The helper started again: it denied what the late link leads to, and
    // it runs as atomic's own executable, which it started again from.
    assert_eq!(got["kept"], "PermissionError", "{got}");
    let (helper, name, _) = helpers
        .first()
        .expect("found no sandbox helper of this process");
    assert_eq!(name, "atomic", "the helper {helper} did not start again");
    assert_eq!(got["env"], false, "{got}");
    assert_eq!(got["self"], false, "{got}");
    assert_eq!(got["seen"], serde_json::json!([]), "{got}");
    assert_eq!(got["secret_fd_var"], false, "{got}");
    assert_eq!(got["fetched"], "HTTP 403 not_allowed_local", "{got}");
    for (pid, name, read) in helpers {
        match read {
            Err(e) => assert_eq!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{pid} ({name}): {e}"
            ),
            Ok(env) => panic!(
                "read the sandbox helper {pid} ({name})'s environment ({} bytes, {} the token)",
                env.len(),
                if String::from_utf8_lossy(&env).contains("CODEX_NETWORK_PROXY_ATTRIBUTION") {
                    "with"
                } else {
                    "without"
                }
            ),
        }
    }
    // A helper started again with the token in its environment, as reading
    // it before the walk would leave it, does not run the command, and says
    // why.
    let mut again = confined.clone();
    again.secret_env = Default::default();
    again
        .env
        .push(("ATOMIC_SANDBOX_RELISTED".into(), "1".into()));
    again
        .env
        .push(("CODEX_NETWORK_PROXY_ATTRIBUTION".into(), "a-token".into()));
    let refused =
        atomic_exec::LocalBackend::run_in(atomic_exec::process::Group::Own, &again).expect("runs");
    assert!(
        !refused.success(),
        "ran: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("started again with the proxy token in its environment"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
