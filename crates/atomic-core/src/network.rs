// SPDX-License-Identifier: Apache-2.0

//! Network access for an exec atom's command.
//!
//! A command has no network unless its atom declares the hosts it reaches,
//! as `network: { allow: [...] }`. Then its sandbox gives it a network
//! namespace of its own whose only way out is a proxy atomic runs, which
//! forwards a request only to a declared host, on port 80 or 443. The
//! command finds the proxy in the usual variables (`HTTP_PROXY`,
//! `HTTPS_PROXY`, `ALL_PROXY` and their kin); it can open no other
//! connection and no Unix socket.
//!
//! What an atom may declare is atomic's policy, checked when the atom loads
//! ([`check_hosts`]) and again when its command is confined: public DNS
//! names only. That list of names is a courtesy; the control is the proxy,
//! which resolves each declared name before it connects and refuses one
//! that resolves to this machine or a private network, or does not
//! resolve. To the addresses Codex's proxy counts as private, atomic adds
//! three IPv6 prefixes that carry an IPv4 address inside them (NAT64, 6to4
//! and Teredo), whatever that address is, and every address the interfaces
//! of atomic's network namespace have, public ones too
//! ([`denied_addresses`]). The proxy never goes through another proxy, even
//! when atomic's own environment names one, and speaks only HTTP and HTTPS
//! `CONNECT`, not SOCKS.
//!
//! While a command that declared hosts runs, its proxy listens on this
//! machine's loopback interface, shared by the running commands that declared
//! the same hosts. The sandbox helper's bridge opens each connection with a
//! token atomic made for its command alone (Codex's attribution frame), and
//! the proxy refuses every request on a connection without a token it issued,
//! so no sandboxed process and no other user's process reaches anything
//! through it. A process of the same user outside any sandbox is not kept
//! out: it can enter the command's namespaces through /proc and use the
//! bridge, which adds the token itself. Who can read the token is narrowed
//! too, not closed off: it is never in an exec environment. The sandbox
//! helper gets it on a Unix socket, which cannot be reopened through /proc,
//! and reads it once its prologue has made it non-dumpable; until then
//! another process of the same user can take it only by attaching to the
//! helper, which Yama's `ptrace_scope` of 1 or more refuses to a process
//! that is not its ancestor, and at `ptrace_scope` 0 allows. `atomic
//! runner`, under `--runner process`, gets it on a socket of its own beside
//! its request, with the digest of that request, and reads it once it is
//! non-dumpable; at `ptrace_scope` 0 that socket is open to the same attach
//! as the helper's, before then. Its request waits in its stdin, a pipe
//! another process of the same user can reopen through /proc before then,
//! to read the request (its program, arguments, directory and whole
//! environment) or replace it; the runner refuses a request the digest does
//! not name, so a replaced one never gets the token, though a replaced
//! request without one still runs as that user's own command, and a request
//! read and written back unchanged still runs. Once
//! they are non-dumpable, no process of the same user without
//! `CAP_SYS_PTRACE` over their user namespace can newly read it from them,
//! or from the helper's bridges, which inherit it; but one that attached
//! during the window, as `ptrace_scope` 0 lets it, keeps what it attached
//! to. A command's token admits no new connection once the command ends,
//! and the proxy stops once no command needs it.

use anyhow::Context as _;
use codex_network_proxy::{
    NetworkDecision, NetworkPolicyDecider, NetworkPolicyDeciderFuture, NetworkPolicyRequest,
    NetworkProxy, NetworkProxyConfig, NetworkProxyConstraints, NetworkProxyHandle,
    NetworkProxyState, PreparedManagedNetwork,
};
use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// The most hosts one atom may declare.
pub const MAX_HOSTS: usize = 32;

/// The ports a declared host is reached on: HTTP and HTTPS.
pub const PORTS: [u16; 2] = [80, 443];

/// The reason the proxy gives for a declared host on another port.
pub const PORT_REFUSED: &str = "port_not_allowed";

/// The reason the proxy gives for a host no atom declared.
pub const HOST_REFUSED: &str = "not_allowed";

/// The reason the proxy gives for a request on a connection that no
/// confined command's bridge opened: one without a token the proxy issued.
pub const UNATTRIBUTED_REFUSED: &str = "not_from_a_command";

