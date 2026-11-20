// SPDX-License-Identifier: Apache-2.0

//! Telemetry and redaction, end to end, through the `atomic` binary:
//!
//! - a run configured by the standard `OTEL_EXPORTER_OTLP_*` variables
//!   exports, over OTLP/HTTP to a collector this test serves on loopback,
//!   spans that name the workflow, each step and each model call;
//! - a secret a provider echoes in its error reaches neither the command's
//!   log lines nor its error report nor the exported spans;
//! - the exporter's own headers, which can carry a collector's credential,
//!   are sent to the collector and reach neither the log nor the atoms'
//!   commands.

#![allow(clippy::unwrap_used)]

use atomic_core::testing::{FakeModel, Reply};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");
const NOTE: &str = r#"{"text": "Exporting a file named café.txt crashes the app."}"#;
const RANK: &str = r#"{"priority": 1}"#;
const SUMMARY: &str = r#"{"line": "P1: Export crashes on accents"}"#;
/// A secret seeded where a provider might echo it.
const SECRET: &str = concat!("sk-ant-", "api03-SEEDEDsecretVALUE0123456789abcdef");
/// A collector credential, in the exporter's headers.
const COLLECTOR_TOKEN: &str = concat!("glpat-", "COLLECTORcredential0123456789");

fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-telemetry-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in [
        "home",
        "project/atoms",
        "project/workflows",
        "project/tools",
    ] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    dir
}

fn object(properties: Value) -> Value {
    let required: Vec<String> = properties.as_object().unwrap().keys().cloned().collect();
    json!({"type": "object", "additionalProperties": false, "required": required, "properties": properties})
}

fn write_atom(project: &Path, name: &str, config: &str, body: &str, input: Value, output: Value) {
    let atoms = project.join("atoms");
    std::fs::write(
        atoms.join(format!("{name}.atom.astro")),
        format!(
            "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/{name}';\n\
             import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
             const {{ input }}: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = {{ {config} }};\n\
             ---\n{body}\n"
        ),
    )
    .unwrap();
    for (role, schema) in [("input", input), ("output", output)] {
        std::fs::write(
            atoms.join(format!("{name}.{role}.schema.json")),
            schema.to_string(),
        )
        .unwrap();
    }
}

/// A project with an exec atom that counts a note's words and records its
/// own environment, two model atoms, and a workflow over the three.
fn project(dir: &Path) -> PathBuf {
    let project = dir.join("project");
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: notes\n").unwrap();
    std::fs::write(
        project.join("tools/count.py"),
        "import json, os, sys\n\
         note = json.load(sys.stdin)\n\
         json.dump(dict(os.environ), open(os.environ['ENV_LOG'], 'w'))\n\
         json.dump({'title': note['text'][:25], 'words': len(note['text'].split())}, sys.stdout)\n",
    )
    .unwrap();
    write_atom(
        &project,
        "count-note",
        &format!(
            "description: 'Count words.', kind: 'exec', exec: {{ command: ['python3', 'tools/count.py'] }}, env: {{ ENV_LOG: '{}' }}",
            dir.join("env.json").display()
        ),
        "",
        object(json!({"text": {"type": "string"}})),
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
    );
    write_atom(
        &project,
        "rank-note",
        "description: 'rank-note'",
        "Rank the note from 1 (urgent) to 3.\nTitle: {input.title}\nWords: {input.words}",
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
        object(json!({"priority": {"type": "integer", "minimum": 1, "maximum": 3}})),
    );
    write_atom(
        &project,
        "summarize-note",
        "description: 'summarize-note'",
        "Write one line for the triage board.\nTitle: {input.title}\nPriority: {input.priority}",
        object(json!({"title": {"type": "string"}, "priority": {"type": "integer"}})),
        object(json!({"line": {"type": "string"}})),
    );
    std::fs::write(
        project.join("workflows/triage.workflow.yaml"),
        "atomic: 1\nname: triage\n\
         input: { $ref: ../atoms/count-note.input.schema.json }\n\
         output: { $ref: ../atoms/summarize-note.output.schema.json }\n\
         steps:\n\
         \x20 count:\n    atom: count-note\n    with: { text: input.text }\n\
         \x20 rank:\n    after: [count]\n    atom: rank-note\n\
         \x20   with: { title: steps.count.output.title, words: steps.count.output.words }\n\
         \x20 summarize:\n    after: [rank]\n    atom: summarize-note\n\
         \x20   with: { title: steps.count.output.title, priority: steps.rank.output.priority }\n\
         returns: steps.summarize.output\n",
    )
    .unwrap();
    project
}

/// One request the collector received: its path, its head and its body.
type Request = (String, String, Vec<u8>);

