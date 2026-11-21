// SPDX-License-Identifier: Apache-2.0

//! The HTTP client a lite atom calls its model endpoint with never takes the
//! key anywhere the endpoint did not name. An endpoint on this machine over
//! plain `http` goes by no proxy, though `HTTP_PROXY` names one, so the key
//! never reaches the proxy in cleartext; an endpoint elsewhere still uses
//! it. And a redirect is never followed, so a downgrade from `https` to
//! `http` on the same host cannot resend the key.
//!
//! This binary sets the proxy variables for its whole process, so it holds
//! one test only.

#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// A server on this machine that answers every request with `response`,
/// and counts the requests it was sent.
fn serve(response: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let counted = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counted.fetch_add(1, Ordering::SeqCst);
            // Read the request head, then answer.
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                line.clear();
            }
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{address}"), seen)
}

#[test]
fn the_model_client_keeps_the_key_off_proxies_and_redirects() {
    let (proxy, proxied) = serve("HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n");
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
    let (endpoint, served) = serve("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
    let (elsewhere_to, followed) = serve("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
    let redirect: &'static str = Box::leak(
        format!("HTTP/1.1 302 Found\r\nlocation: {elsewhere_to}/v1\r\ncontent-length: 0\r\n\r\n")
            .into_boxed_str(),
    );
    let (redirecting, redirected) = serve(redirect);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let get = |base: String| async move {
            let client = atomic_core::lite::model_client(&base, Duration::from_secs(10)).unwrap();
            client
                .get(format!("{base}/responses"))
                .bearer_auth("sk-the-key")
                .send()
                .await
        };
        // On this machine, by address or by name: straight there.
        let port = endpoint.rsplit(':').next().unwrap().to_owned();
        for base in [
            format!("{endpoint}/v1"),
            format!("http://localhost:{port}/v1"),
        ] {
            let answer = get(base.clone()).await.unwrap();
            assert_eq!(answer.status(), 200, "{base}");
        }
        assert_eq!(served.load(Ordering::SeqCst), 2);
        assert_eq!(proxied.load(Ordering::SeqCst), 0, "no proxy saw the key");

        // Elsewhere, the proxy the environment names is used: what holds
        // the key back is the endpoint being on this machine.
        let answer = get("http://model.invalid/v1".to_owned()).await.unwrap();
        assert_eq!(answer.status(), 502);
        assert_eq!(proxied.load(Ordering::SeqCst), 1);

        // A redirect is answered, never followed.
        let answer = get(format!("{redirecting}/v1")).await.unwrap();
        assert_eq!(answer.status(), 302);
        assert_eq!(redirected.load(Ordering::SeqCst), 1);
        assert_eq!(
            followed.load(Ordering::SeqCst),
            0,
            "the redirect was not followed"
        );
    });
}