/// Check an atom's declared hosts against atomic's policy, returning them
/// in a canonical order. Each is a DNS name with at least two labels, such
/// as `api.example.com`, or `*.` and such a name for its subdomains. Refused:
/// IP addresses in any spelling (dotted, decimal, hexadecimal or octal),
/// `localhost` and names under it, single-label names (which resolve on the
/// local network), names under `.local`, `.internal`, `.lan`, `.home.arpa`
/// and other private suffixes, a bare `*`, a wildcard over a public suffix
/// (`*.co.uk`, or `*.github.io` from the list's private section, where each
/// name below is someone else's), and anything with a scheme, a port, a
/// path or a Unix socket.
pub fn check_hosts(hosts: &[String]) -> Result<Vec<String>, String> {
    if hosts.len() > MAX_HOSTS {
        return Err(format!("an atom may declare at most {MAX_HOSTS} hosts"));
    }
    let mut out = BTreeSet::new();
    for host in hosts {
        out.insert(check_host(host)?);
    }
    Ok(out.into_iter().collect())
}

/// Suffixes that name hosts on a private network, never the public one.
const PRIVATE_SUFFIXES: &[&str] = &[
    "localhost",
    "local",
    "internal",
    "intranet",
    "lan",
    "home",
    "corp",
    "private",
    "home.arpa",
    "in-addr.arpa",
    "ip6.arpa",
    "localdomain",
];

/// Whether `label` spells a number, as an IPv4 address's parts may be
/// written: decimal, octal (a leading `0`) or hexadecimal (`0x`).
fn numeric(label: &str) -> bool {
    match label.strip_prefix("0x") {
        Some(hex) => hex.bytes().all(|b| b.is_ascii_hexdigit()),
        None => !label.is_empty() && label.bytes().all(|b| b.is_ascii_digit()),
    }
}

fn check_host(host: &str) -> Result<String, String> {
    let refuse = |why: &str| Err(format!("`{host}`: {why}"));
    let lower = host.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return refuse("a host is a DNS name, such as `api.example.com`");
    }
    if lower.contains("://") || lower.contains('/') || lower.contains(':') || lower.contains('@') {
        return refuse("a host is a bare DNS name: no scheme, port, path, address or Unix socket");
    }
    let name = lower.strip_prefix("*.").unwrap_or(&lower);
    if name.contains('*') {
        return refuse("a wildcard is only a leading `*.`, for a name's subdomains");
    }
    let name = name.strip_suffix('.').unwrap_or(name);
    let labels: Vec<&str> = name.split('.').collect();
    // A resolver reads `0x7f.1`, `8.8.8.0x8` or `2130706433` as an address,
    // as it does `127.0.0.1`: a name whose last label is a number, or with
    // any hexadecimal label, is one.
    if name.parse::<std::net::IpAddr>().is_ok()
        || labels.last().is_some_and(|l| numeric(l))
        || labels.iter().any(|l| l.starts_with("0x") && numeric(l))
    {
        return refuse("an IP address is not a host an atom may declare; name the host");
    }
    if labels.len() < 2 {
        return refuse(
            "a single-label name resolves on the local network; declare a public DNS name",
        );
    }
    for label in &labels {
        let valid = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !valid {
            return refuse("each label of a host is letters, digits and inner `-`");
        }
    }
    if PRIVATE_SUFFIXES
        .iter()
        .any(|suffix| name == *suffix || name.ends_with(&format!(".{suffix}")))
    {
        return refuse(
            "a name on a private network or this machine is not a host an atom may declare",
        );
    }
    if lower.starts_with("*.") && public_suffix(name) {
        return refuse(
            "a wildcard over a public suffix reaches every site anyone has under it; \
             declare the names you reach, or a wildcard under a name you control",
        );
    }
    Ok(lower.strip_suffix('.').unwrap_or(&lower).to_owned())
}

/// Whether `*.<name>` covers names under which anyone can have a site: when
/// `name` is a public suffix on the Public Suffix List, from its ICANN
/// section (`co.uk`) or its private one (`github.io`, `s3.amazonaws.com`),
/// or when a wildcard rule makes every name just below it one (`*.run.app`,
/// `*.kawasaki.jp`), which a child that is a suffix shows. A name above a
/// suffix (`amazonaws.com`, above `s3.amazonaws.com`) passes here; the
/// proxy keeps such a wildcard from reaching that suffix's names
/// ([`declared`]).
fn public_suffix(name: &str) -> bool {
    let is_suffix = |n: &str| psl::suffix(n.as_bytes()).is_some_and(|suffix| suffix == n);
    if is_suffix(name) {
        return true;
    }
    is_suffix(&format!("atomic-probe.{name}"))
}

/// Whether `host` is under `parent` without crossing a public suffix: its
/// own public suffix is shorter than `parent`, so the site it belongs to is
/// at or below `parent`. Under `*.amazonaws.com`, `foo.amazonaws.com` is,
/// and `bucket.s3.amazonaws.com` is not, since `s3.amazonaws.com` is a
/// suffix of its own, whose sites are other people's.
fn within_one_site(host: &str, parent: &str) -> bool {
    psl::suffix(host.as_bytes()).is_some_and(|suffix| suffix.trim().as_bytes().len() < parent.len())
}

