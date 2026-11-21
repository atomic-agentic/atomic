// SPDX-License-Identifier: Apache-2.0

//! A scripted fake model, for tests that need a model without a provider.
//!
//! [`FakeModel`] serves an OpenAI-compatible API on a loopback port and
//! answers each model call with the next entry of its script: a text reply, a
//! tool call or an HTTP error. [`FakeModel::respond`] answers by what each
//! call asks instead, for tests whose calls come in no fixed order, such as a
//! crash suite whose killed steps run again. It speaks the streaming Responses API, which
//! drives a real Codex thread deterministically, and the non-streaming
//! Responses and Chat Completions calls a lite atom makes, and lists one
//! model, `synthetic`, at Ollama's `GET /api/tags`. It counts and
//! records every call, so durability and repair tests can assert exactly how
//! many model calls were made.

use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One scripted reply.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    /// An assistant message with this text.
    Text(String),
    /// A function call to the named tool with these arguments.
    Tool {
        /// The tool's name.
        name: String,
        /// Its arguments.
        arguments: Value,
    },
    /// An HTTP error with this status and body.
    Http {
        /// The status code.
        status: u16,
        /// The response body.
        body: String,
    },
    /// An assistant message with this text, and no usage reported.
    WithoutUsage(String),
    /// This JSON as the whole body of a 200 response, for a non-streaming
    /// call.
    Json(Value),
    /// A streamed answer that begins with this text and breaks off before
    /// it completes, as a dropped connection leaves it: no usage, and no
    /// completion. Answered only on a streaming Responses call.
    Broken(String),
    /// This reply, held until `file` exists: a test creates it to let the
    /// answer go, so it orders the call against something else it watches.
    Gated {
        /// The file whose existence releases the answer.
        file: std::path::PathBuf,
        /// The answer.
        reply: Box<Reply>,
    },
    /// This reply, sent once this long has passed.
    Delayed {
        /// How long to wait before answering.
        after: std::time::Duration,
        /// The answer.
        reply: Box<Reply>,
    },
}

/// Picks the reply to one model call from its request body.
type Responder = Arc<dyn Fn(&Value) -> Reply + Send + Sync>;

#[derive(Default)]
struct State {
    script: Vec<Reply>,
    responder: Option<Responder>,
    requests: Vec<Value>,
    paths: Vec<String>,
    headers: Vec<Vec<(String, String)>>,
    every: Vec<(String, String)>,
}

/// A running fake model. It stops when dropped.
pub struct FakeModel {
    port: u16,
    state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeModel {
    /// Start serving `script` on a free loopback port. Once the script is used
    /// up, every further request gets the text "Synthetic reply.".
    pub async fn start(script: Vec<Reply>) -> std::io::Result<Self> {
        Self::listen(State {
            script,
            ..State::default()
        })
        .await
    }

    /// Start serving on a free loopback port, answering each model call with
    /// what `responder` returns for its request body.
    pub async fn respond(
        responder: impl Fn(&Value) -> Reply + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        Self::listen(State {
            responder: Some(Arc::new(responder)),
            ..State::default()
        })
        .await
    }

    async fn listen(state: State) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let state = Arc::new(Mutex::new(state));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let state = shared.clone();
                tokio::spawn(async move {
                    let _ = serve(&mut socket, &state).await;
                });
            }
        });
        Ok(Self { port, state, task })
    }

    /// The base URL to give a client: `http://127.0.0.1:<port>/v1`.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// How many model calls (`POST …/responses` or `…/chat/completions`)
    /// were made.
    pub fn calls(&self) -> usize {
        self.state.lock().map(|s| s.requests.len()).unwrap_or(0)
    }

    /// The body of every model call, in order.
    pub fn requests(&self) -> Vec<Value> {
        self.state
            .lock()
            .map(|s| s.requests.clone())
            .unwrap_or_default()
    }

    /// Every request it received, model call or not, as method and path,
    /// in order.
    pub fn every_request(&self) -> Vec<(String, String)> {
        self.state
            .lock()
            .map(|s| s.every.clone())
            .unwrap_or_default()
    }

    /// The request headers of every model call, in order, names lowercased.
    pub fn headers(&self) -> Vec<Vec<(String, String)>> {
        self.state
            .lock()
            .map(|s| s.headers.clone())
            .unwrap_or_default()
    }

    /// The path of every model call, in order, such as `/v1/responses`.
    pub fn paths(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|s| s.paths.clone())
            .unwrap_or_default()
    }
}

