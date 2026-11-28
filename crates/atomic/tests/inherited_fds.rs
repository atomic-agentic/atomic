// SPDX-License-Identifier: Apache-2.0

//! A command atomic runs holds only the descriptors atomic hands it, its
//! standard three, even when whatever started atomic left others open: a
//! pipe, a file, a socket, one at a high number. Three ways in:
//!
//! - `atomic run` started with them, for an exec atom on this process's
//!   backend and through a runner process;
//! - this process holding them, with the command confined: the sandbox
//!   helper keeps them from it;
//! - this process holding them, with an unconfined command run through
//!   `atomic runner`, which keeps them from what it starts.
//!
//! atomic refuses to start, and so does its sandbox helper, where it cannot
//! keep them from what it starts. (A turn's shell is `shell_env`'s.) Needs bubblewrap and unprivileged
//! user namespaces, so it runs with the `sandbox-tests` feature; with the
//! feature on, a sandbox that cannot start fails the test.
//!
//! On macOS the first three ways in are tested too: atomic marks what it
//! was started with close-on-exec there, and a confined command's child
//! marks what this process holds before it starts `sandbox-exec`. The tests
//! of the Linux sandbox helper, and of a kernel without `close_range`, are
//! Linux-only.

#![cfg(all(
    any(target_os = "linux", target_os = "macos"),
    feature = "sandbox-tests"
))]
// Helpers only the Linux-only tests use are unused on macOS.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

mod support;

use atomic_core::codex::{CodexPaths, ExecSandbox, StateGuard};
use atomic_exec::{ExecutionBackend, LocalBackend};
use serde_json::{Value, json};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The descriptors the command holds, every one it could have, found
/// by asking each number up to its limit (at most 65536) rather than by
/// listing a directory, which would open one of its own: `{"fds": "0,1,2"}`.
const LIST: &str = "import fcntl, json, resource, sys\n\
    def is_open(fd):\n\
    \x20   try:\n\
    \x20       fcntl.fcntl(fd, fcntl.F_GETFD)\n\
    \x20       return True\n\
    \x20   except OSError:\n\
    \x20       return False\n\
    top = min(resource.getrlimit(resource.RLIMIT_NOFILE)[0], 1 << 16)\n\
    json.dump({'fds': ','.join(str(fd) for fd in range(top) if is_open(fd))}, sys.stdout)\n";

/// A scratch directory of the test's own, not under /tmp: atomic refuses
/// scratch where sandboxed turns may write.
fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-inherited-fds-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    std::fs::canonicalize(&dir).expect("canonical")
}

