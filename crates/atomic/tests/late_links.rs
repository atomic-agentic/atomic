// SPDX-License-Identifier: Apache-2.0

//! A link made inside a denied place after a command's deny list was built
//! is denied when the next command starts, as a turn's commands start, each
//! with the list its thread built when it began: a key linked from
//! `~/.aws/credentials` to a file kept elsewhere cannot be read by its own
//! path, nor through the link.
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot start
//! fails the test.

#![cfg(all(
    any(target_os = "linux", target_os = "macos"),
    feature = "sandbox-tests"
))]

mod support;

use atomic_core::codex::{CodexPaths, ExecSandbox, StateGuard};
use atomic_exec::Confine;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Reads each path the environment names, reporting `ok` or the error.
const PROBE: &str = r#"import json, os
def attempt(path):
    try:
        with open(path) as f:
            f.read()
        return 'ok'
    except Exception as e:
        return type(e).__name__ + ': ' + str(e)
print(json.dumps({name.lower(): attempt(os.environ[name]) for name in ['LINK', 'TARGET', 'PLACE', 'KEPT']}))
"#;

#[test]
fn a_link_made_after_the_list_is_denied_to_the_next_command() {
    support::require_sandbox();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-late-links-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in [
        "bin",
        "person/.aws",
        "person/elsewhere",
        "project",
        "scratch/.tmp",
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
    let (link, target) = (
        dir.join("person/.aws/credentials"),
        dir.join("person/elsewhere/creds"),
    );
    std::fs::write(&target, "the person's key").expect("key");
    // A credential place that is not there when the list is built, and
    // becomes a link to a file kept elsewhere.
    let (place, kept) = (
        dir.join("person/.netrc"),
        dir.join("person/elsewhere/netrc"),
    );
    std::fs::write(&kept, "machine example.com").expect("netrc");
    std::fs::write(dir.join("project/probe.py"), PROBE).expect("probe");
    let state = dir.join("home/projects/p");
    std::fs::create_dir_all(&state).expect("state");
    #[cfg(target_os = "linux")]
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
    let text = |p: &Path| p.display().to_string();
    // The command's list is built now, before the link exists, as a
    // thread's is when it begins.
    let confined = sandbox
        .confine(
            &atomic_exec::Command {
                program: "python3".into(),
                args: vec!["probe.py".into()],
                cwd: dir.join("project"),
                env: vec![
                    ("HOME".into(), text(&dir.join("scratch"))),
                    ("LINK".into(), text(&link)),
                    ("TARGET".into(), text(&target)),
                    ("PLACE".into(), text(&place)),
                    ("KEPT".into(), text(&kept)),
                ],
                writable: vec![dir.join("scratch")],
                time_limit: std::time::Duration::from_secs(60),
                output_limit: 1 << 16,
                ..Default::default()
            },
            &dir.join("scratch/.tmp"),
        )
        .expect("confined");
    let run = || -> Value {
        let done = atomic_exec::LocalBackend::run_in(atomic_exec::process::Group::Own, &confined)
            .expect("runs");
        assert!(done.success(), "{}", String::from_utf8_lossy(&done.stderr));
        serde_json::from_slice(&done.stdout).expect("its report")
    };
    // The positive control: with no link, the file elsewhere is no one's
    // secret yet, and the command reads it.
    let before = run();
    assert_eq!(before["target"], "ok", "{before}");
    assert_eq!(before["kept"], "ok", "{before}");
    // Another process links the key in while the thread runs, and makes the
    // place a link.
    std::os::unix::fs::symlink(&target, &link).expect("link");
    std::os::unix::fs::symlink(&kept, &place).expect("link");
    let after = run();
    eprintln!("before: {before}\nafter: {after}");
    let denied = |p: &Path| {
        if cfg!(target_os = "macos") {
            format!(
                "PermissionError: [Errno 1] Operation not permitted: '{}'",
                p.display()
            )
        } else {
            format!(
                "PermissionError: [Errno 13] Permission denied: '{}'",
                p.display()
            )
        }
    };
    for read in ["kept", "place"] {
        let path = if read == "kept" { &kept } else { &place };
        assert_eq!(after[read], denied(path), "{read}: {after}");
    }
    assert_eq!(after["target"], denied(&target), "{after}");
    // Through the link, inside the denied place, there is nothing to read.
    assert!(
        after["link"]
            .as_str()
            .is_some_and(|r| r.starts_with("FileNotFoundError") || r.starts_with("PermissionError")),
        "read through the link: {after}"
    );
    // Started again once already, as after it found a new link, a helper
    // that finds yet another does not run the command.
    #[cfg(target_os = "linux")]
    {
        let other = dir.join("person/elsewhere/other");
        std::fs::write(&other, "another key").expect("key");
        std::os::unix::fs::symlink(&other, dir.join("person/.aws/config")).expect("link");
        let mut again = confined.clone();
        again
            .env
            .push(("ATOMIC_SANDBOX_RELISTED".into(), "1".into()));
        let done = atomic_exec::LocalBackend::run_in(atomic_exec::process::Group::Own, &again)
            .expect("runs");
        assert!(
            !done.success(),
            "ran: {}",
            String::from_utf8_lossy(&done.stdout)
        );
        assert!(
            String::from_utf8_lossy(&done.stderr).contains("kept changing"),
            "{}",
            String::from_utf8_lossy(&done.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
