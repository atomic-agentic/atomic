// SPDX-License-Identifier: Apache-2.0

//! The local operator's credential: a secret in the OS keyring.
//!
//! A gate decision, like every admin action, carries a human credential
//! that an atom's sandbox cannot read. Locally that is a random token per
//! user and project, kept in the OS keyring under the service `atomic`; the
//! gate store keeps only a verifier of it. Leaving approval off a model's
//! tool list is not the protection; the credential is. The person enrolls it
//! with `atomic gates enroll`, from a terminal; nothing enrolls on first use.
//!
//! An atom's commands reach neither copy, which atomic has verified on
//! Linux only:
//!
//! - The sandbox denies them the state directory's `private/`, where the
//!   gate store is, and the credential file store (see
//!   [`crate::codex::StateGuard`]).
//! - The keyring is the Secret Service, over the D-Bus session bus, with a
//!   cache in the kernel keyrings. The sandbox refuses every socket
//!   `connect` (a future admin socket must therefore be a stream socket:
//!   Codex's filter does not stop a datagram `sendmsg`), and atomic's
//!   sandbox helper denies the command `keyctl`, `add_key` and
//!   `request_key`, after leaving the session keyring (see
//!   [`crate::codex::dispatch`]).
//!
//! On other platforms the sandboxes have not been examined for this, so the
//! platform stores refuse ([`CredentialError::Unprotected`]) and no
//! credentialed decision can be made there.
//!
//! The store is behind [`SecretStore`] so the backend is one choice in one
//! place: [`OsKeyring`] for the platform keyring; [`FileSecrets`], a
//! directory readable by its owner only, for headless machines and
//! containers, where there is no Secret Service and opting in is explicit
//! (`ATOMIC_CREDENTIAL_STORE=file`, see [`configured_store`]); and
//! [`MemorySecrets`] for tests.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The keyring service atomic's secrets are kept under.
pub const SERVICE: &str = "atomic";

/// The environment variable that chooses the store: `keyring` (the
/// default) or `file`.
pub const STORE_VAR: &str = "ATOMIC_CREDENTIAL_STORE";

/// Why a secret could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// The keyring failed.
    #[error("the keyring: {0}")]
    Keyring(String),
    /// There is no usable keyring here, as on a headless machine or in a
    /// container.
    #[error(
        "no OS keyring is available here ({0}); run atomic where one is, or opt in to a file \
         store readable by you only with {STORE_VAR}=file"
    )]
    Unavailable(String),
    /// On this platform atomic has not verified that atoms' sandboxes cannot
    /// reach the credential, so it keeps none.
    #[error(
        "operator credentials are kept only on Linux, where atomic has verified that atoms' \
         sandboxes cannot reach them; on {0} credentialed gate decisions are refused"
    )]
    Unprotected(&'static str),
    /// There is already a credential for the account; enrolling again
    /// would lock out the one the gate store knows.
    #[error("{0} already has an operator credential")]
    Exists(String),
    /// No random bytes were available.
    #[error("no randomness for a new credential: {0}")]
    Random(String),
    /// The file store failed or is misplaced.
    #[error("the credential file store: {0}")]
    File(String),
    /// `ATOMIC_CREDENTIAL_STORE` names no store.
    #[error("{STORE_VAR}={0} names no credential store; use keyring or file")]
    UnknownStore(String),
}

/// Refused unless this platform's sandbox has been verified to keep atoms
/// away from the credential.
pub fn protected_platform() -> Result<(), CredentialError> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        Err(CredentialError::Unprotected(std::env::consts::OS))
    }
}

/// Where secrets are kept, by account, under [`SERVICE`].
pub trait SecretStore: std::fmt::Debug + Send + Sync {
    /// The secret for `account`, if there is one.
    fn load(&self, account: &str) -> Result<Option<String>, CredentialError>;
    /// Keep `secret` for `account`, replacing any secret it had.
    fn save(&self, account: &str, secret: &str) -> Result<(), CredentialError>;
    /// Forget `account`'s secret. Returns whether there was one.
    fn delete(&self, account: &str) -> Result<bool, CredentialError>;
    /// Keep `secret` for `account` only if it has none, and return the one
    /// it has afterwards. A store that cannot do this atomically checks and
    /// then saves, and reads back what it holds, so racing first runs agree
    /// on the secret that was kept last.
    fn save_new(&self, account: &str, secret: &str) -> Result<String, CredentialError> {
        if let Some(existing) = self.load(account)? {
            return Ok(existing);
        }
        self.save(account, secret)?;
        Ok(self.load(account)?.unwrap_or_else(|| secret.to_owned()))
    }
}

