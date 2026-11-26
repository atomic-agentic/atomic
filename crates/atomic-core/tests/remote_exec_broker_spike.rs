// SPDX-License-Identifier: Apache-2.0
//! Pinned Codex remote execution seam proof.
//!
//! This spike does not enable remote environments or parallel coding lanes in
//! production.

#[cfg(target_os = "linux")]
#[allow(clippy::result_large_err)] // The WebSocket handshake callback fixes this response type.
fn authorize_relay(
    request: &tokio_tungstenite::tungstenite::handshake::server::Request,
    response: tokio_tungstenite::tungstenite::handshake::server::Response,
) -> Result<
    tokio_tungstenite::tungstenite::handshake::server::Response,
    tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
> {
    if request
        .headers()
        .get("authorization")
        .is_some_and(|value| value == "Bearer test-broker-token")
    {
        Ok(response)
    } else {
        Err(tokio_tungstenite::tungstenite::http::Response::builder()
            .status(401)
            .body(Some("unauthorized".to_owned()))
            .expect("unauthorized response"))
    }
}

#[cfg(target_os = "linux")]
struct TestPooledBackend {
    inner: atomic_exec::LocalBackend,
    permits: std::sync::Arc<tokio::sync::Semaphore>,
    runtime: tokio::runtime::Handle,
    attempted: std::sync::Arc<std::sync::atomic::AtomicBool>,
    admitted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(target_os = "linux")]
impl atomic_exec::ExecutionBackend for TestPooledBackend {
    fn connect(&self) -> Result<atomic_exec::Statement, atomic_exec::ExecError> {
        self.inner.connect()
    }

    fn open(
        &self,
        request: &atomic_exec::Open,
    ) -> Result<atomic_exec::Workspace, atomic_exec::ExecError> {
        self.inner.open(request)
    }

    fn run(
        &self,
        command: &atomic_exec::Command,
    ) -> Result<atomic_exec::Completed, atomic_exec::ExecError> {
        use std::sync::atomic::Ordering;
        self.attempted.store(true, Ordering::SeqCst);
        let _slot = self
            .runtime
            .block_on(std::sync::Arc::clone(&self.permits).acquire_owned())
            .expect("test host permit remains open");
        self.admitted.store(true, Ordering::SeqCst);
        self.inner.run(command)
    }

    fn close(&self, workspace: &atomic_exec::Workspace) -> Result<(), atomic_exec::ExecError> {
        self.inner.close(workspace)
    }
}

#[cfg(target_os = "linux")]
fn persist_journal(path: &std::path::Path, session_id: &str, process_id: &str) {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path).expect("create broker journal");
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({
            "sessionId": session_id,
            "processId": process_id,
        }),
    )
    .expect("write broker journal");
    file.flush().expect("flush broker journal");
    file.sync_all().expect("sync broker journal");
    std::fs::File::open(path.parent().expect("journal parent"))
        .expect("open journal directory")
        .sync_all()
        .expect("sync journal directory");
}

#[cfg(target_os = "linux")]
fn clear_journal(path: &std::path::Path) {
    std::fs::remove_file(path).expect("clear broker journal");
    std::fs::File::open(path.parent().expect("journal parent"))
        .expect("open journal directory")
        .sync_all()
        .expect("sync journal directory");
}

#[cfg(target_os = "linux")]
fn try_file_permit(path: &std::path::Path) -> Option<nix::fcntl::Flock<std::fs::File>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .expect("open host permit file");
    // flock is advisory; this prototype controls both admission paths.
    match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(locked) => Some(locked),
        Err((_file, nix::errno::Errno::EWOULDBLOCK)) => None,
        Err((_file, error)) => panic!("host lock: {error}"),
    }
}

