// SPDX-License-Identifier: Apache-2.0
use super::*;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A local HTTP server that answers from `replies` in turn and records each
/// request line, headers and body it receives.
async fn server(replies: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        let mut replies = replies.into_iter();
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buffer).into_owned();
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if buffer.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            log.lock()
                .expect("log")
                .push(String::from_utf8_lossy(&buffer).into_owned());
            let (status, body) = replies.next().unwrap_or((500, "{}"));
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    (format!("127.0.0.1:{}", address.port()), seen)
}

const SPEC: &str = r##"openapi: 3.1.0
info: { title: Forecast, version: "1" }
paths:
  /v1/forecast/{city}:
    get:
      operationId: getForecast
      parameters:
        - { name: city, in: path, required: true, schema: { type: string } }
        - { name: days, in: query, required: true, schema: { type: integer, minimum: 1, maximum: 7 } }
        - { name: X-Units, in: header, schema: { type: string, enum: [metric, imperial] } }
      responses:
        "200": { description: ok, content: { application/json: { schema: { type: object } } } }
  /v1/reports:
    post:
      operationId: postReport
      requestBody:
        required: true
        content: { application/json: { schema: { type: object, required: [text], properties: { text: { type: string, maxLength: 20 } } } } }
      responses:
        "201": { description: created }
"##;

fn toolset(host: &str) -> HttpToolset {
    let spec = atomic_schema::yaml::load(std::path::Path::new("spec.yaml"), SPEC)
        .expect("yaml")
        .value;
    let operations = atomic_schema::openapi::operations(&spec)
        .expect("spec")
        .into_iter()
        .map(|op| (op.name.clone(), op))
        .collect();
    HttpToolset {
        base_url: format!("http://{host}/api/").parse().expect("url"),
        allow: vec![host.to_owned()],
        operations,
        headers: vec![("Authorization".into(), "Bearer scoped".into())],
        attempts: 3,
        timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn a_call_builds_its_request_from_the_operation() {
    let (host, seen) = server(vec![(200, r#"{"temperature_c": 17}"#)]).await;
    let out = toolset(&host)
        .call(
            "getForecast",
            &json!({ "city": "São Paulo/x", "days": 2, "X-Units": "metric" }),
        )
        .await
        .expect("call");
    assert_eq!(out, json!({ "temperature_c": 17 }));
    let request = seen.lock().expect("log")[0].clone();
    assert!(
        request.starts_with("GET /api/v1/forecast/S%C3%A3o%20Paulo%2Fx?days=2 HTTP/1.1"),
        "{request}"
    );
    let lower = request.to_ascii_lowercase();
    assert!(
        lower.contains("x-units: metric") && lower.contains("authorization: bearer scoped"),
        "{request}"
    );
}

#[tokio::test]
async fn arguments_that_break_the_full_schema_are_refused_before_any_request() {
    let (host, seen) = server(vec![(200, "{}")]).await;
    let tools = toolset(&host);
    for args in [
        json!({ "city": "Lisbon", "days": 9 }),
        json!({ "city": "Lisbon" }),
        json!({ "city": "Lisbon", "days": 1, "extra": 1 }),
    ] {
        let error = tools.call("getForecast", &args).await.expect_err("refused");
        assert!(matches!(error, CallError::Arguments { .. }), "{error}");
    }
    let error = tools
        .call(
            "postReport",
            &json!({ "body": { "text": "far too long for the twenty limit" } }),
        )
        .await
        .expect_err("refused");
    assert!(matches!(error, CallError::Arguments { .. }), "{error}");
    assert!(seen.lock().expect("log").is_empty(), "no request was sent");
}

#[tokio::test]
async fn a_host_outside_the_allowlist_is_refused_before_any_request() {
    let (host, seen) = server(vec![(200, "{}")]).await;
    let mut tools = toolset(&host);
    tools.allow = vec!["api.open-meteo.com".into()];
    let error = tools
        .call("getForecast", &json!({ "city": "Lisbon", "days": 1 }))
        .await
        .expect_err("refused");
    assert!(matches!(error, CallError::Refused(_)), "{error}");
    assert!(seen.lock().expect("log").is_empty(), "no request was sent");
    // A path that names another host, or a value that climbs out of its
    // segment, stays on the allowed host and under the base URL.
    let (host, seen) = server(vec![(200, "{}"), (200, "{}")]).await;
    let mut tools = toolset(&host);
    let mut op = tools.operations["getForecast"].clone();
    op.path = "//elsewhere.example/steal".into();
    op.input = json!({ "type": "object" });
    tools.operations.insert("escape".into(), op);
    tools
        .call("escape", &json!({}))
        .await
        .expect("stays on the host");
    tools
        .call("getForecast", &json!({ "city": "../../admin", "days": 1 }))
        .await
        .expect("encoded");
    let log = seen.lock().expect("log").clone();
    assert!(
        log[0].starts_with("GET /api/elsewhere.example/steal "),
        "{}",
        log[0]
    );
    assert!(
        log[1].starts_with("GET /api/v1/forecast/..%2F..%2Fadmin?days=1 "),
        "{}",
        log[1]
    );
}

#[tokio::test]
async fn safe_methods_are_retried_and_others_are_sent_once() {
    let (host, seen) = server(vec![(503, "{}"), (200, r#"{"ok": true}"#)]).await;
    let out = toolset(&host)
        .call("getForecast", &json!({ "city": "Lisbon", "days": 1 }))
        .await
        .expect("retried");
    assert_eq!(out, json!({ "ok": true }));
    assert_eq!(seen.lock().expect("log").len(), 2);
    let (host, seen) = server(vec![(503, "{}"), (201, "{}")]).await;
    let error = toolset(&host)
        .call("postReport", &json!({ "body": { "text": "Mild." } }))
        .await
        .expect_err("not retried");
    assert!(
        matches!(error, CallError::Status { status: 503, .. }),
        "{error}"
    );
    assert_eq!(seen.lock().expect("log").len(), 1, "a POST is sent once");
    let request = seen.lock().expect("log")[0].clone();
    assert!(request.contains(r#"{"text":"Mild."}"#), "{request}");
    // A client error is not retried even for a safe method.
    let (host, seen) = server(vec![(404, r#"{"error":"no city"}"#)]).await;
    let error = toolset(&host)
        .call("getForecast", &json!({ "city": "Atlantis", "days": 1 }))
        .await
        .expect_err("404");
    assert!(
        matches!(error, CallError::Status { status: 404, .. }),
        "{error}"
    );
    assert_eq!(seen.lock().expect("log").len(), 1);
}
