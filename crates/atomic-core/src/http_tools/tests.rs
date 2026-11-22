// SPDX-License-Identifier: Apache-2.0
use super::*;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The header every reply of `server` carries: a reply that names no media
/// type, or another one, goes through `server_at`, which writes its own
/// headers.
const JSON_HEADER: &str = "content-type: application/json\r\n";

/// A local HTTP server that answers from `replies` in turn and records each
/// request line, headers and body it receives, each reply a JSON one.
async fn server(replies: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
    server_at(
        replies
            .into_iter()
            .map(|(status, body)| (status, JSON_HEADER, body))
            .collect(),
    )
    .await
}

/// A local HTTP server answering from `replies` in turn, each with the
/// headers to write before its `content-length`: what a reply says it is,
/// that it is at all, and what it carries.
async fn server_at(
    replies: Vec<(u16, &'static str, &'static str)>,
) -> (String, Arc<Mutex<Vec<String>>>) {
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
            let (status, headers, body) = replies.next().unwrap_or((500, JSON_HEADER, "{}"));
            let reply = format!(
                "HTTP/1.1 {status} X\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
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

const JOBS: &str = r##"openapi: 3.1.0
info: { title: Jobs, version: "1" }
paths:
  /jobs:
    post:
      operationId: submit
      responses:
        "200": { description: done, content: { application/json: { schema: { type: object, required: [result], properties: { result: { type: string } } } } } }
        "202": { description: queued, content: { application/json: { schema: { type: object, required: [ticket], properties: { ticket: { type: integer } } } } } }
        "204": { description: nothing to do }
  /ping:
    get:
      operationId: ping
  /health:
    get:
      operationId: health
      responses:
        "200": { description: alive }
  /thing:
    get:
      operationId: thing
      responses:
        "2XX": { description: any }
  /lookup:
    get:
      operationId: lookup
      responses:
        "404": { description: no such thing, content: { application/json: { schema: { type: object } } } }
  /extonly:
    get:
      operationId: extonly
      responses:
        x-internal: true
  /anything:
    get:
      operationId: anything
      responses:
        "200": { description: any media type, content: { "*/*": { schema: { type: object, required: [checked] } } } }
  /stream:
    get:
      operationId: stream
      responses:
        "200": { description: any application body, content: { "application/*": { schema: { type: object, required: [checked], properties: { checked: { type: array } } } } } }
  /either:
    get:
      operationId: either
      responses:
        "200":
          description: a json media type and a wildcard, each with a schema
          content:
            application/json: { schema: { type: object, required: [specific] } }
            "*/*": { schema: { type: object, required: [never_governs] } }
  /problem:
    get:
      operationId: problem
      responses:
        "200":
          description: a thing, or the problem it is, each with its own schema
          content:
            application/json: { schema: { type: object, required: [thing], properties: { thing: { type: string } } } }
            application/problem+json: { schema: { type: object, required: [title], properties: { title: { type: string } } } }
  /param:
    get:
      operationId: param
      responses:
        "200": { description: json, however it is spelled, content: { "application/json; charset=utf-8": { schema: { type: object, required: [count], properties: { count: { type: integer } } } } } }
  /twospelled:
    get:
      operationId: twospelled
      responses:
        "200":
          description: two media types, one spelled with parameters
          content:
            "application/json; charset=utf-8": { schema: { type: object, required: [count], properties: { count: { type: integer } } } }
            application/problem+json: { schema: { type: object, required: [title], properties: { title: { type: string } } } }
  /anytype:
    get:
      operationId: anytype
      responses:
        "200":
          description: two json media types and a wildcard naming neither
          content:
            application/json: { schema: { type: object, required: [thing], properties: { thing: { type: string } } } }
            application/problem+json: { schema: { type: object, required: [title], properties: { title: { type: string } } } }
            "application/*": { schema: { type: object, required: [never_governs] } }
  /declared:
    get:
      operationId: declared
      responses:
        "200":
          description: a thing, or the plain text a server falls back to
          content:
            application/json: { schema: { type: object, required: [thing], properties: { thing: { type: string } } } }
            text/plain: {}
        "201":
          description: any application body, or the same plain text
          content:
            "application/*": { schema: { type: object, required: [thing] } }
            text/plain: {}
  /unkept:
    get:
      operationId: unkept
      responses:
        "200":
          description: a thing, or the same thing spelled out in plain text, each with its schema
          content:
            application/json: { schema: { type: object, required: [thing], properties: { thing: { type: string } } } }
            text/plain: { schema: { type: string } }
  /wildcardtext:
    get:
      operationId: wildcardtext
      responses:
        "200": { description: any application body, content: { "application/*": { schema: { type: object, required: [thing] } } } }
  /twoschema:
    get:
      operationId: twoschema
      responses:
        "200":
          description: two shapes, and a server that names a third
          content:
            application/json: { schema: { type: object, required: [thing] } }
            application/problem+json: { schema: { type: object, required: [title] } }
  /wildopen:
    get:
      operationId: wildopen
      responses:
        "200": { description: any media type with no schema, content: { "*/*": {} } }
  /open:
    get:
      operationId: open
      responses:
        "200": { description: json, content: { application/json: {} } }
  /text:
    get:
      operationId: text
      responses:
        "200": { description: text, content: { text/plain: { schema: { type: string } } } }
  /nullable:
    get:
      operationId: nullable
      responses:
        "200": { description: a body that may be null, content: { application/json: { schema: { type: [object, "null"] } } } }
  /falls-back:
    get:
      operationId: fallback
      responses:
        "200": { description: done, content: { application/json: { schema: { type: object, required: [result], properties: { result: { type: string } } } } } }
        default: { description: error }
"##;

fn jobs(host: &str) -> HttpToolset {
    let spec = atomic_schema::yaml::load(std::path::Path::new("jobs.yaml"), JOBS)
        .expect("yaml")
        .value;
    let mut tools = toolset(host);
    tools.base_url = format!("http://{host}/").parse().expect("url");
    tools.operations = atomic_schema::openapi::operations(&spec)
        .expect("spec")
        .into_iter()
        .map(|op| (op.name.clone(), op))
        .collect();
    tools
}

#[tokio::test]
async fn each_success_is_checked_against_the_response_its_status_declares() {
    let (host, _) = server(vec![
        (200, r#"{"result": "ok"}"#),
        (204, ""),
        (202, r#"{"ticket": 7}"#),
        // A queued reply that would pass as done, and a done one as queued.
        (202, r#"{"result": "ok"}"#),
        (200, r#"{"ticket": 7}"#),
        // A success the spec does not declare.
        (201, r#"{"result": "ok"}"#),
        // An operation that declares no responses takes any success.
        (200, r#"{"anything": [1, 2]}"#),
        // One that declares only a failure declares no success.
        (200, r#"{}"#),
    ])
    .await;
    let tools = jobs(&host);
    let args = json!({});
    let call = || tools.call("submit", &args);
    assert_eq!(call().await.expect("200"), json!({ "result": "ok" }));
    assert_eq!(call().await.expect("204"), Value::Null);
    assert_eq!(call().await.expect("202"), json!({ "ticket": 7 }));
    for (status, key) in [(202, "202"), (200, "200")] {
        let error = call().await.expect_err("does not fit");
        assert!(
            matches!(&error, CallError::Response { status: s, response, .. } if *s == status && response == key),
            "{status}: {error}"
        );
        // Said with the status and the response it was checked against.
        let said = format!("returned {status}, which does not fit its response `{key}`");
        assert!(error.to_string().contains(&said), "{error}");
    }
    // Answered with a status the operation declares no response for: its
    // own error, not to be sent again.
    let error = call().await.expect_err("201");
    assert!(
        matches!(&error, CallError::Undeclared { status: 201, .. }),
        "{error}"
    );
    assert_eq!(
        tools
            .call("ping", &args)
            .await
            .expect("no responses declared"),
        json!({ "anything": [1, 2] })
    );
    let error = tools
        .call("lookup", &args)
        .await
        .expect_err("only a 404 declared");
    assert!(
        matches!(&error, CallError::Undeclared { status: 200, .. }),
        "{error}"
    );
}

// A failure is a failure whatever the spec declares for it: its body is the
// error the server sent, not a result, and it is held to no schema. atomic
// keeps nothing of what an operation declares for its `404`, so a reply that
// would break that declared schema still reaches the caller as the status
// failure it is, carrying the body as the server sent it.
#[tokio::test]
async fn a_declared_failure_response_is_still_reported_as_a_failure() {
    // An array is no `object`, which is what `lookup` declares for its 404.
    let (host, seen) = server(vec![(404, "[1, 2]")]).await;
    let error = jobs(&host)
        .call("lookup", &json!({}))
        .await
        .expect_err("a 404 is a failure");
    match error {
        CallError::Status {
            operation,
            status,
            body,
        } => {
            assert_eq!(operation, "lookup");
            assert_eq!(status, 404);
            assert_eq!(body, "[1, 2]", "the error the server sent, as it sent it");
        }
        other => panic!("expected a failure of the status the server answered, got {other:?}"),
    }
    assert_eq!(seen.lock().expect("log").len(), 1, "a 404 is sent once");
}

// A key of `responses` that names no status declares no response, so an
// operation whose only such key is an extension declares no response at all:
// its reply is passed through as an operation with no `responses` map has
// always passed its own, not refused as a status nobody declared.
#[tokio::test]
async fn an_extension_under_responses_declares_nothing_to_check_a_reply_against() {
    let (host, _) = server(vec![(200, r#"{"anything": 1}"#)]).await;
    assert_eq!(
        jobs(&host)
            .call("extonly", &json!({}))
            .await
            .expect("no response declared"),
        json!({ "anything": 1 })
    );
}

// A response the spec declares with no content declares no body as well, so
// a reply that selects one and comes back with a body breaks the contract.
// It is refused the way every other response violation is, before its body
// is parsed, and never handed to the caller as a result.
//
// A 204 is the one status whose body no client ever reads: the protocol ends
// the reply at its headers, so what arrives for a 204 is empty whatever the
// server wrote after them. The check covers a 204 all the same; the refusals
// below run on statuses whose body does arrive.
#[tokio::test]
async fn a_body_on_a_response_that_declares_no_content_is_refused() {
    // The three replies that carry a body on a response declaring no
    // content, each with the operation that receives it and the response its
    // status selects. The size the message reports is the body's own length,
    // and the body is never echoed: its marker has to be absent from what
    // the caller reads.
    let refused = [
        ("health", "200", r#"{"alive": true}"#, "alive"),
        ("health", "200", "deleted", "deleted"),
        ("thing", "2XX", r#"{"matters": 1}"#, "matters"),
    ];
    let mut script = vec![(204u16, "")]; // exactly what the spec declares
    for body in refused.iter().map(|(_, _, body, _)| *body) {
        script.push((200, body));
    }
    script.push((200, "  \n")); // whitespace is an empty body
    // A status declared with a JSON body is still checked against it.
    script.push((200, r#"{"result": "ok"}"#));
    script.push((200, r#"{"ticket": 7}"#));
    let (host, _) = server(script).await;
    let tools = jobs(&host);
    let args = json!({});
    assert_eq!(
        tools.call("submit", &args).await.expect("an empty 204"),
        Value::Null,
        "an empty body is no result"
    );
    for (operation, key, body, marker) in refused {
        let error = tools
            .call(operation, &args)
            .await
            .expect_err("a body where the spec declares none");
        assert!(
            matches!(&error, CallError::Response { status: 200, response, .. } if response == key),
            "{operation}: {error}"
        );
        // Said with the status and the response it was checked against, and
        // with how much arrived.
        let said = format!("returned 200, which does not fit its response `{key}`");
        assert!(error.to_string().contains(&said), "{error}");
        assert!(error.to_string().contains("no body"), "{error}");
        let count = format!("and {} bytes were returned", body.len());
        assert!(error.to_string().contains(&count), "{operation}: {error}");
        assert!(
            !error.to_string().contains(marker),
            "{operation}: the refused body was echoed: {error}"
        );
    }
    assert_eq!(
        tools
            .call("health", &args)
            .await
            .expect("a body of whitespace is an empty one"),
        Value::Null
    );
    // A status the spec declares a JSON body for is checked against that
    // schema as before: the reply that fits passes, the one that would fit
    // another response does not.
    assert_eq!(
        tools
            .call("submit", &args)
            .await
            .expect("a 200 that fits its own schema"),
        json!({ "result": "ok" })
    );
    let error = tools
        .call("submit", &args)
        .await
        .expect_err("a 200 that fits another response");
    assert!(
        matches!(&error, CallError::Response { status: 200, response, .. } if response == "200"),
        "{error}"
    );
}

// Three responses declare content atomic takes no schema from: a wildcard
// that names none, a media type it does not read, and a JSON media type that
// names no schema. Each declares a body atomic has no schema to check, so
// what comes back is the caller's, unchecked. The `/text` one spells its plain
// text out as `type: string`, and that schema describes a body atomic never
// reads: the reply it governs is a JSON body of any shape, and the object
// below is accepted where a string was written. A refusal here would be a
// claim that no body was declared, which is not what these say. A wildcard
// that does name a schema is checked, which is the test after this one.
#[tokio::test]
async fn a_body_on_content_atomic_gives_no_schema_is_passed_through() {
    let (host, _) = server(vec![
        (200, r#"{"wild": true}"#),
        (200, r#"{"open": 1}"#),
        (200, r#"{"plain": "yes"}"#),
    ])
    .await;
    let tools = jobs(&host);
    let args = json!({});
    assert_eq!(
        tools
            .call("wildopen", &args)
            .await
            .expect("`*/*` with no schema declares content"),
        json!({ "wild": true })
    );
    assert_eq!(
        tools
            .call("open", &args)
            .await
            .expect("a JSON media type with no schema"),
        json!({ "open": 1 })
    );
    assert_eq!(
        tools.call("text", &args).await.expect("text/plain"),
        json!({ "plain": "yes" })
    );
}

// A response whose schema sits under a wildcard media type — `*/*` or
// `application/*`, where no JSON media type says otherwise — declares a body
// that may be JSON and says which shape it has, so that shape is atomic's to
// check. A body that does not fit is refused as the response violation it is
// and never reaches the caller; a body that is not JSON is refused the way an
// unparseable body under a JSON media type always has been. Where the spec
// declares a specific JSON media type as well, that one governs: the reply
// that fits it passes even though it breaks the wildcard's schema.
#[tokio::test]
async fn a_response_declared_under_a_wildcard_media_type_is_checked() {
    let (host, _) = server(vec![
        (200, r#"{"checked": 1}"#),    // `*/*`: fits
        (200, r#"{"ignored": true}"#), // `*/*`: fits none of it
        (200, "not json at all"),      // `*/*`: not a body at all
        (200, r#"{"checked": []}"#),   // `application/*`: fits
        (200, r#"{"checked": 1}"#),    // `application/*`: the wrong shape
        (200, "checked"),              // `application/*`: not JSON
        (200, r#"{"specific": 1}"#),   // both: the specific one governs
        (200, r#"{"unfit": 1}"#),      // both: and it is still held to it
    ])
    .await;
    let tools = jobs(&host);
    let args = json!({});
    assert_eq!(
        tools
            .call("anything", &args)
            .await
            .expect("a `*/*` body that fits"),
        json!({ "checked": 1 })
    );
    // A body that breaks the schema under `*/*` is the response violation it
    // would be under `application/json`: named by the response it was
    // checked against and by what that schema wanted, with none of the body
    // echoed back.
    let error = tools
        .call("anything", &args)
        .await
        .expect_err("a `*/*` body that does not fit");
    assert!(
        matches!(&error, CallError::Response { status: 200, response, .. } if response == "200"),
        "{error}"
    );
    let said = error.to_string();
    assert!(
        said.contains("its response `200`") && said.contains("checked"),
        "{said}"
    );
    assert!(!said.contains("ignored"), "the body was echoed: {said}");
    // A body that is not JSON is refused rather than passed through, the way
    // an unparseable body under a JSON media type always has been.
    let error = tools
        .call("anything", &args)
        .await
        .expect_err("a `*/*` body that is not JSON");
    assert!(matches!(&error, CallError::Transport { .. }), "{error}");
    assert!(error.to_string().contains("not JSON"), "{error}");
    // `application/*` the same way: its schema says `checked` is an array,
    // so a body carrying a number fails on that property, and a body of
    // bytes that are not JSON is refused.
    assert_eq!(
        tools
            .call("stream", &args)
            .await
            .expect("an `application/*` body that fits"),
        json!({ "checked": [] })
    );
    let error = tools
        .call("stream", &args)
        .await
        .expect_err("an `application/*` body that does not fit");
    assert!(
        matches!(&error, CallError::Response { status: 200, response, .. } if response == "200"),
        "{error}"
    );
    let said = error.to_string();
    assert!(said.contains("/checked"), "{said}");
    let error = tools
        .call("stream", &args)
        .await
        .expect_err("an `application/*` body that is not JSON");
    assert!(matches!(&error, CallError::Transport { .. }), "{error}");
    assert!(error.to_string().contains("not JSON"), "{error}");
    // Where the spec declares a specific JSON media type beside the
    // wildcard, the specific one governs and the wildcard's schema is never
    // consulted: a reply that fits the specific one passes although it
    // breaks the wildcard's, and a refusal says what the specific one
    // wanted, never what the wildcard's wanted.
    assert_eq!(
        tools
            .call("either", &args)
            .await
            .expect("the specific JSON schema fits"),
        json!({ "specific": 1 })
    );
    let error = tools
        .call("either", &args)
        .await
        .expect_err("the specific JSON schema does not fit");
    assert!(
        matches!(&error, CallError::Response { status: 200, response, .. } if response == "200"),
        "{error}"
    );
    let said = error.to_string();
    assert!(said.contains("specific"), "{said}");
    assert!(
        !said.contains("never_governs") && !said.contains("unfit"),
        "the wildcard's schema or the body was echoed: {said}"
    );
}

// A blank body is lenient only where nothing was declared. Where a response
// declares a JSON body it is read as JSON, and JSON whitespace is not a
// value — so a whitespace body is a transport failure even where `null`
// would have fitted the schema, as it always was. Form feed is not JSON
// whitespace at all: it is a body whether or not a content was declared.
#[tokio::test]
async fn a_blank_body_is_lenient_only_where_no_content_is_declared() {
    let (host, _) = server(vec![(200, "\n"), (200, "\u{0C}"), (200, "\u{0C}")]).await;
    let tools = jobs(&host);
    let args = json!({});
    // `nullable` declares `{ type: [object, "null"] }`: null fits it, and
    // still the whitespace reply is no value.
    for (what, operation) in [("a line feed", "nullable"), ("a form feed", "nullable")] {
        let error = tools
            .call(operation, &args)
            .await
            .expect_err(&format!("{what} is not a value"));
        assert!(
            matches!(&error, CallError::Transport { .. }),
            "{what}: {error}"
        );
        assert!(error.to_string().contains("not JSON"), "{what}: {error}");
    }
    // Where nothing was declared, JSON whitespace counts as no body and a
    // form feed does not: it is a body, and a body on a response that
    // declares none is the refusal.
    let error = tools
        .call("health", &args)
        .await
        .expect_err("a form feed is a body");
    assert!(
        matches!(&error, CallError::Response { status: 200, response, .. } if response == "200"),
        "{error}"
    );
    assert!(error.to_string().contains("no body"), "{error}");
}

// `default` is a response like any other, and every success no code or range
// declares selects it. A contentless `default` therefore refuses an
// undeclared success that comes back with a body, which is the restrictive
// reading and is kept on purpose: the selected response said there would be
// no body.
#[tokio::test]
async fn a_contentless_default_refuses_a_body_on_an_undeclared_success() {
    let (host, _) = server(vec![(201, r#"{"result": "ok"}"#), (203, "  ")]).await;
    let tools = jobs(&host);
    let args = json!({});
    let error = tools
        .call("fallback", &args)
        .await
        .expect_err("201 falls back to a contentless default");
    assert!(
        matches!(&error, CallError::Response { status: 201, response, .. } if response == "default"),
        "{error}"
    );
    let said = "returned 201, which does not fit its response `default`";
    assert!(error.to_string().contains(said), "{error}");
    assert!(
        error.to_string().contains("and 16 bytes were returned"),
        "{error}"
    );
    assert!(!error.to_string().contains("result"), "{error}");
    // The same fallback with a body that carries nothing is still no result.
    assert_eq!(
        tools
            .call("fallback", &args)
            .await
            .expect("a blank body is no body"),
        Value::Null
    );
}

// A response may declare its body's shape under several JSON media types, and
// a reply says which of them it is held to by the media type it names: the
// schemas are different, so each type's reply is checked against its own and a
// reply that fits another's does not pass. A media type is compared as one is
// compared — type and subtype whatever their case, parameters such as
// `charset` ignored, in the media type the spec wrote as well as the one the
// reply names. A reply that names a media type the response does not declare,
// or names none, is held to the response's only schema where it has exactly
// one, and refused where it has several, since nothing then says which governs
// it. The refusal names the media types the response declares, never the body;
// a reply of some other media type is held to no schema of its own, and a
// wildcard beside specific ones governs nothing.
#[tokio::test]
async fn a_reply_is_checked_against_the_schema_its_content_type_names() {
    // The headers a reply names itself by, where that is not plain JSON.
    const PROBLEM_JSON: &str = "content-type: application/problem+json\r\n";
    const JSON_UTF8: &str = "content-type: APPLICATION/JSON; charset=UTF-8\r\n";
    const TEXT_PLAIN: &str = "content-type: text/plain\r\n";
    const NO_TYPE: &str = "";
    let (host, _) = server_at(vec![
        // `/problem` declares two JSON media types, each with its own schema.
        (200, JSON_HEADER, r#"{"thing": "a"}"#), // fits the one it names
        (200, PROBLEM_JSON, r#"{"title": "gone"}"#), // fits the one it names
        (200, PROBLEM_JSON, r#"{"thing": "a"}"#), // fits the other one's only
        (200, JSON_HEADER, r#"{"title": "gone"}"#), // the same, the other way
        // `/param` declares one schema, under `application/json; charset=utf-8`.
        (200, JSON_UTF8, r#"{"count": 1}"#), // its parameters and case ignored
        (200, JSON_UTF8, r#"{"count": "one"}"#), // and still held to that schema
        (200, NO_TYPE, r#"{"count": 7}"#),   // names none: the only schema governs
        (200, TEXT_PLAIN, r#"{"count": 8}"#), // names another: the same fallback
        // `/twospelled` declares two, one of them written with parameters.
        (200, PROBLEM_JSON, r#"{"title": "gone"}"#), // its own, not the other's
        (200, PROBLEM_JSON, r#"{"title": 1}"#),      // and it is held to that one
        (200, JSON_UTF8, r#"{"count": 3}"#),         // the type written with parameters
        (200, NO_TYPE, r#"{"anything": 1}"#),        // names none, several are declared
        (200, TEXT_PLAIN, r#"{"anything": 1}"#),     // names one it does not declare
        // `/anytype` declares those two media types and a wildcard beside them.
        (200, PROBLEM_JSON, r#"{"title": "gone"}"#), // the specific one governs
        (200, NO_TYPE, r#"{"anything": 1}"#),        // and the wildcard settles nothing
    ])
    .await;
    let tools = jobs(&host);
    let args = json!({});
    // Each declared media type's reply is checked against that media type's own
    // schema: the body that fits it passes, and the one that fits only the other
    // declared type's schema is refused for what its own wanted.
    assert_eq!(
        tools
            .call("problem", &args)
            .await
            .expect("an `application/json` reply that fits"),
        json!({ "thing": "a" })
    );
    assert_eq!(
        tools
            .call("problem", &args)
            .await
            .expect("an `application/problem+json` reply that fits"),
        json!({ "title": "gone" })
    );
    for (what, wanted, absent) in [
        (
            "an `application/problem+json` reply",
            "\"title\" is a required property",
            "\"thing\"",
        ),
        (
            "an `application/json` reply",
            "\"thing\" is a required property",
            "\"title\"",
        ),
    ] {
        let error = tools
            .call("problem", &args)
            .await
            .expect_err(&format!("{what} whose body fits the other schema"));
        assert!(
            matches!(&error, CallError::Response { status: 200, response, .. } if response == "200"),
            "{what}: {error}"
        );
        let said = error.to_string();
        assert!(said.contains(wanted), "{what}: {said}");
        assert!(
            !said.contains(absent),
            "{what}: checked against the other media type's schema: {said}"
        );
    }
    // One schema, declared under a media type written with parameters: a reply
    // that names the same type with its own parameters, in its own case, is that
    // media type and is held to that schema.
    assert_eq!(
        tools
            .call("param", &args)
            .await
            .expect("`APPLICATION/JSON; charset=UTF-8` is `application/json`"),
        json!({ "count": 1 })
    );
    let error = tools
        .call("param", &args)
        .await
        .expect_err("the same media type, a body that does not fit");
    assert!(
        matches!(&error, CallError::Response { status: 200, .. }),
        "{error}"
    );
    assert!(error.to_string().contains("/count"), "{error}");
    // A reply naming no media type, or another one, is held to the only schema
    // the response declares.
    assert_eq!(
        tools
            .call("param", &args)
            .await
            .expect("names no media type, declares one schema"),
        json!({ "count": 7 })
    );
    assert_eq!(
        tools
            .call("param", &args)
            .await
            .expect("names `text/plain`, declares one schema"),
        json!({ "count": 8 })
    );
    // Two schemas, one of them declared under a media type written with
    // parameters: a reply matches either, on what the two say once their
    // parameters are dropped and their case folded.
    assert_eq!(
        tools
            .call("twospelled", &args)
            .await
            .expect("the media type the spec wrote plain"),
        json!({ "title": "gone" })
    );
    let error = tools
        .call("twospelled", &args)
        .await
        .expect_err("that media type, a body that breaks its schema");
    assert!(
        matches!(&error, CallError::Response { status: 200, .. }),
        "{error}"
    );
    assert!(error.to_string().contains("/title"), "{error}");
    assert_eq!(
        tools
            .call("twospelled", &args)
            .await
            .expect("the media type the spec wrote with parameters"),
        json!({ "count": 3 })
    );
    // Where the response declares several schemas and the reply matches none,
    // nothing says which governs: the reply is refused, and what is said names
    // the media types the response declares, never the body.
    for (what, says) in [
        (
            "names no media type",
            "its reply names no media type, which its response declares no schema for",
        ),
        (
            "names `text/plain`",
            "its reply is `text/plain`, which its response declares no schema for",
        ),
    ] {
        let error = tools.call("twospelled", &args).await.expect_err(&format!(
            "a reply that {what} while two schemas are declared"
        ));
        assert!(
            matches!(&error, CallError::Response { status: 200, response, .. } if response == "200"),
            "{what}: {error}"
        );
        let said = error.to_string();
        assert!(said.contains(says), "{what}: {said}");
        assert!(
            said.contains("it declares application/json; charset=utf-8, application/problem+json"),
            "{what}: the media types it declares are not said: {said}"
        );
        assert!(
            !said.contains("anything"),
            "{what}: the body was echoed: {said}"
        );
    }
    // Beside specific media types a wildcard governs nothing: the reply of one
    // declared type is held to that type's schema, which asks for something the
    // wildcard's does not.
    assert_eq!(
        tools
            .call("anytype", &args)
            .await
            .expect("the specific schema, not the wildcard's"),
        json!({ "title": "gone" })
    );
    // And a reply naming no media type is not settled by the wildcard either:
    // the two JSON schemas are still two, and the refusal says those two.
    let error = tools
        .call("anytype", &args)
        .await
        .expect_err("a wildcard does not make several schemas one");
    assert!(
        matches!(&error, CallError::Response { status: 200, response, .. } if response == "200"),
        "{error}"
    );
    let said = error.to_string();
    assert!(
        said.contains("it declares application/json, application/problem+json"),
        "{said}"
    );
    assert!(
        !said.contains("application/*") && !said.contains("anything"),
        "the wildcard was said or the body echoed: {said}"
    );
}

// A response may declare two bodies and describe the shape of only one of
// them for atomic to check: the thing it returns as JSON, and the plain text a
// server falls back to. A reply that names the media type declared with nothing
// beside it is not held to the other media type's schema — a wildcard's beside
// it most of all — because the spec declared that body and gave atomic no
// schema to check it against. So it is passed through with whatever body it
// carries, the same answer a reply of a response declaring no schema at all
// gets. The same answer goes to a media type that does name a schema,
// `text/plain` of `type: string`, which describes a body atomic never reads;
// the test after this one is that reply. It is not treated as some other media
// type either: a reply naming a media type the response never mentions is
// a different reply, which keeps falling to the response's only schema, declared
// under a wildcard or not, and a reply naming the media type that does have a
// schema is held to it as it always was.
#[tokio::test]
async fn a_reply_naming_a_declared_media_type_with_no_schema_accepts_any_body() {
    const TEXT_PLAIN: &str = "content-type: text/plain\r\n";
    const JSON: &str = "content-type: application/json\r\n";
    let (host, _) = server_at(vec![
        // `/declared`: the media type it declares with no schema, under its
        // `200` (a JSON media type beside it) and its `201` (a wildcard), each
        // carrying a body that would break the schema of the other one.
        (200, TEXT_PLAIN, r#"{"nope": 1}"#),
        (201, TEXT_PLAIN, r#"{"nope": 1}"#),
        // `/declared` again, naming the media type it does give a schema.
        (200, JSON, r#"{"nope": 1}"#),
        // `/wildcardtext`: one schema under a wildcard, no other media type.
        (200, TEXT_PLAIN, r#"{"nope": 1}"#),
        (200, JSON, r#"{"thing": "x"}"#),
        // `/declared` once more, its schemaless media type carrying bytes no
        // body atomic reads could be.
        (200, TEXT_PLAIN, "plain words"),
    ])
    .await;
    let tools = jobs(&host);
    let args = json!({});
    // The media type the response declares and keeps no schema for fits any
    // body, and neither the JSON schema beside its `200` nor the wildcard
    // beside its `201` is held to it.
    for status in [200u16, 201] {
        let said = tools
            .call("declared", &args)
            .await
            .unwrap_or_else(|error| panic!("a `text/plain` reply of status {status}: {error}"));
        assert_eq!(said, json!({ "nope": 1 }), "the body it carried");
    }
    // The media type it does describe keeps its contract: the same body of that
    // one is refused as the violation it is.
    let error = tools
        .call("declared", &args)
        .await
        .expect_err("a reply of the media type that does name a schema");
    assert!(
        matches!(&error, CallError::Response { status: 200, response, .. } if response == "200"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("\"thing\" is a required property"),
        "{error}"
    );
    // Where no media type is declared beside the wildcard, a reply naming one is
    // not the media type the response declares with nothing beside it — it is
    // one the response never mentions, and it falls to the wildcard's schema,
    // fit or not.
    let error = tools
        .call("wildcardtext", &args)
        .await
        .expect_err("`text/plain` is not declared, so the wildcard's schema governs");
    assert!(
        matches!(&error, CallError::Response { status: 200, response, .. } if response == "200"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("\"thing\" is a required property"),
        "{error}"
    );
    assert_eq!(
        tools
            .call("wildcardtext", &args)
            .await
            .expect("`application/json` is not declared either, and is checked"),
        json!({ "thing": "x" })
    );
    // Any body fits a schemaless media type as far as its shape goes; atomic
    // still reads bodies as JSON, and bytes that are not stay the failure they
    // have always been.
    let error = tools
        .call("declared", &args)
        .await
        .expect_err("a `text/plain` body that is not JSON");
    assert!(
        matches!(&error, CallError::Transport { operation, .. } if operation == "declared"),
        "{error}"
    );
    assert!(
        error.to_string().contains("the response is not JSON"),
        "{error}"
    );
}

// A media type the response declares with a schema atomic reads no body from —
// `text/plain`, whatever it names — is declared just as much as one named with
// no schema at all, and atomic holds no reply of it to another media type's
// schema. The spec says what the body is; it does not say it is JSON, and the
// schema beside it is for the JSON one. So such a reply fits any body: neither
// the JSON schema beside it, which refuses this one, nor the `text/plain`
// schema the spec wrote for it, which atomic keeps nowhere, governs it.
#[tokio::test]
async fn a_reply_naming_a_media_type_atomic_reads_no_body_from_is_any_body() {
    const TEXT_PLAIN: &str = "content-type: text/plain\r\n";
    const JSON: &str = "content-type: application/json\r\n";
    let (host, _) = server_at(vec![
        (200, TEXT_PLAIN, r#"{"nope": 1}"#),
        (200, JSON, r#"{"nope": 1}"#),
        (200, JSON, r#"{"thing": "x"}"#),
    ])
    .await;
    let tools = jobs(&host);
    let args = json!({});
    // An object, where the JSON schema beside it wants a `thing` and the
    // `text/plain` schema the spec wrote wants a string: it is held to neither,
    // and comes back as it came.
    assert_eq!(
        tools
            .call("unkept", &args)
            .await
            .unwrap_or_else(|error| panic!(
                "a reply of the media type atomic reads no body from: {error}"
            )),
        json!({ "nope": 1 })
    );
    // The other reply of the same operation is held to its own schema, which
    // the plain text one has nothing to say about.
    let error = tools
        .call("unkept", &args)
        .await
        .expect_err("`application/json` keeps its own schema");
    assert!(
        matches!(&error, CallError::Response { status: 200, response, .. } if response == "200"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("\"thing\" is a required property"),
        "{error}"
    );
    assert_eq!(
        tools
            .call("unkept", &args)
            .await
            .expect("the body that does fit the JSON schema"),
        json!({ "thing": "x" })
    );
}

// A refusal on the media type says what came back, and no more. What atomic
// names is the media type the reply wrote with its parameters dropped, so a
// `Content-Type` padded out with parameters contributes its type alone, and one
// whose own subtype runs past `MEDIA_ECHO_LIMIT` bytes is echoed cut short —
// never whole, and never the body beside it. Both replies name a media type the
// response declares no schema for, of the two it declares.
#[tokio::test]
async fn a_long_content_type_is_echoed_cut_short() {
    // A media type of its own and four parameters; then a subtype long enough
    // that the media type alone passes `MEDIA_ECHO_LIMIT`.
    const PARAMS: &str = "content-type: application/vnd.acme.echo+json; charset=x; \
                          charset=x; charset=x; charset=x\r\n";
    const LONG: &str = "content-type: application/vnd.acme.\
                        aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\
                        aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa+json\r\n";
    let (host, _) = server_at(vec![
        (200, PARAMS, r#"{"anything": 1}"#),
        (200, LONG, r#"{"anything": 1}"#),
    ])
    .await;
    let tools = jobs(&host);
    let args = json!({});
    // Parameters are dropped before the media type is named, however many
    // there are: what is said is the type, and none of what padded it.
    let says = "which its response declares no schema for; \
                it declares application/json, application/problem+json";
    let error = tools
        .call("twoschema", &args)
        .await
        .expect_err("a reply naming a media type it declares no schema for");
    let said = error.to_string();
    assert!(
        said.contains("its reply is `application/vnd.acme.echo+json`"),
        "{said}"
    );
    assert!(said.contains(says), "{said}");
    assert!(
        !said.contains("charset"),
        "the parameters were echoed: {said}"
    );
    assert!(!said.contains("anything"), "the body was echoed: {said}");
    // A media type longer than the limit on its own is echoed as far as the
    // limit, marked short, and never whole.
    let error = tools
        .call("twoschema", &args)
        .await
        .expect_err("a reply naming a media type too long to echo");
    let said = error.to_string();
    let whole = &LONG["content-type: ".len()..LONG.len() - "\r\n".len()];
    assert!(
        whole.len() > MEDIA_ECHO_LIMIT,
        "the test's media type is not long: {} bytes",
        whole.len()
    );
    assert!(
        said.contains(&format!("its reply is `{}…", &whole[..MEDIA_ECHO_LIMIT])),
        "the echo is not the first {MEDIA_ECHO_LIMIT} bytes of it: {said}"
    );
    assert!(said.contains(says), "{said}");
    assert!(!said.contains(whole), "the whole type was echoed: {said}");
    assert!(!said.contains("anything"), "the body was echoed: {said}");
}
