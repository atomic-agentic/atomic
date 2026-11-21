// SPDX-License-Identifier: Apache-2.0

//! When the commands that declare hosts have no room left under atomic's
//! soft limit on open files, atomic raises that limit toward its hard one
//! before it refuses a command, and confines the command in the room the
//! raise made. Held to a hard limit no higher, it refuses the command and
//! says why. It changes this test process's limits, so it has a binary of
//! its own. It confines commands without running them, so it needs no
//! sandbox.

use atomic_core::codex::{CodexPaths, ExecSandbox, StateGuard};
use atomic_core::network::DESCRIPTORS_PER_COMMAND;
use atomic_exec::Confine;
use std::path::PathBuf;

/// This process's limits on open files, soft and hard.
#[allow(unsafe_code)]
fn limits() -> libc::rlimit {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes one rlimit.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
    limit
}

/// Set this process's limits on open files.
#[allow(unsafe_code)]
fn set_limits(limit: libc::rlimit) {
    // SAFETY: setrlimit reads one rlimit.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
}

#[test]
#[allow(clippy::unnecessary_cast)]
fn atomic_raises_its_limit_on_open_files_before_it_refuses_a_command() {
    let hard = limits().rlim_max as u64;
    // Room for none under the lowered soft limit, and for some under the
    // hard one.
    let low = DESCRIPTORS_PER_COMMAND + 64;
    assert!(
        hard >= 8 * DESCRIPTORS_PER_COMMAND,
        "this test needs a hard limit on open files of at least {}, and has {hard}",
        8 * DESCRIPTORS_PER_COMMAND
    );
    set_limits(libc::rlimit {
        rlim_cur: low as libc::rlim_t,
        rlim_max: hard as libc::rlim_t,
    });

    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-exec-proxy-raise-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch");
    let paths = CodexPaths::with_helpers(
        PathBuf::from("/opt/atomic"),
        PathBuf::from("/opt/bin/codex-linux-sandbox"),
    );
    let sandbox = ExecSandbox::new(&paths, StateGuard::NoProjectState, dir.join("codex"))
        .expect("a helper");
    let command = atomic_exec::Command {
        program: "sh".into(),
        cwd: dir.clone(),
        network: vec!["allowed.invalid".into()],
        ..Default::default()
    };

    let (_, held) = sandbox
        .confine_held(&command, &dir)
        .expect("confined, in the room the raise made");
    assert!(held.is_some(), "its proxy is held");
    let raised = limits().rlim_cur as u64;
    assert_eq!(raised, hard.min(1 << 16), "raised as far as it may go");
    drop(held);

    // Held to a hard limit with room for no command, it refuses one.
    set_limits(libc::rlimit {
        rlim_cur: low as libc::rlim_t,
        rlim_max: low as libc::rlim_t,
    });
    let Err(error) = sandbox.confine_held(&command, &dir) else {
        panic!("confined past the budget");
    };
    let error = error.to_string();
    assert!(
        error.contains(&format!(
            "0 commands that declare hosts are running, and atomic's limit of {low} open files \
             leaves room for 0 at once"
        )),
        "{error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
