// SPDX-License-Identifier: Apache-2.0

//! What the telemetry suites share: a project, loopback collectors for each
//! OTLP protocol, and running `atomic` against them.

// Each suite uses only some of it.
#![allow(dead_code, clippy::unwrap_used)]

use atomic_core::testing::{FakeModel, Reply};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

pub const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");
pub const NOTE: &str = r#"{"text": "Exporting a file named café.txt crashes the app."}"#;
pub const RANK: &str = r#"{"priority": 1}"#;
pub const SUMMARY: &str = r#"{"line": "P1: Export crashes on accents"}"#;
/// A collector credential, in the exporter's headers.
pub const COLLECTOR_SEED: &str = concat!("glpat-", "COLLECTORcredential0123456789");

/// A scratch directory, removed when the test ends.
pub struct Scratch(pub PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

pub fn scratch(tag: &str) -> Scratch {
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
    Scratch(dir)
}

pub fn object(properties: Value) -> Value {
    let required: Vec<String> = properties.as_object().unwrap().keys().cloned().collect();
    json!({"type": "object", "additionalProperties": false, "required": required, "properties": properties})
}

pub fn write_atom(
    project: &Path,
    name: &str,
    config: &str,
    body: &str,
    input: Value,
    output: Value,
) {
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

/// A project with an exec atom that counts a note's words and fails if its
/// environment holds the exporter's settings, two model atoms, and workflows over them: `triage`,
/// `gated` (with a gate before the summary), `leaky` (an exec atom that
/// fails, writing its input to stderr) and `review` (one Codex atom).
pub fn project(dir: &Path) -> PathBuf {
    let project = dir.join("project");
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: notes\n").unwrap();
    std::fs::write(
        project.join("tools/count.py"),
        "import json, os, sys\n\
         note = json.load(sys.stdin)\n\
         seen = [k for k, v in os.environ.items() if k.startswith('OTEL_') or 'COLLECTOR' in v]\n\
         if seen: sys.exit('the command saw the exporter settings ' + ' '.join(seen))\n\
         json.dump({'title': note['text'][:25], 'words': len(note['text'].split())}, sys.stdout)\n",
    )
    .unwrap();
    std::fs::write(
        project.join("tools/leak.py"),
        "import json, sys\n\
         note = json.load(sys.stdin)\n\
         sys.stderr.write('the provider said ' + note['text'] + '\\n')\n\
         sys.exit(3)\n",
    )
    .unwrap();
    std::fs::write(
        project.join("tools/misfit.py"),
        "import json, sys\n\
         note = json.load(sys.stdin)\n\
         json.dump({'title': 'misfit', 'words': note['text']}, sys.stdout)\n",
    )
    .unwrap();
    let note = || object(json!({"text": {"type": "string"}}));
    write_atom(
        &project,
        "count-note",
        "description: 'Count words.', kind: 'exec', exec: { command: ['python3', 'tools/count.py'] }",
        "",
        note(),
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
    );
    write_atom(
        &project,
        "leak-note",
        "description: 'Fail loudly.', kind: 'exec', exec: { command: ['python3', 'tools/leak.py'] }",
        "",
        note(),
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
    );
    write_atom(
        &project,
        "misfit-note",
        "description: 'Answer out of shape.', kind: 'exec', exec: { command: ['python3', 'tools/misfit.py'] }",
        "",
        note(),
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
        "review-note",
        "description: 'review-note', kind: 'codex'",
        "Rank the note from 1 (urgent) to 3.\nNote: {input.text}",
        note(),
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
    let workflow = |name: &str, count: &str, gate: &str| {
        format!(
            "atomic: 1\nname: {name}\n\
             input: {{ $ref: ../atoms/count-note.input.schema.json }}\n\
             output: {{ $ref: ../atoms/summarize-note.output.schema.json }}\n\
             steps:\n\
             \x20 count:\n    atom: {count}\n    with: {{ text: input.text }}\n\
             \x20 rank:\n    after: [count]\n    atom: rank-note\n\
             \x20   with: {{ title: steps.count.output.title, words: steps.count.output.words }}\n\
             {gate}\
             \x20 summarize:\n    after: [{after}]\n    atom: summarize-note\n\
             \x20   with: {{ title: steps.count.output.title, priority: steps.rank.output.priority }}\n\
             returns: steps.summarize.output\n",
            after = if gate.is_empty() { "rank" } else { "check" },
        )
    };
    let gate = "  check:\n    after: [rank]\n    gate:\n      title: \"'Check the rank'\"\n      \
                show: steps.rank.output\n      timeout: 24h\n";
    for (file, text) in [
        ("triage", workflow("triage", "count-note", "")),
        ("gated", workflow("gated", "count-note", gate)),
        ("leaky", workflow("leaky", "leak-note", "")),
        ("misfit", workflow("misfit", "misfit-note", "")),
        (
            "review",
            "atomic: 1\nname: review\n\
             input: { $ref: ../atoms/review-note.input.schema.json }\n\
             output: { $ref: ../atoms/review-note.output.schema.json }\n\
             steps:\n\
             \x20 review:\n    atom: review-note\n    with: { text: input.text }\n\
             returns: steps.review.output\n"
                .to_owned(),
        ),
    ] {
        std::fs::write(
            project.join(format!("workflows/{file}.workflow.yaml")),
            text,
        )
        .unwrap();
    }
    project
}

/// One request the collector received: its path, its head and its body.
pub type Request = (String, String, Vec<u8>);

/// What the collector received.
#[derive(Clone, Default)]
pub struct Received(pub Arc<Mutex<Vec<Request>>>);

impl Received {
    pub fn bodies(&self) -> String {
        let all = self.0.lock().unwrap();
        all.iter()
            .map(|(_, _, body)| String::from_utf8_lossy(body).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn headers(&self) -> String {
        let all = self.0.lock().unwrap();
        all.iter()
            .map(|(path, headers, _)| format!("{path}\n{headers}"))
            .collect()
    }

    /// The spans of OTLP/JSON requests.
    pub fn spans(&self) -> Vec<Value> {
        let all = self.0.lock().unwrap();
        let mut spans = Vec::new();
        for (_, _, body) in all.iter() {
            let doc: Value = serde_json::from_slice(body).expect("an OTLP/JSON body");
            for resource in doc["resourceSpans"].as_array().into_iter().flatten() {
                for scope in resource["scopeSpans"].as_array().into_iter().flatten() {
                    spans.extend(scope["spans"].as_array().into_iter().flatten().cloned());
                }
            }
        }
        spans
    }
}

/// An attribute of an OTLP/JSON span, as text.
pub fn attribute(span: &Value, key: &str) -> Option<String> {
    let value = &span["attributes"]
        .as_array()?
        .iter()
        .find(|a| a["key"] == key)?["value"];
    let (_, v) = value.as_object()?.iter().next()?;
    Some(match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// The span named `name` among `spans`.
pub fn named<'a>(spans: &'a [Value], name: &str) -> &'a Value {
    let found: Vec<&Value> = spans.iter().filter(|s| s["name"] == name).collect();
    assert_eq!(found.len(), 1, "one span named {name}: {spans:#?}");
    found[0]
}

/// Whether an OTLP/JSON span's status is an error.
pub fn failed(span: &Value) -> bool {
    let code = &span["status"]["code"];
    code == 2 || code == "STATUS_CODE_ERROR"
}

/// An OTLP/HTTP collector on loopback that accepts everything.
pub fn collector() -> (String, Received) {
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
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                }
            });
        }
    });
    (url, received)
}

