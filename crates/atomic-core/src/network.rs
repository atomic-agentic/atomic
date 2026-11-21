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
//! resolve. The proxy never goes through another proxy, even when atomic's
//! own environment names one, and speaks only HTTP and HTTPS `CONNECT`, not
//! SOCKS.
//!
//! While a command that declared hosts runs, its proxy listens on this
//! machine's loopback interface without authentication: another process on
//! this machine can use it to reach the same hosts. It stops once no
//! command needs it.

use codex_network_proxy::{
    NetworkDecision, NetworkPolicyDecider, NetworkPolicyDeciderFuture, NetworkPolicyRequest,
    NetworkProxy, NetworkProxyConfig, NetworkProxyConstraints, NetworkProxyHandle,
    NetworkProxyState, PreparedManagedNetwork,
};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// The most hosts one atom may declare.
pub const MAX_HOSTS: usize = 32;

/// The ports a declared host is reached on: HTTP and HTTPS.
pub const PORTS: [u16; 2] = [80, 443];

/// The reason the proxy gives for a declared host on another port.
pub const PORT_REFUSED: &str = "port_not_allowed";

/// The reason the proxy gives for a host no atom declared.
pub const HOST_REFUSED: &str = "not_allowed";

/// Check an atom's declared hosts against atomic's policy, returning them
/// in a canonical order. Each is a DNS name with at least two labels, such
/// as `api.example.com`, or `*.` and such a name for its subdomains. Refused:
/// IP addresses in any spelling (dotted, decimal, hexadecimal or octal),
/// `localhost` and names under it, single-label names (which resolve on the
/// local network), names under `.local`, `.internal`, `.lan`, `.home.arpa`
/// and other private suffixes, a bare `*`, and anything with a scheme, a
/// port, a path or a Unix socket.
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
    Ok(lower.strip_suffix('.').unwrap_or(&lower).to_owned())
}

/// Whether `host`, as a request names it, is one of `declared`: the name
/// itself, or a subdomain of a `*.` name.
fn declared(declared: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    declared
        .iter()
        .any(|pattern| match pattern.strip_prefix("*.") {
            Some(parent) => host
                .strip_suffix(parent)
                .is_some_and(|rest| rest.len() > 1 && rest.ends_with('.')),
            None => host == *pattern,
        })
}

/// The proxy's decision for a request: a declared host on port 80 or 443.
/// The proxy then resolves the host, and still refuses one on this machine
/// or a private network.
struct DeclaredHosts(Vec<String>);

impl NetworkPolicyDecider for DeclaredHosts {
    fn decide(&self, req: NetworkPolicyRequest) -> NetworkPolicyDeciderFuture<'_> {
        let decision = if !declared(&self.0, &req.host) {
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
}

/// What a command holds while it runs: its proxy runs until no command
/// holds it.
pub(crate) type Lease = Arc<dyn std::any::Any + Send + Sync>;

/// The proxies a confining backend runs, one for each distinct list of
/// hosts, each kept while a command that declared it runs.
#[derive(Clone, Default)]
pub(crate) struct Proxies(Arc<Mutex<HashMap<Vec<String>, Weak<Running>>>>);

impl std::fmt::Debug for Proxies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hosts: Vec<Vec<String>> = self
            .0
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
    /// running, to hold until the command has ended. `loopback` names
    /// loopback addresses the proxy also reaches, for tests only. Called
    /// from a blocking thread, never from inside an async task.
    pub(crate) fn prepare(
        &self,
        hosts: &[String],
        loopback: &[String],
        env: HashMap<String, String>,
    ) -> anyhow::Result<(PreparedManagedNetwork, Lease)> {
        let mut key = hosts.to_vec();
        key.extend(loopback.iter().map(|l| format!("loopback:{l}")));
        let running = {
            let mut proxies = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("the network proxies' lock is poisoned"))?;
            proxies.retain(|_, running| running.strong_count() > 0);
            match proxies
                .get(&key)
                .and_then(Weak::upgrade)
                .filter(|running| alive(running))
            {
                Some(running) => running,
                None => {
                    let running = Arc::new(runtime()?.block_on(start(hosts, loopback))?);
                    proxies.insert(key, Arc::downgrade(&running));
                    running
                }
            }
        };
        let prepared = running.proxy.prepare_for_optional_environment(env, None)?;
        let lease: Lease = running;
        Ok((prepared, lease))
    }

    /// How many proxies run now.
    #[cfg(test)]
    fn running(&self) -> usize {
        self.0
            .lock()
            .map(|m| m.values().filter(|r| r.strong_count() > 0).count())
            .unwrap_or_default()
    }
}

/// Start a proxy that forwards only to `hosts` on ports 80 and 443, never
/// through another proxy and never over SOCKS, and reaches no address on
/// this machine or its network but `loopback`'s.
async fn start(hosts: &[String], loopback: &[String]) -> anyhow::Result<Running> {
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
            "*.s3.amazonaws.com".into(),
            "acme-reports.s3.us-east-1.amazonaws.com.".into(),
        ])
        .expect("public names");
        assert_eq!(
            ok,
            [
                "*.s3.amazonaws.com",
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
        let running = lease.downcast_ref::<Running>().expect("a running proxy");
        let cfg = runtime()
            .expect("runtime")
            .block_on(running.proxy.current_cfg())
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
            first_env.env, second_env.env,
            "one proxy for one list of hosts"
        );
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
}