/// Whether `host`, as a request names it, is one of `declared`: the name
/// itself, or a subdomain of a `*.` name that does not cross a public
/// suffix below it.
fn declared(declared: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    declared
        .iter()
        .any(|pattern| match pattern.strip_prefix("*.") {
            Some(parent) => {
                host.strip_suffix(parent)
                    .is_some_and(|rest| rest.len() > 1 && rest.ends_with('.'))
                    && within_one_site(&host, parent)
            }
            None => host == *pattern,
        })
}

/// The proxy's decision for a request: from a confined command's bridge,
/// for a declared host on port 80 or 443. The proxy then resolves the host,
/// and still refuses one on this machine or a private network.
struct DeclaredHosts(Vec<String>);

impl NetworkPolicyDecider for DeclaredHosts {
    fn decide(&self, req: NetworkPolicyRequest) -> NetworkPolicyDeciderFuture<'_> {
        // The proxy names the execution only for a connection that opened
        // with a token one of this proxy's commands holds: anything else
        // came from some other process on this machine.
        let decision = if req.execution_id.is_none() {
            NetworkDecision::deny(UNATTRIBUTED_REFUSED)
        } else if !declared(&self.0, &req.host) {
            NetworkDecision::deny(HOST_REFUSED)
        } else if !PORTS.contains(&req.port) {
            NetworkDecision::deny(PORT_REFUSED)
        } else {
            NetworkDecision::Allow
        };
        Box::pin(async move { decision })
    }
}

/// A running proxy for one list of hosts.
struct Running {
    proxy: NetworkProxy,
    _handle: NetworkProxyHandle,
    /// This machine's addresses when it started, which it never reaches.
    own: BTreeSet<IpAddr>,
}

impl Running {
    /// Whether a command may use this proxy while this machine has the
    /// addresses `own`: only if it was started with the same ones, so it
    /// refuses every one of them, and still accepts connections.
    fn reusable(&self, own: &BTreeSet<IpAddr>) -> bool {
        self.own == *own && alive(self)
    }
}

/// IPv6 addresses that carry an IPv4 address, as patterns over the way the
/// proxy writes an address (Rust's, lowercase and compressed), for its deny
/// list: NAT64's prefixes, well-known and local-use (`64:ff9b::/32`, which
/// holds both), 6to4 (`2002::/16`) and Teredo (`2001:0::/32`). A route
/// through any of them can end at an address on this machine or a private
/// network, so none is reached, whatever IPv4 address it carries. Each has
/// two colons: the proxy reads a pattern with one as a host and a port.
const EMBEDDING_IPV4: &[&str] = &["64:ff9b:*", "2002:*:*", "2001:0:*", "2001::*"];

/// What the proxy refuses on top of the addresses Codex's proxy already
/// counts as private: the IPv6 forms that embed an IPv4 address, and `own`,
/// this machine's addresses, public ones included. The proxy checks its
/// deny list against a request's host and again, when it dials, against
/// every address a declared name resolved to.
fn denied_addresses(own: &BTreeSet<IpAddr>) -> Vec<String> {
    EMBEDDING_IPV4
        .iter()
        .map(|p| (*p).to_owned())
        .chain(own.iter().map(IpAddr::to_string))
        .collect()
}

/// `own`, read as this machine's addresses, or why not: with its loopback
/// interface up a machine has at least that one's, so a list without any
/// is wrong, and a proxy given it would refuse none of them.
fn checked(own: BTreeSet<IpAddr>) -> anyhow::Result<BTreeSet<IpAddr>> {
    anyhow::ensure!(
        !own.is_empty(),
        "this machine's network interfaces were listed with no addresses; is the loopback \
         interface up?"
    );
    Ok(own)
}

/// Every address the interfaces of this process's network namespace have
/// now: this machine's, as far as the proxy can tell. A container's or a
/// virtual machine's addresses on another namespace are not among them.
#[allow(unsafe_code)]
fn own_addresses() -> anyhow::Result<BTreeSet<IpAddr>> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a list that is freed below with
    // freeifaddrs, and only read before then.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("list this machine's network addresses");
    }
    let mut own = BTreeSet::new();
    let mut entry = list;
    while !entry.is_null() {
        // SAFETY: `entry` is a node of the list getifaddrs returned, and an
        // address it points at is a sockaddr of the family it names.
        unsafe {
            let addr = (*entry).ifa_addr;
            if !addr.is_null() {
                match i32::from((*addr).sa_family) {
                    libc::AF_INET => {
                        let sin = &*addr.cast::<libc::sockaddr_in>();
                        own.insert(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                            sin.sin_addr.s_addr,
                        ))));
                    }
                    libc::AF_INET6 => {
                        let sin6 = &*addr.cast::<libc::sockaddr_in6>();
                        own.insert(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            entry = (*entry).ifa_next;
        }
    }
    // SAFETY: `list` came from getifaddrs and is not used after this.
    unsafe { libc::freeifaddrs(list) };
    Ok(own)
}

