// SPDX-License-Identifier: Apache-2.0

//! Telemetry and redaction, end to end, through the `atomic` binary:
//!
//! - a run configured by the standard `OTEL_EXPORTER_OTLP_*` variables
//!   exports, to a collector this test serves on loopback, over each OTLP
//!   protocol (gRPC, HTTP with protobuf, HTTP with JSON), spans that name the
//!   workflow, each step and each model call, each call's span a child of its
//!   step's, with the run and the step on it;
//! - nothing is exported when the variables turn export off;
//! - a run that stops while it waits at a gate still ends and exports its
//!   workflow's and its gate's spans, and a run that fails marks its spans
//!   failed;
//! - each place atomic redacts is tested with a secret of its own, which
//!   reaches it and no other: a lite provider's error, a command's stderr in
//!   the text and the `--json` report and in the spans, a warning line, and
//!   the runner's own errors (a Codex provider's error is in
//!   `telemetry_sandbox.rs`, since Codex needs its sandbox);
//! - the exporter's own headers, which can carry a collector's credential,
//!   are sent to the collector and reach neither the log nor the exec atoms'
//!   commands (Codex atoms' commands: `telemetry_sandbox.rs`).

#![allow(clippy::unwrap_used)]

mod telemetry_common;

use atomic_core::testing::Reply;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use telemetry_common::*;

#[test]
fn spans_over_http_json_parent_each_call_to_its_step_and_each_step_to_its_run() {
    let dir = scratch("json");
    project(&dir);
    let (model, _rt) = model(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())]);
    let (url, received) = collector();
    let out = run(
        &dir,
        &model.base_url(),
        "triage",
        NOTE,
        &exporting(&url, "http/json"),
    );
    assert!(out.status.success(), "{}", text(&out));

    let spans = received.spans();
    let workflow = named(&spans, "invoke_workflow triage");
    let run_id = attribute(workflow, "atomic.workflow.id").expect("the run");
    for (atom, step) in [
        ("count-note", "count"),
        ("rank-note", "rank"),
        ("summarize-note", "summarize"),
    ] {
        let span = named(&spans, &format!("invoke_agent {atom}"));
        assert_eq!(
            span["parentSpanId"], workflow["spanId"],
            "{step} is the run's"
        );
        assert_eq!(span["traceId"], workflow["traceId"]);
        assert_eq!(attribute(span, "atomic.step").as_deref(), Some(step));
        assert_eq!(attribute(span, "atomic.workflow.id"), Some(run_id.clone()));
        assert!(!failed(span), "{span}");
    }
    let calls: Vec<&Value> = spans
        .iter()
        .filter(|s| s["name"] == "chat synthetic")
        .collect();
    assert_eq!(calls.len(), 2, "one span per model call: {spans:#?}");
    for call in calls {
        let step = attribute(call, "atomic.step").expect("the call's step");
        let atom = if step == "rank" {
            "rank-note"
        } else {
            "summarize-note"
        };
        let parent = named(&spans, &format!("invoke_agent {atom}"));
        assert_eq!(
            call["parentSpanId"], parent["spanId"],
            "the {step} call is its step's"
        );
        assert_eq!(attribute(call, "atomic.workflow.id"), Some(run_id.clone()));
        assert_eq!(
            attribute(call, "gen_ai.provider.name").as_deref(),
            Some("openai_compatible")
        );
        assert!(
            attribute(call, "gen_ai.usage.input_tokens").is_some(),
            "{call}"
        );
        let kind = &call["kind"];
        assert!(
            kind == 3 || kind == "SPAN_KIND_CLIENT",
            "a client span: {call}"
        );
    }
    assert!(received.headers().contains("/v1/traces"));
}

#[test]
fn spans_over_http_protobuf_carry_the_exporters_headers_which_no_atom_sees() {
    let dir = scratch("protobuf");
    project(&dir);
    let (model, _rt) = model(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())]);
    let (url, received) = collector();
    let out = run(
        &dir,
        &model.base_url(),
        "triage",
        NOTE,
        &exporting(&url, "http/protobuf"),
    );
    assert!(out.status.success(), "{}", text(&out));

    let spans = received.bodies();
    for name in [
        "invoke_workflow triage",
        "invoke_agent count-note",
        "invoke_agent rank-note",
        "invoke_agent summarize-note",
        "chat synthetic",
    ] {
        assert!(spans.contains(name), "no {name} in what was exported");
    }
    // Sent where the variables say, with the headers they name.
    let headers = received.headers();
    assert!(headers.contains("/v1/traces"), "{headers}");
    assert!(headers.contains("application/x-protobuf"), "{headers}");
    assert!(
        headers.contains(COLLECTOR_TOKEN),
        "the collector got its credential"
    );
    // Which no log line and no atom's command saw.
    assert!(!text(&out).contains(COLLECTOR_TOKEN), "{}", text(&out));
    let env = std::fs::read_to_string(dir.join("env.json")).unwrap();
    assert!(!env.contains(COLLECTOR_TOKEN), "{env}");
    assert!(!env.contains("OTEL_EXPORTER_OTLP_HEADERS"), "{env}");
}

