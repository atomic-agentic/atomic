// SPDX-License-Identifier: Apache-2.0

//! A place denied to atoms' commands stays denied wherever else it is
//! mounted: with the person's home bound at a second path as well, as
//! toolbox binds the host's files under `/run/host`, an exec atom's command
//! can read their key through neither.
//!
//! The test runs itself again in a user and mount namespace of its own,
//! where the home is bound at the second path; nothing outside sees the
//! mount. Needs bubblewrap and unprivileged user namespaces, so it runs with
//! the `sandbox-tests` feature; with the feature on, a sandbox that cannot
//! start fails the test.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]

mod support;

use atomic_core::codex::{CodexPaths, ExecSandbox, StateGuard};
use atomic_core::runtime::{self, EffectIdentity};
use atomic_core::testing::on_codex_runtime;
use atomic_exec::LocalBackend;
use atomic_schema::registry::Registry;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Set in the run of this test inside its namespace, to its directory.
const CHILD: &str = "ATOMIC_SECOND_MOUNT_DIR";

/// The test's name, which the run inside the namespace runs alone.
const NAME: &str = "a_denied_path_is_denied_at_a_second_mount_of_its_directory";

/// Reads each path the environment names, reporting `ok` or the error.
const PROBE: &str = r#"import json, os, sys
json.load(sys.stdin)
def attempt(path):
    try:
        with open(path) as f:
            f.read()
        return 'ok'
    except Exception as e:
        return type(e).__name__ + ': ' + str(e)
json.dump({'home': attempt(os.environ['HOME_SECRET']), 'second': attempt(os.environ['SECOND_SECRET'])}, sys.stdout)
"#;

/// Run this test again in a user and mount namespace of its own, with
/// `person` bound at `second` there.
fn run_with_second_mount(dir: &Path, person: &Path, second: &Path) {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).expect("a path");
    let (from, to) = (c(person), c(second));
    // SAFETY: getuid and getgid cannot fail and touch no memory.
    #[allow(unsafe_code)]
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let uid_map = format!("{uid} {uid} 1");
    let gid_map = format!("{gid} {gid} 1");
    let mut child = std::process::Command::new(std::env::current_exe().expect("this test"));
    child
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD, dir)
        .env("HOME", person)
        .env("ATOMIC_HOME", dir.join("home"));
    // SAFETY: between fork and exec this calls only async-signal-safe
    // functions, on memory made before the fork.
    #[allow(unsafe_code)]
    unsafe {
        child.pre_exec(move || {
            let fail = || Err(std::io::Error::last_os_error());
            let write = |path: &[u8], text: &[u8]| -> std::io::Result<()> {
                let fd = libc::open(path.as_ptr().cast(), libc::O_WRONLY);
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let n = libc::write(fd, text.as_ptr().cast(), text.len());
                libc::close(fd);
                if n < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            };
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                return fail();
            }
            write(b"/proc/self/setgroups\0", b"deny")?;
            write(b"/proc/self/uid_map\0", uid_map.as_bytes())?;
            write(b"/proc/self/gid_map\0", gid_map.as_bytes())?;
            let none = std::ptr::null();
            if libc::mount(
                none,
                c"/".as_ptr(),
                none,
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ) != 0
            {
                return fail();
            }
            if libc::mount(
                from.as_ptr(),
                to.as_ptr(),
                none,
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            ) != 0
            {
                return fail();
            }
            Ok(())
        });
    }
    let out = child.output().expect("the test runs again");
    assert!(
        out.status.success(),
        "inside its namespace the test failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The harness's summary, whatever the test printed around its own line.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(NAME) && stdout.contains("test result: ok. 1 passed"),
        "it did not run inside its namespace:\n{stdout}"
    );
}

#[test]
fn a_denied_path_is_denied_at_a_second_mount_of_its_directory() {
    support::require_sandbox();
    let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("atomic-second-mount-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("second")).expect("scratch");
        let dir = std::fs::canonicalize(dir).expect("canonical");
        let person = dir.join("person");
        std::fs::create_dir_all(person.join(".ssh")).expect("ssh");
        std::fs::write(person.join(".ssh/id_ed25519"), "the person's key").expect("key");
        run_with_second_mount(&dir, &person, &dir.join("second"));
        let _ = std::fs::remove_dir_all(&dir);
        return;
    };
    let (person, second) = (dir.join("person"), dir.join("second"));
    let home_secret = person.join(".ssh/id_ed25519");
    let second_secret = second.join(".ssh/id_ed25519");
    // The positive control: outside the sandbox, the key is there at both.
    for key in [&home_secret, &second_secret] {
        assert_eq!(
            std::fs::read_to_string(key).expect("the key"),
            "the person's key"
        );
    }
    on_codex_runtime(async move {
        let state = dir.join("home/projects/p");
        std::fs::create_dir_all(&state).expect("state");
        let project = dir.join("project");
        for sub in ["atoms", "tools", "bin"] {
            std::fs::create_dir_all(project.join(sub)).expect("project");
        }
        std::fs::write(
            project.join("atomic.yaml"),
            "atomic: 1\nname: second-mounts\n",
        )
        .expect("manifest");
        std::fs::write(project.join("tools/probe.py"), PROBE).expect("script");
        std::fs::write(
            project.join("atoms/probe.atom.astro"),
            format!(
                "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/probe';\n\
                 import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
                 const {{ input }}: Props = Astro.props;\n\
                 export const config: AtomConfig<Input, Output> = {{ description: 'Probe.', \
                 kind: 'exec', exec: {{ command: ['python3', 'tools/probe.py'] }}, \
                 env: {{ HOME_SECRET: '{}', SECOND_SECRET: '{}' }} }};\n---\n",
                home_secret.display(),
                second_secret.display()
            ),
        )
        .expect("atom");
        std::fs::write(
            project.join("atoms/probe.input.schema.yaml"),
            "type: object\nadditionalProperties: false\nproperties: {}\n",
        )
        .expect("input");
        std::fs::write(
            project.join("atoms/probe.output.schema.yaml"),
            "type: object\nadditionalProperties: false\nrequired: [home, second]\n\
             properties:\n  home: {type: string}\n  second: {type: string}\n",
        )
        .expect("output");
        let mut registry = Registry::new();
        let atom = atomic_core::atom::load(&project.join("atoms/probe.atom.astro"), &mut registry)
            .expect("loads");
        let compiled = atomic_core::compile::compile(&atom, &registry).expect("compiles");
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_atomic"),
            project.join("bin/codex-linux-sandbox"),
        )
        .expect("sandbox helper");
        let paths = CodexPaths::with_helpers(
            PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
            project.join("bin/codex-linux-sandbox"),
        );
        let sandbox = Arc::new(
            ExecSandbox::new(
                &paths,
                StateGuard::deny_state_dir(&state).expect("guard"),
                state.join("codex"),
            )
            .expect("confinement"),
        );
        let backend = LocalBackend::in_dir(state.join("scratch")).confined(sandbox);
        let (out, _) = runtime::run_exec(
            &atom,
            &compiled,
            &json!({}),
            &EffectIdentity {
                run_id: "second-1".into(),
                step_id: "probe".into(),
            },
            Arc::new(backend),
            None,
            &runtime::Artifacts::none(),
            &atomic_core::usage::Metering::none(),
        )
        .await
        .expect("the command runs");
        let denied = |path: &Path| {
            format!(
                "PermissionError: [Errno 13] Permission denied: '{}'",
                path.display()
            )
        };
        assert_eq!(out["home"], denied(&home_secret), "{out}");
        assert_eq!(out["second"], denied(&second_secret), "{out}");
    });
}