/// What a command holds while it runs: its proxy runs until no command
/// holds it, and its token works until it lets go.
pub(crate) type Lease = Arc<dyn std::any::Any + Send + Sync>;

/// One command's hold on a proxy.
struct Leased {
    /// The proxy scoped to this command, which keeps its token registered;
    /// dropped first, so the token stops working before the proxy might.
    _scope: NetworkProxy,
    _running: Arc<Running>,
}

/// A token for one command's bridge to open its connections with: 32 random
/// bytes, in hexadecimal.
fn token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("make a proxy token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The proxies a confining backend runs, one for each distinct list of
/// hosts, each kept while a command that declared it runs.
#[derive(Clone, Default)]
pub(crate) struct Proxies {
    running: Arc<Mutex<HashMap<Vec<String>, Weak<Running>>>>,
    /// This machine's addresses as a test says they are, in place of its
    /// interfaces'.
    #[cfg(test)]
    own_for_tests: Arc<Mutex<Option<BTreeSet<IpAddr>>>>,
}

impl std::fmt::Debug for Proxies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hosts: Vec<Vec<String>> = self
            .running
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        f.debug_tuple("Proxies").field(&hosts).finish()
    }
}

/// The runtime the proxies run on, apart from any caller's, so a command
/// confined from a blocking thread can start one.
fn runtime() -> anyhow::Result<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    if let Some(rt) = RUNTIME.get() {
        return Ok(rt);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("atomic-network-proxy")
        .enable_all()
        .build()?;
    Ok(RUNTIME.get_or_init(|| rt))
}

/// Whether `running` still accepts connections.
fn alive(running: &Running) -> bool {
    std::net::TcpStream::connect_timeout(
        &running.proxy.http_addr(),
        std::time::Duration::from_secs(1),
    )
    .is_ok()
}

