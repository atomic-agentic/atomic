// SPDX-License-Identifier: Apache-2.0

//! The relay in front of a running proxy, which holds each command to its
//! own share of connections.
//!
//! A confined command's bridge connects to the relay, never to Codex's
//! proxy, and opens each connection with the command's token (Codex's
//! attribution frame). The relay reads that frame, finds the command the
//! token was issued to, and takes one of the command's permits: at most
//! [`DEFAULT_CONNECTIONS_PER_COMMAND`] by default, or the number the
//! command's settings give. Only then does it dial the proxy, under
//! [`CONNECT_TIMEOUT`], and open the connection there with a second token,
//! one the proxy knows and the command never sees, so no process can reach
//! the proxy for the command except through the relay. A connection
//! without a frame, with a token the relay did not issue or whose command
//! has ended, or over its command's limit, is closed at once and never
//! reaches the proxy.
//!
//! A permit is held until the proxy has let go of the connection, or the
//! command has ended, not just until the client has gone: Codex's proxy
//! answers `CONNECT` before it dials, and keeps a connection, and the
//! descriptors of its dial, until that dial ends. A request the proxy never
//! answered may have left a dial running even after the proxy closed the
//! connection, so its permit is held until the kernel would have given
//! that dial up ([`Timeouts::linger`]), whether or not the command has
//! ended; so is the command's room among the process's descriptors. A
//! connection whose first request the proxy has not answered within
//! [`CONNECT_TIMEOUT`] is closed: for a `CONNECT` tunnel, bytes past the
//! proxy's `200` answer, which the upstream sends once the proxy's dial
//! reached it; for a plain request, the first bytes of its response.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::time::Instant;

/// How many connections one command may hold through its proxy at once
/// when its settings say nothing else (`exec.connections_per_command`).
pub const DEFAULT_CONNECTIONS_PER_COMMAND: u32 = 64;

/// The most a command may be allowed: the sandbox helper's bridge opens no
/// more than this many connections at once for a command anyway.
pub const MAX_CONNECTIONS_PER_COMMAND: u32 = 128;

/// How long the relay waits for the proxy to accept a connection, and how
/// long a connection's first request may go unanswered before it is closed.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a connection has to send its attribution frame, as Codex's
/// proxy allows.
const FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// Codex's attribution frame: this magic, a big-endian `u16` length and a
/// token of at most [`MAX_TOKEN`] bytes.
const FRAME_MAGIC: &[u8; 8] = b"\0CDXPXY1";
const MAX_TOKEN: usize = 128;

/// The most connections, across every relay in this process, whose frame
/// is still being read. Only a process outside any sandbox can hold one
/// open without a frame: a bridge sends its frame at once.
pub const PENDING_FRAMES: usize = 128;
static PENDING: Semaphore = Semaphore::const_new(PENDING_FRAMES);

/// The relay's timeouts.
#[derive(Clone, Copy, Debug)]
pub(super) struct Timeouts {
    /// The relay's own dial to the proxy, and the wait for a connection's
    /// first answer.
    pub(super) connect: Duration,
    /// How long a dial the proxy started may run after the request that
    /// started it: the kernel's own connect timeout.
    pub(super) linger: Duration,
}

impl Timeouts {
    /// [`CONNECT_TIMEOUT`], and the kernel's connect timeout as this
    /// machine's `tcp_syn_retries` sets it.
    pub(super) fn production() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            linger: connect_lifetime_in(std::path::Path::new("/proc/sys/net/ipv4/tcp_syn_retries")),
        }
    }
}

/// [`connect_lifetime`] for the retries `file` holds; any error reading it,
/// not only its absence, counts as unreadable.
pub(super) fn connect_lifetime_in(file: &std::path::Path) -> Duration {
    connect_lifetime(std::fs::read_to_string(file).ok())
}