/// The platform keyring: the Secret Service (with the kernel keyrings as
/// its cache) on Linux. Refused elsewhere (see [`protected_platform`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct OsKeyring;

/// A keyring failure: on Linux, a missing Secret Service is the common one,
/// so it says how to go on.
fn keyring_error(e: codex_keyring_store::CredentialStoreError) -> CredentialError {
    let message = e.message();
    let lower = message.to_lowercase();
    if [
        "d-bus",
        "dbus",
        "secret service",
        "platform",
        "no such",
        "not provided",
    ]
    .iter()
    .any(|m| lower.contains(m))
    {
        CredentialError::Unavailable(message)
    } else {
        CredentialError::Keyring(message)
    }
}

impl SecretStore for OsKeyring {
    fn load(&self, account: &str) -> Result<Option<String>, CredentialError> {
        use codex_keyring_store::KeyringStore;
        protected_platform()?;
        codex_keyring_store::DefaultKeyringStore
            .load(SERVICE, account)
            .map_err(keyring_error)
    }

    fn save(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        use codex_keyring_store::KeyringStore;
        protected_platform()?;
        codex_keyring_store::DefaultKeyringStore
            .save(SERVICE, account, secret)
            .map_err(keyring_error)
    }

    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        use codex_keyring_store::KeyringStore;
        protected_platform()?;
        codex_keyring_store::DefaultKeyringStore
            .delete(SERVICE, account)
            .map_err(keyring_error)
    }
}

/// Secrets in files, one per account, in a directory readable by its owner
/// only, outside every state directory and the project. Atoms' sandboxes
/// deny the directory (see [`crate::codex::StateGuard`]). For machines with
/// no OS keyring, by explicit choice only.
#[derive(Clone, Debug)]
pub struct FileSecrets {
    dir: PathBuf,
}

impl FileSecrets {
    /// The store in `dir`, created readable by its owner only. Refused when
    /// `dir` is inside atomic's state, or inside or above `project`.
    pub fn open(dir: &Path, project: &Path) -> Result<Self, CredentialError> {
        protected_platform()?;
        let fail = |what: String| CredentialError::File(what);
        placed_apart(dir, Some(project))?;
        crate::state::private_dir(dir).map_err(|e| fail(e.to_string()))?;
        Ok(Self {
            dir: dir.to_owned(),
        })
    }

    fn file(&self, account: &str) -> PathBuf {
        let name: String = account.bytes().map(|b| format!("{b:02x}")).collect();
        self.dir.join(name)
    }

    /// Write `secret` to a new file in the store, readable by its owner only.
    fn staged(&self, secret: &str) -> Result<PathBuf, CredentialError> {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(|e| CredentialError::Random(e.to_string()))?;
        let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let path = self.dir.join(format!(".new-{name}"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(&path)
            .map_err(|e| CredentialError::File(format!("{}: {e}", path.display())))?;
        file.write_all(secret.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| CredentialError::File(format!("{}: {e}", path.display())))?;
        Ok(path)
    }
}

impl SecretStore for FileSecrets {
    fn load(&self, account: &str) -> Result<Option<String>, CredentialError> {
        match std::fs::read_to_string(self.file(account)) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CredentialError::File(e.to_string())),
        }
    }

    fn save(&self, account: &str, secret: &str) -> Result<(), CredentialError> {
        let staged = self.staged(secret)?;
        std::fs::rename(&staged, self.file(account)).map_err(|e| {
            let _ = std::fs::remove_file(&staged);
            CredentialError::File(e.to_string())
        })
    }

    fn delete(&self, account: &str) -> Result<bool, CredentialError> {
        match std::fs::remove_file(self.file(account)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(CredentialError::File(e.to_string())),
        }
    }

