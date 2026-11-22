// SPDX-License-Identifier: Apache-2.0

//! An HTTP toolset's call never takes its credential anywhere the toolset
//! did not name. A service on this machine over plain `http` is reached
//! directly, though `HTTP_PROXY` names a proxy, so the credential never
//! reaches the proxy in cleartext; a service elsewhere still goes by it. And
//! a redirect is never followed, so the credential is not sent on.
//!
//! This binary sets the proxy variables for its whole process, so it holds
//! one test only.

#![allow(clippy::unwrap_used)]

use atomic_core::http_tools::{CallError, HttpToolset};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A server on this machine that answers every request with `response`,
/// and keeps the head of each request it was sent.
fn serve(response: String) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // The request head, then its body, read whole before answering.
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut line = String::new();
            let mut length = 0;
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
                line.clear();
            }
            let _ = reader.read_exact(&mut vec![0; length]);
            log.lock().unwrap().push(head);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (port, seen)
}

const SPEC: &str = r##"openapi: 3.1.0
info: { title: Forecast, version: "1" }
paths:
  /v1/forecast:
    get:
      operationId: getForecast
      responses:
        "200": { description: ok, content: { application/json: { schema: { type: object } } } }
"##;

/// The toolset at `base`, allowed `host`, carrying a credential.
fn toolset(base: &str, host: &str) -> HttpToolset {
    let spec = atomic_schema::yaml::load(std::path::Path::new("spec.yaml"), SPEC)
        .unwrap()
        .value;
    let operations = atomic_schema::openapi::operations(&spec)
        .unwrap()
        .into_iter()
        .map(|op| (op.name.clone(), op))
        .collect();
    HttpToolset {
        base_url: base.parse().unwrap(),
        allow: vec![host.to_owned()],
        operations,
        headers: vec![("Authorization".into(), "Bearer sk-toolset-secret".into())],
        attempts: 1,
        timeout: Duration::from_secs(10),
    }
}

#[test]
fn a_toolset_call_keeps_its_credential_off_proxies_and_redirects() {
    let (proxy, proxied) =
        serve("HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n".to_owned());
    let proxy = format!("http://127.0.0.1:{proxy}");
    // SAFETY: this binary holds this one test, and the only other thread so
    // far is the proxy's accept loop, which never reads the environment; the
    // variables are set before any client or other thread is started.
    #[allow(unsafe_code)]
    unsafe {
        for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
            std::env::set_var(name, &proxy);
        }
        for name in ["NO_PROXY", "no_proxy"] {
            std::env::remove_var(name);
        }
    }
    let (service, served) =
        serve("HTTP/1.1 200 OK\r\ncontent-length: 12\r\n\r\n{\"ok\": true}".to_owned());
    let (elsewhere, followed) = serve("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}".to_owned());
    let (redirecting, redirected) = serve(format!(
        "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{elsewhere}/api/v1/forecast\r\n\
         content-length: 0\r\n\r\n"
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // On this machine, by address or by name: straight there, with the
        // credential.
        for (base, host) in [
            (
                format!("http://127.0.0.1:{service}/api/"),
                format!("127.0.0.1:{service}"),
            ),
            (
                format!("http://localhost:{service}/api/"),
                format!("localhost:{service}"),
            ),
        ] {
            let out = toolset(&base, &host)
                .call("getForecast", &json!({}))
                .await
                .unwrap_or_else(|e| panic!("{base}: {e}"));
            assert_eq!(out, json!({"ok": true}), "{base}");
        }
        let heads = served.lock().unwrap().clone();
        assert_eq!(heads.len(), 2, "{heads:?}");
        for head in &heads {
            assert!(
                head.to_ascii_lowercase()
                    .contains("authorization: bearer sk-toolset-secret"),
                "{head}"
            );
        }
        assert!(
            proxied.lock().unwrap().is_empty(),
            "no proxy saw the credential"
        );

        // Elsewhere, the proxy the environment names is used: what holds
        // the credential back is the service being on this machine. The
        // proxy answers itself, so nothing reaches this private address.
        let error = toolset("http://10.255.255.1/api/", "10.255.255.1")
            .call("getForecast", &json!({}))
            .await
            .expect_err("the proxy's 502");
        assert!(
            matches!(error, CallError::Status { status: 502, .. }),
            "{error}"
        );
        assert_eq!(proxied.lock().unwrap().len(), 1);

        // A redirect is answered, never followed.
        let error = toolset(
            &format!("http://127.0.0.1:{redirecting}/api/"),
            &format!("127.0.0.1:{redirecting}"),
        )
        .call("getForecast", &json!({}))
        .await
        .expect_err("the redirect");
        assert!(
            matches!(error, CallError::Status { status: 302, .. }),
            "{error}"
        );
        assert_eq!(redirected.lock().unwrap().len(), 1);
        assert!(
            followed.lock().unwrap().is_empty(),
            "the redirect was not followed"
        );
    });
}
