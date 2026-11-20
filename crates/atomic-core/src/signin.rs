// SPDX-License-Identifier: Apache-2.0

//! Signing in to a ChatGPT plan, through Codex's own login, for Codex atoms.
//!
//! A developer who signs in with `atomic login` runs Codex atoms on their
//! plan with no API key set. The sign-in is the developer's own, for local
//! use: atomic keeps it once per user, apart from every project, as it keeps
//! the operator credential (see [`crate::credential`]):
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
//! atomic's own process. Lite atoms call a model API directly and are not
//! covered: they need an endpoint and, if it asks for one, a key.

use crate::credential::{CredentialError, STORE_VAR, protected_platform};
use codex_core_api::{AuthCredentialsStoreMode, AuthKeyringBackendKind};
use std::path::{Path, PathBuf};

/// Where a sign-in is kept, and how Codex atoms use it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignIn {
    /// The directory Codex keys its storage by: the file store's directory,
    /// or the name of the keyring entry.
    pub auth_home: PathBuf,
    /// The OS keyring or a file.
    pub store: AuthCredentialsStoreMode,
    /// For tests: the backend Codex atoms call instead of ChatGPT's.
    pub backend: Option<String>,
}

/// Who is signed in, without any token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    /// How: `chatgpt`, or `api key` for a key saved by a login.
    pub mode: String,
    /// The account's email, if the sign-in says.
    pub email: Option<String>,
    /// The ChatGPT plan, if the sign-in says.
    pub plan: Option<String>,
}

/// A resolved setting as `atomic config --explain` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Explained {
    /// The setting's value, safe to print.
    pub value: String,
    /// Where it came from.
    pub source: String,
}

/// What `atomic config --explain` shows for model access from a ChatGPT
/// sign-in, or `None` when no one is signed in or sign-in is unavailable
/// here (off Linux, or no keyring). Model access from a configured endpoint
/// takes precedence (see the engine's endpoint selection).
pub fn explain() -> Option<Explained> {
    SignIn::configured().ok()?.explain()
}

const KEYRING: AuthKeyringBackendKind = AuthKeyringBackendKind::Direct;

impl SignIn {
    /// The sign-in store `ATOMIC_CREDENTIAL_STORE` chooses. Refused off
    /// Linux.
    pub fn configured() -> Result<Self, CredentialError> {
        protected_platform()?;
        let dir = crate::state::credentials_dir()
            .map_err(|e| CredentialError::File(e.to_string()))?
            .join("codex-auth");
        let store = match std::env::var(STORE_VAR).ok().filter(|v| !v.is_empty()) {
            None => AuthCredentialsStoreMode::Keyring,
            Some(v) if v == "keyring" => AuthCredentialsStoreMode::Keyring,
            Some(v) if v == "file" => AuthCredentialsStoreMode::File,
            Some(other) => return Err(CredentialError::UnknownStore(other)),
        };
        crate::state::private_dir(&dir).map_err(|e| CredentialError::File(e.to_string()))?;
        Ok(Self {
            auth_home: dir,
            store,
            backend: None,
        })
    }

    /// A sign-in kept in files under `dir`, for tests.
    pub fn in_files(dir: &Path) -> Self {
        Self {
            auth_home: dir.to_owned(),
            store: AuthCredentialsStoreMode::File,
            backend: None,
        }
    }

    /// Who is signed in, if anyone.
    pub fn account(&self) -> anyhow::Result<Option<Account>> {
        let auth = codex_login::load_auth_dot_json(&self.auth_home, self.store, KEYRING)?;
        Ok(auth.and_then(|a| {
            if let Some(tokens) = &a.tokens {
                return Some(Account {
                    mode: "chatgpt".into(),
                    email: tokens.id_token.email.clone(),
                    plan: tokens.id_token.get_chatgpt_plan_type(),
                });
            }
            a.openai_api_key.as_ref().map(|_| Account {
                mode: "api key".into(),
                email: None,
                plan: None,
            })
        }))
    }

    /// Sign in in a browser, or with a device code on a machine with none
    /// at hand, and keep the sign-in.
    pub async fn login(&self, device: bool) -> anyhow::Result<()> {
        let options = codex_login::ServerOptions::new(
            self.auth_home.clone(),
            codex_login::CLIENT_ID.to_owned(),
            None,
            self.store,
            KEYRING,
            crate::codex::auth_route_config(&self.auth_home).await?,
        );
        if device {
            codex_login::run_device_code_login(options).await?;
        } else {
            let server = codex_login::run_login_server(options)?;
            eprintln!(
                "Sign in in your browser. If it did not open, go to:\n\n  {}\n",
                server.auth_url
            );
            server.block_until_done().await?;
        }
        Ok(())
    }

    /// Forget the sign-in. Returns whether there was one.
    pub fn logout(&self) -> anyhow::Result<bool> {
        Ok(codex_login::logout(&self.auth_home, self.store, KEYRING)?)
    }

    /// Where the sign-in is kept, for a person: never a token, never the
    /// account's email.
    pub fn store_name(&self) -> &'static str {
        match self.store {
            AuthCredentialsStoreMode::File => "the credential file store",
            _ => "the OS keyring",
        }
    }

    /// What `atomic config --explain` shows for model access from this
    /// sign-in, if anyone is signed in: the value and where it comes from.
    /// It names how and where, never a token or an email.
    pub fn explain(&self) -> Option<Explained> {
        let account = self.account().ok().flatten()?;
        let value = match account.plan {
            Some(plan) => format!("signed in with ChatGPT ({plan} plan), for Codex atoms"),
            None => format!("signed in with {}, for Codex atoms", account.mode),
        };
        Some(Explained {
            value,
            source: format!("atomic login, kept in {}", self.store_name()),
        })
    }

    /// The keyring backend Codex's storage uses for it.
    pub(crate) fn keyring_kind(&self) -> AuthKeyringBackendKind {
        KEYRING
    }
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
            "atomic login, kept in the credential file store"
        );
        assert!(!format!("{explained:?}").contains("sk-not"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
