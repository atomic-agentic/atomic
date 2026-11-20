// SPDX-License-Identifier: Apache-2.0

//! Signing in to a ChatGPT plan, through Codex's own login, for Codex atoms.
//!
//! A developer who signs in with `atomic model login` runs Codex atoms on
//! their plan with no API key set. The sign-in is the developer's own, for
//! local use: atomic keeps it once per user, apart from every project, as it
//! keeps the operator credential (see [`crate::credential`]):
//!
//! - in the OS keyring by default, through Codex's keyring storage;
//! - with `ATOMIC_CREDENTIAL_STORE=file`, in `codex-auth/` in the
//!   credential directory, readable by its owner only;
//! - on Linux only, where atomic has verified that atoms' sandboxes cannot
//!   reach it; elsewhere sign-in is refused.
//!
//! Atoms' commands never see it: the credential directory is denied to their
//! sandbox, their commands cannot use the kernel keyrings or reach the
//! Secret Service, and Codex sends the token to the model backend from
//! atomic's own process. A signed-in thread has none of Codex's ChatGPT
//! connectors, plugins or hosted tools (see `crate::codex`), so the token
//! reaches the model and nothing else. Lite atoms call a model API directly
//! and are not covered: they need an endpoint and, if it asks for one, a
//! key.

use crate::credential::{CredentialError, STORE_VAR};
use codex_core_api::{AuthCredentialsStoreMode, AuthKeyringBackendKind};
use std::path::{Path, PathBuf};

/// Where a sign-in is kept, and how Codex atoms use it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignIn {
    auth_home: PathBuf,
    store: AuthCredentialsStoreMode,
    backend: Option<String>,
    issuer: Option<String>,
}

/// Who is signed in, without any token.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Account {
    /// How: `chatgpt`, or `api key` for a key saved by a login.
    pub mode: String,
    /// The account's email, if the sign-in says.
    pub email: Option<String>,
    /// The ChatGPT plan, if the sign-in says.
    pub plan: Option<String>,
    /// Where it is kept: the OS keyring or the credential file store.
    pub store: String,
}

/// A resolved setting as `atomic config --explain` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Explained {
    /// The setting's value, safe to print.
    pub value: String,
    /// Where it came from.
    pub source: String,
}