/// One HTTP/2 frame: its length, type, flags and stream.
pub fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut out = vec![(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags];
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// An OTLP/gRPC collector on loopback: just enough HTTP/2 to take each
/// export request's message and answer it with an empty success.
pub fn grpc_collector() -> (String, Received) {
    const DATA: u8 = 0;
    const HEADERS: u8 = 1;
    const SETTINGS: u8 = 4;
    const PING: u8 = 6;
    const WINDOW_UPDATE: u8 = 8;
    const END_STREAM: u8 = 1;
    const END_HEADERS: u8 = 4;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let received = Received::default();
    let keep = received.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let keep = keep.clone();
            std::thread::spawn(move || {
                let mut preface = [0u8; 24];
                if stream.read_exact(&mut preface).is_err() {
                    return;
                }
                let _ = stream.write_all(&frame(SETTINGS, 0, 0, &[]));
                let mut bodies: std::collections::HashMap<u32, Vec<u8>> = Default::default();
                loop {
                    let mut head = [0u8; 9];
                    if stream.read_exact(&mut head).is_err() {
                        return;
                    }
                    let len = (usize::from(head[0]) << 16)
                        | (usize::from(head[1]) << 8)
                        | usize::from(head[2]);
                    let (kind, flags) = (head[3], head[4]);
                    let id = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
                    let mut payload = vec![0u8; len];
                    if stream.read_exact(&mut payload).is_err() {
                        return;
                    }
                    let mut reply = Vec::new();
                    match kind {
                        SETTINGS if flags & 1 == 0 => reply.extend(frame(SETTINGS, 1, 0, &[])),
                        PING if flags & 1 == 0 => reply.extend(frame(PING, 1, 0, &payload)),
                        HEADERS => {
                            bodies.entry(id).or_default();
                        }
                        DATA => {
                            bodies.entry(id).or_default().extend_from_slice(&payload);
                            if len > 0 {
                                let more = (len as u32).to_be_bytes();
                                reply.extend(frame(WINDOW_UPDATE, 0, 0, &more));
                                reply.extend(frame(WINDOW_UPDATE, 0, id, &more));
                            }
                            if flags & END_STREAM != 0 {
                                let body = bodies.remove(&id).unwrap_or_default();
                                keep.0
                                    .lock()
                                    .unwrap()
                                    .push(("grpc".into(), String::new(), body));
                                // :status 200, content-type application/grpc.
                                let mut answer = vec![0x88, 0x0f, 0x10, 16];
                                answer.extend_from_slice(b"application/grpc");
                                reply.extend(frame(HEADERS, END_HEADERS, id, &answer));
                                reply.extend(frame(DATA, 0, id, &[0, 0, 0, 0, 0]));
                                let mut trailers = vec![0x00, 11];
                                trailers.extend_from_slice(b"grpc-status");
                                trailers.extend_from_slice(&[1, b'0']);
                                reply.extend(frame(
                                    HEADERS,
                                    END_HEADERS | END_STREAM,
                                    id,
                                    &trailers,
                                ));
                            }
                        }
                        _ => {}
                    }
                    if !reply.is_empty() && stream.write_all(&reply).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, received)
}

/// `atomic` with `args` in `dir`'s project, against the model at `model`,
/// with `env` added.
pub fn command(dir: &Path, model: &str, args: &[&str], env: &[(&str, String)]) -> Command {
    let mut command = Command::new(ATOMIC);
    command
        .args(args)
        .current_dir(dir.join("project"))
        .env("HOME", dir.join("home"))
        .env("ATOMIC_HOME", dir.join("atomic-home"))
        .env("USER", "dev")
        .env("ATOMIC_MODEL_BASE_URL", model)
        .env("ATOMIC_MODEL", "synthetic")
        .env_remove("ATOMIC_MODEL_API_KEY_ENV")
        .env_remove("ATOMIC_DATABASE_URL")
        .env_remove("RUST_BACKTRACE")
        .stdin(Stdio::null());
    for var in [
        "OTEL_SDK_DISABLED",
        "OTEL_TRACES_EXPORTER",
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    ] {
        command.env_remove(var);
    }
    command.envs(env.iter().map(|(k, v)| (k, v)));
    command
}

/// The variables that export to `collector` over `protocol`.
pub fn exporting(collector: &str, protocol: &str) -> Vec<(&'static str, String)> {
    vec![
        ("OTEL_EXPORTER_OTLP_ENDPOINT", collector.to_owned()),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", protocol.to_owned()),
        (
            "OTEL_EXPORTER_OTLP_HEADERS",
            format!("authorization=Bearer {COLLECTOR_SEED}"),
        ),
    ]
}

/// `atomic run <workflow>` on `data`, exporting as `env` says.
pub fn run(dir: &Path, model: &str, workflow: &str, data: &str, env: &[(&str, String)]) -> Output {
    command(dir, model, &["run", workflow, "--data", data], env)
        .output()
        .unwrap()
}

pub fn text(out: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Whether any file under `dir` (the state directory: the durable store,
/// Codex's home, logs) holds `secret`.
pub fn stored(dir: &Path, secret: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if std::fs::read(&path)
                .is_ok_and(|b| b.windows(secret.len()).any(|w| w == secret.as_bytes()))
            {
                found.push(path);
            }
        }
    }
    found
}

/// A note whose text is `secret`, as `--data`.
pub fn note(secret: &str) -> String {
    json!({"text": secret}).to_string()
}

/// The fake model, served on a runtime of its own for the test's length.
pub fn model(script: Vec<Reply>) -> (FakeModel, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let model = rt.block_on(FakeModel::start(script)).unwrap();
    (model, rt)
}

/// A provider's 401 that echoes `secret`, as OpenAI's API does a wrong key.
pub fn echoing(secret: &str) -> Reply {
    Reply::Http {
        status: 401,
        body: format!(
            r#"{{"error": {{"message": "Incorrect API key provided: {secret}", "type": "invalid_request_error"}}}}"#
        ),
    }
}

/// A collector that accepts connections and never answers.
pub fn hung_collector() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            held.extend(stream.ok());
        }
    });
    url
}

