// SPDX-License-Identifier: Apache-2.0

//! The offline catalog checker's own suites, run as a control.
//!
//! `tools/deno_exec.py` and `tools/yaml_scalar.py` are checkers that stand in for
//! atomic until atomic's own loader checks the same files. `deno_exec.py` keeps an
//! atom's code inside the project, answers every import from a lockfile the project
//! holds, and refuses what it cannot see or cannot read. `yaml_scalar.py` is
//! `crates/atomic-schema/src/yaml.rs`'s scalar classifier ported to Python, which
//! is what `tools/check_examples.py` reads every schema value and key through.
//! Their guards are Python, and a control that cannot be switched off and seen to
//! bite is not a control anyone has checked. So this runs their suites,
//! `tools/deno_exec_test.py` and `tools/yaml_scalar_test.py`, in a language both
//! share with them: one test per suite that fails on any case the suite reports as
//! failed, which is how a row in `scripts/controls/` can switch one guard off in
//! the checker and show that a test catches it. CI runs the same scripts as steps
//! of their own; this is what makes their cases visible to cargo.
//!
//! The suites are inert -- JSON, TypeScript-shaped text and plain scalars, under a
//! temporary directory. They need no provider, no network and no test database.
//! python3 is what runs them, as it is in the development container and on a CI
//! runner; where that is missing this fails, as every suite that cannot reach what
//! it tests does, rather than passing for want of an interpreter. Neither suite
//! needs the YAML or JSON Schema libraries the catalog checker itself imports,
//! which is why they can run here at all.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Run one checker's Python suite, and fail on any case it does not vouch for.
fn suite(says: &str) {
    let output = Command::new("python3")
        .arg(says)
        .current_dir(repository())
        .output()
        .unwrap_or_else(|e| panic!("python3 cannot run {says}: {e}"));
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
        "{says} did not vouch for every case it makes: {said}"
    );
}

#[test]
fn the_catalog_checker_refuses_what_it_cannot_read() {
    suite("tools/deno_exec_test.py");
}

#[test]
fn the_example_checker_reads_a_plain_scalar_as_the_loader_does() {
    suite("tools/yaml_scalar_test.py");
}