    fn save_new(&self, account: &str, secret: &str) -> Result<String, CredentialError> {
        // A hard link fails if the account's file exists: the first wins.
        let staged = self.staged(secret)?;
        let linked = std::fs::hard_link(&staged, self.file(account));
        let _ = std::fs::remove_file(&staged);
        match linked {
            Ok(()) => Ok(secret.to_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => self
                .load(account)?
                .ok_or_else(|| CredentialError::File("the credential vanished".into())),
            Err(e) => Err(CredentialError::File(e.to_string())),
        }
    }
}

/// Refused unless `dir` is an absolute path outside every state directory
/// and, given one, outside and not above `project`: where the private file
/// stores may live.
pub(crate) fn placed_apart(dir: &Path, project: Option<&Path>) -> Result<(), CredentialError> {
    let fail = |what: String| CredentialError::File(what);
    if !dir.is_absolute() {
        return Err(fail(format!("{} is not an absolute path", dir.display())));
    }
    let real = crate::state::as_it_really_is;
    let projects = real(
        &crate::state::base_dir()
            .map_err(|e| fail(e.to_string()))?
            .join("projects"),
    );
    let here = real(dir);
    if here.starts_with(&projects) || projects.starts_with(&here) {
        return Err(fail(format!(
            "{} overlaps atomic's state directories",
            dir.display()
        )));
    }
    if let Some(project) = project {
        let project = real(project);
        if here.starts_with(&project) || project.starts_with(&here) {
            return Err(fail(format!("{} overlaps the project", dir.display())));
        }
    }
    Ok(())
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

    fn save_new(&self, account: &str, secret: &str) -> Result<String, CredentialError> {
        Ok(self
            .map()
            .entry(account.to_owned())
            .or_insert_with(|| secret.to_owned())
            .clone())
    }
}

/// The store `ATOMIC_CREDENTIAL_STORE` chooses for the project rooted at
/// `project`: the OS keyring by default; with `file`, [`FileSecrets`] in
/// [`crate::state::credentials_dir`], refused while an older atomic's
/// credentials are stranded at its old place (see
/// [`crate::state::credentials_dir_in_use`]).
pub fn configured_store(project: &Path) -> Result<Box<dyn SecretStore>, CredentialError> {
    let chosen = crate::state::process_env(STORE_VAR).and_then(|v| v.into_string().ok());
    match chosen.filter(|v| !v.is_empty()) {
        None => Ok(Box::new(OsKeyring)),
        Some(v) if v == "keyring" => Ok(Box::new(OsKeyring)),
        Some(v) if v == "file" => {
            let dir = crate::state::credentials_dir_in_use().map_err(CredentialError::File)?;
            Ok(Box::new(FileSecrets::open(&dir, project)?))
        }
        Some(other) => Err(CredentialError::UnknownStore(other)),
    }
}

/// The local operator's account name: `$USER`, or `$LOGNAME`.
pub fn local_user() -> Option<String> {
    ["USER", "LOGNAME"]
        .into_iter()
        .find_map(|v| std::env::var(v).ok().filter(|u| !u.is_empty()))
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

/// 32 random bytes, hex-encoded.
pub fn fresh_secret() -> Result<String, CredentialError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| CredentialError::Random(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The operator credential for `account`: the one `store` keeps, or, the
/// first time, a new one it keeps from now on. Racing first runs agree on
/// one secret. Returns it and whether it is new.
pub fn load_or_create(
    store: &dyn SecretStore,
    account: &str,
) -> Result<(String, bool), CredentialError> {
    if let Some(existing) = store.load(account)? {
        return Ok((existing, false));
    }
    let fresh = fresh_secret()?;
    let kept = store.save_new(account, &fresh)?;
    let created = kept == fresh;
    Ok((kept, created))
}

/// Make a new operator credential for `account` and keep it in `store`.
/// Refused when `account` already has one.
pub fn new_operator_secret(
    store: &dyn SecretStore,
    account: &str,
) -> Result<String, CredentialError> {
    match load_or_create(store, account)? {
        (secret, true) => Ok(secret),
        (_, false) => Err(CredentialError::Exists(account.to_owned())),
    }
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
        assert_eq!(
            load_or_create(&store, "operator:dev@p").expect("load"),
            (first.clone(), false)
        );
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
            "the account names a different user or project"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("atomic-credential-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_file_store_is_private_atomic_and_kept_apart() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("file");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).expect("project");
        let store = FileSecrets::open(&dir.join("credentials"), &project).expect("open");
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join("credentials")), 0o700);

        // Racing first runs agree on one secret.
        let racers: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                std::thread::spawn(move || load_or_create(&store, "operator:dev@p").expect("race"))
            })
            .collect();
        let results: Vec<(String, bool)> = racers
            .into_iter()
            .map(|t| t.join().expect("racer"))
            .collect();
        assert!(results.windows(2).all(|w| w[0].0 == w[1].0), "{results:?}");
        assert_eq!(results.iter().filter(|(_, created)| *created).count(), 1);
        assert_eq!(mode(&store.file("operator:dev@p")), 0o600);

