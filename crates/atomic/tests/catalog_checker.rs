// SPDX-License-Identifier: Apache-2.0

//! The offline catalog checker's own suite, run as a control.
//!
//! `tools/deno_exec.py` is the checker that reads the example catalog until
//! atomic's loader does the same: it keeps an atom's code inside the project,
//! answers every import from a lockfile the project holds, and refuses what it
//! cannot see or cannot read. Its guards are Python, and a control that cannot
//! be switched off and seen to bite is not a control anyone has checked. So
//! this runs the checker's suite, `tools/deno_exec_test.py`, in a language both
//! share with it: one test that fails on any case the suite reports as failed,
//! which is how a row in `scripts/controls/` can switch one guard off in
//! `tools/deno_exec.py` and show that a test catches it. CI runs the same script
//! as a step of its own; this is what makes its cases visible to cargo.
//!
//! The suite is inert -- JSON and TypeScript-shaped text under a temporary
//! directory. It needs no provider, no network and no test database. python3 is
//! what runs it, as it is in the development container and on a CI runner;
//! where that is missing this fails, as every suite that cannot reach what it
//! tests does, rather than passing for want of an interpreter.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn the_catalog_checker_refuses_what_it_cannot_read() {
    let output = Command::new("python3")
        .arg("tools/deno_exec_test.py")
        .current_dir(repository())
        .output()
        .expect("python3 runs the catalog checker's suite");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut reported = said
        .lines()
        .filter(|line| line.starts_with("FAIL ") || line.starts_with("SKIP "))
        .peekable();
    assert!(
        output.status.success() && reported.peek().is_none(),
        "the catalog checker's suite did not vouch for every case it makes: {said}"
    );
}