/// What the collector received.
#[derive(Clone, Default)]
struct Received(Arc<Mutex<Vec<Request>>>);

impl Received {
    fn bodies(&self) -> String {
        let all = self.0.lock().unwrap();
        all.iter()
            .map(|(_, _, body)| String::from_utf8_lossy(body).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn headers(&self) -> String {
        let all = self.0.lock().unwrap();
        all.iter()
            .map(|(path, headers, _)| format!("{path}\n{headers}"))
            .collect()
    }
}

/// An OTLP/HTTP collector on loopback that accepts everything.
fn collector() -> (String, Received) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let received = Received::default();
    let keep = received.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let keep = keep.clone();
            std::thread::spawn(move || {
                loop {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        match stream.read(&mut byte) {
                            Ok(1) => head.push(byte[0]),
                            _ => return,
                        }
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let length: usize = head
                        .lines()
                        .filter_map(|l| l.split_once(':'))
                        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, v)| v.trim().parse().ok())
                        .unwrap_or(0);
                    let mut body = vec![0u8; length];
                    if stream.read_exact(&mut body).is_err() {
                        return;
                    }
                    let path = head
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned();
                    keep.0.lock().unwrap().push((path, head, body));
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\n\r\n",
                    );
                }
            });
        }
    });
    (url, received)
}

/// `atomic run triage` in `dir`'s project, against the model at `model`,
/// exporting to `collector`.
fn run(dir: &Path, model: &str, collector: &str) -> Output {
    let project = dir.join("project");
    Command::new(ATOMIC)
        .args(["run", "triage", "--data", NOTE])
        .current_dir(&project)
        .env("HOME", dir.join("home"))
        .env("ATOMIC_HOME", dir.join("atomic-home"))
        .env("USER", "dev")
        .env("ATOMIC_MODEL_BASE_URL", model)
        .env("ATOMIC_MODEL", "synthetic")
        .env_remove("ATOMIC_MODEL_API_KEY_ENV")
        .env_remove("ATOMIC_DATABASE_URL")
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", collector)
        .env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf")
        .env(
            "OTEL_EXPORTER_OTLP_HEADERS",
            format!("authorization=Bearer {COLLECTOR_TOKEN}"),
        )
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The fake model, served on a runtime of its own for the test's length.
fn model(script: Vec<Reply>) -> (FakeModel, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let model = rt.block_on(FakeModel::start(script)).unwrap();
    (model, rt)
}

#[test]
fn a_run_exports_spans_naming_its_workflow_each_step_and_each_model_call() {
    let dir = scratch("spans");
    project(&dir);
    let (model, _rt) = model(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())]);
    let (url, received) = collector();
    let out = run(&dir, &model.base_url(), &url);
    assert!(out.status.success(), "{}", text(&out));

    let spans = received.bodies();
    for name in [
        // The workflow, by operation and by name.
        "invoke_workflow triage",
        "atomic.workflow.id",
        // Each step: the exec atom and the two model atoms.
        "invoke_agent count-note",
        "invoke_agent rank-note",
        "invoke_agent summarize-note",
        "atomic.step",
        // Each model call, with its model and what it spent.
        "chat synthetic",
        "gen_ai.usage.input_tokens",
    ] {
        assert!(spans.contains(name), "no {name} in what was exported");
    }
    assert_eq!(
        spans.matches("chat synthetic").count(),
        2,
        "one span per model call"
    );
    // Sent where the variables say, with the headers they name.
    let headers = received.headers();
    assert!(headers.contains("/v1/traces"), "{headers}");
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
fn a_secret_a_provider_echoes_reaches_no_log_line_report_or_span() {
    let dir = scratch("secret");
    project(&dir);
    let echoed = format!(
        r#"{{"error": {{"message": "Incorrect API key provided: {SECRET}", "type": "invalid_request_error"}}}}"#
    );
    let (model, _rt) = model(vec![Reply::Http {
        status: 401,
        body: echoed,
    }]);
    let (url, received) = collector();
    let out = run(&dir, &model.base_url(), &url);
    assert!(!out.status.success(), "the call failed: {}", text(&out));
    let shown = text(&out);
    assert!(shown.contains("401"), "the error is reported: {shown}");
    assert!(!shown.contains(SECRET), "the secret was logged: {shown}");
    assert!(shown.contains("[REDACTED]"), "{shown}");

    let spans = received.bodies();
    assert!(
        spans.contains("chat synthetic"),
        "the failed call has its span"
    );
    assert!(!spans.contains(SECRET), "the secret was exported");
    assert!(
        spans.contains("[REDACTED]"),
        "its redacted error was exported"
    );
}
