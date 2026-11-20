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
