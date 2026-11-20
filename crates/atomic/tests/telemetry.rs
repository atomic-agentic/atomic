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

use std::process::Stdio;
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
        headers.contains(COLLECTOR_SEED),
        "the collector got its credential"
    );
    // Which no log line saw, and no atom's command: count-note's fails the
    // run if its environment holds any OTEL_ variable or the credential.
    assert!(!text(&out).contains(COLLECTOR_SEED), "{}", text(&out));
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
    wait_for_gate(&dir, &model.base_url(), &mut child);
    signal(&child, "TERM");
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
/// the run's stored error never holds the key, nor does anything shown.
/// Each body puts its key right after an escape or a colour.
#[test]
fn a_key_a_lite_provider_echoes_is_redacted_in_its_excerpt() {
    let bodies = [
        (
            concat!("sk-ant-", "api03-LITEjsonSEEDED0123456789abcdef"),
            "{\"error\":{\"message\":\"Incorrect API key provided:\\nSEED\",\"type\":\"invalid_request_error\"}}",
        ),
        (
            concat!("ghp_", "LITEtabSEEDED0123456789abcdefghijkl"),
            "key rejected:\\tSEED",
        ),
        (
            concat!("ghp_", "LITEansiSEEDED0123456789abcdefghijk"),
            "\x1b[1;33mSEED\x1b[0m",
        ),
    ];
    for (index, (seeded, body)) in bodies.iter().enumerate() {
        let dir = scratch(&format!("lite-seeded-{index}"));
        project(&dir);
        let (model, _rt) = model(vec![Reply::Http {
            status: 401,
            body: body.replace("SEED", seeded),
        }]);
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
        assert!(!shown.contains(seeded), "body {index} was shown");
        assert!(shown.contains("[REDACTED]"), "{shown}");
        assert!(
            stored(&dir.join("atomic-home"), seeded).is_empty(),
            "body {index} was stored"
        );
        assert!(
            !received.bodies().contains(seeded),
            "body {index} was exported"
        );
        if index > 0 {
            continue;
        }
        // Its spans say it failed, and why, redacted.
        let spans = received.spans();
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
}

/// A command's stderr is redacted, then cut to its tail, where the step's
/// error is made: the run's stored error never holds it.
#[test]
fn a_key_in_a_commands_stderr_is_redacted_in_its_tail() {
    const SEEDED: &str = concat!("glpat-", "STDERRtailSEEDED0123456789");
    let dir = scratch("stderr-tail");
    project(&dir);
    let out = run(&dir, "http://127.0.0.1:9/v1", "leaky", &note(SEEDED), &[]);
    assert!(!out.status.success(), "{}", text(&out));
    let shown = text(&out);
    assert!(shown.contains("the provider said [REDACTED]"), "{shown}");
    assert!(!shown.contains(SEEDED), "the key was shown");
    assert!(
        stored(&dir.join("atomic-home"), SEEDED).is_empty(),
        "the key was stored"
    );
}

/// An output that does not fit its schema is quoted in the step's error as
/// it was: the report redacts it, as text and as JSON, and so does the
/// exporter.
#[test]
fn a_key_in_a_steps_error_is_redacted_in_the_text_report() {
    const SEEDED: &str = concat!("glpat-", "TEXTreportSEEDED0123456789");
    let dir = scratch("text-report");
    project(&dir);
    let out = run(&dir, "http://127.0.0.1:9/v1", "misfit", &note(SEEDED), &[]);
    assert!(!out.status.success(), "{}", text(&out));
    let shown = text(&out);
    assert!(shown.contains("[REDACTED]"), "{shown}");
    assert!(!shown.contains(SEEDED), "the key was shown");
}

#[test]
fn a_key_in_a_steps_error_is_redacted_in_the_json_report() {
    const SEEDED: &str = concat!("glpat-", "JSONreportSEEDED0123456789");
    let dir = scratch("json-report");
    project(&dir);
    let out = command(
        &dir,
        "http://127.0.0.1:9/v1",
        &["--json", "run", "misfit", "--data", &note(SEEDED)],
        &[],
    )
    .output()
    .unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    let report: Value = serde_json::from_slice(&out.stdout).expect("a JSON report");
    assert!(report.to_string().contains("[REDACTED]"), "{report}");
    assert!(!text(&out).contains(SEEDED), "the key was shown");
}

#[test]
fn a_key_in_a_steps_error_is_redacted_in_its_spans() {
    const SEEDED: &str = concat!("glpat-", "SPANstatusSEEDED0123456789");
    let dir = scratch("span-status");
    project(&dir);
    let (url, received) = collector();
    let out = run(
        &dir,
        "http://127.0.0.1:9/v1",
        "misfit",
        &note(SEEDED),
        &exporting(&url, "http/json"),
    );
    assert!(!out.status.success(), "{}", text(&out));
    let spans = received.spans();
    let step = named(&spans, "invoke_agent misfit-note");
    assert!(failed(step), "{step}");
    let why = step["status"]["message"].as_str().unwrap_or_default();
    assert!(why.contains("[REDACTED]"), "{step}");
    assert!(!received.bodies().contains(SEEDED), "the key was exported");
}

/// The rest of a child's output, read to its end on a thread of its own, so
/// the child never writes to a closed pipe.
fn drain(mut out: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut out, &mut std::io::sink());
    })
}

