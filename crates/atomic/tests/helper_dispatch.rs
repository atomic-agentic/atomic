// SPDX-License-Identifier: Apache-2.0

//! atomic started as one of Codex's helpers, as Codex starts it inside a
//! turn, touches no state before it runs as that helper. Its `HOME` is then
//! the turn's scratch directory, which the turn may write: a helper that
//! resolved and created a state directory there would follow whatever the
//! turn planted, outside the sandbox.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Stdio};

#[test]
fn a_codex_helper_creates_nothing_under_its_home() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-helper-dispatch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["home", "elsewhere", "bin", "cwd"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
    // What a turn could plant in its HOME: a link to somewhere else.
    std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("home/.local")).expect("link");
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_atomic"),
        dir.join("bin/codex-linux-sandbox"),
    )
    .expect("helper");
    let none: Vec<&str> = Vec::new();
    for (program, args) in [
        (dir.join("bin/codex-linux-sandbox"), none.clone()),
        (
            Path::new(env!("CARGO_BIN_EXE_atomic")).to_owned(),
            vec!["--codex-run-as-apply-patch"],
        ),
    ] {
        let _ = Command::new(&program)
            .args(&args)
            .current_dir(dir.join("cwd"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", dir.join("home"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("the helper runs");
        let created: Vec<_> = std::fs::read_dir(dir.join("elsewhere"))
            .expect("read")
            .map(|e| e.expect("entry").path())
            .collect();
        assert!(
            created.is_empty(),
            "{} {args:?} created {created:?} through the planted link",
            program.display()
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