#[cfg(target_os = "linux")]
async fn acquire_file_permit(path: &std::path::Path) -> nix::fcntl::Flock<std::fs::File> {
    loop {
        if let Some(file) = try_file_permit(path) {
            return file;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(target_os = "linux")]
struct KillOnDrop(std::process::Child);

#[cfg(target_os = "linux")]
impl KillOnDrop {
    fn stop(&mut self) {
        use std::os::unix::process::ExitStatusExt as _;
        self.0.kill().expect("send SIGKILL to test subprocess");
        let status = self.0.wait().expect("reap test subprocess");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
}

#[cfg(target_os = "linux")]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(target_os = "linux")]
fn spawn_test_child(test_name: &str, env: &[(&str, &str)]) -> KillOnDrop {
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command.arg("--exact").arg(test_name);
    for (key, value) in env {
        command.env(key, value);
    }
    KillOnDrop(command.spawn().expect("spawn test subprocess"))
}

#[cfg(target_os = "linux")]
async fn wait_for_marker(path: &std::path::Path, what: &'static str) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect(what);
}

#[cfg(target_os = "linux")]
async fn wait_child_exit(child: &mut KillOnDrop, what: &'static str) {
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(status) = child.0.try_wait().expect("test subprocess status") {
                break status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect(what);
    assert!(status.success(), "{what}: {status}");
}

#[cfg(target_os = "linux")]
async fn run_broker_or_backend_subprocess() {
    use atomic_exec::ExecutionBackend as _;
    if let Ok(home) = std::env::var("ATOMIC_BROKER_SPIKE_BROKER_HOME") {
        let home = std::path::PathBuf::from(home);
        let upstream = std::env::var("ATOMIC_BROKER_SPIKE_UPSTREAM").expect("upstream URL");
        let (url, _methods, _permits, _relay, _recovery_hold, _recovery_file) =
            authenticated_relay(
                upstream,
                home.join("broker-journal.json"),
                Some(home.join("host-permit.lock")),
            )
            .await;
        std::fs::write(home.join("broker-ready"), url).expect("announce broker address");
        std::future::pending::<()>().await;
    }
    if let Ok(home) = std::env::var("ATOMIC_BROKER_SPIKE_BACKEND_HOME") {
        let home = std::path::PathBuf::from(home);
        std::fs::write(home.join("backend-attempted"), b"attempted")
            .expect("announce backend attempt");
        let lock = home.join("host-permit.lock");
        let journal = home.join("broker-journal.json");
        loop {
            if let Some(_permit) = try_file_permit(&lock) {
                // The lock is released by SIGKILL; the journal prevents an
                // unrelated backend process from entering before reconciliation.
                if !journal.exists() {
                    std::fs::write(home.join("backend-admitted"), b"admitted")
                        .expect("announce backend admission");
                    let command = atomic_exec::Command {
                        program: "/bin/sh".into(),
                        args: vec!["-c".into(), "printf backend-process-ok".into()],
                        cwd: std::env::current_dir().expect("backend cwd"),
                        time_limit: std::time::Duration::from_secs(10),
                        output_limit: 1024,
                        ..Default::default()
                    };
                    let backend = atomic_exec::LocalBackend::in_dir(home.join("backend-scratch"));
                    let done = backend.run(&command).expect("real backend command");
                    assert_eq!(done.code, Some(0));
                    assert_eq!(done.stdout, b"backend-process-ok");
                    std::fs::write(home.join("backend-done"), b"done")
                        .expect("announce backend completion");
                    std::process::exit(0);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

#[cfg(target_os = "linux")]
async fn reconcile_orphan(upstream_url: String, journal: std::path::PathBuf) {
    use futures_util::SinkExt as _;
    use futures_util::StreamExt as _;
    use tokio_tungstenite::tungstenite::Message;

    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal).expect("read broker journal"))
            .expect("parse broker journal");
    let session_id = record["sessionId"].as_str().expect("journal session ID");
    let process_id = record["processId"].as_str().expect("journal process ID");
    // Codex can observe the old socket closing after our task has stopped.
    // Its own recovery test retries this exact already-attached response.
    let mut upstream = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (mut upstream, _) = tokio_tungstenite::connect_async(upstream_url.as_str())
                .await
                .expect("resume upstream connection");
            upstream
                .send(Message::Text(
                    serde_json::json!({
                        "id": 1,
                        "method": "initialize",
                        "params": {"clientName": "atomic-broker-recovery", "resumeSessionId": session_id}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("resume session request");
            let initialized = upstream
                .next()
                .await
                .expect("initialize response")
                .expect("initialize frame");
            let response: serde_json::Value =
                serde_json::from_str(initialized.to_text().expect("initialize response text"))
                    .expect("initialize response JSON");
            assert_eq!(response["id"], 1, "unexpected initialize response");
            if response["error"]["code"] == -32010 {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                continue;
            }
            if let Some(error) = response.get("error") {
                std::fs::write(
                    journal.with_file_name("recovery-refusal.json"),
                    serde_json::to_vec(error).expect("encode resume refusal"),
                )
                .expect("record resume refusal for test");
                panic!("resume refused: {error}");
            }
            assert_eq!(
                response["result"]["sessionId"], session_id,
                "resume must attach the journaled session"
            );
            break upstream;
        }
    })
    .await
    .expect("resume session before retention expires");
    upstream
        .send(Message::Text(
            serde_json::json!({"method": "initialized", "params": {}})
                .to_string()
                .into(),
        ))
        .await
        .expect("initialized notification");
    let mut request_id = 2;
    loop {
        upstream
            .send(Message::Text(
                serde_json::json!({
                    "id": request_id,
                    "method": "process/read",
                    "params": {"processId": process_id, "afterSeq": null, "maxBytes": null, "waitMs": 100}
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("read orphan state");
        loop {
            let message = upstream
                .next()
                .await
                .expect("orphan response")
                .expect("orphan frame");
            let value: serde_json::Value =
                serde_json::from_str(message.to_text().expect("orphan response text"))
                    .expect("orphan response JSON");
            if value["method"] == "process/exited" && value["params"]["processId"] == process_id {
                clear_journal(&journal);
                return;
            }
            if value["id"] == request_id {
                assert!(value.get("error").is_none(), "orphan read failed: {value}");
                if value["result"]["exited"] == true {
                    clear_journal(&journal);
                    return;
                }
                break;
            }
        }
        request_id += 1;
    }
}

#[cfg(target_os = "linux")]
async fn authenticated_relay(
    upstream_url: String,
    journal: std::path::PathBuf,
    host_lock: Option<std::path::PathBuf>,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    std::sync::Arc<tokio::sync::Semaphore>,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<std::sync::Mutex<Option<tokio::sync::OwnedSemaphorePermit>>>,
    std::sync::Arc<std::sync::Mutex<Option<nix::fcntl::Flock<std::fs::File>>>>,
) {
    use futures_util::SinkExt as _;
    use futures_util::StreamExt as _;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::connect_async;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("relay listener");
    let url = format!("ws://{}", listener.local_addr().expect("relay address"));
    let methods = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = std::sync::Arc::clone(&methods);
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let recovery_file_hold = if journal.exists() {
        match &host_lock {
            Some(path) => Some(acquire_file_permit(path).await),
            None => None,
        }
    } else {
        None
    };
    let recovery_file_hold = std::sync::Arc::new(std::sync::Mutex::new(recovery_file_hold));
    let recovery_hold = if journal.exists() {
        Some(
            std::sync::Arc::clone(&permits)
                .acquire_owned()
                .await
                .expect("reserve orphan permit before admission"),
        )
    } else {
        None
    };
    let recovery_hold = std::sync::Arc::new(std::sync::Mutex::new(recovery_hold));
    if journal.exists() {
        let recovery_journal = journal.clone();
        let recovery_url = upstream_url.clone();
        let recovery_hold_for_task = std::sync::Arc::clone(&recovery_hold);
        let recovery_file_for_task = std::sync::Arc::clone(&recovery_file_hold);
        tokio::spawn(async move {
            reconcile_orphan(recovery_url, recovery_journal).await;
            recovery_hold_for_task.lock().expect("recovery hold").take();
            recovery_file_for_task.lock().expect("recovery file").take();
        });
    }
    let shared_permits = std::sync::Arc::clone(&permits);
    let relay = tokio::spawn(async move {
        let mut clients = tokio::task::JoinSet::new();
        loop {
            let (socket, _) = tokio::select! {
                accepted = listener.accept() => accepted.expect("relay accept"),
                joined = clients.join_next(), if !clients.is_empty() => {
                    joined.expect("client join").expect("client task");
                    continue;
                }
            };
            let upstream_url = upstream_url.clone();
            let methods = std::sync::Arc::clone(&recorded);
            let permits = std::sync::Arc::clone(&shared_permits);
            let journal = journal.clone();
            let host_lock = host_lock.clone();
            clients.spawn(async move {
                let Ok(mut client) = accept_hdr_async(socket, authorize_relay).await else {
                    return;
                };
                let (mut upstream, _) = connect_async(upstream_url).await.expect("upstream");
                let mut held = std::collections::HashMap::<
                    String,
                    (
                        tokio::sync::OwnedSemaphorePermit,
                        Option<nix::fcntl::Flock<std::fs::File>>,
                    ),
                >::new();
                let mut client_open = true;
                let mut session_id: Option<String> = None;
                loop {
                    tokio::select! {
                        incoming = client.next(), if client_open => {
                            let Some(Ok(message)) = incoming else {
                                client_open = false;
                                if held.is_empty() { break; }
                                continue;
                            };
                            // Forward the original frame bytes. Parsing is only for
                            // observation and must never rewrite an RPC payload.
                            let value: serde_json::Value = serde_json::from_str(
                                message.to_text().expect("JSON-RPC text frame"),
                            )
                            .expect("JSON-RPC frame");
                            if let Some(method) = value.get("method").and_then(|method| method.as_str()) {
                                methods.lock().expect("methods").push(method.to_owned());
                                if method == "process/start" {
                                    let process_id = value
                                        .get("params")
                                        .and_then(|params| params.get("processId"))
                                        .and_then(|id| id.as_str())
                                        .expect("start process id");
                                    let permit = std::sync::Arc::clone(&permits)
                                        .acquire_owned()
                                        .await
                                        .expect("host permit");
                                    let file_permit = match &host_lock {
                                        Some(path) => Some(acquire_file_permit(path).await),
                                        None => None,
                                    };
                                    persist_journal(&journal, session_id.as_deref().expect("session before process start"), process_id);
                                    assert!(held.insert(process_id.to_owned(), (permit, file_permit)).is_none());
                                }
                            }
                            upstream.send(message).await.expect("forward request");
                        }
                        outgoing = upstream.next() => {
                            let Some(Ok(message)) = outgoing else { break; };
                            if let Ok(text) = message.to_text()
                                && let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
                            {
                                if let Some(id) = value.pointer("/result/sessionId").and_then(|id| id.as_str()) {
                                    session_id = Some(id.to_owned());
                                }
                                if value.get("method").and_then(|method| method.as_str()) == Some("process/exited") {
                                    let process_id = value
                                        .get("params")
                                        .and_then(|params| params.get("processId"))
                                        .and_then(|id| id.as_str())
                                        .expect("exit process id");
                                    clear_journal(&journal);
                                    assert!(held.remove(process_id).is_some(), "unmatched exit");
                                }
                            }
                            if client_open {
                                client.send(message).await.expect("forward response");
                            } else if held.is_empty() {
                                break;
                            }
                        }
                    }
                }
                assert!(held.is_empty(), "do not release an unobserved process");
            });
        }
    });
    (
        url,
        methods,
        permits,
        relay,
        recovery_hold,
        recovery_file_hold,
    )
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_codex_can_run_a_shell_through_its_remote_exec_server() {
    use atomic_exec::ExecutionBackend as _;
    use codex_exec_server::EnvironmentManager;
    use codex_exec_server::ExecParams;
    use codex_exec_server::ExecServerRuntimeOptions;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    use std::time::Duration;
    use tokio::net::TcpListener;

    if let Ok(home) = std::env::var("ATOMIC_BROKER_SPIKE_CHILD_HOME") {
        let release_marker =
            std::env::var("ATOMIC_BROKER_SPIKE_RELEASE_MARKER").expect("release marker path");
        let manager = EnvironmentManager::from_codex_home(
            home,
            None,
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        )
        .await
        .expect("child environment");
        let cwd = std::env::current_dir().expect("child cwd");
        let params: ExecParams = serde_json::from_value(serde_json::json!({
            "processId": "atomic-broker-crashed-client",
            "argv": ["timeout", "60s", "/bin/sh", "-c", "while [ ! -e \"$ATOMIC_BROKER_RELEASE\" ]; do sleep 0.05; done"],
            "cwd": format!("file://{}", cwd.display()),
            "env": {"ATOMIC_BROKER_RELEASE": release_marker},
            "tty": false,
            "arg0": null,
            "sandbox": null
        }))
        .expect("child exec params");
        let _running = manager
            .default_environment()
            .expect("child remote environment")
            .get_exec_backend()
            .start(params)
            .await
            .expect("child remote shell start");
        std::fs::write(
            std::env::var("ATOMIC_BROKER_SPIKE_CHILD_MARKER").expect("child marker path"),
            b"started",
        )
        .expect("child marker");
        // Model an Atomic process crash: destructors and process unregister
        // cannot release the remote shell's host permit.
        std::process::exit(0);
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("allocate loopback port");
    let address = listener.local_addr().expect("loopback address");
    drop(listener);
    let url = format!("ws://{address}");
    let server_url = url.clone();
    let server = tokio::spawn(async move {
        codex_exec_server::run_main(
            &server_url,
            ExecServerRuntimeOptions::new(
                std::env::current_exe().expect("test executable"),
                /*codex_linux_sandbox_exe*/ None,
            )
            .expect("runtime paths"),
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        )
        .await
        .expect("exec server");
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("exec server starts");

    let temporary_home = tempfile::Builder::new()
        .prefix("atomic-broker-spike-")
        .tempdir()
        .expect("private Codex home");
    let home = temporary_home.path().to_path_buf();
    let journal = home.join("broker-journal.json");
    let (relay_url, methods, permits, relay, _first_recovery_hold, _first_recovery_file) =
        authenticated_relay(url.clone(), journal.clone(), None).await;
    assert!(
        tokio_tungstenite::connect_async(&relay_url).await.is_err(),
        "relay must reject a client without its bearer token"
    );
    std::fs::write(
        home.join("environments.toml"),
        format!(
            "include_local = false\ndefault = \"broker\"\n[[environments]]\nid = \"broker\"\nurl = \"{relay_url}\"\nauth_bearer_token = \"test-broker-token\"\n"
        ),
    )
    .expect("controlled environment config");
    let manager = EnvironmentManager::from_codex_home(
        &home,
        None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
    .await
    .expect("controlled environment manager");
    let environment = manager.default_environment().expect("remote environment");
    assert!(environment.is_remote());
    let cwd = std::env::current_dir().expect("cwd");
    let params: ExecParams = serde_json::from_value(serde_json::json!({
        "processId": "atomic-broker-seam",
        "argv": ["/bin/sh", "-c", "printf atomic-remote-ok"],
        "cwd": format!("file://{}", cwd.display()),
        "env": {},
        "tty": false,
        "arg0": null,
        "sandbox": null
    }))
    .expect("exec params");
    let started = tokio::time::timeout(
        Duration::from_secs(10),
        environment.get_exec_backend().start(params),
    )
    .await
    .expect("remote start timeout")
    .expect("remote start");
    let process = started.process;
    let mut output = Vec::new();
    let exit_code = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let read = process
                .read(None, None, Some(1_000))
                .await
                .expect("remote read");
            for chunk in read.chunks {
                output.extend_from_slice(chunk.chunk.as_ref());
            }
            if read.exited {
                break read.exit_code.expect("exit code");
            }
        }
    })
    .await
    .expect("remote read timeout");
    assert_eq!(exit_code, 0);
    assert_eq!(output, b"atomic-remote-ok");
    tokio::time::timeout(Duration::from_secs(2), async {
        while permits.available_permits() != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shell slot returned on exit");
    let fixture = home.join("source.txt");
    std::fs::write(&fixture, b"atomic-fs-ok").expect("fixture");
    let file: codex_exec_server::FsGetMetadataParams = serde_json::from_value(serde_json::json!({
        "path": format!("file://{}", fixture.display()),
        "sandbox": null
    }))
    .expect("file URI");
    let contents = environment
        .get_filesystem()
        .read_file(
            &file.path,
            codex_exec_server::ReadFileOptions::default(),
            None,
        )
        .await
        .expect("remote filesystem read");
    assert_eq!(contents, b"atomic-fs-ok");

    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;
    let http_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP fixture listener");
    let http_address = http_listener.local_addr().expect("HTTP fixture address");
    let http_fixture = tokio::spawn(async move {
        let (mut socket, _) = http_listener.accept().await.expect("HTTP fixture accept");
        let mut request = [0_u8; 1024];
        let bytes = socket.read(&mut request).await.expect("HTTP fixture read");
        assert!(request[..bytes].starts_with(b"GET /fixture HTTP/1.1"));
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("HTTP fixture response");
    });
    let request: codex_exec_server::HttpRequestParams = serde_json::from_value(serde_json::json!({
        "method": "GET",
        "url": format!("http://{http_address}/fixture"),
        "headers": [],
        "requestId": "atomic-broker-http"
    }))
    .expect("HTTP request");
    let response = environment
        .get_http_client()
        .http_request(request)
        .await
        .expect("remote HTTP request");
    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_ref(), b"ok");
    http_fixture.await.expect("HTTP fixture task");
    {
        let seen = methods.lock().expect("methods");
        for method in ["process/start", "fs/readFile", "http/request"] {
            assert!(
                seen.iter().any(|recorded| recorded == method),
                "missing {method}"
            );
        }
    }

    let child_marker = home.join("child-started");
    let release_marker = home.join("release-shell");
    assert!(!release_marker.exists(), "release marker must begin absent");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg("pinned_codex_can_run_a_shell_through_its_remote_exec_server")
        .env("ATOMIC_BROKER_SPIKE_CHILD_HOME", &home)
        .env("ATOMIC_BROKER_SPIKE_CHILD_MARKER", &child_marker)
        .env("ATOMIC_BROKER_SPIKE_RELEASE_MARKER", &release_marker)
        .spawn()
        .expect("crash client process");
    let child_status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = child.try_wait().expect("client process status") {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        child.kill().expect("kill stuck client");
        child.wait().expect("reap stuck client");
        panic!("client failed to start shell within ten seconds")
    });
    assert!(child_status.success(), "crash client did not start shell");
    assert_eq!(
        std::fs::read(child_marker).expect("child marker"),
        b"started"
    );
    assert_eq!(
        permits.available_permits(),
        0,
        "client crash must not free a running shell's permit"
    );
    assert!(
        journal.exists(),
        "start intent must be durable before forwarding"
    );
    // Aborting the broker task drops its JoinSet-owned client handlers.
    // This exercises lost sockets, not a separate broker process crash.
    relay.abort();
    relay
        .await
        .expect_err("broker task abort must cancel client handlers");
    let (
        restart_relay_url,
        restart_methods,
        restart_permits,
        restart_relay,
        _recovery_hold,
        _recovery_file,
    ) = authenticated_relay(url, journal.clone(), None).await;
    assert_eq!(
        restart_permits.available_permits(),
        0,
        "broker restart must reserve the orphan permit before admission"
    );
    std::fs::write(
        home.join("environments.toml"),
        format!(
            "include_local = false\ndefault = \"broker\"\n[[environments]]\nid = \"broker\"\nurl = \"{restart_relay_url}\"\nauth_bearer_token = \"test-broker-token\"\n"
        ),
    )
    .expect("restarted broker environment config");

    let backend_attempted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let backend_admitted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let backend = TestPooledBackend {
        inner: atomic_exec::LocalBackend::in_dir(home.join("backend-scratch")),
        permits: std::sync::Arc::clone(&restart_permits),
        runtime: tokio::runtime::Handle::current(),
        attempted: std::sync::Arc::clone(&backend_attempted),
        admitted: std::sync::Arc::clone(&backend_admitted),
    };
    let backend_command = atomic_exec::Command {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "printf backend-ok".into()],
        cwd: cwd.clone(),
        time_limit: Duration::from_secs(10),
        output_limit: 1024,
        ..Default::default()
    };
    let backend_task = tokio::task::spawn_blocking(move || {
        backend.run(&backend_command).expect("real backend command")
    });

    let second_manager = EnvironmentManager::from_codex_home(
        &home,
        None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
    .await
    .expect("second independent client");
    let second_environment = second_manager
        .default_environment()
        .expect("second remote environment");
    let second_params: ExecParams = serde_json::from_value(serde_json::json!({
        "processId": "atomic-broker-waiting-client",
        "argv": ["/bin/sh", "-c", "printf second-ok"],
        "cwd": format!("file://{}", cwd.display()),
        "env": {},
        "tty": false,
        "arg0": null,
        "sandbox": null
    }))
    .expect("second exec params");
    let waiting = tokio::spawn(async move {
        let _manager = second_manager;
        let process = second_environment
            .get_exec_backend()
            .start(second_params)
            .await
            .expect("second shell start")
            .process;
        loop {
            let read = process
                .read(None, None, Some(1_000))
                .await
                .expect("second shell read");
            if read.exited {
                assert_eq!(read.exit_code, Some(0));
                break;
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let second_start_seen = restart_methods
                .lock()
                .expect("methods")
                .iter()
                .filter(|method| method.as_str() == "process/start")
                .count()
                == 1;
            if second_start_seen {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("second shell reached relay");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !backend_attempted.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("backend reached the host permit");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !backend_admitted.load(std::sync::atomic::Ordering::SeqCst),
        "backend command bypassed the Codex shell's permit"
    );
    assert!(
        !backend_task.is_finished(),
        "backend command finished before shell exit"
    );
    assert!(!waiting.is_finished(), "second shell bypassed host permit");
    std::fs::write(&release_marker, b"release").expect("release first shell");
    tokio::time::timeout(Duration::from_secs(10), async {
        while journal.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("restarted broker must confirm orphan exit before clearing journal");
    let backend_result = tokio::time::timeout(Duration::from_secs(10), backend_task)
        .await
        .expect("backend command remained blocked")
        .expect("backend command task");
    assert_eq!(backend_result.code, Some(0));
    assert_eq!(backend_result.stdout, b"backend-ok");
    tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("second shell remained blocked")
        .expect("second shell task");
    tokio::time::timeout(Duration::from_secs(2), async {
        while restart_permits.available_permits() != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("second shell slot returned");
    restart_relay.abort();
    server.abort();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_broker_process_kill_recovers_host_permit() {
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;

    run_broker_or_backend_subprocess().await;
    let home = tempfile::Builder::new()
        .prefix("atomic-broker-process-")
        .tempdir()
        .expect("broker home");
    let home_path = home.path();
    let home_text = home_path.to_str().expect("UTF-8 broker home");
    let journal = home_path.join("broker-journal.json");
    let lock = home_path.join("host-permit.lock");
    let ready = home_path.join("broker-ready");
    let release = home_path.join("release-shell");
    let child_marker = home_path.join("child-started");
    let backend_attempted = home_path.join("backend-attempted");
    let backend_admitted = home_path.join("backend-admitted");
    let backend_done = home_path.join("backend-done");

    let port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("upstream port");
    let address = port.local_addr().expect("upstream address");
    drop(port);
    let upstream_url = format!("ws://{address}");
    let server_url = upstream_url.clone();
    let upstream = tokio::spawn(async move {
        codex_exec_server::run_main(
            &server_url,
            codex_exec_server::ExecServerRuntimeOptions::new(
                std::env::current_exe().expect("test executable"),
                None,
            )
            .expect("upstream runtime paths"),
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        )
        .await
        .expect("upstream exec server");
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while tokio::net::TcpStream::connect(address).await.is_err() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("upstream server starts");

    let test_name = "actual_broker_process_kill_recovers_host_permit";
    let mut broker = spawn_test_child(
        test_name,
        &[
            ("ATOMIC_BROKER_SPIKE_BROKER_HOME", home_text),
            ("ATOMIC_BROKER_SPIKE_UPSTREAM", &upstream_url),
        ],
    );
    wait_for_marker(&ready, "broker process starts").await;
    let relay_url = std::fs::read_to_string(&ready).expect("broker URL");
    std::fs::write(
        home_path.join("environments.toml"),
        format!("include_local = false\ndefault = \"broker\"\n[[environments]]\nid = \"broker\"\nurl = \"{relay_url}\"\nauth_bearer_token = \"test-broker-token\"\n"),
    )
    .expect("broker client config");
    let mut client = spawn_test_child(
        "pinned_codex_can_run_a_shell_through_its_remote_exec_server",
        &[
            ("ATOMIC_BROKER_SPIKE_CHILD_HOME", home_text),
            (
                "ATOMIC_BROKER_SPIKE_CHILD_MARKER",
                child_marker.to_str().expect("marker path"),
            ),
            (
                "ATOMIC_BROKER_SPIKE_RELEASE_MARKER",
                release.to_str().expect("release path"),
            ),
        ],
    );
    wait_for_marker(&child_marker, "Atomic client starts shell").await;
    wait_child_exit(&mut client, "Atomic client exits").await;
    assert!(
        journal.exists(),
        "shell start intent must survive client exit"
    );
    assert!(
        try_file_permit(&lock).is_none(),
        "broker shell must own host lock"
    );

    let mut backend = spawn_test_child(
        test_name,
        &[("ATOMIC_BROKER_SPIKE_BACKEND_HOME", home_text)],
    );
    wait_for_marker(&backend_attempted, "backend process attempts command").await;
    assert!(
        !backend_admitted.exists(),
        "backend entered while shell held host lock"
    );
    broker.stop(); // std::process::Child::kill sends SIGKILL on Linux.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(journal.exists(), "SIGKILL must leave the start journal");
    assert!(
        !backend_admitted.exists(),
        "backend bypassed orphan journal after broker SIGKILL"
    );

    std::fs::remove_file(&ready).expect("clear old ready marker");
    let mut restarted = spawn_test_child(
        test_name,
        &[
            ("ATOMIC_BROKER_SPIKE_BROKER_HOME", home_text),
            ("ATOMIC_BROKER_SPIKE_UPSTREAM", &upstream_url),
        ],
    );
    wait_for_marker(&ready, "restarted broker process starts").await;
    assert!(
        try_file_permit(&lock).is_none(),
        "restarted broker must reserve orphan host lock"
    );
    assert!(
        !backend_admitted.exists(),
        "backend entered before trusted drain"
    );
    std::fs::write(&release, b"release").expect("release orphan shell");
    wait_for_marker(&backend_done, "backend runs after broker confirms exit").await;
    wait_child_exit(&mut backend, "backend process exits").await;
    assert!(!journal.exists(), "trusted exit clears broker journal");
    restarted.stop();
    upstream.abort();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_orphan_refuses_host_permit() {
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;

    run_broker_or_backend_subprocess().await;
    let home = tempfile::Builder::new()
        .prefix("atomic-broker-unknown-")
        .tempdir()
        .expect("broker home");
    let home_path = home.path();
    let home_text = home_path.to_str().expect("UTF-8 broker home");
    persist_journal(
        &home_path.join("broker-journal.json"),
        "unknown-session",
        "unknown-process",
    );
    let port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("upstream port");
    let address = port.local_addr().expect("upstream address");
    drop(port);
    let upstream_url = format!("ws://{address}");
    let server_url = upstream_url.clone();
    let upstream = tokio::spawn(async move {
        codex_exec_server::run_main(
            &server_url,
            codex_exec_server::ExecServerRuntimeOptions::new(
                std::env::current_exe().expect("test executable"),
                None,
            )
            .expect("upstream runtime paths"),
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        )
        .await
        .expect("upstream exec server");
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while tokio::net::TcpStream::connect(address).await.is_err() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("upstream server starts");
    let test_name = "unknown_orphan_refuses_host_permit";
    let mut broker = spawn_test_child(
        test_name,
        &[
            ("ATOMIC_BROKER_SPIKE_BROKER_HOME", home_text),
            ("ATOMIC_BROKER_SPIKE_UPSTREAM", &upstream_url),
        ],
    );
    wait_for_marker(
        &home_path.join("broker-ready"),
        "unknown-orphan broker starts",
    )
    .await;
    let refusal = home_path.join("recovery-refusal.json");
    wait_for_marker(&refusal, "pinned server refuses unknown session").await;
    let error: serde_json::Value =
        serde_json::from_slice(&std::fs::read(refusal).expect("read resume refusal"))
            .expect("parse resume refusal");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("unknown session id unknown-session")),
        "unexpected resume refusal: {error}"
    );
    assert!(
        try_file_permit(&home_path.join("host-permit.lock")).is_none(),
        "unknown orphan must reserve host lock"
    );
    let mut backend = spawn_test_child(
        test_name,
        &[("ATOMIC_BROKER_SPIKE_BACKEND_HOME", home_text)],
    );
    wait_for_marker(
        &home_path.join("backend-attempted"),
        "backend attempts unknown orphan",
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        !home_path.join("backend-admitted").exists(),
        "unknown orphan admitted backend"
    );
    broker.stop();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        home_path.join("broker-journal.json").exists(),
        "failed resume must retain journal"
    );
    assert!(
        !home_path.join("backend-admitted").exists(),
        "unknown orphan admitted backend after broker SIGKILL"
    );
    backend.stop();
    upstream.abort();
}
