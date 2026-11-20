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
//! A recording holds the request body and the response, never a header: the
//! API key is not stored. Request bodies hold whatever the atom was asked,
//! eval inputs included, so a cassette is a fixture to review like any other
//! test data. (Redaction of secrets in bodies is not applied yet.)

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
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

/// One recording.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recording {
    /// The endpoint the request was meant for.
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
}

/// A running cassette. It stops when dropped.
pub struct Cassette {
    port: u16,
    tally: Arc<Mutex<Tally>>,
    task: tokio::task::JoinHandle<()>,
}

/// The identity of a request for `endpoint` at `path` with `body`: what its
/// recording is keyed by.
pub fn identity(endpoint: &str, path: &str, body: &Value) -> (String, Value) {
    let mut request = canonical(body);
    if let Value::Object(fields) = &mut request {
        for field in VOLATILE {
            fields.remove(*field);
        }
    }
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

struct Shared {
    dir: PathBuf,
    mode: Mode,
    /// The real endpoint's base URL, such as `https://api.example/v1`.
    endpoint: String,
    client: reqwest::Client,
    tally: Arc<Mutex<Tally>>,
}

impl Cassette {
    /// Serve recordings in `dir` for calls meant for `endpoint`, on a free
    /// loopback port.
    pub async fn start(dir: &Path, endpoint: &str, mode: Mode) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let tally = Arc::new(Mutex::new(Tally::default()));
        let shared = Arc::new(Shared {
            dir: dir.to_owned(),
            mode,
            endpoint: endpoint.trim_end_matches('/').to_owned(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .map_err(std::io::Error::other)?,
            tally: tally.clone(),
        });
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _ = serve(&mut socket, &shared).await;
                });
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
    loop {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if socket.read(&mut byte).await? == 0 {
                return Ok(());
            }
            head.push(byte[0]);
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
    if method != "POST" {
        return failed(
            "404 Not Found",
            format!("the cassette serves model calls, not {method} {path}"),
        );
    }
    let request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let (key, normalized) = identity(&shared.endpoint, path, &request);
    let file = shared.dir.join(format!("{key}.json"));
    let count = |f: &dyn Fn(&mut Tally)| {
        if let Ok(mut t) = shared.tally.lock() {
            f(&mut t);
        }
    };
    match shared.mode {
        Mode::Replay => match std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str::<Recording>(&t).ok())
        {
            Some(recording) => {
                count(&|t| t.replayed += 1);
                (
                    format!("{} Recorded", recording.status),
                    recording.content_type,
                    recording.body,
                )
            }
            None => {
                count(&|t| t.missed += 1);
                failed(
                    "502 Bad Gateway",
                    format!(
                        "no recording answers this request ({key}): the prompt, schema, model or \
                         settings differ from every recorded one; run the eval with --live to \
                         record it"
                    ),
                )
            }
        },
        Mode::Record => {
            let url = format!("{}{path}", origin(&shared.endpoint));
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
                    endpoint: shared.endpoint.clone(),
                    path: path.to_owned(),
                    request: normalized,
                    status,
                    content_type: content_type.clone(),
                    body: text.clone(),
                };
                let written = std::fs::create_dir_all(&shared.dir).and_then(|()| {
                    std::fs::write(
                        &file,
                        serde_json::to_string_pretty(&recording).unwrap_or_default() + "\n",
                    )
                });
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
