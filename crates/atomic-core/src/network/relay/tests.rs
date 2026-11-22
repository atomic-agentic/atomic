// SPDX-License-Identifier: Apache-2.0

//! The relay against a stand-in for Codex's proxy on this machine's
//! loopback interface, which records the token each connection opens with
//! and keeps the connection until the test lets it go.

use super::*;
use std::time::Duration;

/// Each connection a stand-in accepted: its token, and the connection while
/// it is kept.
type Kept = Arc<Mutex<Vec<(String, Option<TcpStream>)>>>;

/// A stand-in proxy: what it accepted, each connection's token with the
/// connection, kept open until it lets go.
struct Stand {
    addr: SocketAddr,
    accepted: Kept,
}

impl Stand {
    /// A stand-in that reads each connection's frame and, if `answer` is
    /// given, reads its first bytes and answers with it.
    async fn start(answer: Option<&'static [u8]>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let addr = listener.local_addr().expect("address");
        let accepted: Kept = Arc::default();
        let kept = accepted.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let kept = kept.clone();
                tokio::spawn(async move {
                    let Ok(token) = read_frame(&mut stream).await else {
                        kept.lock()
                            .expect("kept")
                            .push((String::new(), Some(stream)));
                        return;
                    };
                    if let Some(answer) = answer {
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(answer).await;
                    }
                    kept.lock().expect("kept").push((token, Some(stream)));
                });
            }
        });
        Self { addr, accepted }
    }

    /// The tokens of the connections it has accepted.
    fn tokens(&self) -> Vec<String> {
        self.accepted
            .lock()
            .expect("accepted")
            .iter()
            .map(|(t, _)| t.clone())
            .collect()
    }

    /// Wait until it has accepted `n` connections, or two seconds.
    async fn until(&self, n: usize) -> usize {
        let started = Instant::now();
        while self.tokens().len() < n && started.elapsed() < Duration::from_secs(2) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.tokens().len()
    }

    /// Let go of the first connection it still keeps.
    fn let_go(&self) {
        let mut accepted = self.accepted.lock().expect("accepted");
        let kept = accepted
            .iter_mut()
            .find(|(_, s)| s.is_some())
            .expect("one kept");
        kept.1 = None;
    }
}

/// A connection to `relay` that opens with `token`'s frame, or none.
async fn open(relay: SocketAddr, token: Option<&str>) -> TcpStream {
    let mut stream = TcpStream::connect(relay).await.expect("connects");
    let mut frame = Vec::new();
    match token {
        Some(token) => {
            codex_network_proxy::write_attribution_frame(&mut frame, token).expect("frame")
        }
        None => frame.extend_from_slice(b"GET http://allowed.invalid/ HTTP/1.1\r\n\r\n"),
    }
    stream.write_all(&frame).await.expect("frame sent");
    stream
}

/// Whether the relay closed `stream` within `within`.
async fn closed(stream: &mut TcpStream, within: Duration) -> bool {
    let mut buf = [0u8; 64];
    loop {
        match tokio::time::timeout(within, stream.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => return true,
            Ok(Ok(_)) => continue,
            Err(_) => return false,
        }
    }
}

fn timeouts(connect: Duration, linger: Duration) -> Timeouts {
    Timeouts { connect, linger }
}

/// A command holds no more connections than its limit: one more is closed
/// at once and never reaches the proxy, while another command keeps its
/// own. Each connection reaches the proxy with the proxy's token for its
/// command, never the relay's. Once the proxy lets go of one, the command
/// may open another.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_holds_no_more_connections_than_its_limit() {
    let stand = Stand::start(None).await;
    let relay = Relay::start(stand.addr, Timeouts::production())
        .await
        .expect("starts");
    let _one = relay
        .admit("relay-one".into(), "proxy-one".into(), 2, Box::new(()))
        .expect("admitted");
    let _two = relay
        .admit("relay-two".into(), "proxy-two".into(), 2, Box::new(()))
        .expect("admitted");
    let a = open(relay.addr(), Some("relay-one")).await;
    let b = open(relay.addr(), Some("relay-one")).await;
    assert_eq!(stand.until(2).await, 2);
    let mut over = open(relay.addr(), Some("relay-one")).await;
    assert!(
        closed(&mut over, Duration::from_secs(2)).await,
        "over the limit, closed at once"
    );
    let c = open(relay.addr(), Some("relay-two")).await;
    let d = open(relay.addr(), Some("relay-two")).await;
    assert_eq!(stand.until(4).await, 4, "the other command keeps its own");
    let mut tokens = stand.tokens();
    tokens.sort();
    assert_eq!(tokens, ["proxy-one", "proxy-one", "proxy-two", "proxy-two"]);

    // The client's going does not free a permit; the proxy's letting go does.
    drop(a);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut still = open(relay.addr(), Some("relay-one")).await;
    assert!(
        closed(&mut still, Duration::from_secs(2)).await,
        "held while the proxy holds it"
    );
    stand.let_go();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _e = open(relay.addr(), Some("relay-one")).await;
    assert_eq!(stand.until(5).await, 5, "room again once the proxy let go");
    drop((b, c, d));
}

