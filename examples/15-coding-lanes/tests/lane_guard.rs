// SPDX-License-Identifier: Apache-2.0

//! The Cargo target the production controls-by-removal driver can run.
//! The guard itself remains the same Python command used by the lane's
//! criteria and trial merge checks.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn decide(input: &str) -> Output {
    let mut child = Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tools/lane_guard.py"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the example's guard");
    child
        .stdin
        .take()
        .expect("guard stdin")
        .write_all(input.as_bytes())
        .expect("send the finding to the guard");
    child.wait_with_output().expect("read the guard's decision")
}

#[test]
fn lane_guard_refuses_unverified() {
    let result = decide(r#"{"claim":"the timeout is too short","verification":null}"#);
    assert!(
        result.status.success(),
        "guard error: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8(result.stdout).expect("guard writes UTF-8 JSON"),
        "{\"claim\": \"the timeout is too short\", \"confirmed\": false, \"reason\": \"unverified finding\"}\n"
    );
}

#[test]
fn lane_guard_confirms_a_host_record() {
    let result = decide(
        r#"{"claim":"the timeout is too short","verification":{"verified_by":"host"}}"#,
    );
    assert!(
        result.status.success(),
        "guard error: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8(result.stdout).expect("guard writes UTF-8 JSON"),
        "{\"claim\": \"the timeout is too short\", \"confirmed\": true, \"reason\": \"\"}\n"
    );
}
