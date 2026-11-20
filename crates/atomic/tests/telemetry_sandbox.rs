// SPDX-License-Identifier: Apache-2.0

//! Telemetry and redaction with Codex atoms, which need Codex's Linux
//! sandbox even for a turn that runs no command:
//!
//! - a Codex provider's error, with a secret of its own, is redacted where
//!   atomic takes Codex's error event, so the run's stored error never holds
//!   the secret;
//! - a Codex atom's commands see none of the exporter's settings, its
//!   collector credential among them, while the run exports over gRPC.
//!
//! Runs with the `sandbox-tests` feature; with it on, a sandbox that cannot
//! start fails the tests.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![allow(clippy::unwrap_used)]

mod support;
mod telemetry_common;

use atomic_core::testing::Reply;
use serde_json::json;
use std::path::PathBuf;
use telemetry_common::*;

/// A Codex provider's error: Codex's error event is redacted where atomic
/// takes it, so the stored error never holds the secret. Codex starts its
/// sandbox helper even for a turn that runs no command.
#[test]
fn a_secret_a_codex_provider_echoes_is_redacted_from_its_error_event() {
    const SEEDED: &str = concat!("sk-ant-", "api03-CODEXeventSEEDED0123456789abcdef");
    support::require_sandbox();
    let dir = scratch("codex-secret");
    project(&dir);
    // A 400 ends the turn; Codex answers a 401 by trying to sign in again.
    let refused = Reply::Http {
        status: 400,
        body: format!(
            r#"{{"error": {{"message": "Refused the key {SEEDED}", "type": "invalid_request_error"}}}}"#
        ),
    };
    let (model, _rt) = model(vec![refused; 8]);
    let out = run(&dir, &model.base_url(), "review", NOTE, &[]);
    assert!(!out.status.success(), "the turn failed: {}", text(&out));
    let shown = text(&out);
    assert!(
        shown.contains("Refused the key [REDACTED]"),
        "the error is reported: {shown}"
    );
    assert!(!shown.contains(SEEDED), "the secret was shown: {shown}");
    assert_eq!(
        stored(&dir.join("atomic-home"), SEEDED),
        Vec::<PathBuf>::new()
    );
}

/// A Codex atom's commands see none of the exporter's settings, its
/// collector credential among them, while the run exports over gRPC.
#[test]
fn a_codex_atoms_commands_see_none_of_the_exporters_settings() {
    support::require_sandbox();
    let dir = scratch("codex-env");
    let project = project(&dir);
    let (model, _rt) = model(vec![
        Reply::Tool {
            name: "exec_command".into(),
            arguments: json!({"cmd": "env; echo env-exit=$?", "workdir": project, "yield_time_ms": 10000}),
        },
        Reply::Text(RANK.into()),
    ]);
    let (url, received) = grpc_collector();
    let out = run(
        &dir,
        &model.base_url(),
        "review",
        NOTE,
        &exporting(&url, "grpc"),
    );
    assert!(out.status.success(), "{}", text(&out));
    let requests = model.requests();
    let follow_up = requests
        .get(1)
        .expect("Codex answered the tool call")
        .to_string();
    assert!(
        follow_up.contains("env-exit=0"),
        "the command ran: {follow_up}"
    );
    assert!(
        follow_up.contains("PATH="),
        "and printed its environment: {follow_up}"
    );
    for unseen in [
        COLLECTOR_SEED,
        "OTEL_EXPORTER_OTLP_HEADERS",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
    ] {
        assert!(!follow_up.contains(unseen), "the command saw {unseen}");
    }
    assert!(received.bodies().contains("invoke_agent review-note"));
    assert!(received.bodies().contains("chat synthetic"));
}