/// A connection the relay cannot attribute to a running command is closed
/// before anything reaches the proxy: one with no frame, one with a token
/// the relay never issued, and one whose command has ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unattributed_connection_never_reaches_the_proxy() {
    let stand = Stand::start(None).await;
    let relay = Relay::start(stand.addr, Timeouts::production())
        .await
        .expect("starts");
    let running = relay
        .admit("relay-one".into(), "proxy-one".into(), 4, Box::new(()))
        .expect("admitted");
    let ended = relay
        .admit("relay-two".into(), "proxy-two".into(), 4, Box::new(()))
        .expect("admitted");
    drop(ended);
    for token in [None, Some("forged"), Some("relay-two")] {
        let mut stream = open(relay.addr(), token).await;
        assert!(
            closed(&mut stream, Duration::from_secs(5)).await,
            "{token:?} closed"
        );
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(stand.tokens(), Vec::<String>::new(), "nothing reached it");
    let _ok = open(relay.addr(), Some("relay-one")).await;
    assert_eq!(stand.until(1).await, 1);
    assert_eq!(stand.tokens(), ["proxy-one"]);
    drop(running);
}

/// A tunnel whose upstream never answers past the proxy's `200` is closed
/// at the timeout, and its permit is held until the proxy lets go of it
/// and the dial it may have started has ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unanswered_tunnel_is_closed_at_the_timeout() {
    let stand = Stand::start(Some(b"HTTP/1.1 200 Connection established\r\n\r\n")).await;
    let relay = Relay::start(
        stand.addr,
        timeouts(Duration::from_secs(1), Duration::from_secs(2)),
    )
    .await
    .expect("starts");
    let _one = relay
        .admit("relay-one".into(), "proxy-one".into(), 1, Box::new(()))
        .expect("admitted");
    let mut tunnel = open(relay.addr(), Some("relay-one")).await;
    tunnel
        .write_all(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n")
        .await
        .expect("sent");
    let mut answer = [0u8; 64];
    let n = tunnel.read(&mut answer).await.expect("answered");
    assert!(answer[..n].starts_with(b"HTTP/1.1 200"));
    tunnel.write_all(b"client hello").await.expect("sent");
    let started = Instant::now();
    assert!(
        closed(&mut tunnel, Duration::from_secs(5)).await,
        "closed at the timeout"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    // The proxy still holds it: the permit is held.
    let mut more = open(relay.addr(), Some("relay-one")).await;
    assert!(closed(&mut more, Duration::from_secs(2)).await);
    stand.let_go();
    // Once the proxy has let go and the linger since the request has passed.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut again = open(relay.addr(), Some("relay-one")).await;
    again
        .write_all(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n")
        .await
        .expect("sent");
    assert_eq!(stand.until(2).await, 2, "room again");
}

/// A proxy that does not accept the relay's dial in time: the relay gives
/// up at its own connect timeout and closes the client's connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(unsafe_code)]
async fn the_relays_dial_to_the_proxy_times_out() {
    use std::os::fd::AsRawFd;
    // A listener with no room in its queue drops every SYN past the first.
    let full = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    // SAFETY: listen on a socket this test owns.
    assert_eq!(unsafe { libc::listen(full.as_raw_fd(), 0) }, 0);
    let addr = full.local_addr().expect("address");
    let _queued = std::net::TcpStream::connect(addr).expect("the queue's one");
    let relay = Relay::start(
        addr,
        timeouts(Duration::from_secs(1), Duration::from_secs(1)),
    )
    .await
    .expect("starts");
    let _one = relay
        .admit("relay-one".into(), "proxy-one".into(), 1, Box::new(()))
        .expect("admitted");
    let started = Instant::now();
    let mut stream = open(relay.addr(), Some("relay-one")).await;
    assert!(
        closed(&mut stream, Duration::from_secs(5)).await,
        "closed at the timeout"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

/// What counts as the proxy's answer: past a `CONNECT`'s `200`, the
/// upstream's bytes; a refused `CONNECT`'s answer, and a plain request's
/// response, themselves.
#[test]
fn only_an_upstreams_bytes_answer_a_tunnel() {
    let now = Instant::now();
    let mut tunnel = Exchange::default();
    tunnel.sent(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n", now);
    tunnel.received(b"HTTP/1.1 200 Connection");
    tunnel.received(b" established\r\n\r");
    assert!(tunnel.owed.is_some() && !tunnel.answered, "the 200 is not");
    tunnel.received(b"\n");
    assert!(tunnel.owed.is_some() && !tunnel.answered, "nor its end");
    tunnel.received(b"\x16\x03\x03");
    assert!(
        tunnel.owed.is_none() && tunnel.answered,
        "the upstream's is"
    );

    let mut joined = Exchange::default();
    joined.sent(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n", now);
    joined.received(b"HTTP/1.1 200 OK\r\n\r\n\x16\x03\x03");
    assert!(joined.answered, "in one read");

    let mut refused = Exchange::default();
    refused.sent(b"CONNECT example.com:22 HTTP/1.1\r\n\r\n", now);
    refused.received(b"HTTP/1.1 403 Forbidden\r\n\r\n");
    assert!(refused.answered, "a refusal dialled nothing");

    let mut plain = Exchange::default();
    plain.sent(b"GET http://example.com/ HTTP/1.1\r\n\r\n", now);
    assert!(plain.overdue(Duration::ZERO, now));
    plain.received(b"HTTP/1.1 200 OK\r\n");
    assert!(plain.answered && !plain.overdue(Duration::ZERO, now));
    plain.sent(b"GET http://example.com/again HTTP/1.1\r\n\r\n", now);
    assert!(plain.owed.is_some(), "a second request is owed");

    // A short first read that could begin a CONNECT is taken for one.
    let mut short = Exchange::default();
    short.sent(b"CON", now);
    short.received(b"HTTP/1.1 200 OK\r\n\r\n");
    assert!(!short.answered);
}

/// The kernel's connect timeout, from `tcp_syn_retries`: the most it may
/// be when that cannot be read.
#[test]
fn the_linger_is_the_kernels_connect_timeout() {
    assert_eq!(
        connect_lifetime(Some("6\n".into())),
        Duration::from_secs(130)
    );
    assert_eq!(connect_lifetime(Some("0".into())), Duration::from_secs(4));
    let most = connect_lifetime(None);
    assert!(most > Duration::from_secs(3600), "{most:?}");
    for unreadable in ["", "six", "-1", "128"] {
        assert_eq!(
            connect_lifetime(Some(unreadable.into())),
            most,
            "{unreadable:?}"
        );
    }
}

/// The retries are read from a file; one that cannot be read for any
/// reason, a file without permission, a directory or a link to itself,
/// counts as unreadable, and so as the longest timeout. Each holds `6`
/// where it can, so a reader that ignored the error, or ran as root, would
/// get the shorter one.
#[test]
fn an_unreadable_retries_file_is_the_longest_timeout() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/atomic-relay-tests")
        .join(format!("retries-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    let readable = dir.join("readable");
    std::fs::write(&readable, "6\n").expect("written");
    assert_eq!(connect_lifetime_in(&readable), Duration::from_secs(130));
    let closed = dir.join("closed");
    std::fs::write(&closed, "6\n").expect("written");
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).expect("mode");
    let looped = dir.join("looped");
    std::os::unix::fs::symlink(&looped, &looped).expect("link");
    let most = connect_lifetime(None);
    for unreadable in [closed.clone(), dir.clone(), looped, dir.join("absent")] {
        assert_eq!(
            connect_lifetime_in(&unreadable),
            most,
            "{}",
            unreadable.display()
        );
    }
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o600)).expect("mode");
    let _ = std::fs::remove_dir_all(&dir);
}