/// How long the kernel keeps trying to connect with `syn_retries`, as
/// `/proc/sys/net/ipv4/tcp_syn_retries` reads: one second for the first
/// SYN, doubling for each retry up to the kernel's 120-second cap, and a
/// few seconds' margin. Unreadable or out of range, the most the kernel
/// allows, 127 retries: longer, never shorter.
pub(super) fn connect_lifetime(syn_retries: Option<String>) -> Duration {
    let retries = syn_retries
        .and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|r| *r <= 127)
        .unwrap_or(127);
    let secs: u64 = (0..=retries).map(|i| (1u64 << i.min(7)).min(120)).sum();
    Duration::from_secs(secs + 3)
}

/// A command the relay admits connections for.
struct Admitted {
    /// The token the relay opens the command's connections to the proxy
    /// with.
    codex_token: String,
    /// One for each connection the command may hold.
    permits: Arc<Semaphore>,
    /// Changes, as its sender drops, when the command ends.
    ended: watch::Receiver<()>,
    /// What the command keeps until its last connection has let go: its
    /// room among the process's file descriptors.
    _keep: Box<dyn std::any::Any + Send + Sync>,
}

struct Shared {
    /// The proxy's own address.
    codex: SocketAddr,
    timeouts: Timeouts,
    /// The commands admitted, by the token their bridges send.
    commands: Mutex<HashMap<String, Arc<Admitted>>>,
}

impl Shared {
    /// The command `token` was issued to, while it runs.
    fn admitted(&self, token: &str) -> Option<Arc<Admitted>> {
        let commands = self
            .commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        commands.get(token).cloned()
    }
}

/// A relay listening on this machine's loopback interface.
pub(super) struct Relay {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// A command's admission to a relay: its token admits connections until
/// this drops.
pub(super) struct Admission {
    shared: Arc<Shared>,
    token: String,
    ended: Option<watch::Sender<()>>,
    /// The command, while any of its connections holds it.
    admitted: std::sync::Weak<Admitted>,
}

/// How long the end of a command waits for its connections to let go,
/// so that a command started right after it finds its room given back.
/// One the proxy has not let go of by then keeps the room.
const RELEASE_WAIT: Duration = Duration::from_secs(1);

impl Drop for Admission {
    fn drop(&mut self) {
        self.shared
            .commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.token);
        drop(self.ended.take());
        let started = std::time::Instant::now();
        while self.admitted.strong_count() > 0 && started.elapsed() < RELEASE_WAIT {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Relay {
    /// Start a relay in front of the proxy at `codex`, on the current
    /// runtime.
    pub(super) async fn start(codex: SocketAddr, timeouts: Timeouts) -> anyhow::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            codex,
            timeouts,
            commands: Mutex::default(),
        });
        let accept = tokio::spawn(accept(listener, shared.clone()));
        Ok(Self {
            addr,
            shared,
            accept,
        })
    }

    /// Where the relay listens.
    pub(super) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Admit the command whose bridge sends `token`, opening its
    /// connections to the proxy with `codex_token`, at most `limit` at once,
    /// and keeping `keep` until the last of them has let go.
    pub(super) fn admit(
        &self,
        token: String,
        codex_token: String,
        limit: u32,
        keep: Box<dyn std::any::Any + Send + Sync>,
    ) -> anyhow::Result<Admission> {
        anyhow::ensure!(
            (1..=MAX_CONNECTIONS_PER_COMMAND).contains(&limit),
            "a command may hold from 1 to {MAX_CONNECTIONS_PER_COMMAND} connections at once, \
             not {limit}"
        );
        let (ended, receiver) = watch::channel(());
        let admitted = Arc::new(Admitted {
            codex_token,
            permits: Arc::new(Semaphore::new(limit as usize)),
            ended: receiver,
            _keep: keep,
        });
        let mut commands = self
            .shared
            .commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(
            !commands.contains_key(&token),
            "the relay already admits a command with this token"
        );
        let weak = Arc::downgrade(&admitted);
        commands.insert(token.clone(), admitted);
        Ok(Admission {
            shared: self.shared.clone(),
            token,
            ended: Some(ended),
            admitted: weak,
        })
    }
}