impl Proxies {
    /// `env` with the variables that send a command's traffic through the
    /// proxy for `hosts`, started now if none runs for them, and what the
    /// sandbox needs to route it there; with the lease that keeps the proxy
    /// running, to hold until the command has ended. The variables include
    /// the command's own token, for the helper's bridge and never the
    /// command. `loopback` names loopback addresses the proxy also reaches,
    /// for tests only: those are on the proxy's own list, so a request for
    /// one is not checked for a token. Called from a blocking thread, never
    /// from inside an async task.
    pub(crate) fn prepare(
        &self,
        hosts: &[String],
        loopback: &[String],
        env: HashMap<String, String>,
    ) -> anyhow::Result<(PreparedManagedNetwork, Lease)> {
        let mut key = hosts.to_vec();
        key.extend(loopback.iter().map(|l| format!("loopback:{l}")));
        // A proxy started before this machine's addresses changed would
        // reach a new one: start another.
        let own = self.own()?;
        let running = {
            let mut proxies = self
                .running
                .lock()
                .map_err(|_| anyhow::anyhow!("the network proxies' lock is poisoned"))?;
            proxies.retain(|_, running| running.strong_count() > 0);
            match proxies
                .get(&key)
                .and_then(Weak::upgrade)
                .filter(|running| running.reusable(&own))
            {
                Some(running) => running,
                None => {
                    let running = Arc::new(runtime()?.block_on(start(hosts, loopback, own))?);
                    proxies.insert(key, Arc::downgrade(&running));
                    running
                }
            }
        };
        // Each command gets a token of its own, which its helper's bridge
        // sends first on every connection; the command never sees it.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let token = token()?;
        let scope = running.proxy.for_execution(
            "atomic",
            &format!("command-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
            token.clone(),
            None,
            None,
        )?;
        let prepared = scope.prepare_for_optional_environment(env, None)?;
        anyhow::ensure!(
            prepared
                .env
                .get(codex_network_proxy::PROXY_ATTRIBUTION_TOKEN_ENV_KEY)
                .is_some_and(|t| *t == token),
            "the proxy did not hand the command's bridge its token"
        );
        let lease: Lease = Arc::new(Leased {
            _scope: scope,
            _running: running,
        });
        Ok((prepared, lease))
    }

    /// This machine's addresses now, or, in a test that set them, the
    /// test's.
    fn own(&self) -> anyhow::Result<BTreeSet<IpAddr>> {
        #[cfg(test)]
        let own = match self
            .own_for_tests
            .lock()
            .map_err(|_| anyhow::anyhow!("the test's addresses' lock is poisoned"))?
            .clone()
        {
            Some(own) => own,
            None => own_addresses()?,
        };
        #[cfg(not(test))]
        let own = own_addresses()?;
        checked(own)
    }

    /// Have the proxies take `own` as this machine's addresses.
    #[cfg(test)]
    fn set_own_for_tests(&self, own: BTreeSet<IpAddr>) {
        *self.own_for_tests.lock().expect("addresses") = Some(own);
    }

    /// How many proxies run now.
    #[cfg(test)]
    fn running(&self) -> usize {
        self.running
            .lock()
            .map(|m| m.values().filter(|r| r.strong_count() > 0).count())
            .unwrap_or_default()
    }
}

/// Start a proxy that forwards only to `hosts` on ports 80 and 443, never
/// through another proxy and never over SOCKS, and reaches no address on
/// this machine or its network but `loopback`'s: none of `own`, this
/// machine's addresses, and none [`denied_addresses`] names.
async fn start(
    hosts: &[String],
    loopback: &[String],
    own: BTreeSet<IpAddr>,
) -> anyhow::Result<Running> {
    let mut config = NetworkProxyConfig {
        enabled: true,
        allow_upstream_proxy: false,
        enable_socks5: false,
        enable_socks5_udp: false,
        allow_local_binding: Some(false),
        dangerously_allow_all_unix_sockets: Some(false),
        ..Default::default()
    };
    // Nothing is on the proxy's own list but test loopback addresses, so
    // every request reaches the decider, which checks host and port.
    config.set_allowed_domains(loopback.to_vec());
    // A test's loopback address is on this machine too, and stays reachable.
    let reachable: BTreeSet<IpAddr> = loopback.iter().filter_map(|l| l.parse().ok()).collect();
    let denied: BTreeSet<IpAddr> = own.difference(&reachable).copied().collect();
    config.set_denied_domains(denied_addresses(&denied));
    let state = codex_network_proxy::build_config_state(
        config,
        NetworkProxyConstraints::default(),
        codex_network_proxy::Platform::native(),
    )?;
    let proxy = NetworkProxy::builder()
        .state(Arc::new(NetworkProxyState::with_reloader(
            state.clone(),
            Arc::new(Fixed(state)),
        )))
        .policy_decider(DeclaredHosts(hosts.to_vec()))
        .managed_by_codex(true)
        .build()
        .await?;
    let handle = proxy.run().await?;
    Ok(Running {
        proxy,
        _handle: handle,
        own,
    })
}

/// A proxy configuration that never changes.
struct Fixed(codex_network_proxy::ConfigState);

impl codex_network_proxy::ConfigReloader for Fixed {
    fn source_label(&self) -> String {
        "an atom's declared hosts".to_owned()
    }

    fn maybe_reload(
        &self,
    ) -> codex_network_proxy::ConfigReloaderFuture<'_, Option<codex_network_proxy::ConfigState>>
    {
        Box::pin(async { Ok(None) })
    }

    fn reload_now(
        &self,
    ) -> codex_network_proxy::ConfigReloaderFuture<'_, codex_network_proxy::ConfigState> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_dns_names_may_be_declared() {
        let ok = check_hosts(&[
            "API.example.com".into(),
            "*.example.com".into(),
            "*.acme.github.io".into(),
            "acme-reports.s3.us-east-1.amazonaws.com.".into(),
        ])
        .expect("public names");
        assert_eq!(
            ok,
            [
                "*.acme.github.io",
                "*.example.com",
                "acme-reports.s3.us-east-1.amazonaws.com",
                "api.example.com"
            ]
        );
        for refused in [
            "",
            "*",
            "*.com.*",
            "localhost",
            "api.localhost",
            "intranet",
            "printer.local",
            "metadata.google.internal",
            "router.home.arpa",
            "127.0.0.1",
            "10.0.0.8",
            "169.254.169.254",
            "::1",
            "[::1]",
            "2130706433",
            "0x8.0x8.0x8.0x8",
            "8.8.8.0x8",
            "0x7f.1",
            "0x7f.0.0.1",
            "0177.0.0.1",
            "example.0x10.com",
            "0xdead.example.com",
            "https://example.com",
            "example.com:443",
            "example.com/path",
            "unix:/var/run/docker.sock",
            "/var/run/docker.sock",
            "user@example.com",
            "-bad.example.com",
            "bad_label.example.com",
            "*.github.io",
            "*.s3.amazonaws.com",
            "*.S3.amazonaws.com.",
            "*.co.uk",
            "*.com.au",
        ] {
            assert!(check_hosts(&[refused.into()]).is_err(), "{refused:?}");
        }
        let many: Vec<String> = (0..=MAX_HOSTS)
            .map(|i| format!("h{i}.example.com"))
            .collect();
        assert!(check_hosts(&many).is_err());
    }