/// Send `child` the signal named `name` (`TERM`, `INT` or `KILL`).
pub fn signal(child: &std::process::Child, name: &str) {
    let number = match name {
        "TERM" => libc::SIGTERM,
        "INT" => libc::SIGINT,
        "KILL" => libc::SIGKILL,
        other => panic!("no signal {other} here"),
    };
    let pid = libc::pid_t::try_from(child.id()).expect("a pid");
    // SAFETY: kill only sends a signal, to the one process given.
    #[allow(unsafe_code)]
    let sent = unsafe { libc::kill(pid, number) };
    assert_eq!(sent, 0, "kill -{name}: {}", std::io::Error::last_os_error());
}

/// How long [`wait_for_gate`] waits for a run to reach its gate: far more
/// than a run of one model call takes, however loaded the machine.
pub const GATE_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);

/// What a run started with piped output printed, read as it goes so the
/// run never blocks on a full pipe.
pub struct Drained {
    stdout: std::thread::JoinHandle<Vec<u8>>,
    stderr: std::thread::JoinHandle<Vec<u8>>,
}

impl Drained {
    /// Start reading `child`'s piped stdout and stderr.
    pub fn of(child: &mut std::process::Child) -> Self {
        let read = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut all = Vec::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.read_to_end(&mut all);
                }
                all
            })
        };
        let stdout = child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>);
        let stderr = child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>);
        Self {
            stdout: read(stdout),
            stderr: read(stderr),
        }
    }

    /// Wait for `child` to end and return how it ended and all it printed.
    pub fn wait(self, child: &mut std::process::Child) -> Output {
        let status = child.wait().unwrap();
        Output {
            status,
            stdout: self.stdout.join().unwrap(),
            stderr: self.stderr.join().unwrap(),
        }
    }
}

