// SPDX-License-Identifier: Apache-2.0

//! What the sandbox suites share.

use std::process::Command;

/// Fail, naming what is missing, unless Codex's Linux sandbox can start
/// here. The sandbox runs commands under bubblewrap from `PATH`, which needs
/// unprivileged user namespaces and a fresh `/proc`. A suite that cannot
/// reach its backend fails; it never skips.
pub fn require_sandbox() {
    let probe = Command::new("bwrap")
        .args([
            "--unshare-user",
            "--unshare-pid",
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "true",
        ])
        .output();
    match probe {
        Err(e) => panic!(
            "the sandbox tests need Codex's Linux sandbox, which runs bubblewrap, and `bwrap` \
             cannot be run from PATH: {e}"
        ),
        Ok(out) if !out.status.success() => panic!(
            "the sandbox tests need Codex's Linux sandbox, and bubblewrap cannot start here: {}. \
             It needs unprivileged user namespaces and a fresh /proc: under scripts/dev.sh set \
             ATOMIC_DEV_SANDBOX=on",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Ok(_) => {}
    }
}

/// The descriptors [`plant`] leaves open in the process a command starts:
/// a pipe's read and write ends, a file, a socket, and the pipe's read end
/// again at a high number, none close-on-exec.
#[allow(dead_code)]
pub const PLANTED: [i32; 5] = [600, 601, 602, 603, 900];

/// Leave [`PLANTED`] open in the process `command` starts, as whatever
/// starts atomic may, a CI runner, a shell or an editor: a pipe, a file at
/// `file`, a socket. Returns this process's own ends, to keep until the
/// process has started.
#[allow(dead_code, unsafe_code)]
pub fn plant(command: &mut Command, file: &std::path::Path) -> Vec<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, OwnedFd};
    let (read, write) = std::io::pipe().expect("pipe");
    let file = std::fs::File::create(file).expect("file");
    let (socket, peer) = std::os::unix::net::UnixStream::pair().expect("socket");
    let sources = [
        read.as_raw_fd(),
        write.as_raw_fd(),
        file.as_raw_fd(),
        socket.as_raw_fd(),
        read.as_raw_fd(),
    ];
    // SAFETY: between fork and exec the closure makes only dup2 calls, which
    // are async-signal-safe, and allocates nothing. A descriptor dup2 makes
    // is not close-on-exec.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(command, move || {
            for (source, target) in sources.into_iter().zip(PLANTED) {
                if libc::dup2(source, target) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    vec![
        OwnedFd::from(read),
        OwnedFd::from(write),
        OwnedFd::from(file),
        OwnedFd::from(socket),
        OwnedFd::from(peer),
    ]
}
