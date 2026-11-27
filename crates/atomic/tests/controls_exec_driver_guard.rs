// SPDX-License-Identifier: Apache-2.0

//! Focused removal tests for the controls exec driver's own trust checks.
//! The driver uses GNU `timeout`, provided by the Linux development image.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

fn driver_case(name: &str) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let script = repo.join("scripts/controls-exec-test.sh");
    let output = Command::new("bash")
        .arg(script)
        .arg(name)
        .current_dir(repo)
        .output()
        .expect("run focused driver test");
    assert!(
        output.status.success(),
        "driver {name}: status {:?}; stdout: {}; stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn copy_tree_equality_refuses_mismatch_before_any_row() {
    driver_case("guard-tree-equality");
}

#[test]
fn trusted_runner_pin_refuses_changed_bytes_before_any_row() {
    driver_case("guard-runner-pin");
}

#[test]
fn selected_control_target_requires_standard_libtest_harness() {
    driver_case("guard-selected-harness");
}