/// Wait until the run `child` runs in `dir`'s project waits at a gate, as
/// `atomic gates list` shows, reading its output meanwhile. A run that ends
/// first, or has not reached its gate within [`GATE_LIMIT`], fails the
/// test with how it ended and what it printed; one still running then is
/// killed first.
pub fn wait_for_gate(dir: &Path, model: &str, child: &mut std::process::Child) -> Drained {
    let drained = Drained::of(child);
    let deadline = std::time::Instant::now() + GATE_LIMIT;
    loop {
        let listed = command(dir, model, &["gates", "list", "--json"], &[])
            .output()
            .unwrap();
        let open: Value = serde_json::from_slice(&listed.stdout).unwrap_or(json!([]));
        if open.as_array().is_some_and(|g| !g.is_empty()) {
            return drained;
        }
        let ended = child.try_wait().unwrap();
        if ended.is_some() || std::time::Instant::now() >= deadline {
            let why = match ended {
                Some(_) => "the run ended before its gate".to_owned(),
                None => {
                    signal(child, "KILL");
                    format!("no gate opened in {GATE_LIMIT:?}, so the run was killed")
                }
            };
            let out = drained.wait(child);
            panic!(
                "{why}; it ended {}: {}\n`atomic gates list` last said: {}",
                out.status,
                text(&out),
                text(&listed)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}