#[test]
fn spans_over_grpc_are_exported_from_the_commands_runtime() {
    let dir = scratch("grpc");
    project(&dir);
    let (model, _rt) = model(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())]);
    let (url, received) = grpc_collector();
    let out = run(
        &dir,
        &model.base_url(),
        "triage",
        NOTE,
        &exporting(&url, "grpc"),
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains("panicked"), "{}", text(&out));
    assert!(!text(&out).contains("telemetry is off"), "{}", text(&out));
    let spans = received.bodies();
    for name in [
        "invoke_workflow triage",
        "invoke_agent count-note",
        "invoke_agent rank-note",
        "invoke_agent summarize-note",
        "chat synthetic",
    ] {
        assert!(
            spans.contains(name),
            "no {name} in what was exported over gRPC"
        );
    }
}

#[test]
fn nothing_is_exported_when_the_variables_turn_export_off() {
    for (var, value) in [
        ("OTEL_SDK_DISABLED", "true"),
        ("OTEL_TRACES_EXPORTER", "none"),
    ] {
        let dir = scratch(&format!("off-{var}"));
        project(&dir);
        let (model, _rt) = model(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())]);
        let (url, received) = collector();
        let mut env = exporting(&url, "http/json");
        env.push((var, value.into()));
        let out = run(&dir, &model.base_url(), "triage", NOTE, &env);
        assert!(out.status.success(), "{}", text(&out));
        assert!(
            received.0.lock().unwrap().is_empty(),
            "{var}={value} exported"
        );
    }
}

#[test]
fn a_run_stopped_at_a_gate_ends_and_exports_its_spans() {
    let dir = scratch("gate");
    project(&dir);
    let (model, _rt) = model(vec![Reply::Text(RANK.into())]);
    let (url, received) = collector();
    let mut child = command(
        &dir,
        &model.base_url(),
        &["run", "gated", "--data", NOTE],
        &exporting(&url, "http/json"),
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    // Waiting at the gate once `atomic gates list` shows it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let listed = command(&dir, &model.base_url(), &["gates", "list", "--json"], &[])
            .output()
            .unwrap();
        let open: Value = serde_json::from_slice(&listed.stdout).unwrap_or(json!([]));
        if open.as_array().is_some_and(|g| !g.is_empty()) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no gate opened: {}",
            text(&listed)
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "the run ended before its gate"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let killed = Command::new("/bin/sh")
        .args(["-c", &format!("kill -TERM {}", child.id())])
        .status()
        .unwrap();
    assert!(killed.success());
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "{}", text(&out));

    let spans = received.spans();
    let workflow = named(&spans, "invoke_workflow gated");
    let gate = named(&spans, "atomic.gate check");
    assert_eq!(gate["parentSpanId"], workflow["spanId"]);
    for span in [workflow, gate] {
        assert_eq!(
            attribute(span, "atomic.stopped").as_deref(),
            Some("true"),
            "{span}"
        );
        assert!(!failed(span), "a stop is not a failure: {span}");
    }
    named(&spans, "invoke_agent rank-note");
}