/// Accept connections until the relay stops. An accept that fails, as when
/// the process is out of descriptors, leaves the connection waiting, and
/// it is never passed on unread.
async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                // Too many connections without a frame yet: this one is
                // closed.
                let Ok(pending) = PENDING.try_acquire() else {
                    continue;
                };
                tokio::spawn(serve(shared.clone(), stream, pending));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// Relay one connection, or close it unrelayed.
async fn serve(
    shared: Arc<Shared>,
    mut front: TcpStream,
    pending: tokio::sync::SemaphorePermit<'static>,
) {
    let Ok(Ok(token)) = tokio::time::timeout(FRAME_TIMEOUT, read_frame(&mut front)).await else {
        return;
    };
    drop(pending);
    let Some(admitted) = shared.admitted(&token) else {
        return;
    };
    let Ok(permit) = admitted.permits.clone().try_acquire_owned() else {
        return;
    };
    let connect = shared.timeouts.connect;
    let Ok(Ok(mut codex)) = tokio::time::timeout(connect, TcpStream::connect(shared.codex)).await
    else {
        return;
    };
    let mut frame = Vec::with_capacity(FRAME_MAGIC.len() + 2 + MAX_TOKEN);
    if codex_network_proxy::write_attribution_frame(&mut frame, &admitted.codex_token).is_err() {
        return;
    }
    if !matches!(
        tokio::time::timeout(connect, codex.write_all(&frame)).await,
        Ok(Ok(()))
    ) {
        return;
    }
    let owed = splice(front, codex, shared.timeouts, admitted.ended.clone()).await;
    if let Some(since) = owed {
        tokio::time::sleep_until(since + shared.timeouts.linger).await;
    }
    drop(permit);
}

/// The token in the attribution frame `front` opens with.
async fn read_frame(front: &mut TcpStream) -> std::io::Result<String> {
    let invalid = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, why.to_owned());
    let mut magic = [0u8; FRAME_MAGIC.len()];
    front.read_exact(&mut magic).await?;
    if &magic != FRAME_MAGIC {
        return Err(invalid("no attribution frame"));
    }
    let len = usize::from(front.read_u16().await?);
    if len == 0 || len > MAX_TOKEN {
        return Err(invalid("an attribution token of the wrong length"));
    }
    let mut token = vec![0u8; len];
    front.read_exact(&mut token).await?;
    String::from_utf8(token).map_err(|_| invalid("an attribution token that is not UTF-8"))
}

/// What one spliced connection has shown about the proxy's dials.
#[derive(Default)]
struct Exchange {
    /// Whether the client's first bytes have been seen, and opened a
    /// `CONNECT`.
    first: Option<bool>,
    /// The proxy's answer to a `CONNECT`, while it is being read: no
    /// evidence of a dial, which starts after it.
    answer: Option<Vec<u8>>,
    /// When the client's first bytes went to the proxy.
    started: Option<Instant>,
    /// Since when the client has sent bytes the proxy has not answered.
    owed: Option<Instant>,
    /// Whether the proxy has answered anything.
    answered: bool,
    /// Whether the proxy's answer to a `CONNECT` refused it, once its
    /// status has been read.
    refused: Option<bool>,
}

/// The statuses of a `CONNECT` the proxy tunnels.
const STATUS: [&[u8]; 2] = [b"HTTP/1.1 200", b"HTTP/1.0 200"];

/// The longest `CONNECT` answer read for its status.
const MAX_ANSWER: usize = 16 << 10;

impl Exchange {
    /// The client sent `bytes` toward the proxy.
    fn sent(&mut self, bytes: &[u8], now: Instant) {
        if self.first.is_none() {
            // A first read shorter than the method that could still begin
            // it counts as one: what that costs is a longer hold.
            let connect = if bytes.len() >= 7 {
                bytes.starts_with(b"CONNECT")
            } else {
                b"CONNECT".starts_with(bytes)
            };
            self.first = Some(connect);
            if connect {
                self.answer = Some(Vec::new());
            }
            self.started = Some(now);
        }
        self.owed.get_or_insert(now);
    }