impl Drop for FakeModel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Read one HTTP/1.1 request and answer it.
async fn serve(socket: &mut tokio::net::TcpStream, state: &Mutex<State>) -> std::io::Result<()> {
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
        let headers: Vec<(String, String)> = head
            .lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_owned()))
            .collect();
        let length: usize = lines
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        socket.read_exact(&mut body).await?;
        let mut parts = request_line.split_whitespace();
        let (method, path) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        let path = path.split('?').next().unwrap_or_default();
        if let Ok(mut s) = state.lock() {
            s.every.push((method.to_owned(), path.to_owned()));
        }

        let chat = path.ends_with("/chat/completions");
        let (status, content_type, text) = if method == "GET" && path == "/api/tags" {
            // What a local Ollama lists, so a test can stand it in for one.
            (
                "200 OK".to_owned(),
                "application/json",
                json!({"models": [{"name": "synthetic", "model": "synthetic"}]}).to_string(),
            )
        } else if method == "POST" && (chat || path.ends_with("/responses")) {
            let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let reply = match state.lock() {
                Ok(mut s) => {
                    let index = s.requests.len();
                    s.requests.push(request.clone());
                    s.paths.push(path.to_owned());
                    s.headers.push(headers.clone());
                    match &s.responder {
                        Some(respond) => Some(respond(&request)),
                        None => s.script.get(index).cloned(),
                    }
                }
                Err(_) => None,
            };
            let mut reply = reply.unwrap_or_else(|| Reply::Text("Synthetic reply.".into()));
            loop {
                match reply {
                    Reply::Delayed { after, reply: then } => {
                        tokio::time::sleep(after).await;
                        reply = *then;
                    }
                    Reply::Gated { file, reply: then } => {
                        while !file.exists() {
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                        reply = *then;
                    }
                    answer => {
                        reply = answer;
                        break;
                    }
                }
            }
            let streaming = request.get("stream").and_then(Value::as_bool) != Some(false);
            match &reply {
                Reply::Json(body) => ("200 OK".to_owned(), "application/json", body.to_string()),
                Reply::Http { status, body } => {
                    (format!("{status} Error"), "application/json", body.clone())
                }
                _ if chat => (
                    "200 OK".to_owned(),
                    "application/json",
                    chat_completion(&request, &reply).to_string(),
                ),
                _ if streaming => (
                    "200 OK".to_owned(),
                    "text/event-stream",
                    stream(&request, &reply),
                ),
                _ => (
                    "200 OK".to_owned(),
                    "application/json",
                    completed(&request, &reply).to_string(),
                ),
            }
        } else {
            (
                "404 Not Found".to_owned(),
                "application/json",
                "{}".to_owned(),
            )
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{text}",
            text.len()
        );
        socket.write_all(response.as_bytes()).await?;
        socket.flush().await?;
    }
}

fn model_of(request: &Value) -> &str {
    request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("synthetic")
}

/// The Responses API's output item for one reply.
fn output_item(reply: &Reply) -> Value {
    match reply {
        Reply::Text(text) | Reply::WithoutUsage(text) | Reply::Broken(text) => json!({
            "type": "message", "id": "msg_synthetic", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        }),
        Reply::Tool { name, arguments } => json!({
            "type": "function_call", "id": "fc_synthetic", "call_id": "call_synthetic",
            "name": name, "arguments": arguments.to_string(), "status": "completed"
        }),
        Reply::Http { .. } | Reply::Delayed { .. } | Reply::Gated { .. } | Reply::Json(_) => {
            Value::Null
        }
    }
}