        // Saving replaces; nothing staged is left behind.
        store.save("operator:dev@p", "rotated").expect("save");
        assert_eq!(
            store.load("operator:dev@p").expect("load").as_deref(),
            Some("rotated")
        );
        let names: Vec<_> = std::fs::read_dir(dir.join("credentials"))
            .expect("list")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.starts_with(".new-")), "{names:?}");

        // Never inside the project, nor in atomic's state.
        assert!(FileSecrets::open(&project.join("creds"), &project).is_err());
        assert!(
            FileSecrets::open(&dir, &project).is_err(),
            "above the project"
        );
        let state = crate::state::base_dir()
            .expect("base")
            .join("projects/x/creds");
        assert!(FileSecrets::open(&state, &project).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_store_is_refused() {
        // Only this test sets the variable, and it reads it back at once.
        #[allow(unsafe_code)]
        // SAFETY: no other test in this binary reads or writes it.
        unsafe {
            std::env::set_var(STORE_VAR, "plaintext")
        };
        let refused = configured_store(Path::new("/nowhere"));
        #[allow(unsafe_code)]
        // SAFETY: as above.
        unsafe {
            std::env::remove_var(STORE_VAR)
        };
        assert!(matches!(refused, Err(CredentialError::UnknownStore(_))));
    }

    /// The file store a gate opens for the operator credential is refused,
    /// naming both places, while an older atomic's credentials are still at
    /// their old place and the new one holds none — also after the denial
    /// has made the new directory, empty, as every run with a Codex or exec
    /// atom does before a gate reads the credential — rather than opened
    /// empty, where the credential would read as missing.
    #[test]
    fn the_file_store_is_refused_while_an_older_atomics_credentials_are_stranded() {
        let scratch = crate::state::TestScratch::new("stranded-store");
        for dir in [
            "home",
            "legacy-config/atomic/credentials",
            "project",
            "state",
        ] {
            std::fs::create_dir_all(scratch.join(dir)).expect("a directory of the case");
        }
        let legacy = scratch.join("legacy-config/atomic/credentials");
        std::fs::write(legacy.join("6f70"), "secret").expect("an older atomic's credential");
        let _process = crate::state::as_process_with_dirs(
            &scratch,
            &[("HOME", "/home"), (STORE_VAR, "file")],
            None,
            Some("legacy-config"),
            Some("data"),
        );
        let _guard =
            crate::codex::StateGuard::deny_state_dir(scratch.join("state")).expect("the guard");
        let new = scratch.join("home/.config/atomic/credentials");
        assert_eq!(
            std::fs::read_dir(&new).map(|entries| entries.count()).ok(),
            Some(0),
            "the guard did not make the new credential directory, empty"
        );
        let refused = configured_store(&scratch.join("project")).map(|_| ());
        let Err(CredentialError::File(why)) = refused else {
            panic!("the file store was opened beside stranded credentials: {refused:?}");
        };
        assert!(
            why.starts_with(&format!(
                "credentials are kept in {} now, which holds none of them, and an older atomic \
                 kept them in {}, which still holds them: move everything in",
                new.display(),
                legacy.display()
            )),
            "{why}"
        );
        std::fs::rename(legacy.join("6f70"), new.join("6f70")).expect("moved");
        let moved_store = configured_store(&scratch.join("project"));
        if cfg!(target_os = "linux") {
            let store = moved_store.expect("the moved store");
            assert_eq!(
                store.load("op").ok().flatten().as_deref(),
                Some("secret"),
                "the moved credential is not the one read"
            );
        } else {
            assert!(matches!(
                moved_store,
                Err(CredentialError::Unprotected(platform)) if platform == std::env::consts::OS
            ));
        }
    }
}
