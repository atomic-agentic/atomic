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

/// What a Codex turn says of a provider's failure on its own span is what its
/// step's span and its run's say: that text, bounded, never the whole failure,
/// whose tail quotes the provider beyond what a span may carry.
#[test]
fn a_codex_steps_span_says_what_its_turn_said_and_never_the_failure_whole() {
    const SEEDED: &str = concat!("sk-ant-", "api03-CODEXspanSEEDED0123456789abcdef");
    support::require_sandbox();
    let dir = scratch("codex-span");
    project(&dir);
    // A 400 ends the turn; Codex answers a 401 by trying to sign in again.
    // The provider's message runs past what any span may carry, and ends with
    // a phrase that must not reach one.
    let body = format!(
        r#"{{"error": {{"message": "Refused the key {SEEDED} {} ENDOFTHEPROVIDER", "type": "invalid_request_error"}}}}"#,
        "waste ".repeat(60)
    );
    let (model, _rt) = model(vec![Reply::Http { status: 400, body }; 8]);
    let (url, received) = collector();
    let out = run(
        &dir,
        &model.base_url(),
        "review",
        NOTE,
        &exporting(&url, "http/json"),
    );
    assert!(!out.status.success(), "the turn failed: {}", text(&out));
    let spans = received.spans();
    // The turn's own span: the class, and what it may say of the provider.
    let call = named(&spans, "chat synthetic");
    assert!(failed(call), "{call}");
    let said = attribute(call, "error.message").expect("the turn says why it failed");
    assert!(
        !said.contains(SEEDED) && said.contains("[REDACTED]"),
        "the key reached the turn's span: {said}"
    );
    // Its step's span and its run's say exactly that, cut short, and carry
    // neither the provider's end nor the rest of the chain the run keeps.
    for name in ["invoke_agent review-note", "invoke_workflow review"] {
        let span = named(&spans, name);
        assert!(failed(span), "{span}");
        let why = span["status"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("{span} carries no status message: {span}"));
        assert_eq!(why, said, "{span} says other than the turn did");
        assert!(
            !why.contains("ENDOFTHEPROVIDER"),
            "{span} carries the provider's own words: {why}"
        );
        assert!(
            why.chars().count() <= atomic_core::usage::MAX_SPAN_ERROR,
            "{} characters on {span}: {why}",
            why.chars().count()
        );
    }
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