/// A lite provider's error: its excerpt is redacted where it is made, so
/// the run's stored error never holds the secret, nor does anything shown.
#[test]
fn a_secret_a_lite_provider_echoes_is_redacted_in_its_excerpt() {
    const SECRET: &str = concat!("sk-ant-", "api03-LITEexcerptSECRET0123456789abcdef");
    let dir = scratch("lite-secret");
    project(&dir);
    let (model, _rt) = model(vec![echoing(SECRET)]);
    let (url, received) = collector();
    let out = run(
        &dir,
        &model.base_url(),
        "triage",
        NOTE,
        &exporting(&url, "http/json"),
    );
    assert!(!out.status.success(), "the call failed: {}", text(&out));
    let shown = text(&out);
    assert!(shown.contains("401"), "the error is reported: {shown}");
    assert!(!shown.contains(SECRET), "the secret was shown: {shown}");
    assert!(shown.contains("[REDACTED]"), "{shown}");
    assert_eq!(
        stored(&dir.join("atomic-home"), SECRET),
        Vec::<PathBuf>::new()
    );

    // Its spans say it failed, and why, redacted.
    let spans = received.spans();
    assert!(
        !received.bodies().contains(SECRET),
        "the secret was exported"
    );
    let call = named(&spans, "chat synthetic");
    assert!(failed(call), "{call}");
    assert_eq!(
        attribute(call, "error.type").as_deref(),
        Some("atomic.model_call")
    );
    assert!(
        attribute(call, "error.message")
            .unwrap()
            .contains("[REDACTED]")
    );
    for name in ["invoke_agent rank-note", "invoke_workflow triage"] {
        let span = named(&spans, name);
        assert!(failed(span), "{span}");
        assert_eq!(
            attribute(span, "error.type").as_deref(),
            Some("atomic.failed"),
            "{span}"
        );
    }
}

/// A command's stderr is part of its step's error as it was: the report
/// redacts it, as text and as JSON, and so does the exporter.
#[test]
fn a_secret_in_a_commands_stderr_is_redacted_in_the_text_report() {
    const SECRET: &str = concat!("glpat-", "TEXTreportSECRET0123456789");
    let dir = scratch("text-report");
    project(&dir);
    let out = run(&dir, "http://127.0.0.1:9/v1", "leaky", &note(SECRET), &[]);
    assert!(!out.status.success(), "{}", text(&out));
    let shown = text(&out);
    assert!(shown.contains("the provider said [REDACTED]"), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");
}

#[test]
fn a_secret_in_a_commands_stderr_is_redacted_in_the_json_report() {
    const SECRET: &str = concat!("glpat-", "JSONreportSECRET0123456789");
    let dir = scratch("json-report");
    project(&dir);
    let out = command(
        &dir,
        "http://127.0.0.1:9/v1",
        &["--json", "run", "leaky", "--data", &note(SECRET)],
        &[],
    )
    .output()
    .unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    let report: Value = serde_json::from_slice(&out.stdout).expect("a JSON report");
    assert!(
        report.to_string().contains("the provider said [REDACTED]"),
        "{report}"
    );
    assert!(!text(&out).contains(SECRET), "{}", text(&out));
}

#[test]
fn a_secret_in_a_commands_stderr_is_redacted_in_its_spans() {
    const SECRET: &str = concat!("glpat-", "SPANstatusSECRET0123456789");
    let dir = scratch("span-status");
    project(&dir);
    let (url, received) = collector();
    let out = run(
        &dir,
        "http://127.0.0.1:9/v1",
        "leaky",
        &note(SECRET),
        &exporting(&url, "http/json"),
    );
    assert!(!out.status.success(), "{}", text(&out));
    let spans = received.spans();
    let step = named(&spans, "invoke_agent leak-note");
    assert!(failed(step), "{step}");
    let why = step["status"]["message"].as_str().unwrap_or_default();
    assert!(why.contains("the provider said [REDACTED]"), "{step}");
    assert!(
        !received.bodies().contains(SECRET),
        "the secret was exported"
    );
}

/// A warning line, here about a setting that quotes what it was given.
#[test]
fn a_secret_in_a_warning_line_is_redacted() {
    const SECRET: &str = concat!("glpat-", "WARNINGlineSECRET0123456789");
    let dir = scratch("warning");
    project(&dir);
    let (url, _received) = collector();
    let mut env = exporting(&url, "http/json");
    env.push(("OTEL_EXPORTER_OTLP_PROTOCOL", format!("bogus {SECRET}")));
    let out = command(&dir, "http://127.0.0.1:9/v1", &["debug", "state-dir"], &env)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let shown = text(&out);
    assert!(shown.contains("telemetry is off"), "{shown}");
    assert!(shown.contains("[REDACTED]"), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");
}

/// The runner role's own errors, which quote the request they refuse.
#[test]
fn a_secret_in_a_runners_error_is_redacted() {
    const SECRET: &str = concat!("glpat-", "RUNNERerrorSECRET0123456789");
    let dir = scratch("runner");
    let mut child = command(&dir, "http://127.0.0.1:9/v1", &["runner"], &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(json!({"protocol": SECRET}).to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(64), "{}", text(&out));
    let shown = text(&out);
    assert!(shown.contains("atomic runner:"), "{shown}");
    assert!(shown.contains("[REDACTED]"), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");
}
