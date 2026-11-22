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

//!
//! A test whose run executes an exec atom needs Codex's Linux sandbox, as
//! every exec atom's command runs under it: on Linux it runs with the
//! `sandbox-tests` feature, and with it on, a sandbox that cannot start
//! fails the test.

#![allow(clippy::unwrap_used)]

#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
mod support;
mod telemetry_common;

#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
use atomic_core::testing::Reply;
use serde_json::{Value, json};
use std::io::Write;

use std::process::Stdio;
use telemetry_common::*;

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn spans_over_http_json_parent_each_call_to_its_step_and_each_step_to_its_run() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn spans_over_http_protobuf_carry_the_exporters_headers_which_no_atom_sees() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn spans_over_grpc_are_exported_from_the_commands_runtime() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn nothing_is_exported_when_the_variables_turn_export_off() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_run_stopped_at_a_gate_ends_and_exports_its_spans() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("gate");
    project(&dir);
    let (model, _rt) = model(vec![Reply::Text(RANK.into())]);
    let (url, received) = collector();
    let mut child = Child::new(
        command(
            &dir,
            &model.base_url(),
            &["run", "gated", "--data", NOTE],
            &exporting(&url, "http/json"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap(),
    );
    let drained = wait_for_gate(&dir, &model.base_url(), &mut child);
    signal(&child, "TERM");
    let out = drained.wait(&mut child);
    assert!(!out.status.success(), "{}", text(&out));
    // The run has until its flush's deadline to send the last spans. If it
    // missed that, it says so on stderr, and that is what failed here: not a
    // span some collector never took.
    let shown = String::from_utf8_lossy(&out.stderr);
    assert!(!shown.contains(DROPPED_SPANS), "{shown}");

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

/// A command that ends before a gate opens fails the wait for it, and the
/// message that fails the test quotes what the last `atomic gates list`
/// answered and says how many polls that took and how long the last one
/// took: what a poll which stops answering is judged by.
#[test]
fn a_wait_for_a_gate_that_never_opens_says_how_it_was_waited_for() {
    let dir = scratch("no-gate");
    project(&dir);
    // A command that ends at once, in the project the gate would open in:
    // no gate can ever be listed for it.
    let mut child = Child::new(
        command(&dir, "http://127.0.0.1:9/v1", &["debug", "state-dir"], &[])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let why = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for_gate(&dir, "http://127.0.0.1:9/v1", &mut child)
    }))
    .err()
    .and_then(|payload| payload.downcast_ref::<String>().cloned())
    .expect("waiting for a gate that never opens fails the test");
    assert!(
        why.contains("the run ended before its gate"),
        "not reported as what happened: {why}"
    );
    // The poll's own answer, which is only there if its output was read.
    assert!(
        why.contains("`atomic gates list` last said: stdout: ["),
        "the last poll is not quoted: {why}"
    );
    let counted = why
        .split(" poll(s), the last took ")
        .next()
        .and_then(|head| head.rsplit('(').next())
        .and_then(|polls| polls.parse::<usize>().ok())
        .expect("the message counts the polls it made");
    assert!(counted >= 1, "no poll counted: {why}");
    let took = why
        .split(" poll(s), the last took ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|ms| ms.parse::<u128>().ok())
        .expect("the message says how long the last poll took");
    assert!(took > 0, "the last poll took no time at all: {why}");
}

/// The wait for a gate ends at its own limit, and what it counts when it does
/// is what its polls really did: a poll started with no time left on the wait
/// is killed where it stands, and what it had printed by then is not the last
/// thing said. The limit here is the wait's argument rather than
/// [`GATE_LIMIT`], so the test sits through seconds and not through minutes.
#[test]
fn a_wait_for_a_gate_that_opens_by_its_limit_counts_no_killed_poll() {
    let dir = scratch("gate-limit");
    project(&dir);
    // A run that outlasts the wait, in the project a gate would open in, so
    // that nothing but the wait's own limit can end it: 400 s, against the
    // wait's 3. It is `sleep` itself and not a shell asleep over it, because
    // the wait ends the command it is handed and nothing that command forked
    // of its own.
    let mut child = Child::new(
        std::process::Command::new("/bin/sleep")
            .arg("400")
            .current_dir(dir.join("project"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let limit = std::time::Duration::from_secs(3);
    let began = std::time::Instant::now();
    let why = failed_wait(|| {
        wait_for_gate_by(&dir, "http://127.0.0.1:9/v1", &mut child, limit);
    });
    let ended = began.elapsed();
    // Its own limit ended it, and not the run: `sleep` does not stop on its
    // own, and the other reason the wait gives is that the run ended first.
    assert!(
        why.contains(&format!(
            "no gate opened in {limit:?}, so the run was killed"
        )),
        "not the wait's own limit that ended it: {why}"
    );
    // It came back at its limit: far past it would be waiting for a run that
    // sleeps on rather than ending it, and short of it is a wait that gave up
    // before it had polled. The room below is for the polls still running when
    // the deadline passes, each of which is waited for as long as it takes to
    // end once the wait has killed it.
    assert!(
        ended >= limit,
        "the wait gave up after {ended:?}, short of its {limit:?}"
    );
    assert!(
        ended < limit + std::time::Duration::from_secs(30),
        "the wait went on for {ended:?} past its {limit:?}"
    );
    // And it ended the run rather than leaving it asleep for 400 s.
    let pid = libc::pid_t::try_from(child.id()).expect("a pid");
    // SAFETY: a signal of 0 only asks whether that process is there.
    #[allow(unsafe_code)]
    let still_there = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!still_there, "the run outlived the wait that ends it");
    // What the message ends with: how many polls it made, how long the one it
    // quotes took, and, when it had to kill one, how many.
    let polls = why
        .split(" poll(s), ")
        .next()
        .and_then(|head| head.rsplit('(').next())
        .and_then(|polls| polls.parse::<usize>().ok())
        .expect("the message counts the polls it made");
    assert!(polls >= 2, "the wait stopped after {polls} poll: {why}");
    let counted = why
        .rsplit("poll(s), ")
        .next()
        .and_then(|tail| tail.strip_suffix(')'))
        .expect("the message ends with how its polls went");
    let parts: Vec<&str> = counted.split(", ").collect();
    let took = parts[0]
        .strip_prefix("the last took ")
        .and_then(|ms| ms.strip_suffix(" ms"))
        .and_then(|ms| ms.parse::<u128>().ok())
        .expect("the count does not give the time of the poll it quoted");
    assert!(took > 0, "the poll it quoted took no time at all: {why}");
    // Every poll of a wait whose command answers answered: none was killed,
    // so the count says nothing beyond how long the poll it quotes took. This
    // is what the deadline guard buys: switching it off
    // (`scripts/controls/telemetry-poll-not-started-late`) makes the wait
    // start one last poll it has no time left to wait for, which is killed
    // where it stands and counted here.
    assert_eq!(parts.len(), 1, "the wait had to kill a poll: {why}");
    // What it quotes is a poll that answered: `gates list --json` answers
    // with a JSON array, and an empty one is what a project holds no gate
    // for. Half of an answer, which is all a killed poll has, is no array.
    assert!(
        why.contains("`atomic gates list` last said: stdout: ["),
        "the last poll that answered is not quoted: {why}"
    );
    let quoted = why
        .split("`atomic gates list` last said: stdout: ")
        .nth(1)
        .and_then(|rest| rest.split("\nstderr:").next())
        .expect("the message quotes what the last poll printed");
    let listed: Vec<Value> = serde_json::from_str(quoted.trim()).unwrap_or_else(|why| {
        panic!("what the wait quoted is no answer of `gates list`: {why}\nquoted: {quoted}")
    });
    assert!(listed.is_empty(), "a gate open for no run: {listed:?}");
}

/// What a run writes as it goes is read line by line as it goes: a test can
/// wait for one line before the run ends, and a run that has not written it
/// fails the test with the lines it did write. And a command left running
/// when a test ends is killed and reaped, not left behind.
#[test]
fn a_line_is_read_off_a_run_as_it_writes_it() {
    // Two lines, then nothing: the reader has to hand them out while the
    // writer still runs, and a line after the sleep never comes.
    let mut child = Child::new(
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("echo first >&2; echo the second line >&2; sleep 30; echo never >&2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut drained = Drained::of(&mut child);
    // The wait goes on past the first line to the one it is for.
    drained.wait_for_stderr("the second", std::time::Duration::from_secs(30));
    // A line written before the call counts too: the run said it already.
    drained.wait_for_stderr("first", std::time::Duration::from_secs(30));
    // The writer is asleep and the line is not coming: the wait gives up at
    // its limit, with the lines the run did write, without waiting for it to
    // end.
    let why = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drained.wait_for_stderr("never", std::time::Duration::from_millis(500))
    }))
    .err()
    .and_then(|payload| payload.downcast_ref::<String>().cloned())
    .expect("waiting for a line that never comes fails the test");
    assert!(
        why.contains("no stderr line holds \"never\" within 500ms"),
        "{why}"
    );
    assert!(why.contains("the second line"), "{why}");
    assert!(!why.contains("never\n"), "a line never written: {why}");
    // The writer is still asleep. Ending the test must not leave that direct
    // child running, or unreaped. What the shell forked for its `sleep` is
    // the shell's own child, which this guard does not reach and which ends
    // in its own time.
    let pid = libc::pid_t::try_from(child.id()).expect("a pid");
    // And the guard has to end the run rather than wait it out: with the kill
    // gone the guard would only wait, and wait until the sleep ran out, so
    // timing the end is what tells a guard that kills from one that waits for
    // its own good.
    let began = std::time::Instant::now();
    drop(child);
    let ended = began.elapsed();
    assert!(
        ended < std::time::Duration::from_secs(10),
        "leaving the run took {ended:?}, which is waiting for a run that \
         sleeps on rather than killing it",
    );
    // SAFETY: a signal of 0 only asks whether that process is there.
    #[allow(unsafe_code)]
    let still_there = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!still_there, "the run outlived the guard that ends it");
}

/// What a run wrote last, with no line end on it, is a line too, and is
/// handed out clean as every other line: a wait for something else fails the
/// test with that text and nothing stuck on the end of it.
#[test]
fn a_last_line_with_no_end_of_line_is_read_like_the_others() {
    let dir = scratch("partial-line");
    project(&dir);
    // One whole line, then text with no line end on it, and spaces after
    // each: what the reader hands out is to read the same whether or not the
    // run ended its line, and to hold neither the line end nor what came
    // before it.
    let mut child = Child::new(
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("printf 'whole line  \\npartial at the end  ' >&2")
            .current_dir(dir.join("project"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut drained = Drained::of(&mut child);
    // Both lines the run wrote are handed out: the one with an end of its own,
    // and the last one, which has none and is handed out only once the run has
    // ended its stderr. Each is waited for as long as a loaded machine may
    // take, and each with its spaces still on it, so that what is left to
    // check is whether they came through.
    drained.wait_for_stderr("whole line", std::time::Duration::from_secs(30));
    drained.wait_for_stderr("partial at the end", std::time::Duration::from_secs(30));
    // Now a line the run never wrote, and a short limit: the two it did write
    // are already in, so this wait has nothing left to wait for and fails with
    // them.
    let why = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drained.wait_for_stderr("no such line", std::time::Duration::from_millis(500))
    }))
    .err()
    .and_then(|payload| payload.downcast_ref::<String>().cloned())
    .expect("waiting for a line the run never wrote fails the test");
    assert!(why.contains("no such line"), "{why}");
    // And both clean, with nothing of what the run put after them. What the
    // wait quotes ends with the run's last line, so the space the run left
    // after it would still stand out had it come through.
    assert!(
        !why.contains("whole line "),
        "the line with an end of its own kept what came before the end: {why}"
    );
    assert!(
        !why.contains("partial at the end "),
        "the run's last line kept what came after it: {why}"
    );
}

/// A lite provider's error: its excerpt is redacted where it is made, so
/// the run's stored error never holds the key, nor does anything shown.
/// Each body puts its key right after an escape or a colour.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_key_a_lite_provider_echoes_is_redacted_in_its_excerpt() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        // Its call's span says it failed, and why: the status and the type
        // of error the provider named, if it named one, none of its own
        // text, which the run's stored error keeps; and the status code, as
        // an integer.
        let spans = received.spans();
        let call = named(&spans, "chat synthetic");
        assert!(failed(call), "{call}");
        assert_eq!(
            attribute(call, "error.type").as_deref(),
            Some("atomic.model_call")
        );
        let expected = if index == 0 {
            "401 Unauthorized: invalid_request_error"
        } else {
            "401 Unauthorized"
        };
        assert_eq!(
            attribute(call, "error.message").as_deref(),
            Some(expected),
            "{call}"
        );
        let status = call["attributes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a["key"] == "http.response.status_code")
            .map(|a| &a["value"]);
        // OTLP/JSON may write an integer as a number or a string; either
        // way it is an intValue, not a stringValue.
        let int = status
            .and_then(|v| v.get("intValue"))
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned));
        assert_eq!(int.as_deref(), Some("401"), "{call}");
        // What the step's span and its run's say of why they failed is just
        // what the call that failed said — its status and the type the
        // provider named, or the status alone when it named none — and no
        // part of what the provider wrote, which the run's stored error keeps.
        for name in ["invoke_agent rank-note", "invoke_workflow triage"] {
            let span = named(&spans, name);
            assert!(failed(span), "{span}");
            assert_eq!(
                attribute(span, "error.type").as_deref(),
                Some("atomic.failed"),
                "{span}"
            );
            let why = span["status"]["message"]
                .as_str()
                .unwrap_or_else(|| panic!("{span} carries no status message: {span}"));
            assert_eq!(why, expected, "{span} says other than the call did");
        }
    }
}

