// SPDX-License-Identifier: Apache-2.0

//! The local operator's credential: a secret in the OS keyring.
//!
//! A gate decision, like every admin action, carries a human credential
//! that an atom's sandbox cannot read. Locally that is a random token per
//! user and project, kept in the OS keyring under the service `atomic`; the
//! gate store keeps only a verifier of it. Leaving approval off a model's
//! tool list is not the protection; the credential is.
//!
//! An atom's commands reach neither copy. The sandbox denies them atomic's
//! state directory, where the gate store lives (see
//! [`crate::codex::TurnSpec::state_dir`]). On Linux the keyring is the
//! Secret Service, over the D-Bus session bus, with a cache in the kernel
//! session keyring: the sandbox refuses every socket `connect`, and its
//! helper leaves the session keyring before it starts the command (see
//! [`crate::codex::dispatch`]).
//!
//! The store is behind [`SecretStore`] so the backend is one choice in one
//! place: [`OsKeyring`] for the platform keyring, [`MemorySecrets`] for
//! tests.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// The keyring service atomic's secrets are kept under.
pub const SERVICE: &str = "atomic";

/// Why a secret could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// The keyring failed or is unavailable.
    #[error("the keyring: {0}")]
    Keyring(String),
    /// There is already a credential for the account; enrolling again
    /// would lock out the one the gate store knows.
    #[error("{0} already has an operator credential")]
    Exists(String),
    /// No random bytes were available.
    #[error("no randomness for a new credential: {0}")]
    Random(String),
}

/// Where secrets are kept, by account, under [`SERVICE`].
pub trait SecretStore: std::fmt::Debug + Send + Sync {
    /// The secret for `account`, if there is one.
    fn load(&self, account: &str) -> Result<Option<String>, CredentialError>;
    /// Keep `secret` for `account`, replacing any secret it had.
    fn save(&self, account: &str, secret: &str) -> Result<(), CredentialError>;
    /// Forget `account`'s secret. Returns whether there was one.
    fn delete(&self, account: &str) -> Result<bool, CredentialError>;
}

/// The platform keyring: the Secret Service (with the kernel keyring as its
/// cache) on Linux, Keychain on macOS, Credential Manager on Windows.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsKeyring;

impl SecretStore for OsKeyring {
    fn load(&self, account: &str) -> Result<Option<String>, CredentialError> {
        use codex_keyring_store::KeyringStore;
        codex_keyring_store::DefaultKeyringStore
            .load(SERVICE, account)
            .map_err(|e| CredentialError::Keyring(e.message()))
    }

    fn save(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        use codex_keyring_store::KeyringStore;
        codex_keyring_store::DefaultKeyringStore
            .save(SERVICE, account, secret)
            .map_err(|e| CredentialError::Keyring(e.message()))
    }

    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        use codex_keyring_store::KeyringStore;
        codex_keyring_store::DefaultKeyringStore
            .delete(SERVICE, account)
            .map_err(|e| CredentialError::Keyring(e.message()))
    }
}

/// Secrets kept in memory, for tests. Nothing outlives the value.
#[derive(Debug, Default)]
pub struct MemorySecrets(Mutex<BTreeMap<String, String>>);

impl MemorySecrets {
    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        // A panic while the lock was held leaves a map that is still whole.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SecretStore for MemorySecrets {
    fn load(&self, account: &str) -> Result<Option<String>, CredentialError> {
        Ok(self.map().get(account).cloned())
    }

    fn save(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        self.map().insert(account.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        Ok(self.map().remove(account).is_some())
    }
}

/// The keyring account of `user`'s operator credential for the project
/// whose state is `state`: one credential per user and project.
pub fn operator_account(state: &crate::state::ProjectState, user: &str) -> String {
    let project = state
        .dir()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("operator:{user}@{project}")
}

/// The operator credential kept for `account`, if there is one.
pub fn operator_secret(
    store: &dyn SecretStore,
    account: &str,
) -> Result<Option<String>, CredentialError> {
    store.load(account)
}

/// Make a new operator credential for `account` and keep it in `store`:
/// 32 random bytes, hex-encoded. Refused when `account` already has one.
pub fn new_operator_secret(
    store: &dyn SecretStore,
    account: &str,
) -> Result<String, CredentialError> {
    if store.load(account)?.is_some() {
        return Err(CredentialError::Exists(account.to_owned()));
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| CredentialError::Random(e.to_string()))?;
    let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    store.save(account, &secret)?;
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_credential_is_random_kept_and_never_replaced() {
        let store = MemorySecrets::default();
        assert!(
            operator_secret(&store, "operator:dev@p")
                .expect("load")
                .is_none()
        );
        let first = new_operator_secret(&store, "operator:dev@p").expect("new");
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(
            operator_secret(&store, "operator:dev@p")
                .expect("load")
                .as_deref(),
            Some(first.as_str())
        );
        assert!(matches!(
            new_operator_secret(&store, "operator:dev@p"),
            Err(CredentialError::Exists(_))
        ));
        let other = new_operator_secret(&store, "operator:dev@q").expect("another project");
        assert_ne!(first, other);
        assert!(store.delete("operator:dev@p").expect("delete"));
        assert!(!store.delete("operator:dev@p").expect("delete"));
    }

    #[test]
    fn the_account_names_the_user_and_the_project() {
        let dir = std::env::temp_dir().join(format!("atomic-credential-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let state = crate::state::ProjectState::resolve(&dir).expect("state");
        let account = operator_account(&state, "dev");
        assert!(
            account.starts_with("operator:dev@atomic-credential-"),
            "{account}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
