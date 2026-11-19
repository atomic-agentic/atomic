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

// URL parsing removes `.` and `..` segments, so a path argument spelled as
// one would drop part of the operation's path while staying on the allowed
// host. Such values are refused before any request; ones that only contain
// dots are encoded into their own segment as usual.
#[test]
fn a_path_argument_cannot_change_the_operations_path() {
    let tools = toolset("api.example.com");
    for city in [".", "..", ""] {
        let error = tools
            .request("getForecast", &json!({ "city": city, "days": 1 }))
            .expect_err("refused");
        assert!(
            matches!(error, CallError::Arguments { .. }),
            "{city:?}: {error}"
        );
    }
    for (city, path) in [
        ("...", "/api/v1/forecast/..."),
        ("St. Ives", "/api/v1/forecast/St.%20Ives"),
        ("../..", "/api/v1/forecast/..%2F.."),
        ("%2E%2E", "/api/v1/forecast/%252E%252E"),
    ] {
        let request = tools
            .request("getForecast", &json!({ "city": city, "days": 1 }))
            .expect("built");
        assert_eq!(request.url.path(), path, "{city:?}");
    }
}

const EXTRA: &str = r##"openapi: 3.1.0
info: { title: Extra, version: "1" }
paths:
  /{resource}:batch:
    get:
      operationId: batch
      parameters: [ { name: resource, in: path, required: true, schema: { type: string } } ]
  /v1/search:
    get:
      operationId: search
      parameters:
        - { name: tags, in: query, schema: { type: array, items: { type: string } } }
        - { name: ids, in: query, explode: false, schema: { type: array, items: { type: integer } } }
        - { name: filter, in: query, style: deepObject, schema: { type: object, additionalProperties: { type: string } } }
        - { name: session, in: cookie, schema: { type: string } }
        - { name: Authorization, in: header, schema: { type: string } }
        - { name: X-Trace, in: header, schema: { type: string } }
      responses:
        "200":
          description: ok
          content:
            "application/json; charset=utf-8":
              schema: { type: object, required: [count], properties: { count: { type: integer } } }
  /v1/upload:
    post:
      operationId: upload
      requestBody: { required: true, content: { multipart/form-data: { schema: { type: object } } } }
      responses: { "201": { description: created } }
"##;

fn extra(host: &str, base: &str) -> HttpToolset {
    let spec = atomic_schema::yaml::load(std::path::Path::new("extra.yaml"), EXTRA)
        .expect("yaml")
        .value;
    let mut tools = toolset(host);
    tools.base_url = base.parse().expect("url");
    tools.operations = atomic_schema::openapi::operations(&spec)
        .expect("spec")
        .into_iter()
        .map(|op| (op.name.clone(), op))
        .collect();
    tools
}

// The operation's path follows the base URL's whole path, with or without
// its trailing slash, and a first segment with a colon is still a path.
#[test]
fn the_base_url_path_is_kept() {
    for base in ["http://api.example.com/api", "http://api.example.com/api/"] {
        let mut tools = toolset("api.example.com");
        tools.base_url = base.parse().expect("url");
        let request = tools
            .request("getForecast", &json!({ "city": "Paris", "days": 1 }))
            .expect("built");
        assert_eq!(request.url.path(), "/api/v1/forecast/Paris", "{base}");
    }
    let tools = extra("api.example.com", "http://api.example.com/api/v2");
    let request = tools
        .request("batch", &json!({ "resource": "things" }))
        .expect("built");
    assert_eq!(
        request.url.as_str(),
        "http://api.example.com/api/v2/things:batch"
    );
}

// Parameters follow their OpenAPI style; a model cannot add a header the
// toolset sets, or a cookie of its own.
#[test]
fn parameters_are_serialized_by_style_and_cannot_reach_reserved_headers() {
    let mut tools = extra("api.example.com", "http://api.example.com/");
    let request = tools
        .request(
            "search",
            &json!({
                "tags": ["a", "b"], "ids": [1, 2], "filter": { "kind": "x" },
                "session": "abc; admin=1", "X-Trace": "t1",
            }),
        )
        .expect("built");
    assert_eq!(
        request.url.query(),
        Some("tags=a&tags=b&ids=1%2C2&filter%5Bkind%5D=x")
    );
    let cookie = request
        .headers
        .iter()
        .find(|(name, _)| name == "Cookie")
        .expect("cookie");
    assert_eq!(cookie.1, "session=abc%3B%20admin=1");
    // `Authorization` was ignored on import, so the argument breaks the
    // input schema; a header the toolset sets is refused by name.
    let error = tools
        .request("search", &json!({ "Authorization": "Bearer model-chosen" }))
        .expect_err("refused");
    assert!(matches!(error, CallError::Arguments { .. }), "{error}");
    tools.headers.push(("x-trace".into(), "host".into()));
    let error = tools
        .request("search", &json!({ "X-Trace": "t1" }))
        .expect_err("refused");
    assert!(matches!(error, CallError::Refused(_)), "{error}");
    // An operation atomic cannot call is refused, and credentials stay out
    // of debug output.
    let error = tools.request("upload", &json!({})).expect_err("refused");
    assert!(error.to_string().contains("multipart/form-data"), "{error}");
    let shown = format!(
        "{tools:?} {:?}",
        tools
            .request("batch", &json!({ "resource": "r" }))
            .expect("built")
    );
    assert!(
        !shown.contains("scoped") && !shown.contains("host\""),
        "{shown}"
    );
}

#[tokio::test]
async fn a_response_that_breaks_its_schema_is_an_error() {
    let (host, _) = server(vec![(200, r#"{"count": 2}"#), (200, r#"{"count": "two"}"#)]).await;
    let tools = extra(&host, &format!("http://{host}/"));
    assert_eq!(
        tools.call("search", &json!({})).await.expect("fits"),
        json!({ "count": 2 })
    );
    let error = tools.call("search", &json!({})).await.expect_err("breaks");
    assert!(matches!(error, CallError::Response { .. }), "{error}");
}
