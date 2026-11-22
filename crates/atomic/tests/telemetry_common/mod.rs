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

/// A command a test started and must leave ended: as this goes out of
/// scope, however the test ends, the command is killed if it still runs and
/// then reaped, so a test that panicked part way through leaves neither this
/// command running nor this command unreaped. Only this command, the direct
/// child: what it forked of its own, a shell's `sleep` say, is left to end in
/// its own time, and killing the direct child does not reach it.
pub struct Child(std::process::Child);

impl Child {
    /// Wrap `child`, which this then takes responsibility for ending.
    pub fn new(child: std::process::Child) -> Self {
        Self(child)
    }
}

impl std::ops::Deref for Child {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // What a test that reaped the command itself leaves here is one
        // `try_wait` and one `wait`, both of which answer at once.
        if self.0.try_wait().unwrap_or(None).is_none() {
            // Not `signal`, which asserts: a panic here, where a test may
            // already be unwinding, aborts the binary instead of failing the
            // test. std's own `kill` answers at once, without signalling,
            // once std has been given the exit status of a wait, so this
            // never signals a pid the kernel has since handed to another
            // process.
            // `scripts/controls/telemetry-child-killed-on-drop` switches
            // this kill off; `telemetry-child-reaped-on-drop` switches the
            // wait below off.
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

/// How long [`wait_for_gate`] waits for a run to reach its gate, and with
/// it each `atomic gates list` it runs: far more than a run of one model
/// call takes, however loaded the machine. No test sits through it: the ones
/// that test the wait itself pass a limit of their own to
/// [`wait_for_gate_by`].
pub const GATE_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);

/// The lines a run has written to stderr, as its reader read them: each
/// with its line end, and the whitespace before it, off the end. The channel
/// hands each line out once, so every line read is kept in `seen`: a test
/// that waits for one can still be told all the run wrote, whether or not an
/// earlier wait took it first.
struct Lines {
    unread: std::sync::mpsc::Receiver<String>,
    seen: Vec<String>,
}

impl Lines {
    /// Move every line read so far into `seen`, without waiting for more.
    fn drain(&mut self) {
        while let Ok(line) = self.unread.try_recv() {
            self.seen.push(line);
        }
    }

    /// Every line read so far, whether an earlier call looked at it or not.
    fn so_far(&mut self) -> Vec<String> {
        self.drain();
        self.seen.clone()
    }

    /// Whether a line read so far holds `needle`. A line the run is still
    /// writing is not counted until its end arrives.
    fn holds(&mut self, needle: &str) -> bool {
        self.drain();
        self.seen.iter().any(|l| l.contains(needle))
    }

    /// Wait until a line holds `needle`, panicking with every line read so
    /// far once `limit` has passed, or as soon as the run's stderr closes
    /// without it. A line that came before this call counts: the run may
    /// have said it already.
    fn wait_for(&mut self, needle: &str, limit: std::time::Duration) {
        let by = std::time::Instant::now() + limit;
        loop {
            if self.holds(needle) {
                return;
            }
            match self
                .unread
                .recv_timeout(std::time::Duration::from_millis(50))
            {
                // Nothing new yet: the line the run is writing now may be
                // the one.
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                // The run's last word.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(line) => self.seen.push(line),
            }
            if std::time::Instant::now() >= by {
                break;
            }
        }
        let seen = self.so_far();
        panic!(
            "no stderr line holds {needle:?} within {limit:?}; the run had written:\n{}",
            seen.join("\n"),
        );
    }
}

/// What a run started with piped output printed, read as it goes so the
/// run never blocks on a full pipe. Each stderr line it reads goes to a
/// channel too, so a test can act on what the run says while it still runs.
pub struct Drained {
    stdout: std::thread::JoinHandle<Vec<u8>>,
    stderr: std::thread::JoinHandle<Vec<u8>>,
    lines: Lines,
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
        let (sent, unread) = std::sync::mpsc::channel();
        let read_stderr = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut all = Vec::new();
                let Some(mut pipe) = pipe else { return all };
                let mut partial: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 4096];
                // A read error ends the lines as surely as the end of the
                // pipe; either way what was read is all there is.
                while let Ok(read) = pipe.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    all.extend_from_slice(&chunk[..read]);
                    for byte in &chunk[..read] {
                        if *byte != b'\n' {
                            partial.push(*byte);
                            continue;
                        }
                        let line = String::from_utf8_lossy(&partial).trim_end().to_owned();
                        partial.clear();
                        let _ = sent.send(line);
                    }
                }
                // What the run wrote last, with no line end on it, is a
                // line too, and is handed out the same way as one with.
                if !partial.is_empty() {
                    let _ = sent.send(String::from_utf8_lossy(&partial).trim_end().to_owned());
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
            stderr: read_stderr(stderr),
            lines: Lines {
                unread,
                seen: Vec::new(),
            },
        }
    }

    /// Wait for `child` to end and return how it ended and all it printed:
    /// the readers go on to the end of the pipes, so these bytes are the
    /// run's whole output. A test that has to act on a line the run writes
    /// before it ends uses [`Drained::wait_for_stderr`].
    pub fn wait(self, child: &mut std::process::Child) -> Output {
        let status = child.wait().unwrap();
        let Self {
            stdout,
            stderr,
            lines,
        } = self;
        let stdout = stdout.join().unwrap();
        let stderr = stderr.join().unwrap();
        // Dropped last: the readers send here until the pipes close, and a
        // channel with no receiver on the other side loses their lines.
        drop(lines);
        Output {
            status,
            stdout,
            stderr,
        }
    }

    /// Block until one of the run's stderr lines holds `needle`, or fail the
    /// test with every line the run had written once `limit` passes. A line
    /// it wrote before this call counts: the run may have said it already.
    /// Once the run has ended, its whole stderr is in the bytes
    /// [`Drained::wait`] returns, and that is what to test against.
    pub fn wait_for_stderr(&mut self, needle: &str, limit: std::time::Duration) {
        self.lines.wait_for(needle, limit);
    }
}