    #[test]
    fn a_request_is_declared_by_its_name_or_a_wildcard_parent() {
        let hosts = ["api.example.com".to_owned(), "*.cdn.example.net".to_owned()];
        for yes in [
            "api.example.com",
            "API.Example.com.",
            "a.cdn.example.net",
            "x.y.cdn.example.net",
        ] {
            assert!(declared(&hosts, yes), "{yes}");
        }
        for no in [
            "example.com",
            "evil-api.example.com",
            "cdn.example.net",
            "xcdn.example.net",
            "api.example.com.evil.test",
            "127.0.0.1",
        ] {
            assert!(!declared(&hosts, no), "{no}");
        }
    }

    /// A wildcard reaches the names below it only as far as the site they
    /// belong to: one above a public suffix does not reach that suffix's
    /// sites, which are other people's.
    #[test]
    fn a_wildcard_does_not_reach_across_a_public_suffix_below_it() {
        let hosts = [
            "*.amazonaws.com".to_owned(),
            "*.example.com".to_owned(),
            "*.acme.github.io".to_owned(),
            "api.example.net".to_owned(),
        ];
        for yes in [
            "foo.amazonaws.com",
            "a.b.example.com",
            "www.acme.github.io",
            "api.example.net",
        ] {
            assert!(declared(&hosts, yes), "{yes}");
        }
        for no in [
            "attacker.s3.amazonaws.com",
            "x.s3.us-east-1.amazonaws.com",
            "lb.us-east-1.elb.amazonaws.com",
            "other.github.io",
        ] {
            assert!(!declared(&hosts, no), "{no}");
        }
    }

    /// The proxy never forwards through another proxy, whatever atomic's
    /// environment names, and speaks no SOCKS: a declared host is reached
    /// directly, after the proxy has checked where it resolves.
    #[test]
    fn a_proxy_uses_no_upstream_proxy_and_no_socks() {
        let proxies = Proxies::default();
        let hosts = ["api.example.com".to_owned()];
        let (prepared, lease) = proxies
            .prepare(&hosts, &[], HashMap::new())
            .expect("starts");
        let leased = lease.downcast_ref::<Leased>().expect("a running proxy");
        let cfg = runtime()
            .expect("runtime")
            .block_on(leased._running.proxy.current_cfg())
            .expect("config");
        assert!(!cfg.allow_upstream_proxy, "{cfg:?}");
        assert!(!cfg.enable_socks5 && !cfg.enable_socks5_udp, "{cfg:?}");
        assert!(
            prepared
                .env
                .get("ALL_PROXY")
                .is_none_or(|v| !v.starts_with("socks")),
            "{:?}",
            prepared.env
        );
    }

    #[test]
    fn a_proxy_runs_while_a_command_holds_it_and_stops_after() {
        let proxies = Proxies::default();
        let hosts = ["api.example.com".to_owned()];
        let (first_env, first) = proxies
            .prepare(&hosts, &[], HashMap::new())
            .expect("starts");
        let (second_env, second) = proxies
            .prepare(&hosts, &[], HashMap::new())
            .expect("reused");
        assert_eq!(
            first_env.env.get("HTTP_PROXY"),
            second_env.env.get("HTTP_PROXY"),
            "one proxy for one list of hosts"
        );
        let token = |p: &PreparedManagedNetwork| {
            p.env
                .get(codex_network_proxy::PROXY_ATTRIBUTION_TOKEN_ENV_KEY)
                .cloned()
        };
        assert!(token(&first_env).is_some());
        assert_ne!(token(&first_env), token(&second_env), "a token each");
        assert_eq!(proxies.running(), 1);
        drop(first);
        assert_eq!(proxies.running(), 1, "a command still holds it");
        drop(second);
        assert_eq!(proxies.running(), 0, "it stops with its last command");
        let (_, again) = proxies
            .prepare(&hosts, &[], HashMap::new())
            .expect("starts again");
        assert_eq!(proxies.running(), 1);
        drop(again);
    }

    /// What a request for `http://allowed.invalid/` through the proxy at
    /// `addr` gets, sent from this machine's loopback interface, opening
    /// with `token`'s attribution frame or none: `403 <reason>`, or what
    /// went wrong.
    fn fetch(addr: &str, token: Option<&str>) -> String {
        fetch_from(addr, token, "allowed.invalid")
    }

