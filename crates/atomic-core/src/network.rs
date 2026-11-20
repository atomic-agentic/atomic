// SPDX-License-Identifier: Apache-2.0

//! Network access for an exec atom's command.
//!
//! A command has no network unless its atom declares the hosts it reaches,
//! as `network: { allow: [...] }`. Then its sandbox gives it a network
//! namespace of its own whose only way out is a proxy atomic runs, which
//! forwards a request only to a declared host. The command finds the proxy
//! in the usual variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and
//! their kin); it can open no other connection and no Unix socket.
//!
//! What an atom may declare is atomic's policy, checked when the atom loads
//! ([`check_hosts`]): public DNS names only, so a declaration never reaches
//! this machine, its private network or a cloud metadata service.

use codex_network_proxy::{
    NetworkProxy, NetworkProxyConfig, NetworkProxyConstraints, NetworkProxyHandle,
    NetworkProxyState, PreparedManagedNetwork,
};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

/// The most hosts one atom may declare.
pub const MAX_HOSTS: usize = 32;

/// Check an atom's declared hosts against atomic's policy, returning them
/// in a canonical order. Each is a DNS name with at least two labels, such
/// as `api.example.com`, or `*.` and such a name for its subdomains. Refused:
/// IP addresses, `localhost` and names under it, single-label names (which
/// resolve on the local network), names under `.local`, `.internal`,
/// `.lan`, `.home.arpa` and other private suffixes, a bare `*`, and anything
/// with a scheme, a port, a path or a Unix socket.
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
    if name.parse::<std::net::IpAddr>().is_ok()
        || name
            .split('.')
            .all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()))
    {
        return refuse("an IP address is not a host an atom may declare; name the host");
    }
    let labels: Vec<&str> = name.split('.').collect();
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

/// A running proxy for one list of hosts.
struct Running {
    proxy: NetworkProxy,
    _handle: NetworkProxyHandle,
}

/// The proxies a confining backend runs, one for each distinct list of
/// hosts, kept for as long as the process runs.
#[derive(Clone, Default)]
pub(crate) struct Proxies(Arc<Mutex<HashMap<Vec<String>, Arc<Running>>>>);

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

impl Proxies {
    /// `env` with the variables that send a command's traffic through the
    /// proxy for `hosts`, started now if none runs for them yet, and what
    /// the sandbox needs to route it there. Called from a blocking thread,
    /// never from inside an async task.
    pub(crate) fn prepare(
        &self,
        hosts: &[String],
        env: HashMap<String, String>,
    ) -> anyhow::Result<PreparedManagedNetwork> {
        let running = {
            let mut proxies = self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("the network proxies' lock is poisoned"))?;
            match proxies.get(hosts) {
                Some(running) => running.clone(),
                None => {
                    let running = Arc::new(runtime()?.block_on(start(hosts))?);
                    proxies.insert(hosts.to_vec(), running.clone());
                    running
                }
            }
        };
        running.proxy.prepare_for_optional_environment(env, None)
    }
}

/// Start a proxy that forwards only to `hosts`: no other host, no address
/// on this machine or its network, and no Unix socket.
async fn start(hosts: &[String]) -> anyhow::Result<Running> {
    let mut config = NetworkProxyConfig {
        enabled: true,
        allow_local_binding: Some(false),
        dangerously_allow_all_unix_sockets: Some(false),
        ..Default::default()
    };
    config.set_allowed_domains(hosts.to_vec());
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
}