/// Why there is no sign-in to use or keep.
#[derive(Debug, thiserror::Error)]
pub enum SignInError {
    /// Not on a platform where atoms' reach to it has been verified.
    #[error(
        "a ChatGPT sign-in is kept only on Linux, where atomic has verified that atoms' \
         sandboxes cannot reach it; on {0} configure a model endpoint and API key instead"
    )]
    Unprotected(&'static str),
    /// The store could not be chosen or opened.
    #[error(transparent)]
    Store(#[from] CredentialError),
}

const KEYRING: AuthKeyringBackendKind = AuthKeyringBackendKind::Direct;

/// Refused unless sign-ins may be kept on this platform.
pub fn protected() -> Result<(), SignInError> {
    protected_on(std::env::consts::OS)
}

fn protected_on(os: &'static str) -> Result<(), SignInError> {
    if os == "linux" {
        Ok(())
    } else {
        Err(SignInError::Unprotected(os))
    }
}

/// What `atomic config --explain` shows for model access from a ChatGPT
/// sign-in, or `None` when no one is signed in or sign-in is unavailable
/// here (off Linux, or no keyring). Model access from a configured endpoint
/// takes precedence (see the engine's endpoint selection). Creates nothing.
pub fn explain() -> Option<Explained> {
    SignIn::located().ok()?.explain()
}

impl SignIn {
    /// Where the sign-in store `ATOMIC_CREDENTIAL_STORE` chooses is, without
    /// creating anything.
    fn located() -> Result<Self, SignInError> {
        protected()?;
        let dir = crate::state::credentials_dir()
            .map_err(|e| CredentialError::File(e.to_string()))?
            .join("codex-auth");
        crate::credential::placed_apart(&dir, None)?;
        // One spelling, so the keyring entry Codex keys by it is the same
        // however the directory is reached.
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        let store = match std::env::var(STORE_VAR).ok().filter(|v| !v.is_empty()) {
            None => AuthCredentialsStoreMode::Keyring,
            Some(v) if v == "keyring" => AuthCredentialsStoreMode::Keyring,
            Some(v) if v == "file" => AuthCredentialsStoreMode::File,
            Some(other) => return Err(CredentialError::UnknownStore(other).into()),
        };
        Ok(Self {
            auth_home: dir,
            store,
            backend: None,
            issuer: None,
        })
    }

    /// The sign-in store `ATOMIC_CREDENTIAL_STORE` chooses, its directory
    /// created readable by its owner only. Refused off Linux.
    pub fn configured() -> Result<Self, SignInError> {
        let located = Self::located()?;
        crate::state::private_dir(&located.auth_home)
            .map_err(|e| CredentialError::File(e.to_string()))?;
        let auth_home = std::fs::canonicalize(&located.auth_home).unwrap_or(located.auth_home);
        Ok(Self {
            auth_home,
            ..located
        })
    }

    /// A sign-in kept in files under `dir`, for tests.
    #[cfg(any(test, feature = "fake-model"))]
    pub fn in_files(dir: &Path) -> Self {
        Self {
            auth_home: dir.to_owned(),
            store: AuthCredentialsStoreMode::File,
            backend: None,
            issuer: None,
        }
    }

    /// For tests: every ChatGPT route at `backend` (such as
    /// `http://127.0.0.1:<port>/backend-api`) instead of ChatGPT's.
    #[cfg(any(test, feature = "fake-model"))]
    pub fn with_backend(mut self, backend: &str) -> Self {
        self.backend = Some(backend.trim_end_matches('/').to_owned());
        self
    }

    /// For tests: log in at `issuer` instead of OpenAI's.
    #[cfg(any(test, feature = "fake-model"))]
    pub fn with_issuer(mut self, issuer: &str) -> Self {
        self.issuer = Some(issuer.trim_end_matches('/').to_owned());
        self
    }

    /// The directory Codex keys its storage by.
    pub fn auth_home(&self) -> &Path {
        &self.auth_home
    }

    /// The backend tests point the sign-in at, if any.
    pub(crate) fn backend(&self) -> Option<&str> {
        self.backend.as_deref()
    }

    pub(crate) fn store(&self) -> AuthCredentialsStoreMode {
        self.store
    }

    /// Where the sign-in is kept, for a person: never a token.
    pub fn store_name(&self) -> &'static str {
        match self.store {
            AuthCredentialsStoreMode::File => "the credential file store",
            _ => "the OS keyring",
        }
    }

    /// Who is signed in, if anyone.
    pub fn account(&self) -> anyhow::Result<Option<Account>> {
        protected()?;
        let auth = codex_login::load_auth_dot_json(&self.auth_home, self.store, KEYRING)?;
        let store = self.store_name().to_owned();
        Ok(auth.and_then(|a| {
            if let Some(tokens) = &a.tokens {
                return Some(Account {
                    mode: "chatgpt".into(),
                    email: tokens.id_token.email.clone(),
                    plan: tokens.id_token.get_chatgpt_plan_type(),
                    store: store.clone(),
                });
            }
            a.openai_api_key.as_ref().map(|_| Account {
                mode: "api key".into(),
                email: None,
                plan: None,
                store: store.clone(),
            })
        }))
    }

    /// What `atomic config --explain` shows for model access from this
    /// sign-in, if anyone is signed in: how and where, never a token or an
    /// email.
    pub fn explain(&self) -> Option<Explained> {
        let account = self.account().ok().flatten()?;
        let value = match account.plan {
            Some(plan) => format!("signed in with ChatGPT ({plan} plan), for Codex atoms"),
            None => format!("signed in with {}, for Codex atoms", account.mode),
        };
        Some(Explained {
            value,
            source: format!("atomic model login, kept in {}", self.store_name()),
        })
    }

    /// Sign in in a browser, or with a device code on a machine with none
    /// at hand, and keep the sign-in. The keyring is checked first, so a
    /// machine without one is told so before any browser flow.
    pub async fn login(&self, device: bool) -> anyhow::Result<()> {
        protected()?;
        if self.store != AuthCredentialsStoreMode::File {
            keyring_usable()?;
        }
        let mut options = codex_login::ServerOptions::new(
            self.auth_home.clone(),
            codex_login::CLIENT_ID.to_owned(),
            None,
            self.store,
            KEYRING,
            crate::codex::auth_route_config(&self.auth_home).await?,
        );
        if let Some(issuer) = &self.issuer {
            options.issuer = issuer.clone();
            options.open_browser = false;
            options.port = 0;
        }
        if device {
            codex_login::run_device_code_login(options).await?;
        } else {
            let server = codex_login::run_login_server(options)?;
            eprintln!(
                "Sign in in your browser. If it did not open, go to:\n\n  {}\n",
                server.auth_url
            );
            #[cfg(any(test, feature = "fake-model"))]
            if let Some(hook) = LOGIN_STARTED.get() {
                hook(&server.auth_url, server.actual_port);
            }
            server.block_until_done().await?;
        }
        Ok(())
    }

    /// Revoke the sign-in's tokens where the issuer allows, and forget every
    /// copy of it, in the keyring and in files, even when revoking fails.
    /// Returns whether there was one.
    pub async fn logout(&self) -> anyhow::Result<bool> {
        protected()?;
        let route = crate::codex::auth_route_config(&self.auth_home).await?;
        Ok(codex_login::logout_with_revoke(&self.auth_home, self.store, KEYRING, &route).await?)
    }

    /// The keyring backend Codex's storage uses for it.
    pub(crate) fn keyring_kind(&self) -> AuthKeyringBackendKind {
        KEYRING
    }
}