    /// As [`fetch`], for `http://<host>:80/`.
    fn fetch_from(addr: &str, token: Option<&str>, host: &str) -> String {
        use std::io::{Read, Write};
        let attempt = || -> std::io::Result<String> {
            let mut stream = std::net::TcpStream::connect(addr)?;
            stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
            if let Some(token) = token {
                codex_network_proxy::write_attribution_frame(&mut stream, token)?;
            }
            stream.write_all(
                format!(
                    "GET http://{host}:80/ HTTP/1.1\r\nHost: {host}:80\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response)?;
            let response = String::from_utf8_lossy(&response).into_owned();
            let status = response.split(' ').nth(1).unwrap_or("none").to_owned();
            let body = response.split_once("\r\n\r\n").map_or("", |(_, b)| b);
            let reason = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v["reason"].as_str().map(str::to_owned))
                .unwrap_or_else(|| body.trim().to_owned());
            Ok(format!("{status} {reason}").trim().to_owned())
        };
        attempt().unwrap_or_else(|e| format!("{:?}", e.kind()))
    }

    /// Only a confined command's bridge can use its proxy. A client on this
    /// machine's loopback interface without the command's token is refused
    /// before its host is looked at, as is one with a token the proxy never
    /// issued, or a token whose command has ended. With the command's token
    /// the same request passes, to be refused only by the address check,
    /// since the host never resolves.
    #[test]
    fn only_a_commands_bridge_can_use_its_proxy() {
        let proxies = Proxies::default();
        let hosts = ["allowed.invalid".to_owned()];
        let (prepared, lease) = proxies
            .prepare(&hosts, &[], HashMap::new())
            .expect("starts");
        // Another command keeps the proxy running after the first ends.
        let (other_env, other) = proxies
            .prepare(&hosts, &[], HashMap::new())
            .expect("reused");
        let other_token =
            other_env.env[codex_network_proxy::PROXY_ATTRIBUTION_TOKEN_ENV_KEY].clone();
        // A connection dropped at the frame: the proxy accepted it and
        // answered nothing.
        let dropped = |got: &str| matches!(got, "none" | "ConnectionReset" | "BrokenPipe");
        let addr = prepared.env["HTTP_PROXY"]
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_owned();
        let token = prepared.env[codex_network_proxy::PROXY_ATTRIBUTION_TOKEN_ENV_KEY].clone();

        assert_eq!(fetch(&addr, None), format!("403 {UNATTRIBUTED_REFUSED}"));
        let forged = fetch(&addr, Some(&"0".repeat(64)));
        assert!(dropped(&forged), "{forged}");
        assert_eq!(fetch(&addr, Some(&token)), "403 not_allowed_local");

        drop(lease);
        assert_eq!(proxies.running(), 1, "the other command holds it");
        let ended = fetch(&addr, Some(&token));
        assert!(dropped(&ended), "{ended}");
        assert_eq!(
            fetch(&addr, Some(&other_token)),
            "403 not_allowed_local",
            "the other command's token still works"
        );
        drop(other);
    }

    /// The proxy reaches no IPv6 address that carries an IPv4 one (NAT64,
    /// 6to4, Teredo) and none of this machine's addresses, public ones
    /// included. Offline, no name resolves to them, so this asks for each
    /// address itself: the proxy checks its deny list against a request's
    /// host this way, and against each address a name resolves to when it
    /// dials. Each is refused as denied before the proxy looks for a token
    /// or a declared host. `2001:db8::5` stands in for a public address of
    /// this machine; Codex's proxy counts it as public.
    #[test]
    fn embedded_ipv4_and_this_machines_addresses_are_never_reached() {
        let own: BTreeSet<IpAddr> = ["2001:db8::5".parse().expect("address")].into();
        let running = runtime()
            .expect("runtime")
            .block_on(start(&["allowed.invalid".to_owned()], &[], own))
            .expect("starts");
        let addr = running.proxy.http_addr().to_string();
        for (host, why) in [
            ("[64:ff9b::7f00:1]", "NAT64 for 127.0.0.1"),
            ("[64:ff9b::a00:1]", "NAT64 for 10.0.0.1"),
            ("[64:ff9b::808:808]", "NAT64 for a public address"),
            ("[64:ff9b:1::7f00:1]", "local-use NAT64"),
            ("[2002:7f00:1::]", "6to4 for 127.0.0.1"),
            ("[2002:a00:1::1]", "6to4 for 10.0.0.1"),
            ("[2001:0:4136:e378:8000:63bf:3fff:fdd2]", "Teredo"),
            ("[2001::1]", "Teredo, compressed"),
            ("[2001:db8::5]", "this machine's public address"),
        ] {
            assert_eq!(fetch_from(&addr, None, host), "403 denied", "{why}: {host}");
        }
        // Another public address is not denied: it reaches the next check.
        assert_eq!(
            fetch_from(&addr, None, "[2001:db8::6]"),
            format!("403 {UNATTRIBUTED_REFUSED}")
        );
    }

    /// The machine's addresses are its interfaces': loopback's among them.
    /// A list with none is refused, since there is always loopback's.
    #[test]
    fn this_machines_addresses_come_from_its_interfaces() {
        let own = own_addresses().expect("addresses");
        assert!(own.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)), "{own:?}");
        let denied = denied_addresses(&own);
        assert!(denied.contains(&"127.0.0.1".to_owned()), "{denied:?}");
        assert!(checked(BTreeSet::new()).is_err());
        let proxies = Proxies::default();
        proxies.set_own_for_tests(BTreeSet::new());
        let Err(refused) = proxies.prepare(&["allowed.invalid".to_owned()], &[], HashMap::new())
        else {
            panic!("a proxy without this machine's addresses");
        };
        assert!(
            refused.to_string().contains("loopback interface up"),
            "{refused}"
        );
    }

    /// A proxy a command prepares refuses this machine's own addresses: one
    /// of loopback's is refused as denied, by the list, before the proxy's
    /// own check of local addresses (`not_allowed_local`) is reached.
    #[test]
    fn a_prepared_proxy_refuses_this_machines_addresses() {
        let proxies = Proxies::default();
        let (prepared, _lease) = proxies
            .prepare(&["allowed.invalid".to_owned()], &[], HashMap::new())
            .expect("starts");
        let addr = prepared.env["HTTP_PROXY"]
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_owned();
        assert_eq!(fetch_from(&addr, None, "127.0.0.1"), "403 denied");
    }

    /// A proxy is used again only while this machine has the addresses it
    /// started with: once they change, a command gets a new proxy, which
    /// refuses the new ones, while the old one runs on for its command.
    #[test]
    fn a_proxy_is_not_reused_once_this_machines_addresses_change() {
        let proxies = Proxies::default();
        let hosts = ["allowed.invalid".to_owned()];
        let one: IpAddr = "2001:db8::5".parse().expect("address");
        let two: IpAddr = "2001:db8::6".parse().expect("address");
        let proxy_of = |p: &PreparedManagedNetwork| {
            p.env["HTTP_PROXY"]
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_owned()
        };
        proxies.set_own_for_tests([one].into());
        let (first, _first) = proxies
            .prepare(&hosts, &[], HashMap::new())
            .expect("starts");
        let (again, _again) = proxies
            .prepare(&hosts, &[], HashMap::new())
            .expect("reused");
        assert_eq!(
            proxy_of(&first),
            proxy_of(&again),
            "the same addresses: reused"
        );
        proxies.set_own_for_tests([two].into());
        let (moved, _moved) = proxies
            .prepare(&hosts, &[], HashMap::new())
            .expect("starts");
        assert_ne!(
            proxy_of(&first),
            proxy_of(&moved),
            "new addresses: a new proxy"
        );
        assert_eq!(
            fetch_from(&proxy_of(&moved), None, "[2001:db8::6]"),
            "403 denied"
        );
        assert_eq!(
            fetch_from(&proxy_of(&first), None, "[2001:db8::5]"),
            "403 denied"
        );
    }

    /// A wildcard over a public suffix, from the list's ICANN section or its
    /// private one, would reach every tenant under it, and is refused; a
    /// wildcard under one tenant's name, and an exact name that is itself a
    /// suffix, are not.
    #[test]
    fn a_wildcard_over_a_public_suffix_is_refused() {
        for refused in [
            "*.github.io",
            "*.s3.amazonaws.com",
            "*.co.uk",
            // Instances of wildcard rules (`*.ck`, `*.kawasaki.jp`).
            "*.co.ck",
            "*.foo.kawasaki.jp",
            // Parents of wildcard rules: every name below is a suffix.
            "*.run.app",
            "*.kawasaki.jp",
            "*.elb.amazonaws.com",
            // On the list, so name the exact hosts instead.
            "*.googleapis.com",
            "*.githubusercontent.com",
            "*.cloudfront.net",
        ] {
            let why = check_hosts(&[refused.into()]).expect_err(refused);
            assert!(why.contains("public suffix"), "{refused}: {why}");
        }
        for accepted in [
            "*.example.com",
            "*.acme.github.io",
            "github.io",
            "allowed.invalid",
            // An exception to the `*.kawasaki.jp` rule is one site.
            "*.city.kawasaki.jp",
            // Above a suffix: it loads, and the proxy keeps it to its own
            // sites (see the next test).
            "*.amazonaws.com",
        ] {
            assert!(check_hosts(&[accepted.into()]).is_ok(), "{accepted}");
        }
    }
}