    /// The proxy sent `bytes` toward the client.
    fn received(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        if let Some(answer) = self.answer.as_mut() {
            let seen = answer.len();
            answer.extend_from_slice(bytes);
            if self.refused.is_none() && answer.len() >= STATUS.len() {
                self.refused = Some(!STATUS.iter().any(|s| answer.starts_with(s)));
            }
            let from = seen.saturating_sub(3);
            match answer[from..].windows(4).position(|w| w == b"\r\n\r\n") {
                Some(at) => {
                    // Refused, the proxy never dialled, and its answer is the
                    // answer; otherwise only the upstream's bytes, after it,
                    // are. A status never read counts as a tunnel's.
                    let end = (from + at + 4).saturating_sub(seen).min(bytes.len());
                    rest = if self.refused == Some(true) {
                        bytes
                    } else {
                        &bytes[end..]
                    };
                    self.answer = None;
                }
                None => {
                    // Only the last bytes matter to find the end.
                    if answer.len() > MAX_ANSWER {
                        answer.drain(..answer.len() - 3);
                    }
                    rest = &[];
                }
            }
        }
        if !rest.is_empty() {
            self.owed = None;
            self.answered = true;
        }
    }

    /// Whether the first request has gone unanswered past `timeout`.
    fn overdue(&self, timeout: Duration, now: Instant) -> bool {
        !self.answered && self.started.is_some_and(|t| now >= t + timeout)
    }
}

/// Copy bytes both ways between `front` and `codex` until the proxy lets go
/// of the connection, the first request goes unanswered past the timeout,
/// or the command ends; then close the client's side and, while the
/// command runs, wait for the proxy to let go. Returns since when a request
/// was left unanswered, if one was.
async fn splice(
    front: TcpStream,
    codex: TcpStream,
    timeouts: Timeouts,
    mut ended: watch::Receiver<()>,
) -> Option<Instant> {
    let exchange = Mutex::new(Exchange::default());
    let note = |f: &mut dyn FnMut(&mut Exchange)| {
        f(&mut exchange
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    };
    let (mut front_read, mut front_write) = front.into_split();
    let (mut codex_read, mut codex_write) = codex.into_split();
    let mut codex_closed = false;
    {
        let up = async {
            let mut buf = vec![0u8; 16 << 10];
            loop {
                match front_read.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        note(&mut |e| e.sent(&buf[..n], Instant::now()));
                        if codex_write.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = codex_write.shutdown().await;
        };
        // Reads the proxy's side to its end, even once the client is gone,
        // since that end is when the proxy has let go.
        let down = async {
            let mut buf = vec![0u8; 16 << 10];
            let mut client = true;
            loop {
                match codex_read.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        note(&mut |e| e.received(&buf[..n]));
                        client = client && front_write.write_all(&buf[..n]).await.is_ok();
                    }
                }
            }
        };
        tokio::pin!(up, down);
        let mut up_done = false;
        loop {
            tokio::select! {
                () = &mut up, if !up_done => up_done = true,
                () = &mut down => {
                    codex_closed = true;
                    break;
                }
                _ = ended.changed() => break,
                () = tokio::time::sleep(Duration::from_millis(100)) => {
                    let overdue = exchange
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .overdue(timeouts.connect, Instant::now());
                    if overdue {
                        break;
                    }
                }
            }
        }
    }
    // The client's side closes now; the proxy's stays open until the proxy
    // lets go, or the command ends. Then a request left unanswered is held
    // for as long as the dial it may have started could still run (see
    // `serve`).
    drop(front_read);
    drop(front_write);
    if !codex_closed {
        let _ = codex_write.shutdown().await;
        let drain = async {
            let mut buf = vec![0u8; 4 << 10];
            while matches!(codex_read.read(&mut buf).await, Ok(n) if n > 0) {}
        };
        tokio::select! {
            () = drain => {}
            _ = ended.changed() => {}
        }
    }
    exchange
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .owed
}

#[cfg(test)]
mod tests;