/// What a stopped process writes when a signal cuts its last flush short.
const STOPPED_FLUSH: &str = "stopping now, without sending the last spans";

/// A command that ends normally sends its last spans, and a signal stops a
/// collector that never answers from holding it up at all.
#[test]
fn a_signal_stops_the_last_flush_of_a_command_that_ended() {
    use std::io::BufRead;
    let dir = scratch("hung-normal");
    project(&dir);
    let (model, _rt) = model(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())]);
    let url = hung_collector();
    let mut child = command(
        &dir,
        &model.base_url(),
        &["run", "triage", "--data", NOTE],
        &exporting(&url, "http/json"),
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    // The run has printed its result: what is left is the flush.
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert!(!first.is_empty(), "the run printed nothing");
    let drained = drain(stdout);
    // Well inside the flush's deadline.
    std::thread::sleep(std::time::Duration::from_millis(300));
    signal(&child, "INT");
    let out = child.wait_with_output().unwrap();
    drained.join().unwrap();
    assert_eq!(out.status.code(), Some(130), "{}", text(&out));
    assert!(text(&out).contains(STOPPED_FLUSH), "{}", text(&out));
}

/// A command that ends normally with a collector that never answers exits
/// within the flush's deadline, not the exporter's ten-second timeout.
#[test]
fn a_collector_that_never_answers_delays_an_exit_only_briefly() {
    use std::io::BufRead;
    let dir = scratch("hung-bounded");
    project(&dir);
    let (model, _rt) = model(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())]);
    let url = hung_collector();
    let mut child = command(
        &dir,
        &model.base_url(),
        &["run", "triage", "--data", NOTE],
        &exporting(&url, "http/json"),
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    // The run has printed its result: what is left is the flush.
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert!(!first.is_empty(), "the run printed nothing");
    let printed = std::time::Instant::now();
    let drained = drain(stdout);
    let out = child.wait_with_output().unwrap();
    let taken = printed.elapsed();
    drained.join().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    // The exporter's own timeout is ten seconds.
    assert!(
        taken < std::time::Duration::from_secs(8),
        "took {taken:?} to exit after its result: {}",
        text(&out)
    );
    assert!(
        text(&out).contains("they are dropped"),
        "the dropped spans were not reported: {}",
        text(&out)
    );
}

/// A panic's message, through the hook `main` installs.
#[test]
fn a_key_in_a_panic_is_redacted() {
    const SEEDED: &str = concat!("glpat-", "PANICmessageSEEDED0123456789");
    let dir = scratch("panic");
    let out = command(
        &dir,
        "http://127.0.0.1:9/v1",
        &["debug", "panic", &format!("the provider said {SEEDED}")],
        &[],
    )
    .output()
    .unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    let shown = text(&out);
    assert!(shown.contains("the provider said [REDACTED]"), "{shown}");
    assert!(!shown.contains(SEEDED), "the key was shown");
}