/// A completed Responses API response for one reply, as a non-streaming call
/// returns it.
fn completed(request: &Value, reply: &Reply) -> Value {
    let mut done = json!({
        "id": "resp_synthetic", "object": "response", "status": "completed",
        "model": model_of(request), "output": [output_item(reply)],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    if let (Reply::WithoutUsage(_), Some(d)) = (reply, done.as_object_mut()) {
        d.remove("usage");
    }
    done
}

/// A Chat Completions response for one reply.
fn chat_completion(request: &Value, reply: &Reply) -> Value {
    let message = match reply {
        Reply::Tool { name, arguments } => json!({
            "role": "assistant", "content": null,
            "tool_calls": [{"id": "call_synthetic", "type": "function",
                            "function": {"name": name, "arguments": arguments.to_string()}}]
        }),
        Reply::Text(text) | Reply::WithoutUsage(text) | Reply::Broken(text) => {
            json!({"role": "assistant", "content": text})
        }
        Reply::Http { .. } | Reply::Delayed { .. } | Reply::Gated { .. } | Reply::Json(_) => {
            Value::Null
        }
    };
    let mut done = json!({
        "id": "chatcmpl_synthetic", "object": "chat.completion", "model": model_of(request),
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
    if let (Reply::WithoutUsage(_), Some(d)) = (reply, done.as_object_mut()) {
        d.remove("usage");
    }
    done
}

/// The Responses API's server-sent events for one reply.
fn stream(request: &Value, reply: &Reply) -> String {
    if let Reply::Broken(text) = reply {
        // Begun, with some of its text, and never completed.
        let mut response = completed(request, reply);
        response["status"] = json!("in_progress");
        response["output"] = json!([]);
        if let Some(r) = response.as_object_mut() {
            r.remove("usage");
        }
        let item = json!({"type": "message", "id": "msg_synthetic", "role": "assistant",
                          "status": "in_progress", "content": []});
        return [
            (
                "response.created",
                json!({"type": "response.created", "response": response}),
            ),
            (
                "response.output_item.added",
                json!({"type": "response.output_item.added", "output_index": 0, "item": item}),
            ),
            (
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "item_id": "msg_synthetic",
                       "output_index": 0, "content_index": 0, "delta": text}),
            ),
        ]
        .iter()
        .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
        .collect();
    }
    let item = output_item(reply);
    let done = completed(request, reply);
    let mut response = done.clone();
    response["status"] = json!("in_progress");
    response["output"] = json!([]);
    if let Some(r) = response.as_object_mut() {
        r.remove("usage");
    }
    let events = [
        (
            "response.created",
            json!({"type": "response.created", "response": response}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 0, "item": item}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": done}),
        ),
    ];
    events
        .iter()
        .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
        .collect()
}

/// Run `f` on a runtime like the one Codex's entry point builds: worker
/// threads with 16 MiB stacks, which Codex's turn needs.
pub fn on_codex_runtime<F: std::future::Future<Output = ()> + Send + 'static>(f: F) {
    const STACK: usize = 16 * 1024 * 1024;
    let result = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(STACK)
                .enable_all()
                .build()
                .map(|rt| rt.block_on(f))
        })
        .map(|t| t.join());
    match result {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => panic!("could not build the runtime: {e}"),
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(e) => panic!("could not start the thread: {e}"),
    }
}

/// `backend` with nothing denied to its workspaces' commands, for tests whose
/// Codex turns run without Codex's Linux sandbox helper
/// ([`crate::codex::CodexPaths::without_helpers`]). Atomic refuses such a
/// turn on Linux when its workspace has paths to deny, the backend's scratch
/// directories, since only the helper can deny them. A test that uses this
/// gives that denial up, and its turns' commands may read other attempts'
/// scratch directories; the tests that run turns with the helper show the
/// denial holds. This module is built only for tests (the `fake-model`
/// feature, which only dev-dependencies enable), so no atomic binary can
/// reach it.
pub fn denying_nothing(backend: atomic_exec::Backend) -> atomic_exec::Backend {
    atomic_exec::Backend::connect(DenyingNothing(backend))
        .expect("a connected backend states what it stated before")
}

/// See [`denying_nothing`].
struct DenyingNothing(atomic_exec::Backend);

impl atomic_exec::ExecutionBackend for DenyingNothing {
    fn connect(&self) -> Result<atomic_exec::Statement, atomic_exec::ExecError> {
        Ok(self.0.statement())
    }

    fn open(
        &self,
        request: &atomic_exec::Open,
    ) -> Result<atomic_exec::Workspace, atomic_exec::ExecError> {
        let mut workspace = self.0.shared().open(request)?;
        workspace.denied.clear();
        Ok(workspace)
    }

    fn run(
        &self,
        command: &atomic_exec::Command,
    ) -> Result<atomic_exec::Completed, atomic_exec::ExecError> {
        self.0.shared().run(command)
    }

    fn close(&self, workspace: &atomic_exec::Workspace) -> Result<(), atomic_exec::ExecError> {
        self.0.shared().close(workspace)
    }
}

/// Unpadded base64url, for a test token.
pub fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, b| (n << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

/// An unsigned ChatGPT id token, as a test issuer would hand out, for
/// `email` on a Plus plan in workspace `account`.
pub fn fake_id_token(email: &str, account: &str) -> String {
    format!(
        "{}.{}.sig",
        b64url(br#"{"alg":"none","typ":"JWT"}"#),
        b64url(
            serde_json::json!({
                "email": email,
                "https://api.openai.com/auth": {
                    "chatgpt_plan_type": "plus",
                    "chatgpt_account_id": account,
                    "chatgpt_user_id": "user-1",
                },
            })
            .to_string()
            .as_bytes()
        )
    )
}
