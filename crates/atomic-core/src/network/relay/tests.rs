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
        Self::serving(answer, None).await
    }

    /// A stand-in that answers each connection's first bytes with
    /// `answer`, then closes it `after` the relay has closed its side, as a
    /// proxy closes a connection its client has left.
    async fn closing(answer: &'static [u8], after: Duration) -> Self {
        Self::serving(Some(answer), Some(after)).await
    }

    async fn serving(answer: Option<&'static [u8]>, closing: Option<Duration>) -> Self {
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
                    if let Some(after) = closing {
                        kept.lock().expect("kept").push((token, None));
                        let mut buf = [0u8; 1024];
                        while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
                        tokio::time::sleep(after).await;
                        return;
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

/// Answered connections can release their live permit while rama keeps
/// secondary address dials. A command cannot accumulate them indefinitely.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answered_connections_are_rate_limited_for_a_connect_lifetime() {
    let stand = Stand::closing(
        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        Duration::ZERO,
    )
    .await;
    let relay = Relay::start(
        stand.addr,
        timeouts(Duration::from_secs(1), Duration::from_secs(5)),
    )
    .await
    .expect("starts");
    let _command = relay
        .admit("relay-one".into(), "proxy-one".into(), 1, Box::new(()))
        .expect("admitted");
    for count in 1..=CONNECTIONS_PER_LINGER as usize {
        let mut connection = open(relay.addr(), Some("relay-one")).await;
        connection
            .write_all(b"GET http://allowed.invalid/ HTTP/1.1\r\n\r\n")
            .await
            .expect("request");
        let mut answer = [0u8; 64];
        assert!(connection.read(&mut answer).await.expect("answer") > 0);
        drop(connection);
        assert_eq!(stand.until(count).await, count);
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let mut over = open(relay.addr(), Some("relay-one")).await;
    assert!(closed(&mut over, Duration::from_secs(2)).await);
    assert_eq!(stand.tokens().len(), CONNECTIONS_PER_LINGER as usize);
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

/// A relay whose one permit is taken by a `CONNECT` the stand-in answers
/// with `200` and whose upstream never answers; with when it was sent.
async fn an_unanswered_tunnel(stand: &Stand, linger: Duration) -> (Relay, Admission, Instant) {
    let relay = Relay::start(stand.addr, timeouts(Duration::from_secs(1), linger))
        .await
        .expect("starts");
    let one = relay
        .admit("relay-one".into(), "proxy-one".into(), 1, Box::new(()))
        .expect("admitted");
    let mut tunnel = open(relay.addr(), Some("relay-one")).await;
    let sent = Instant::now();
    tunnel
        .write_all(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n")
        .await
        .expect("sent");
    let mut answer = [0u8; 64];
    let n = tunnel.read(&mut answer).await.expect("answered");
    assert!(answer[..n].starts_with(b"HTTP/1.1 200"));
    tunnel.write_all(b"client hello").await.expect("sent");
    assert!(
        closed(&mut tunnel, Duration::from_secs(5)).await,
        "closed at the timeout"
    );
    assert!(
        sent.elapsed() < Duration::from_secs(3),
        "{:?}",
        sent.elapsed()
    );
    (relay, one, sent)
}

/// Whether the relay refuses a further connection for `relay-one`, as it
/// does with no permit free: closed at once, where one let through would
/// be left open waiting for the stand-in's answer.
async fn refused(relay: &Relay) -> bool {
    let mut more = open(relay.addr(), Some("relay-one")).await;
    closed(&mut more, Duration::from_secs(1)).await
}

/// A tunnel whose upstream never answers past the proxy's `200` is closed
/// at the timeout, and its permit is held until the proxy lets go of it
/// and the dial it may have started has ended: the linger since the
/// request was sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unanswered_tunnel_is_closed_at_the_timeout() {
    let stand = Stand::start(Some(b"HTTP/1.1 200 Connection established\r\n\r\n")).await;
    let linger = Duration::from_secs(4);
    let (relay, _one, sent) = an_unanswered_tunnel(&stand, linger).await;
    // The proxy still holds it: the permit is held.
    assert!(refused(&relay).await, "held while the proxy holds it");
    stand.let_go();
    // The proxy has let go, but the dial it may have started may still run.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(refused(&relay).await, "held for the linger");
    assert!(sent.elapsed() < linger, "{:?}", sent.elapsed());
    // Once the linger since the request has passed.
    tokio::time::sleep_until(sent + linger + Duration::from_millis(500)).await;
    let mut again = open(relay.addr(), Some("relay-one")).await;
    again
        .write_all(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n")
        .await
        .expect("sent");
    assert_eq!(stand.until(2).await, 2, "room again");
}

/// A tunnel closed at the timeout keeps its permit while the proxy holds
/// the connection, even past the linger, and gives it back once the proxy
/// lets go.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tunnel_closed_at_the_timeout_is_held_while_the_proxy_holds_it() {
    let stand = Stand::start(Some(b"HTTP/1.1 200 Connection established\r\n\r\n")).await;
    let linger = Duration::from_millis(1500);
    let (relay, _one, sent) = an_unanswered_tunnel(&stand, linger).await;
    tokio::time::sleep_until(sent + linger + Duration::from_secs(1)).await;
    assert!(refused(&relay).await, "held past the linger");
    stand.let_go();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut again = open(relay.addr(), Some("relay-one")).await;
    again
        .write_all(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n")
        .await
        .expect("sent");
    assert_eq!(stand.until(2).await, 2, "room again once the proxy let go");
}

/// The end of a command waits for its connections to let go, except on a
/// relay's own thread, where the relay could not let go while it waited.
/// Each command here holds a connection for the linger, past the wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_end_of_a_command_on_a_relays_thread_does_not_wait() {
    let stand = Stand::start(None).await;
    let relay = Relay::start(
        stand.addr,
        timeouts(Duration::from_secs(30), Duration::from_secs(30)),
    )
    .await
    .expect("starts");
    let mut waits = Vec::new();
    for (n, thread) in [(1, "a-command"), (2, RUNTIME_THREAD)] {
        let token = format!("relay-{n}");
        let admission = relay
            .admit(token.clone(), format!("proxy-{n}"), 1, Box::new(()))
            .expect("admitted");
        let mut stream = open(relay.addr(), Some(&token)).await;
        stream
            .write_all(b"GET http://example.com/ HTTP/1.1\r\n\r\n")
            .await
            .expect("sent");
        assert_eq!(stand.until(n).await, n);
        // Let the request reach the relay's count of what is owed.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let ended = std::thread::Builder::new()
            .name(thread.to_owned())
            .spawn(move || {
                let started = std::time::Instant::now();
                drop(admission);
                started.elapsed()
            })
            .expect("thread");
        let waited = tokio::task::spawn_blocking(move || ended.join().expect("ended"))
            .await
            .expect("joined");
        waits.push(waited);
    }
    let (command, relays) = (waits[0], waits[1]);
    assert!(
        command >= RELEASE_WAIT,
        "a command's thread waits: {command:?}"
    );
    assert!(
        relays < RELEASE_WAIT / 2,
        "a relay's thread does not: {relays:?}"
    );
}

/// A proxy that does not accept the relay's dial in time: the relay gives
/// up at its own connect timeout and closes the client's connection.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(unsafe_code)]
async fn the_relays_dial_to_the_proxy_times_out() {
    use std::os::fd::AsRawFd;
    // Linux drops further SYNs once this zero-backlog listener holds one connection.
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

    // An answer to a first request too short to tell names no host, so
    // no dial: it is the answer.
    let mut short = Exchange::default();
    short.sent(b"CON\r\n\r\n", now);
    short.received(b"HTTP/1.1 400 Bad Request\r\n\r\n");
    assert!(short.answered && short.owed.is_none() && !short.tunnel);
}

/// A first `CONNECT` read in pieces is told once enough of it has come:
/// its `200` is not the answer, and the upstream's bytes make it a tunnel.
#[test]
fn a_connect_split_across_reads_is_a_tunnels() {
    let now = Instant::now();
    let mut split = Exchange::default();
    split.sent(b"CONN", now);
    split.sent(b"ECT example.com:443 HTTP/1.1\r\n\r\n", now);
    split.received(b"HTTP/1.1 200 OK\r\n\r\n");
    assert!(
        split.owed.is_some() && !split.answered,
        "the 200 is not the answer"
    );
    split.received(b"\x16\x03\x03");
    assert!(split.tunnel, "the upstream's bytes make it a tunnel");
}

/// Only a first request that opens with `CONNECT` and its space is a
/// tunnel's: a method that only begins with it is a plain request, read
/// whole or in pieces, whose answer makes no tunnel.
#[test]
fn only_an_exact_connect_first_is_a_tunnel() {
    let now = Instant::now();
    for reads in [
        &[&b"CONNECTX http://example.com/ HTTP/1.1\r\n\r\n"[..]][..],
        &[
            &b"C"[..],
            &b"ONNECTX http://example.com/ HTTP/1.1\r\n\r\n"[..],
        ][..],
    ] {
        let mut plain = Exchange::default();
        for read in reads {
            plain.sent(read, now);
        }
        plain.received(b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\n\r\nbody");
        assert!(!plain.tunnel, "{} reads: no tunnel", reads.len());
        plain.sent(b"\x15\x03\x03", now);
        assert!(plain.owed.is_some(), "{} reads: still owes", reads.len());
    }
}

/// A tunnel is never told from a later request: one sent as the body of a
/// request the proxy answered with `100 Continue`, or after a response's
/// headers, may have its answer forged by an upstream's body. Such a
/// connection still owes what its client sends, and a cut holds it.
#[test]
fn a_tunnel_is_never_told_from_a_later_request() {
    let now = Instant::now();
    let later = now + Duration::from_secs(1);
    let routes: [(&[u8], &[u8]); 2] = [
        (
            b"POST http://example.com/ HTTP/1.1\r\nexpect: 100-continue\r\n\
              content-length: 64\r\n\r\n",
            b"HTTP/1.1 100 Continue\r\n\r\n",
        ),
        (
            b"GET http://example.com/ HTTP/1.1\r\n\r\n",
            b"HTTP/1.1 200 OK\r\ncontent-length: 64\r\n\r\n",
        ),
    ];
    for (n, (request, first)) in routes.into_iter().enumerate() {
        let mut forged = Exchange::default();
        forged.sent(request, now);
        forged.received(first);
        forged.sent(b"CONNECT example.com:81 HTTP/1.1\r\n\r\n", later);
        forged.received(b"HTTP/1.1 200 OK\r\n\r\nX");
        assert!(!forged.tunnel, "route {n}: no tunnel");
        assert_eq!(
            forged.held_since(true),
            Some(later),
            "route {n}: a cut holds it"
        );
        forged.sent(b"\x15\x03\x03", later);
        assert!(forged.owed.is_some(), "route {n}: still owes");
    }
}

/// A `200` whose first read is shorter than its status is still the
/// tunnel's, not its answer.
#[test]
fn a_200_read_in_pieces_is_not_the_answer() {
    let now = Instant::now();
    for split in 1..STATUS_LEN {
        let whole = b"HTTP/1.1 200 Connection established\r\n\r\n";
        let mut tunnel = Exchange::default();
        tunnel.sent(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n", now);
        tunnel.received(&whole[..split]);
        tunnel.received(&whole[split..]);
        assert_eq!(tunnel.refused, Some(false), "split at {split}");
        assert!(
            tunnel.owed.is_some() && !tunnel.answered,
            "split at {split}: the 200 is not the answer"
        );
        assert!(tunnel.overdue(Duration::ZERO, now), "split at {split}");
        tunnel.received(b"\x16\x03\x03");
        assert!(tunnel.answered, "split at {split}: the upstream's is");
    }
}

/// Once a tunnel's upstream has answered, what the client sends through
/// it owes nothing: no dial follows it. A refused `CONNECT`'s connection,
/// on which the proxy might still read a request, still owes.
#[test]
fn an_answered_tunnel_owes_nothing_more() {
    let now = Instant::now();
    let mut tunnel = Exchange::default();
    tunnel.sent(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n", now);
    tunnel.received(b"HTTP/1.1 200 OK\r\n\r\n\x16\x03\x03");
    assert!(tunnel.answered && tunnel.owed.is_none());
    // A TLS close_notify, which no reply follows.
    tunnel.sent(b"\x15\x03\x03\x00\x02\x01\x00", now);
    assert!(tunnel.owed.is_none(), "the tunnel owes nothing");

    let mut refused = Exchange::default();
    refused.sent(b"CONNECT example.com:22 HTTP/1.1\r\n\r\n", now);
    refused.received(b"HTTP/1.1 403 Forbidden\r\n\r\n");
    assert!(refused.answered && refused.owed.is_none());
    refused.sent(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n", now);
    assert!(refused.owed.is_some(), "a request after a refusal owes");

    // Before the upstream's first answer, the `CONNECT` itself is owed.
    let mut early = Exchange::default();
    early.sent(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n", now);
    early.received(b"HTTP/1.1 200 OK\r\n\r\n");
    early.sent(b"\x16\x03\x01", now);
    assert!(early.owed.is_some(), "the client hello owes");
}

/// A `CONNECT` after blank lines is a tunnel's: its `200` is not the
/// answer, since the proxy dials after it.
#[test]
fn a_connect_after_blank_lines_is_a_tunnels() {
    let now = Instant::now();
    let mut padded = Exchange::default();
    padded.sent(b"\r\nCONNECT example.com:443 HTTP/1.1\r\n\r\n", now);
    padded.received(b"HTTP/1.1 200 OK\r\n\r\n");
    assert!(
        padded.owed.is_some() && !padded.answered,
        "the 200 is not the answer"
    );
    padded.received(b"\x16\x03\x03");
    assert!(padded.answered && padded.tunnel, "the upstream's is");

    // Blank lines alone begin nothing; the request after them does.
    let mut apart = Exchange::default();
    apart.sent(b"\r\n", now);
    apart.sent(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n", now);
    apart.received(b"HTTP/1.1 200 OK\r\n\r\n");
    assert!(
        apart.owed.is_some() && !apart.answered,
        "the 200 is not the answer"
    );
}

/// A `CONNECT` sent after an earlier request on a connection kept alive,
/// or after a refused `CONNECT`, has its `200` read as no answer; but it
/// never makes the connection a tunnel (see
/// `a_tunnel_is_never_told_from_a_later_request`).
#[test]
fn a_connect_after_an_earlier_request_waits_for_its_upstream() {
    let now = Instant::now();
    for (earlier, answer) in [
        (
            &b"GET http://undeclared.invalid/ HTTP/1.1\r\n\r\n"[..],
            &b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n"[..],
        ),
        (
            &b"CONNECT undeclared.invalid:443 HTTP/1.1\r\n\r\n"[..],
            &b"HTTP/1.1 403 Forbidden\r\n\r\n"[..],
        ),
    ] {
        let mut kept = Exchange::default();
        kept.sent(earlier, now);
        kept.received(answer);
        assert!(kept.owed.is_none(), "the earlier request was answered");
        kept.sent(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n", now);
        kept.received(b"HTTP/1.1 200 OK\r\n\r\n");
        assert!(
            kept.owed.is_some() && !kept.tunnel,
            "the 200 is not the answer"
        );
        kept.received(b"\x16\x03\x03");
        assert!(kept.owed.is_none() && !kept.tunnel, "the upstream's is");
    }
}

/// A connection the relay cuts is held since what the client last sent,
/// whatever the proxy answered, even a tunnel the upstream has answered;
/// one the proxy let go of, only for a request left unanswered.
#[test]
fn a_cut_connection_is_held_since_its_last_send() {
    let now = Instant::now();
    let later = now + Duration::from_secs(1);
    let mut plain = Exchange::default();
    plain.sent(b"GET http://example.com/ HTTP/1.1\r\n\r\n", now);
    plain.received(b"HTTP/1.1 200 OK\r\n\r\nhello");
    plain.sent(b"GET http://example.com/again", later);
    plain.received(b"HTTP/1.1 200 OK\r\n\r\nhello");
    assert_eq!(plain.held_since(false), None);
    assert_eq!(plain.held_since(true), Some(later));

    let mut tunnel = Exchange::default();
    tunnel.sent(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n", now);
    tunnel.received(b"HTTP/1.1 200 OK\r\n\r\n\x16\x03\x03");
    tunnel.sent(b"\x15\x03\x03", later);
    assert!(tunnel.tunnel);
    assert_eq!(
        tunnel.held_since(false),
        None,
        "an answered tunnel owes nothing"
    );
    assert_eq!(tunnel.held_since(true), Some(later), "but a cut holds it");

    assert_eq!(Exchange::default().held_since(true), None, "nothing sent");
}

/// When a command ends while the proxy still holds a connection whose
/// request it answered, the relay closes it, and holds the command's room
/// for the linger since the client last sent: a dial may still run for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_cut_at_the_end_is_held_for_the_linger() {
    let stand = Stand::start(Some(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello")).await;
    let linger = Duration::from_secs(3);
    let relay = Relay::start(stand.addr, timeouts(Duration::from_secs(10), linger))
        .await
        .expect("starts");
    let room = Arc::new(());
    let one = relay
        .admit(
            "relay-one".into(),
            "proxy-one".into(),
            1,
            Box::new(room.clone()),
        )
        .expect("admitted");
    let mut kept = open(relay.addr(), Some("relay-one")).await;
    kept.write_all(b"GET http://example.com/ HTTP/1.1\r\n\r\n")
        .await
        .expect("sent");
    let sent = Instant::now();
    let mut answer = [0u8; 64];
    let n = kept.read(&mut answer).await.expect("answered");
    assert!(answer[..n].starts_with(b"HTTP/1.1 200"));
    let room = {
        let weak = Arc::downgrade(&room);
        drop(room);
        weak
    };
    tokio::task::spawn_blocking(move || drop(one))
        .await
        .expect("ended");
    assert!(
        closed(&mut kept, Duration::from_secs(2)).await,
        "closed at the end"
    );
    assert!(room.upgrade().is_some(), "room held for the linger");
    assert!(sent.elapsed() < linger, "{:?}", sent.elapsed());
    tokio::time::sleep_until(sent + linger + Duration::from_millis(500)).await;
    assert!(room.upgrade().is_none(), "room given back after it");
}

/// A connection whose client closed as its command ended is let go of by
/// the proxy within the grace the end gives it, and so is not held: the
/// command's room is given back at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_closed_as_the_command_ends_is_not_held() {
    let stand = Stand::closing(
        b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello",
        Duration::from_millis(50),
    )
    .await;
    let relay = Relay::start(
        stand.addr,
        timeouts(Duration::from_secs(10), Duration::from_secs(30)),
    )
    .await
    .expect("starts");
    let room = Arc::new(());
    let one = relay
        .admit(
            "relay-one".into(),
            "proxy-one".into(),
            1,
            Box::new(room.clone()),
        )
        .expect("admitted");
    let mut client = open(relay.addr(), Some("relay-one")).await;
    client
        .write_all(b"GET http://example.com/ HTTP/1.1\r\n\r\n")
        .await
        .expect("sent");
    let mut got = Vec::new();
    while !got.ends_with(b"hello") {
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("in time")
            .expect("answered");
        assert!(n > 0, "the whole answer");
        got.extend_from_slice(&buf[..n]);
    }
    let room = {
        let weak = Arc::downgrade(&room);
        drop(room);
        weak
    };
    // The command exits, closing its connection, and its run returns.
    drop(client);
    tokio::task::spawn_blocking(move || drop(one))
        .await
        .expect("ended");
    assert!(room.upgrade().is_none(), "room given back at the end");
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