/// `atomic check --json` reports its warnings redacted, as its log lines are.
#[test]
fn a_key_in_a_check_warning_is_redacted() {
    const SEEDED: &str = concat!("glpat-", "CHECKwarningSEEDED0123456789");
    let dir = scratch("check");
    let project = project(&dir);
    std::fs::write(
        project.join("workflows/warned.workflow.yaml"),
        format!(
            "atomic: 1\nname: warned\n\
             input: {{ $ref: ../atoms/count-note.input.schema.json }}\n\
             output: {{ $ref: ../atoms/count-note.output.schema.json }}\n\
             steps:\n\
             \x20 count:\n    atom: count-note\n    with: {{ text: \"input.text + ' {SEEDED}'\" }}\n\
             returns: steps.count.output\n"
        ),
    )
    .unwrap();
    let out = command(&dir, "http://127.0.0.1:9/v1", &["--json", "check"], &[])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let report: Value = serde_json::from_slice(&out.stdout).expect("a JSON report");
    let warnings = report["warnings"].to_string();
    assert!(warnings.contains("[REDACTED]"), "{warnings}");
    assert!(!text(&out).contains(SEEDED), "the key was shown");
}

/// A stop while a model call is in flight ends the run's spans, the call's
/// among them, and exports them before the process exits.
#[test]
fn a_stop_during_a_model_call_exports_the_runs_open_spans() {
    let dir = scratch("stop-call");
    project(&dir);
    let (model, _rt) = model(vec![Reply::Delayed {
        after: std::time::Duration::from_secs(60),
        reply: Box::new(Reply::Text(RANK.into())),
    }]);
    let (url, received) = collector();
    let mut child = command(
        &dir,
        &model.base_url(),
        &["run", "triage", "--data", NOTE],
        &exporting(&url, "http/json"),
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while model.calls() == 0 {
        assert!(std::time::Instant::now() < deadline, "no model call began");
        assert!(child.try_wait().unwrap().is_none(), "the run ended first");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Ctrl-C, and again, since the call would not reach a checkpoint.
    for _ in 0..2 {
        signal(&child, "INT");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "{}", text(&out));

    let spans = received.spans();
    let workflow = named(&spans, "invoke_workflow triage");
    let step = named(&spans, "invoke_agent rank-note");
    let call = named(&spans, "chat synthetic");
    assert_eq!(call["parentSpanId"], step["spanId"]);
    assert_eq!(step["parentSpanId"], workflow["spanId"]);
    for span in [workflow, step, call] {
        assert_eq!(
            attribute(span, "atomic.stopped").as_deref(),
            Some("true"),
            "{span}"
        );
    }
}

/// A collector that never answers holds up a stopping process only until
/// the next signal.
#[test]
fn a_second_signal_stops_at_once_while_spans_are_sent() {
    let dir = scratch("hung");
    project(&dir);
    let (model, _rt) = model(vec![Reply::Text(RANK.into())]);
    let url = hung_collector();
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
    wait_for_gate(&dir, &model.base_url(), &mut child);
    signal(&child, "TERM");
    // The gate's wait is no running step, so the flush starts at once: the
    // second signal comes well inside its deadline.
    std::thread::sleep(std::time::Duration::from_millis(300));
    signal(&child, "TERM");
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains(STOPPED_FLUSH), "{}", text(&out));
}

/// A warning line, here about a setting that quotes what it was given.
#[test]
fn a_secret_in_a_warning_line_is_redacted() {
    const SEEDED: &str = concat!("glpat-", "WARNINGlineSEEDED0123456789");
    let dir = scratch("warning");
    project(&dir);
    let (url, _received) = collector();
    let mut env = exporting(&url, "http/json");
    env.push(("OTEL_EXPORTER_OTLP_PROTOCOL", format!("bogus {SEEDED}")));
    let out = command(&dir, "http://127.0.0.1:9/v1", &["debug", "state-dir"], &env)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let shown = text(&out);
    assert!(shown.contains("telemetry is off"), "{shown}");
    assert!(shown.contains("[REDACTED]"), "{shown}");
    assert!(!shown.contains(SEEDED), "{shown}");
}

/// The runner role's own errors, which quote the request they refuse.
#[test]
fn a_secret_in_a_runners_error_is_redacted() {
    const SEEDED: &str = concat!("glpat-", "RUNNERerrorSEEDED0123456789");
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
        .write_all(json!({"protocol": SEEDED}).to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(64), "{}", text(&out));
    let shown = text(&out);
    assert!(shown.contains("atomic runner:"), "{shown}");
    assert!(shown.contains("[REDACTED]"), "{shown}");
    assert!(!shown.contains(SEEDED), "{shown}");
}