/// A command's stderr is redacted, then cut to its tail, where the step's
/// error is made: the run's stored error never holds it.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_key_in_a_commands_stderr_is_redacted_in_its_tail() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_key_in_a_steps_error_is_redacted_in_the_text_report() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    const SEEDED: &str = concat!("glpat-", "TEXTreportSEEDED0123456789");
    let dir = scratch("text-report");
    project(&dir);
    let out = run(&dir, "http://127.0.0.1:9/v1", "misfit", &note(SEEDED), &[]);
    assert!(!out.status.success(), "{}", text(&out));
    let shown = text(&out);
    assert!(shown.contains("[REDACTED]"), "{shown}");
    assert!(!shown.contains(SEEDED), "the key was shown");
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_key_in_a_steps_error_is_redacted_in_the_json_report() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_key_in_a_steps_error_is_redacted_in_its_spans() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
fn drain(mut out: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut out, &mut std::io::sink());
    })
}

/// What a stopped process writes when a signal cuts its last flush short.
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
const STOPPED_FLUSH: &str = "stopping now, without sending the last spans";

/// What a stopped process writes when its collector has not taken its last
/// spans by the flush's deadline, after which they are lost. A test that
/// then reads the spans looks for this first, so that a missed flush is
/// reported as the run's own words and not as a span that is missing.
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
const DROPPED_SPANS: &str = "they are dropped";

