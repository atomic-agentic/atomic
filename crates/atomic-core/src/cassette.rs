// SPDX-License-Identifier: Apache-2.0

//! Eval cassettes: model responses recorded under the request that produced
//! them, so evals can run again offline and deterministically.
//!
//! A [`Cassette`] is a local HTTP endpoint that stands in front of the model
//! endpoint. Atoms are pointed at it as at any OpenAI-compatible endpoint.
//! For each model call it computes the call's **identity**: the endpoint it
//! is meant for, the request path and the request body as sent, which holds
//! the compiled messages and normalized input, the generation schema, the
//! model and its settings and any tool definitions. A few fields that change
//! from run to run without changing what is asked (such as a prompt-cache
//! key) are left out. The identity's SHA-256 names the recording.
//!
//! In [`Mode::Replay`] a call is answered from its recording, with no model
//! call; a call with no recording fails, saying how to record one. Anything
//! in the identity that changed, such as the instructions, the schema or the
//! model, therefore misses: a recording only answers the request it was made
//! for. In [`Mode::Record`] every call goes to the real endpoint and its
//! answer is recorded, replacing any recording of the same identity.
//!
//! The same request made more than once in one session, as two cases with
//! the same input make it, is recorded once per occurrence (`<key>.json`,
//! then `<key>.2.json` and so on), and replayed in the same order.
//!
//! A recording holds the request body and the response, never a header: the
//! API key is not stored, and the endpoint it names has no credentials or
//! query (a cassette refuses to stand in front of one that has). Request
//! bodies hold whatever the atom was asked, eval inputs included, so a
//! cassette is a fixture to review like any other test data. (Redaction of
//! secrets in bodies is not applied yet.)
//!
//! Recordings are written whole, through a temporary file renamed into
//! place, and never through a symbolic link: the cassette directory and every
//! directory between it and the project root must be real directories.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How a cassette answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// From recordings only: no model call is made.
    Replay,
    /// From the real endpoint, recording each answer.
    Record,
}

/// Fields of a request body that differ between runs without changing what
/// is asked, left out of its identity.
const VOLATILE: &[&str] = &["prompt_cache_key", "metadata", "user", "client_metadata"];

/// The largest request head and body the cassette reads.
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 32 * 1024 * 1024;

/// One recording.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recording {
    /// The endpoint the request was meant for, normalized.
    pub endpoint: String,
    /// The request path, such as `/v1/chat/completions`.
    pub path: String,
    /// The request body, as it entered the identity.
    pub request: Value,
    /// The response's HTTP status.
    pub status: u16,
    /// The response's content type.
    pub content_type: String,
    /// The response body.
    pub body: String,
}

/// What a cassette did, for a report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Calls answered from a recording.
    pub replayed: usize,
    /// Calls sent to the endpoint and recorded.
    pub recorded: usize,
    /// Calls with no recording, in replay.
    pub missed: usize,
    /// Calls whose recording could not be read, or did not answer them.
    pub corrupt: usize,
    /// Calls that arrived with an `Authorization` header, in replay, where
    /// no key is needed.
    pub keyed: usize,
}