/// How long one `atomic gates list` inside [`wait_for_gate_by`] is waited for
/// before it is killed and tried again: far more than that command takes,
/// however loaded the machine, and far less than [`GATE_LIMIT`], so a CLI
/// stuck on a lock costs one poll rather than the whole wait. A `limit`
/// shorter than this bounds the poll instead, down to what the wait has left
/// of its own time — which is why a poll only starts with at least
/// [`POLL_GRACE`] of that left.
const POLL_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// How much of the wait's own time a poll must have left to be started at
/// all: what bounds a poll that then runs past it is this, not the deadline,
/// so a poll is killed for not answering rather than for the wait running out
/// mid-poll. It is far more than `atomic gates list` takes and far less than
/// [`POLL_LIMIT`].
const POLL_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Wait until the run `child` runs in `dir`'s project waits at a gate, as
/// `atomic gates list` shows, reading its output meanwhile, for no longer
/// than [`GATE_LIMIT`]: [`wait_for_gate_by`] with that limit, which is what
/// every test that only needs a run at its gate calls.
pub fn wait_for_gate(dir: &Path, model: &str, child: &mut Child) -> Drained {
    wait_for_gate_by(dir, model, child, GATE_LIMIT)
}

/// [`wait_for_gate`]'s wait with its bound given: a run that ends first, or
/// that has not reached its gate within `limit`, fails the test with how it
/// ended and what it printed; a run still running then is killed first. A
/// test of the wait's own bounds passes a limit of seconds, so that it sits
/// through seconds rather than through [`GATE_LIMIT`].
///
/// Each poll is spawned and waited for with `try_wait`, under a
/// [`POLL_LIMIT`] of its own that never runs past `limit`: one that stops
/// answering is killed, reaped and tried again, and none starts once the
/// deadline has passed. The message that then fails the test counts how many
/// polls ran and how many of them had to be killed, and quotes what the last
/// poll that answered said with how long that one took, or, if none
/// answered, that none did. None of it quotes a killed poll, whose output
/// stops where the kill landed and would hide the last real answer.
///
/// `a_wait_for_a_gate_that_opens_by_its_limit_counts_no_killed_poll` runs
/// this with a limit of seconds, longer than one poll is given, so that it
/// reaches its deadline in seconds: it holds the count to what happened — more
/// than one poll, none of them killed — and reads back as an answer what the
/// message quotes. With the guard there switched off, the wait starts one more
/// poll, one it has no time left to wait for, and that poll is killed where it
/// stands; the test fails on the count.
/// `a_wait_for_a_gate_that_never_opens_says_how_it_was_waited_for` holds
/// what the message quotes, and how long it says that took, to what a run
/// that ends before its gate really did.
///
/// [`POLL_LIMIT`] and [`GATE_LIMIT`] have no row under `scripts/controls/`:
/// a row needs a test that fails with the control off, and nothing in the
/// repository makes `atomic gates list` stop answering on its own, while no
/// test runs to [`GATE_LIMIT`]. Both are measured by hand. With the poll
/// replaced by a command that never ends, the wait kills it, counts it and
/// tries again, and with the poll's bound removed the same wait does not come
/// back at all. A poll that had to be killed is kept out of what the message
/// quotes because such a poll holds only what it got out before the kill,
/// which for a command that never answered is nothing.
pub(crate) fn wait_for_gate_by(
    dir: &Path,
    model: &str,
    child: &mut Child,
    limit: std::time::Duration,
) -> Drained {
    let drained = Drained::of(child);
    let began = std::time::Instant::now();
    let deadline = began + limit;
    let mut polls = 0usize;
    let mut killed = 0usize;
    // What the last poll that answered said, and how long that one took. A
    // poll killed for not answering is kept out of here on purpose: what it
    // printed is what it had got out before the kill, usually nothing.
    let mut answered: Option<(Output, std::time::Duration)> = None;
    loop {
        // A poll starts only while the wait has a real budget left to wait
        // for one: a poll spawned with nothing left has no time to answer, is
        // killed where it stands, and what it had printed by then is not the
        // last thing the command said. Weighing the grace rather than the bare
        // deadline is also what leaves a poll killed only for not answering: a
        // poll started with [`POLL_GRACE`] left is bounded by that, not cut
        // short where the deadline happens to fall. The deadline is weighed
        // again below, so the wait ends there whether a poll ran or not.
        // `scripts/controls/telemetry-poll-not-started-late` switches this
        // off, and the wait then leaves one poll killed at birth, which
        // `a_wait_for_a_gate_that_opens_by_its_limit_counts_no_killed_poll`
        // fails on.
        if std::time::Instant::now() + POLL_GRACE < deadline {
            let started = std::time::Instant::now();
            let mut poll = Child::new(
                command(dir, model, &["gates", "list", "--json"], &[])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            // The poll's pipes are read as they fill, as the run's are, so a
            // poll is never held up by output nobody is taking either.
            let printed = Drained::of(&mut poll);
            let by = started
                + POLL_LIMIT.min(deadline.saturating_duration_since(std::time::Instant::now()));
            while poll.try_wait().unwrap().is_none() && std::time::Instant::now() < by {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let took = started.elapsed();
            polls += 1;
            let stuck = poll.try_wait().unwrap().is_none();
            if stuck {
                // Not going to answer: killed and reaped here, and tried
                // again next time round.
                signal(&poll, "KILL");
                killed += 1;
            }
            let listed = printed.wait(&mut poll);
            // What a poll that answered said, and how long it took, kept
            // apart from the count of killed ones: a killed poll's output
            // stops where the kill landed, so it is never what the wait
            // goes on to quote.
            if !stuck {
                answered = Some((listed, took));
            }
            let open: Value = answered
                .as_ref()
                .map(|(listed, _)| serde_json::from_slice(&listed.stdout).unwrap_or(json!([])))
                .unwrap_or(json!([]));
            if open.as_array().is_some_and(|g| !g.is_empty()) {
                return drained;
            }
        }
        let ended = child.try_wait().unwrap();
        // The last thing the wait weighed before it gave up: its own bound,
        // and what a poll still running was given, are said apart because a
        // poll's own bound can take the wait past its bound by that much.
        let past = std::time::Instant::now();
        if ended.is_some() || past >= deadline {
            let why = match ended {
                Some(_) => "the run ended before its gate".to_owned(),
                None => {
                    signal(child, "KILL");
                    // A wait whose bound is shorter than what it gives one of
                    // its polls ends on that poll's own bound, so it says how
                    // long it sat rather than restating a bound it did not
                    // keep to the millisecond.
                    let sat = past.duration_since(began);
                    let poll_gave = if limit < POLL_GRACE {
                        format!(", and {} ms for the poll it had started", sat.as_millis())
                    } else {
                        String::new()
                    };
                    format!("no gate opened in {limit:?}{poll_gave}, so the run was killed")
                }
            };
            let out = drained.wait(child);
            panic!(
                "{why}; it ended {}: {}\n{}",
                out.status,
                text(&out),
                last_poll(&answered, polls, killed),
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// How the wait for a gate reports its polls: the last thing a poll actually
/// answered and how long that one took, or that none answered, and separately
/// how many polls it made and how many it had to kill.
fn last_poll(
    answered: &Option<(Output, std::time::Duration)>,
    polls: usize,
    killed: usize,
) -> String {
    let counted = format!(
        "({polls} poll(s), {}{})",
        match answered {
            Some((_, took)) => format!("the last took {} ms", took.as_millis()),
            None => "none answered".to_owned(),
        },
        if killed == 0 {
            String::new()
        } else {
            format!(", {killed} killed")
        },
    );
    match answered {
        Some((listed, _)) => format!("`atomic gates list` last said: {} {counted}", text(listed)),
        None => format!("no `atomic gates list` answered: {counted}"),
    }
}

/// The message `call` failed with; when it did not fail, or failed with
/// something that is no message, this test fails rather than comparing
/// nothing: what a test that bounds a wait has to check.
pub(crate) fn failed_wait(call: impl FnOnce()) -> String {
    let why = std::panic::catch_unwind(std::panic::AssertUnwindSafe(call))
        .expect_err("the wait did not fail, so it gave no message to compare");
    why.downcast_ref::<String>()
        .cloned()
        .expect("the wait failed with no message of its own")
}