/// For tests: told the login URL and port once the login server listens.
#[cfg(any(test, feature = "fake-model"))]
#[allow(clippy::type_complexity)]
pub static LOGIN_STARTED: std::sync::OnceLock<Box<dyn Fn(&str, u16) + Send + Sync>> =
    std::sync::OnceLock::new();

/// Whether the OS keyring takes and gives back a value, so a missing one is
/// reported before a browser flow that could not be kept.
fn keyring_usable() -> Result<(), CredentialError> {
    use crate::credential::SecretStore;
    let probe = format!("probe:{}", std::process::id());
    let store = crate::credential::OsKeyring;
    store.save(&probe, "probe")?;
    let _ = store.delete(&probe);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_names_how_and_where_and_no_secret() {
        let dir = std::env::temp_dir().join(format!("atomic-signin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let sign_in = SignIn::in_files(&dir);
        assert_eq!(sign_in.explain(), None, "no one is signed in");
        std::fs::write(
            dir.join("auth.json"),
            r#"{"OPENAI_API_KEY": "sk-not-a-real-key"}"#,
        )
        .expect("auth.json");
        let explained = sign_in.explain().expect("signed in");
        assert_eq!(explained.value, "signed in with api key, for Codex atoms");
        assert_eq!(
            explained.source,
            "atomic model login, kept in the credential file store"
        );
        assert!(!format!("{explained:?}").contains("sk-not"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `atomic model login`'s browser flow, end to end against a test
    /// issuer and with this test as the browser: the sign-in it keeps is the
    /// issuer's, and no request leaves the machine.
    #[test]
    fn login_keeps_the_sign_in_the_issuer_gives() {
        crate::testing::on_codex_runtime(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let issuer = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("issuer");
            let port = issuer.local_addr().expect("addr").port();
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let log = seen.clone();
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = issuer.accept().await {
                    let mut request = vec![0u8; 16 * 1024];
                    let n = socket.read(&mut request).await.unwrap_or(0);
                    let text = String::from_utf8_lossy(&request[..n]).into_owned();
                    let line = text.lines().next().unwrap_or_default().to_owned();
                    log.lock().expect("log").push(line.clone());
                    let body = if line.starts_with("POST /oauth/token") {
                        serde_json::json!({
                            "id_token": crate::testing::fake_id_token("dev@example.invalid", "account-1"),
                            "access_token": "issued-access-token",
                            "refresh_token": "issued-refresh-token",
                        })
                        .to_string()
                    } else {
                        "{}".to_owned()
                    };
                    let status = if line.starts_with("POST /oauth/token") {
                        "200 OK"
                    } else {
                        "404 Not Found"
                    };
                    let reply = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                }
            });
            let dir = std::env::temp_dir().join(format!("atomic-login-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("dir");
            let sign_in = SignIn::in_files(&dir).with_issuer(&format!("http://127.0.0.1:{port}"));
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = LOGIN_STARTED.set(Box::new(move |url: &str, port: u16| {
                let _ = tx.send((url.to_owned(), port));
            }));
            // The browser: read the state from the authorize URL, and come back
            // to the login server's callback with a code.
            let browser = std::thread::spawn(move || {
                let (url, port) = rx.recv().expect("the login server started");
                let state = url
                    .split(['?', '&'])
                    .find_map(|p| p.strip_prefix("state="))
                    .expect("a state")
                    .to_owned();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                runtime.block_on(async {
                    reqwest::Client::new()
                        .get(format!(
                            "http://127.0.0.1:{port}/auth/callback?code=the-code&state={state}"
                        ))
                        .send()
                        .await
                        .map(|r| r.status())
                })
            });
            sign_in.login(false).await.expect("logged in");
            let status = browser.join().expect("browser");
            assert!(status.is_ok(), "{status:?}");
            let account = sign_in.account().expect("account").expect("signed in");
            assert_eq!(account.mode, "chatgpt");
            assert_eq!(account.email.as_deref(), Some("dev@example.invalid"));
            let kept = std::fs::read_to_string(dir.join("auth.json")).expect("kept");
            assert!(kept.contains("issued-access-token"));
            let seen = seen.lock().expect("log").clone();
            assert!(
                seen.iter().any(|l| l.starts_with("POST /oauth/token")),
                "the code was exchanged at the test issuer: {seen:?}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn sign_in_is_refused_off_linux_with_its_own_message() {
        assert!(protected_on("linux").is_ok());
        for os in ["macos", "windows"] {
            let refused = protected_on(os).expect_err("refused").to_string();
            assert!(
                refused.contains("ChatGPT sign-in is kept only on Linux"),
                "{refused}"
            );
            assert!(refused.contains(os), "{refused}");
        }
    }
}