/// Why a cassette did not start.
#[derive(Debug, thiserror::Error)]
pub enum CassetteError {
    /// The endpoint cannot be recorded against.
    #[error("the cassette cannot stand in front of {0}")]
    Endpoint(String),
    /// The directory is not one the cassette may write to or read from.
    #[error("the cassette directory {0}")]
    Directory(String),
    /// The loopback listener did not start.
    #[error("start the cassette: {0}")]
    Io(#[from] std::io::Error),
}

/// A running cassette. It stops, with every connection it is serving, when
/// dropped.
pub struct Cassette {
    port: u16,
    tally: Arc<Mutex<Tally>>,
    task: tokio::task::JoinHandle<()>,
}

/// `endpoint` as recordings name it: scheme and host in lower case, the
/// default port left out, no trailing slash. An endpoint with credentials,
/// a query or a fragment in its URL is refused: they would be recorded, and
/// they are not part of where a request goes.
pub fn normalize(endpoint: &str) -> Result<String, CassetteError> {
    let refuse = |why: &str| CassetteError::Endpoint(format!("this endpoint: {why}"));
    let url = reqwest::Url::parse(endpoint).map_err(|e| refuse(&format!("not a URL ({e})")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(refuse("only http and https endpoints are recorded"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(refuse(
            "its URL holds credentials, which a recording would keep",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(refuse("its URL has a query or a fragment"));
    }
    let host = url.host_str().ok_or_else(|| refuse("it names no host"))?;
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    Ok(format!(
        "{}://{}{port}{}",
        url.scheme(),
        host.to_ascii_lowercase(),
        url.path().trim_end_matches('/')
    ))
}

/// The identity of a request for `endpoint` (normalized, as [`normalize`]
/// gives it) at `path` with `body`: what its recording is keyed by.
pub fn identity(endpoint: &str, path: &str, body: &Value) -> (String, Value) {
    // Left out before the keys are put in order: removing a key from an
    // ordered map can move another into its place.
    let request = match body {
        Value::Object(fields) => canonical(&Value::Object(
            fields
                .iter()
                .filter(|(k, _)| !VOLATILE.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )),
        other => canonical(other),
    };
    let mut hash = Sha256::new();
    hash.update(endpoint.trim_end_matches('/').as_bytes());
    hash.update([0]);
    hash.update(path.as_bytes());
    hash.update([0]);
    hash.update(request.to_string().as_bytes());
    (format!("{:x}", hash.finalize()), request)
}

/// `value` with every object's keys in order, so equal requests hash alike.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut keys: Vec<&String> = fields.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                out.insert(key.clone(), canonical(&fields[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// Check that `dir` and every directory between `root` and it is a real
/// directory, not a link; in record mode, create what is missing.
fn real_directory(root: &Path, dir: &Path, create: bool) -> Result<(), CassetteError> {
    let fail = |m: String| CassetteError::Directory(m);
    let root = std::fs::canonicalize(root).map_err(|e| fail(format!("{}: {e}", root.display())))?;
    let relative = dir.strip_prefix(&root).map_err(|_| {
        fail(format!(
            "{} is outside the project at {}",
            dir.display(),
            root.display()
        ))
    })?;
    let mut at = root.clone();
    for part in relative.components() {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => {
                return Err(fail(format!(
                    "{}: {} is a link or a file, not a directory",
                    dir.display(),
                    at.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                std::fs::create_dir(&at).map_err(|e| fail(format!("{}: {e}", at.display())))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(fail(format!("{}: {e}", at.display()))),
        }
    }
    Ok(())
}

struct Shared {
    dir: PathBuf,
    mode: Mode,
    /// The real endpoint's base URL, normalized.
    endpoint: String,
    client: reqwest::Client,
    tally: Arc<Mutex<Tally>>,
    /// How many times each identity has been asked for in this session.
    seen: Mutex<HashMap<String, usize>>,
}

impl Cassette {
    /// Serve recordings in `dir`, a directory under the project at `root`,
    /// for calls meant for `endpoint`, on a free loopback port.
    pub async fn start(
        dir: &Path,
        root: &Path,
        endpoint: &str,
        mode: Mode,
    ) -> Result<Self, CassetteError> {
        let endpoint = normalize(endpoint)?;
        let dir = match std::fs::canonicalize(dir) {
            Ok(real) if real == dir => real,
            Ok(_) => {
                return Err(CassetteError::Directory(format!(
                    "{} is reached through a link",
                    dir.display()
                )));
            }
            Err(_) => dir.to_owned(),
        };
        real_directory(root, &dir, mode == Mode::Record)?;
        if mode == Mode::Record {
            remove_stale(&dir);
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let tally = Arc::new(Mutex::new(Tally::default()));
        let shared = Arc::new(Shared {
            dir,
            mode,
            endpoint,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .map_err(std::io::Error::other)?,
            tally: tally.clone(),
            seen: Mutex::new(HashMap::new()),
        });
        let task = tokio::spawn(async move {
            // Dropped with the task, which aborts every connection's.
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((mut socket, _)) = listener.accept().await {
                let shared = shared.clone();
                connections.spawn(async move {
                    let _ = serve(&mut socket, &shared).await;
                });
                while connections.try_join_next().is_some() {}
            }
        });
        Ok(Self { port, tally, task })
    }

    /// The base URL to give atoms in place of the real endpoint's: the real
    /// one's path, on this cassette.
    pub fn base_url(&self, endpoint: &str) -> String {
        let path = endpoint
            .split_once("://")
            .and_then(|(_, rest)| rest.split_once('/'))
            .map(|(_, path)| format!("/{path}"))
            .unwrap_or_default();
        format!(
            "http://127.0.0.1:{}{}",
            self.port,
            path.trim_end_matches('/')
        )
    }

    /// What it has done so far.
    pub fn tally(&self) -> Tally {
        self.tally.lock().map(|t| t.clone()).unwrap_or_default()
    }
}

impl Drop for Cassette {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The endpoint's origin, `scheme://host[:port]`.
fn origin(endpoint: &str) -> &str {
    match endpoint.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split('/').next().unwrap_or_default();
            &endpoint[..scheme.len() + 3 + host.len()]
        }
        None => endpoint,
    }
}

async fn serve(socket: &mut tokio::net::TcpStream, shared: &Shared) -> std::io::Result<()> {
    let refuse = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned());
    loop {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if socket.read(&mut byte).await? == 0 {
                return Ok(());
            }
            head.push(byte[0]);
            if head.len() > MAX_HEAD {
                return Err(refuse("request head too large"));
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_owned();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
            .collect();
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        let length: usize = header("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if length > MAX_BODY {
            return Err(refuse("request body too large"));
        }
        let mut body = vec![0u8; length];
        socket.read_exact(&mut body).await?;
        let mut parts = request_line.split_whitespace();
        let (method, path) = (
            parts.next().unwrap_or_default().to_owned(),
            parts.next().unwrap_or_default().to_owned(),
        );
        let (status, content_type, text) =
            answer(shared, &method, &path, &body, header("authorization")).await;
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{text}",
            text.len()
        );
        socket.write_all(response.as_bytes()).await?;
        socket.flush().await?;
    }
}

/// The file the `n`th occurrence of identity `key` is recorded in.
fn file_for(dir: &Path, key: &str, n: usize) -> PathBuf {
    if n <= 1 {
        dir.join(format!("{key}.json"))
    } else {
        dir.join(format!("{key}.{n}.json"))
    }
}

/// Whether `name` is one of the cassette's own temporary files,
/// `.<recording>.<sixteen hex digits>.tmp`, as [`write_whole`] names them.
fn is_temporary(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".tmp")) else {
        return false;
    };
    rest.rsplit_once('.').is_some_and(|(recording, suffix)| {
        recording.ends_with(".json")
            && suffix.len() == 16
            && suffix.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// Remove the temporary files a recording cut short left in `dir`: a
/// session that records owns the directory, so none of them is live.
fn remove_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_temporary(&name) && entry.file_type().is_ok_and(|t| t.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Write `text` to `file` whole: through a temporary file beside it, renamed
/// over it, so a reader never sees half of it, and never through a link.
fn write_whole(file: &Path, text: &str) -> std::io::Result<()> {
    if std::fs::symlink_metadata(file).is_ok_and(|m| !m.file_type().is_file()) {
        return Err(std::io::Error::other("it is a link or not a file"));
    }
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("recording");
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).map_err(|e| std::io::Error::other(e.to_string()))?;
    let suffix: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
    let temporary = file.with_file_name(format!(".{name}.{suffix}.tmp"));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(text.as_bytes())?;
            f.sync_all()
        })
        .and_then(|()| std::fs::rename(&temporary, file));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

async fn answer(
    shared: &Shared,
    method: &str,
    path: &str,
    body: &[u8],
    authorization: Option<String>,
) -> (String, String, String) {
    let failed = |status: &str, message: String| {
        (
            status.to_owned(),
            "application/json".to_owned(),
            serde_json::json!({"error": {"message": message, "type": "cassette"}}).to_string(),
        )
    };
    let count = |f: &dyn Fn(&mut Tally)| {
        if let Ok(mut t) = shared.tally.lock() {
            f(&mut t);
        }
    };
    if method != "POST" || !path.starts_with('/') {
        return failed(
            "404 Not Found",
            format!("the cassette serves model calls, not {method} {path}"),
        );
    }
    let Ok(request) = serde_json::from_slice::<Value>(body) else {
        return failed(
            "400 Bad Request",
            "the cassette records JSON requests, and this body is not JSON".into(),
        );
    };
    let endpoint = &shared.endpoint;
    let (key, normalized) = identity(endpoint, path, &request);
    let occurrence = shared
        .seen
        .lock()
        .map(|mut seen| {
            let n = seen.entry(key.clone()).or_default();
            *n += 1;
            *n
        })
        .unwrap_or(1);
    let file = file_for(&shared.dir, &key, occurrence);
    let which = if occurrence > 1 {
        format!("{key}, occurrence {occurrence}")
    } else {
        key.clone()
    };
    match shared.mode {
        Mode::Replay => {
            if authorization.is_some() {
                count(&|t| t.keyed += 1);
            }
            let corrupt = |why: String| {
                count(&|t| t.corrupt += 1);
                failed(
                    "502 Bad Gateway",
                    format!(
                        "the recording {} is corrupt: {why}; record it again with --live",
                        file.display()
                    ),
                )
            };
            let text = match std::fs::symlink_metadata(&file) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    count(&|t| t.missed += 1);
                    return failed(
                        "502 Bad Gateway",
                        format!(
                            "no recording answers this request to {endpoint} ({which}): the \
                             prompt, schema, model or settings differ from every recorded one; \
                             run the eval with --live to record it"
                        ),
                    );
                }
                Ok(m) if !m.file_type().is_file() => {
                    return corrupt("it is a link or not a file".into());
                }
                _ => match std::fs::read_to_string(&file) {
                    Ok(text) => text,
                    Err(e) => return corrupt(e.to_string()),
                },
            };
            let recording = match serde_json::from_str::<Recording>(&text) {
                Ok(r) => r,
                Err(e) => return corrupt(format!("it is not a whole recording ({e})")),
            };
            if recording.endpoint != *endpoint
                || recording.path != path
                || recording.request != normalized
            {
                return corrupt(format!(
                    "it records a request to {}{}, not this one",
                    recording.endpoint, recording.path
                ));
            }
            count(&|t| t.replayed += 1);
            (
                format!("{} Recorded", recording.status),
                recording.content_type,
                recording.body,
            )
        }
        Mode::Record => {
            let url = format!("{}{path}", origin(endpoint));
            let mut forward = shared
                .client
                .post(&url)
                .header("content-type", "application/json")
                .body(body.to_vec());
            if let Some(authorization) = authorization {
                forward = forward.header("authorization", authorization);
            }
            let response = match forward.send().await {
                Ok(r) => r,
                Err(e) => return failed("502 Bad Gateway", format!("POST {url}: {e}")),
            };
            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_owned();
            let text = match response.text().await {
                Ok(t) => t,
                Err(e) => return failed("502 Bad Gateway", format!("POST {url}: {e}")),
            };
            // Only a successful answer is worth replaying.
            if (200..300).contains(&status) {
                let recording = Recording {
                    endpoint: endpoint.clone(),
                    path: path.to_owned(),
                    request: normalized,
                    status,
                    content_type: content_type.clone(),
                    body: text.clone(),
                };
                let written = write_whole(
                    &file,
                    &(serde_json::to_string_pretty(&recording).unwrap_or_default() + "\n"),
                );
                if let Err(e) = written {
                    return failed(
                        "500 Internal Server Error",
                        format!("record {}: {e}", file.display()),
                    );
                }
                count(&|t| t.recorded += 1);
            }
            (format!("{status} Forwarded"), content_type, text)
        }
    }
}

#[cfg(test)]
mod tests;