/// Descriptors left open in this process, as whatever started it may have
/// left them: a pipe's two ends, a file, a socket, and one at 900 or above,
/// none close-on-exec. Kept open until dropped.
#[allow(unsafe_code)]
fn left_open_here(dir: &Path) -> Vec<OwnedFd> {
    let (read, write) = std::io::pipe().expect("pipe");
    let file = std::fs::File::create(dir.join("left-open")).expect("file");
    let (socket, _peer) = std::os::unix::net::UnixStream::pair().expect("socket");
    let mut fds = Vec::new();
    for (fd, at_least) in [
        (read.as_raw_fd(), 3),
        (write.as_raw_fd(), 3),
        (file.as_raw_fd(), 3),
        (socket.as_raw_fd(), 3),
        (read.as_raw_fd(), 900),
    ] {
        // SAFETY: F_DUPFD takes integers and returns a new descriptor,
        // without close-on-exec, which this test owns.
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD, at_least) };
        assert!(copy >= at_least, "{}", std::io::Error::last_os_error());
        // SAFETY: F_GETFD only reads the flags.
        assert_eq!(
            unsafe { libc::fcntl(copy, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        // SAFETY: the descriptor was just made, and is owned here alone.
        fds.push(unsafe { OwnedFd::from_raw_fd(copy) });
    }
    fds
}

/// The command that lists its descriptors, in `cwd`.
fn listing(cwd: &Path) -> atomic_exec::Command {
    atomic_exec::Command {
        program: "python3".into(),
        args: vec!["-c".into(), LIST.into()],
        cwd: cwd.to_owned(),
        env: vec![
            ("HOME".into(), cwd.display().to_string()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ],
        time_limit: Duration::from_secs(60),
        output_limit: 1 << 16,
        writable: vec![cwd.to_owned()],
        ..atomic_exec::Command::default()
    }
}

/// What `backend` gives the listing command, opened on `dir`'s project.
fn listed_on(backend: &dyn ExecutionBackend, dir: &Path) -> String {
    std::fs::create_dir_all(dir.join("project")).expect("project");
    std::fs::create_dir_all(dir.join("cwd")).expect("cwd");
    backend.connect().expect("connects");
    let workspace = backend
        .open(&atomic_exec::Open {
            label: "inherited-fds".into(),
            source: Some(dir.join("project")),
            ..atomic_exec::Open::default()
        })
        .expect("opens");
    let done = backend.run(&listing(&dir.join("cwd"))).expect("runs");
    assert!(done.success(), "{}", String::from_utf8_lossy(&done.stderr));
    backend.close(&workspace).expect("closes");
    let got: Value = serde_json::from_slice(&done.stdout).expect("a listing");
    got["fds"].as_str().expect("fds").to_owned()
}

/// Left open in this process, the descriptors reach no confined command:
/// on Linux the sandbox helper this process starts keeps them from it, and
/// on macOS the child marks them close-on-exec before it starts
/// `sandbox-exec`.
#[test]
fn a_confined_command_holds_none_of_the_descriptors_left_open_in_its_parent() {
    support::require_sandbox();
    let dir = scratch("confined");
    let _left = left_open_here(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("bin");
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
    let backend = LocalBackend::in_dir(dir.join("scratch")).confined(Arc::new(sandbox));
    assert_eq!(listed_on(&backend, &dir), "0,1,2");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Left open in this process, the descriptors reach no command `atomic
/// runner` starts, unconfined as it is where there is no sandbox: the
/// runner keeps them from what it starts.
#[test]
fn a_runners_command_holds_none_of_the_descriptors_left_open_in_its_parent() {
    support::require_sandbox();
    let dir = scratch("runner");
    let _left = left_open_here(&dir);
    let runner = atomic_exec::runner::ProcessRunner {
        workspaces: LocalBackend::in_dir(dir.join("scratch")),
        ..atomic_exec::runner::ProcessRunner::atomic(PathBuf::from(env!("CARGO_BIN_EXE_atomic")))
    };
    assert_eq!(listed_on(&runner, &dir), "0,1,2");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Write the project: `list-fds`, an exec atom whose command lists its
/// descriptors.
fn write_project(project: &Path) {
    for sub in ["atoms", "tools"] {
        std::fs::create_dir_all(project.join(sub)).expect("project");
    }
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: fds\n").expect("manifest");
    std::fs::write(
        project.join("tools/list.py"),
        format!("import json, sys\njson.load(sys.stdin)\n{LIST}"),
    )
    .expect("script");
    std::fs::write(
        project.join("atoms/list-fds.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/list-fds';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'List its descriptors.', \
         kind: 'exec', exec: { command: ['python3', 'tools/list.py'] } };\n---\n",
    )
    .expect("atom");
    let schema = |properties: Value| {
        let required: Vec<&String> = properties.as_object().expect("object").keys().collect();
        json!({"type": "object", "additionalProperties": false, "required": required,
               "properties": properties})
        .to_string()
    };
    std::fs::write(
        project.join("atoms/list-fds.input.schema.json"),
        schema(json!({})),
    )
    .expect("input");
    std::fs::write(
        project.join("atoms/list-fds.output.schema.json"),
        schema(json!({"fds": {"type": "string"}})),
    )
    .expect("output");
}

/// `atomic run`, started with descriptors left open, runs an exec atom
/// whose command holds none of them: on this process's backend and
/// through a runner process, with them left by a Rust parent (a pipe, a
/// file, a socket, one at 900), and on this process's backend with them
/// left by a shell that opened them for it (`exec 7<file 8<>file`).
#[test]
fn an_exec_atoms_command_holds_none_of_the_descriptors_atomic_was_started_with() {
    support::require_sandbox();
    let dir = scratch("run");
    let project = dir.join("project");
    write_project(&project);
    std::fs::create_dir_all(dir.join("home")).expect("home");
    let left = dir.join("left-by-the-shell");
    std::fs::write(&left, "").expect("file");
    let atomic = |program: &str, args: &[&str]| {
        let mut run = std::process::Command::new(program);
        run.args(args)
            .current_dir(&project)
            .env("HOME", dir.join("home"))
            .env("ATOMIC_HOME", dir.join("atomic-home"))
            .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
            .stdin(std::process::Stdio::null());
        for name in [
            "CODEX_HOME",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "ATOMIC_DATABASE_URL",
            "ATOMIC_EXECUTOR_ID",
            "ATOMIC_MODEL_BASE_URL",
            "ATOMIC_MODEL",
            "ATOMIC_RUNNER",
            "OPENAI_API_KEY",
        ] {
            run.env_remove(name);
        }
        run
    };
    let run_args = |runner: &'static str| {
        vec![
            "--json", "run", "list-fds", "--runner", runner, "--data", "{}",
        ]
    };
    let left_text = left.display().to_string();
    let mut shell_args = vec![
        "-c",
        "exec 7<\"$1\" 8<>\"$1\"; shift; exec \"$@\"",
        "sh",
        left_text.as_str(),
        env!("CARGO_BIN_EXE_atomic"),
    ];
    shell_args.extend(run_args("local"));
    let runs = [
        (
            "local",
            atomic(env!("CARGO_BIN_EXE_atomic"), &run_args("local")),
            true,
        ),
        (
            "process",
            atomic(env!("CARGO_BIN_EXE_atomic"), &run_args("process")),
            true,
        ),
        ("left by a shell", atomic("/bin/sh", &shell_args), false),
    ];
    for (how, mut run, plant) in runs {
        let planted = plant.then(|| support::plant(&mut run, &dir.join(format!("planted-{how}"))));
        let out = run.output().expect("atomic runs");
        drop(planted);
        assert!(
            out.status.success(),
            "{how}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let doc: Value = serde_json::from_slice(&out.stdout).expect("a result");
        assert_eq!(doc["output"]["fds"], "0,1,2", "{how}: {doc}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Start `command` where `close_range` answers `ENOSYS`, as on a kernel
/// before 5.11, and, when `no_listing`, in a mount namespace of its own whose
/// `/proc` is an empty directory, so that it cannot list its descriptors
/// either.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn without_close_range(command: &mut std::process::Command, no_listing: bool) {
    // SAFETY: between fork and exec the closure makes only system calls
    // (unshare, mount, prctl), which are async-signal-safe, and allocates
    // nothing: the filter is on its stack.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(command, move || {
            let fail = || Err(std::io::Error::last_os_error());
            if no_listing {
                if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                    return fail();
                }
                let none = std::ptr::null();
                if libc::mount(
                    none,
                    c"/".as_ptr(),
                    none,
                    libc::MS_REC | libc::MS_PRIVATE,
                    none.cast(),
                ) != 0
                {
                    return fail();
                }
                if libc::mount(
                    c"none".as_ptr(),
                    c"/proc".as_ptr(),
                    c"tmpfs".as_ptr(),
                    0,
                    none.cast(),
                ) != 0
                {
                    return fail();
                }
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return fail();
            }
            // Load the call's number; answer close_range with ENOSYS, allow
            // the rest.
            let op = |code: u32, jt: u8, jf: u8, k: u32| libc::sock_filter {
                code: code as u16,
                jt,
                jf,
                k,
            };
            let filter = [
                op(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0),
                op(
                    libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
                    0,
                    1,
                    libc::SYS_close_range as u32,
                ),
                op(
                    libc::BPF_RET | libc::BPF_K,
                    0,
                    0,
                    libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32,
                ),
                op(libc::BPF_RET | libc::BPF_K, 0, 0, libc::SECCOMP_RET_ALLOW),
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_ptr().cast_mut(),
            };
            if libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
            ) != 0
            {
                return fail();
            }
            Ok(())
        });
    }
}

/// atomic, unable to keep the descriptors it was started with from what it
/// starts, refuses to start, saying why; with only `close_range` refused it
/// lists them instead, and starts.
#[cfg(target_os = "linux")]
#[test]
fn atomic_refuses_to_start_where_it_cannot_keep_descriptors_from_its_children() {
    support::require_sandbox();
    for no_listing in [true, false] {
        let mut atomic = std::process::Command::new(env!("CARGO_BIN_EXE_atomic"));
        atomic.arg("--version");
        without_close_range(&mut atomic, no_listing);
        let out = atomic.output().expect("started");
        let said = String::from_utf8_lossy(&out.stderr);
        let refused = said
            .contains("keep the descriptors atomic was started with from the programs it starts");
        if no_listing {
            assert!(!out.status.success() && refused, "{no_listing}: {said}");
        } else {
            assert!(out.status.success() && !refused, "{no_listing}: {said}");
        }
    }
}

/// The sandbox helper, unable to keep the descriptors it was started with
/// from the command, does not start it, saying why; with only `close_range`
/// refused it lists them instead, and goes on.
#[cfg(target_os = "linux")]
#[test]
fn the_sandbox_helper_refuses_where_it_cannot_keep_descriptors_from_the_command() {
    support::require_sandbox();
    let dir = scratch("helper-refuses");
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_atomic"),
        dir.join("codex-linux-sandbox"),
    )
    .expect("sandbox helper");
    for no_listing in [true, false] {
        let mut helper = std::process::Command::new(dir.join("codex-linux-sandbox"));
        helper.args(["--sandbox-policy-cwd", "/"]);
        without_close_range(&mut helper, no_listing);
        let out = helper.output().expect("started");
        let said = String::from_utf8_lossy(&out.stderr);
        let refused = said.contains("cannot keep the descriptors it was started with");
        if no_listing {
            assert!(!out.status.success() && refused, "{no_listing}: {said}");
        } else {
            assert!(!refused, "{no_listing}: {said}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