/// What a run writes as it begins stopping, before it sends its last spans.
/// A test that stops one waits for this line before it signals again.
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
const STOPPING: &str = "stopping;";

/// How long a run gets to show on its stderr that it noticed a signal: far
/// more than noticing one takes, however loaded the machine.
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
const SEEN_BY_RUN: std::time::Duration = std::time::Duration::from_secs(30);

/// A command that ends normally sends its last spans, and a signal stops a
/// collector that never answers from holding it up at all.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_signal_stops_the_last_flush_of_a_command_that_ended() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_collector_that_never_answers_delays_an_exit_only_briefly() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_stop_during_a_model_call_exports_the_runs_open_spans() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_second_signal_stops_at_once_while_spans_are_sent() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("hung");
    project(&dir);
    let (model, _rt) = model(vec![Reply::Text(RANK.into())]);
    let url = hung_collector();
    let mut child = Child::new(
        command(
            &dir,
            &model.base_url(),
            &["run", "gated", "--data", NOTE],
            &exporting(&url, "http/json"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap(),
    );
    let mut drained = wait_for_gate(&dir, &model.base_url(), &mut child);
    signal(&child, "TERM");
    // Wait for the run's own word that it is stopping rather than for a
    // fixed beat, which a loaded machine can spend before the run has even
    // noticed the first signal; then a short beat. What the beat is against
    // is the second signal arriving before the first wait in `runs::stop`,
    // just after that word, has been polled: a signal arriving then is
    // recorded and can be read by that first wait, which logs it as
    // "stopping now" and takes it, leaving the later wait for the flush to
    // wait on a third signal that never comes. A signal that arrives once
    // that wait has been polled is recorded for the flush and stops it,
    // which is what the run is asked to say.
    drained.wait_for_stderr(STOPPING, SEEN_BY_RUN);
    std::thread::sleep(std::time::Duration::from_millis(300));
    signal(&child, "TERM");
    let out = drained.wait(&mut child);
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
