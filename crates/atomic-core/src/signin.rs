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
//! Codex atoms' sandboxed commands never see it: the credential directory is
//! denied to their sandbox, their commands cannot use the kernel keyrings or
//! reach the Secret Service, and Codex sends the token to the model backend
//! from atomic's own process. Exec atoms' commands run under the same sandbox
//! and are kept from it alike. A signed-in thread has none of Codex's ChatGPT
//! connectors, plugins or hosted tools (see `crate::codex`), so the token
//! reaches the model and nothing else. Lite atoms call a model API directly
//! and are not covered: they need an endpoint and, if it asks for one, a
//! key.
//!
//! A model endpoint someone configured comes first; with none, the sign-in
//! comes before what atomic detects (`OPENAI_API_KEY`, a local Ollama).
//! The sign-in is personal: a shared store or `atomic serve` never uses it
//! (the caller refuses it there). Codex's managed login restrictions
//! (`forced_login_method`, `forced_chatgpt_workspace_id`, and the managed
//! `allowed_login_methods` and `allowed_chatgpt_workspaces`) hold for it, on
//! login and when a thread starts.

use crate::credential::{CredentialError, STORE_VAR};
use codex_core_api::{AuthCredentialsStoreMode, AuthKeyringBackendKind};
use codex_protocol::config_types::ForcedLoginMethod;
use std::path::{Path, PathBuf};

/// Where a sign-in is kept, and how Codex atoms use it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignIn {
    auth_home: PathBuf,
    /// The directory as it was reached, before any link was followed.
    reached: PathBuf,
    /// Whether this invocation made the directory, and so may remove it.
    made: bool,
    store: AuthCredentialsStoreMode,
    backend: Option<String>,
    issuer: Option<String>,
}

/// Who is signed in, without any token. Its `Debug` masks the email.
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    /// How: `chatgpt`, or `api key` for a key saved by a login.
    pub mode: String,
    /// The account's email, if the sign-in says. Never shown as it is:
    /// [`mask_email`] it first.
    pub email: Option<String>,
    /// The ChatGPT plan, if the sign-in says.
    pub plan: Option<String>,
    /// Where it is kept: the OS keyring or the credential file store.
    pub store: String,
    /// The ChatGPT workspace it belongs to, for the managed restrictions.
    workspace: Option<String>,
}

impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("mode", &self.mode)
            .field("email", &self.email.as_deref().map(mask_email))
            .field("plan", &self.plan)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// `email` with all of its local part but the first character hidden, as a
/// new string built from those parts: `d***@example.com`. Masking a masked
/// email gives it back.
pub fn mask_email(email: &str) -> String {
    let Some((local, domain)) = email.rsplit_once('@') else {
        return "***".to_owned();
    };
    let mut shown = String::with_capacity(domain.len() + 5);
    if let Some(first) = local.chars().next() {
        shown.push(first);
    }
    shown.push_str("***@");
    shown.push_str(domain);
    shown
}

/// What a logout forgot.
#[derive(Debug)]
pub struct Forgotten {
    /// Whether there was a copy in the credential file store.
    pub file: bool,
    /// Whether there was a copy in the OS keyring, or why it could not be
    /// checked.
    pub keyring: Result<bool, String>,
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
    /// Codex's managed login restrictions on this machine refuse it.
    #[error("{0}")]
    Restricted(String),
}

const KEYRING: AuthKeyringBackendKind = AuthKeyringBackendKind::Direct;

/// Refused when `dir` is inside a checkout holding `project`, its own or
/// one around it (a submodule's superproject, the checkout a worktree sits
/// in): a file there is one a repository could have supplied. Paths are
/// compared as they really are, a directory not made yet by its nearest
/// existing parent.
fn outside_the_checkout(dir: &Path, project: Option<&Path>) -> Result<(), SignInError> {
    let Some(project) = project else {
        return Ok(());
    };
    let (here, project) = (
        crate::state::as_it_really_is(dir),
        crate::state::as_it_really_is(project),
    );
    let checkouts = crate::state::checkouts_holding(&project)
        .map_err(|e| SignInError::Store(CredentialError::File(e)))?;
    match checkouts
        .into_iter()
        .find(|checkout| here.starts_with(checkout))
    {
        Some(checkout) => Err(SignInError::Store(CredentialError::File(format!(
            "{} is inside the checkout holding the project, at {}, so the repository could \
             have supplied it; point ATOMIC_HOME outside it",
            dir.display(),
            checkout.display()
        )))),
        None => Ok(()),
    }
}

/// The first link on the way to `dir` that leads nowhere, if any.
fn dangling_link(dir: &Path) -> Option<&Path> {
    dir.ancestors().find(|a| a.is_symlink() && !a.exists())
}

/// Where the sign-in store `ATOMIC_CREDENTIAL_STORE` chooses keeps its
/// directory, as written, whether or not [`SignIn::located`] accepts it
/// there: a refused location that holds one may still hold a sign-in.
pub fn location() -> Option<PathBuf> {
    crate::state::credentials_dir()
        .ok()
        .map(|dir| dir.join("codex-auth"))
}

/// The model endpoint a sign-in's Codex atoms use: ChatGPT's Codex backend.
pub const CODEX_BACKEND: &str = "https://chatgpt.com/backend-api/codex";

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

impl SignIn {
    /// Where the sign-in store `ATOMIC_CREDENTIAL_STORE` chooses is, without
    /// creating anything: outside every state directory and, when there is
    /// one, outside and not above `project`, as the operator credential's
    /// file store is, and outside the checkout holding it, which the
    /// project's own files could reach. Outside any project (as from the
    /// home directory, above the store) there is no project to keep it apart
    /// from.
    pub fn located(project: Option<&Path>) -> Result<Self, SignInError> {
        protected()?;
        let dir = crate::state::credentials_dir_in_use()
            .map_err(CredentialError::File)?
            .join("codex-auth");
        // A link on the way that leads nowhere: making the directory would
        // fail, or make what it names, somewhere not checked here.
        if let Some(link) = dangling_link(&dir) {
            return Err(SignInError::Store(CredentialError::File(format!(
                "{} is a link to {}, which does not exist; make it, or point ATOMIC_HOME \
                 elsewhere",
                link.display(),
                std::fs::read_link(link)
                    .map(|t| t.display().to_string())
                    .unwrap_or_else(|_| "somewhere unreadable".into())
            ))));
        }
        crate::credential::placed_apart(&dir, project)?;
        outside_the_checkout(&dir, project)?;
        // One spelling, so the keyring entry Codex keys by it is the same
        // however the directory is reached.
        let located = dir.clone();
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        let store = match std::env::var(STORE_VAR).ok().filter(|v| !v.is_empty()) {
            None => AuthCredentialsStoreMode::Keyring,
            Some(v) if v == "keyring" => AuthCredentialsStoreMode::Keyring,
            Some(v) if v == "file" => AuthCredentialsStoreMode::File,
            Some(other) => return Err(CredentialError::UnknownStore(other).into()),
        };
        Ok(Self {
            reached: located,
            auth_home: dir,
            made: false,
            store,
            backend: None,
            issuer: None,
        })
    }

    /// The sign-in store `ATOMIC_CREDENTIAL_STORE` chooses, as
    /// [`SignIn::located`] finds it, its directory created readable by its
    /// owner only. Refused off Linux.
    pub fn configured(project: Option<&Path>) -> Result<Self, SignInError> {
        Self::made_at(Self::located(project)?, project)
    }

    /// [`SignIn::configured`] once `located` is found: its directory made,
    /// and checked again where it was made.
    fn made_at(located: Self, project: Option<&Path>) -> Result<Self, SignInError> {
        let file = |e: String| SignInError::Store(CredentialError::File(e));
        // Made here, or found: decided by the one call that makes it, so two
        // invocations never both take it for theirs.
        // Its parents as making it whole would make them: readable by their
        // owner only, those that exist left as they are.
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        if let Some(parent) = located.auth_home.parent() {
            builder
                .recursive(true)
                .create(parent)
                .map_err(|e| file(format!("{}: {e}", parent.display())))?;
            builder.recursive(false);
        }
        // Found, it is checked where it is and never made again here: one
        // another invocation removed meanwhile is made by this one, which
        // then knows it made it.
        let mut existed = None;
        for _ in 0..3 {
            match builder.create(&located.auth_home) {
                Ok(()) => existed = Some(false),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    #[cfg(test)]
                    tests::between_create_and_check(&located.auth_home);
                    if crate::state::existing_private_dir(&located.auth_home)
                        .map_err(|e| file(e.to_string()))?
                    {
                        existed = Some(true);
                    }
                }
                Err(e) => return Err(file(format!("{}: {e}", located.auth_home.display()))),
            }
            if existed.is_some() {
                break;
            }
        }
        let Some(existed) = existed else {
            return Err(file(format!(
                "{} kept appearing and going while it was made; try again",
                located.auth_home.display()
            )));
        };
        let auth_home =
            std::fs::canonicalize(&located.auth_home).unwrap_or_else(|_| located.auth_home.clone());
        // Where it was made, as it really is: a link on the way, such as a
        // linked home, could have put it somewhere the checks above did not
        // see. Refused there, it is removed again.
        let mut placed =
            crate::credential::placed_apart(&auth_home, project).map_err(SignInError::from);
        if placed.is_ok() {
            placed = outside_the_checkout(&auth_home, project);
        }
        if let Err(e) = placed {
            if !existed {
                let _ = std::fs::remove_dir(&auth_home);
            }
            return Err(e);
        }
        Ok(Self {
            auth_home,
            made: !existed,
            ..located
        })
    }

    /// Remove the sign-in's directory, so no store is read (see
    /// [`SignIn::account`]), when it holds nothing but Codex's empty lock
    /// files and no one holds any of them. Only for a directory known to
    /// hold no sign-in: one this invocation made, or one a logout forgot
    /// in both stores. With the keyring store a live sign-in's directory
    /// holds only lock files too. Not when the directory is itself a link:
    /// that would remove what it leads to and leave the link behind. `after`
    /// says what this invocation did to the store first, which decides what
    /// a sign-in found in it means.
    fn tidy(&self, after: Tidy) {
        if self.reached.is_symlink() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&self.auth_home) else {
            return;
        };
        let mut held = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let lock = entry.file_name().to_string_lossy().ends_with(".lock")
                && std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() == 0);
            // Codex takes these with `File::try_lock`: one it can take now
            // is one no process holds, and it is held until removed.
            let taken = lock
                && std::fs::OpenOptions::new()
                    .read(true)
                    .open(&path)
                    .is_ok_and(|file| {
                        let free = file.try_lock().is_ok();
                        if free {
                            held.push((path.clone(), file));
                        }
                        free
                    });
            if !taken {
                return;
            }
        }
        // A sign-in another login kept meanwhile is seen here, and stays.
        // That this read comes after such a save rests on Codex's own
        // storage at its pinned revision: its save blocks on auth.lock,
        // which is held here, and makes the directory again
        // (create_dir_all in CommittedStorage::lock) if it is gone by then.
        // A Codex update could change either.
        let locked = !held.is_empty();
        let keep = match after {
            // The store could not be made to forget an entry that outlived
            // the directory: whatever it reads is no sign-in of this
            // invocation's, and the directory goes, failing closed. A
            // sign-in another login saved in that moment stays in the store,
            // but with no directory reads as no sign-in until it logs in
            // again.
            Tidy::Login(Forgot::Failed) => false,
            Tidy::Login(forgot) => match self.stored() {
                // Forgotten before the flow began (see `log_in`), so a sign-in
                // there now was saved since. Not forgotten, it is one only if
                // a save, which leaves its locks, put it there; locks a
                // concurrent logout or a refresh left keep it too, failing
                // towards keeping a sign-in.
                Ok(Some(_)) => forgot.keeps_a_sign_in(locked),
                Ok(None) => false,
                // Unknown: a directory with a lock file in it may have had a
                // save, which Codex makes only with both locks, so it stays.
                Err(_) => locked,
            },
            Tidy::Logout { made, deleted } => match self.stored() {
                // Trusted once the logout's own delete went through, or when
                // the directory was there before the logout. One it made,
                // whose delete did not go through, goes: that disowns a
                // concurrent login's save, even one that has finished, which
                // stays in the store and reads as no sign-in until it logs in
                // again.
                Ok(Some(_)) => !matches!((made, deleted), (true, false)),
                Ok(None) => false,
                // A logout that made the directory and cannot read the store
                // removes it whatever locks its own forgetting left: it may
                // disown a sign-in another login saved in that moment, which
                // stays in the store, deletes nothing, and fails closed.
                Err(_) => locked && !made,
            },
        };
        if keep {
            return;
        }
        for (path, _file) in &held {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&self.auth_home);
    }

    /// A sign-in kept in files under `dir`, for tests.
    #[cfg(any(test, feature = "fake-model"))]
    pub fn in_files(dir: &Path) -> Self {
        Self {
            auth_home: dir.to_owned(),
            reached: dir.to_owned(),
            made: false,
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

    /// Who is signed in, if anyone. `atomic model login` creates the
    /// sign-in's directory, so with none no one is, and no store is read:
    /// a keyring entry that outlived its directory is not a sign-in.
    pub fn account(&self) -> anyhow::Result<Option<Account>> {
        protected()?;
        if !self.auth_home.is_dir() {
            return Ok(None);
        }
        let auth = self.stored()?;
        let store = self.store_name().to_owned();
        Ok(auth.and_then(|a| {
            if let Some(tokens) = &a.tokens {
                return Some(Account {
                    mode: "chatgpt".into(),
                    email: tokens.id_token.email.clone(),
                    plan: tokens.id_token.get_chatgpt_plan_type(),
                    store: store.clone(),
                    workspace: tokens.id_token.chatgpt_account_id.clone(),
                });
            }
            a.openai_api_key.as_ref().map(|_| Account {
                mode: "api key".into(),
                email: None,
                plan: None,
                store: store.clone(),
                workspace: None,
            })
        }))
    }

    /// The sign-in its store keeps, if any.
    fn stored(&self) -> std::io::Result<Option<codex_login::AuthDotJson>> {
        #[cfg(test)]
        if self.store != AuthCredentialsStoreMode::File
            && let Some(kept) = tests::keyring_holds(&self.auth_home)
        {
            return kept;
        }
        codex_login::load_auth_dot_json(&self.auth_home, self.store, KEYRING)
    }

    /// Refused unless `policy`, Codex's login restrictions as a thread's
    /// configuration resolves them, allows the kept sign-in: its login
    /// method, and for ChatGPT its workspace.
    pub(crate) fn allowed_by(&self, policy: &codex_login::AuthConfig) -> anyhow::Result<()> {
        let Some(account) = self.account()? else {
            return Ok(());
        };
        let method = if account.mode == "chatgpt" {
            ForcedLoginMethod::Chatgpt
        } else {
            ForcedLoginMethod::Api
        };
        if !policy.is_login_method_allowed(method) {
            return Err(SignInError::Restricted(format!(
                "Codex's login restrictions on this machine do not allow a {} sign-in; configure \
                 a model endpoint, or sign in as they allow",
                account.mode
            ))
            .into());
        }
        if method == ForcedLoginMethod::Chatgpt
            && let Some(workspaces) = policy.effective_chatgpt_workspaces()
            && !account
                .workspace
                .as_ref()
                .is_some_and(|w| workspaces.contains(w))
        {
            return Err(SignInError::Restricted(format!(
                "Codex's login restrictions on this machine allow ChatGPT workspace(s) {} only, \
                 and the sign-in belongs to another; sign in again with `atomic model login`",
                workspaces.join(", ")
            ))
            .into());
        }
        Ok(())
    }

    /// What `atomic config --explain` shows for model access from this
    /// sign-in, if anyone is signed in: how and where, never a token or an
    /// email. Model access from a configured endpoint takes precedence (see
    /// the engine's endpoint selection).
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
        let mut forgot = Forgot::NotReached;
        let outcome = self.log_in(device, &mut forgot).await;
        // Only a directory this login made: one that was here may hold a
        // sign-in the keyring keeps, which a failed login leaves as it was.
        if outcome.is_err() && self.made {
            self.tidy(Tidy::Login(forgot));
        }
        outcome
    }

    /// Forget what the store holds for this directory, without revoking
    /// it: for a directory this invocation made, whatever the store holds
    /// outlived the directory it was kept for, and is no sign-in.
    fn forget(&self) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(forgot) = tests::forget(&self.auth_home) {
            return forgot;
        }
        codex_login::logout(&self.auth_home, self.store, KEYRING).map(|_| ())
    }

    /// Whether the OS keyring takes, gives back and forgets a value; the
    /// test keyring answers for itself.
    fn keyring_usable(&self) -> Result<(), CredentialError> {
        #[cfg(test)]
        if let Some(probed) = tests::probe(&self.auth_home) {
            return probed;
        }
        keyring_usable()
    }

    async fn log_in(&self, device: bool, forgot: &mut Forgot) -> anyhow::Result<()> {
        protected()?;
        // Codex's login restrictions, as its own login applies them: the
        // method first, then the workspaces the issuer must sign in to.
        let policy = crate::codex::auth_policy(&self.auth_home).await?;
        if !policy.is_login_method_allowed(ForcedLoginMethod::Chatgpt) {
            return Err(SignInError::Restricted(
                "Codex's login restrictions on this machine do not allow a ChatGPT sign-in; \
                 configure a model endpoint and API key instead"
                    .into(),
            )
            .into());
        }
        if self.store != AuthCredentialsStoreMode::File {
            self.keyring_usable()?;
        }
        // An entry that outlived the directory it was kept for is no
        // sign-in: forgotten before the flow starts, so neither a login
        // stopped part way nor a run that refreshes it meanwhile brings it
        // back. A logout still revokes one, since it forgets, never this.
        if self.made {
            if let Err(e) = self.forget() {
                *forgot = Forgot::Failed;
                return Err(e.into());
            }
            *forgot = Forgot::Done;
        }
        let mut options = codex_login::ServerOptions::new(
            self.auth_home.clone(),
            codex_login::CLIENT_ID.to_owned(),
            policy.effective_chatgpt_workspaces(),
            self.store,
            KEYRING,
            policy.auth_route_config.clone(),
        );
        // For tests of the `atomic` executable: an issuer of their own, and
        // a port of the system's choosing, never Codex's fixed one, whose
        // listener a busy port would make Codex cancel.
        #[cfg(feature = "fake-model")]
        let test_issuer = std::env::var("ATOMIC_TEST_LOGIN_ISSUER").ok();
        #[cfg(not(feature = "fake-model"))]
        let test_issuer: Option<String> = None;
        if let Some(issuer) = self.issuer.as_ref().or(test_issuer.as_ref()) {
            options.issuer = issuer.clone();
            options.open_browser = false;
            options.port = 0;
        }
        if device {
            codex_login::run_device_code_login(options).await?;
        } else {
            let server = codex_login::run_login_server(options)?;
            // The address is Codex's own, holding no secret: a random state
            // or challenge can look like a token, so it is written as it is
            // (see `redact::log_sign_in_address`).
            crate::redact::log_sign_in_address(&server.auth_url);
            #[cfg(any(test, feature = "fake-model"))]
            if let Some(hook) = LOGIN_STARTED.get() {
                hook(&server.auth_url, server.actual_port);
            }
            server.block_until_done().await?;
        }
        Ok(())
    }

    /// Revoke the sign-in's tokens where the issuer allows, and forget every
    /// copy of it, in the credential file store and in the OS keyring,
    /// whichever store `ATOMIC_CREDENTIAL_STORE` now chooses and even when
    /// revoking fails. Says what it found in each.
    pub async fn logout(&self) -> anyhow::Result<Forgotten> {
        let outcome = self.log_out().await;
        // A logout that made the directory and stopped short of the stores,
        // or of the keyring, leaves no directory that would make an entry
        // still in the keyring read as a sign-in.
        let stopped_short = outcome.is_err() && self.made;
        if stopped_short {
            self.tidy(Tidy::Logout {
                made: true,
                deleted: false,
            });
        }
        outcome
    }

    async fn log_out(&self) -> anyhow::Result<Forgotten> {
        protected()?;
        #[cfg(test)]
        if tests::logout_fails(&self.auth_home) {
            anyhow::bail!("the test's logout stops before the stores");
        }
        let route = crate::codex::auth_policy(&self.auth_home)
            .await?
            .auth_route_config;
        // The file first: forgetting the keyring's copy removes a file copy
        // too, which would then be counted as the keyring's.
        let file = codex_login::logout_with_revoke(
            &self.auth_home,
            AuthCredentialsStoreMode::File,
            KEYRING,
            &route,
        )
        .await?;
        let keyring = codex_login::logout_with_revoke(
            &self.auth_home,
            AuthCredentialsStoreMode::Keyring,
            KEYRING,
            &route,
        )
        .await
        .map_err(|e| e.to_string());
        // A keyring that could not be checked may still hold the sign-in,
        // and its directory is what says so.
        let nothing_kept = self.made || keyring.is_ok();
        if nothing_kept {
            self.tidy(Tidy::Logout {
                made: self.made,
                deleted: keyring.is_ok(),
            });
        }
        Ok(Forgotten { file, keyring })
    }

    /// The keyring backend Codex's storage uses for it.
    pub(crate) fn keyring_kind(&self) -> AuthKeyringBackendKind {
        KEYRING
    }
}

/// How far a login got in forgetting what the store held for a directory it
/// made. It holds no token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Forgot {
    /// It returned before it tried.
    NotReached,
    /// It tried, and the store did not forget.
    Failed,
    /// The store forgot.
    Done,
}

impl Forgot {
    /// Whether a sign-in found in the store after this is one to keep, the
    /// directory holding lock files or not.
    fn keeps_a_sign_in(self, locked: bool) -> bool {
        match self {
            Forgot::Done => true,
            Forgot::NotReached => locked,
            Forgot::Failed => false,
        }
    }
}

/// What a tidy follows.
#[derive(Clone, Copy, Debug)]
enum Tidy {
    /// A login that failed, and how far its forgetting got.
    Login(Forgot),
    /// A logout: whether it made the directory, and whether its delete in
    /// the keyring went through.
    Logout { made: bool, deleted: bool },
}

/// For tests: told the login URL and port once the login server listens.
#[cfg(any(test, feature = "fake-model"))]
#[allow(clippy::type_complexity)]
pub static LOGIN_STARTED: std::sync::OnceLock<Box<dyn Fn(&str, u16) + Send + Sync>> =
    std::sync::OnceLock::new();

/// Whether the OS keyring takes, gives back and forgets a value, so a
/// missing or broken one is reported before a browser flow that could not
/// be kept.
fn keyring_usable() -> Result<(), CredentialError> {
    use crate::credential::SecretStore;
    let probe = format!("probe:{}", std::process::id());
    let store = crate::credential::OsKeyring;
    store.save(&probe, "probe")?;
    let read = store.load(&probe);
    let deleted = store.delete(&probe);
    match (read, deleted) {
        (Ok(Some(v)), Ok(_)) if v == "probe" => Ok(()),
        (_, Err(e)) => Err(CredentialError::Keyring(format!(
            "it kept a test value ({probe}) and would not forget it: {e}"
        ))),
        (Err(e), _) => Err(e),
        (Ok(_), _) => Err(CredentialError::Keyring(
            "it did not give back a test value it took".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sign-ins are kept on Linux only: off it, these are refused before
    // they look.
    #[cfg(target_os = "linux")]
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

    #[test]
    fn an_email_is_shown_masked() {
        assert_eq!(mask_email("dev@example.invalid"), "d***@example.invalid");
        assert_eq!(mask_email("a.b@c@example.com"), "a***@example.com");
        assert_eq!(mask_email("@example.com"), "***@example.com");
        assert_eq!(mask_email("no-at-sign"), "***");
        assert_eq!(mask_email("d***@example.invalid"), "d***@example.invalid");
    }

    /// A test issuer on loopback: it hands out a sign-in for `account` at
    /// its token endpoint and records the request line of everything it is
    /// asked.
    async fn issuer(account: &'static str) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
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
                let token = line.starts_with("POST /oauth/token");
                let body = if token {
                    serde_json::json!({
                        "id_token": crate::testing::fake_id_token("dev@example.invalid", account),
                        "access_token": "issued-access-token",
                        "refresh_token": "issued-refresh-token",
                    })
                    .to_string()
                } else {
                    "{}".to_owned()
                };
                let status = if token { "200 OK" } else { "404 Not Found" };
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (port, seen)
    }

    fn auth_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atomic-login-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    /// Log in at the test issuer on `port`, with this test as the browser:
    /// it reads the state from the authorize URL and comes back to the
    /// login server's callback with a code.
    /// The addresses browser logins were started at, as Codex built them.
    static LOGIN_URLS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    async fn browser_login(sign_in: &SignIn) -> anyhow::Result<()> {
        static STARTED: std::sync::Mutex<Option<std::sync::mpsc::Sender<(String, u16)>>> =
            std::sync::Mutex::new(None);
        let (tx, rx) = std::sync::mpsc::channel();
        *STARTED.lock().expect("hook") = Some(tx);
        let _ = LOGIN_STARTED.set(Box::new(|url: &str, port: u16| {
            LOGIN_URLS.lock().expect("urls").push(url.to_owned());
            if let Some(tx) = STARTED.lock().expect("hook").as_ref() {
                let _ = tx.send((url.to_owned(), port));
            }
        }));
        let browser = std::thread::spawn(move || {
            let Ok((url, port)) = rx.recv_timeout(std::time::Duration::from_secs(60)) else {
                return None;
            };
            let state = url
                .split(['?', '&'])
                .find_map(|p| p.strip_prefix("state="))
                .expect("a state")
                .to_owned();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            Some(runtime.block_on(async {
                reqwest::Client::new()
                    .get(format!(
                        "http://127.0.0.1:{port}/auth/callback?code=the-code&state={state}"
                    ))
                    .send()
                    .await
                    .map(|r| r.status())
            }))
        });
        let outcome = sign_in.login(false).await;
        *STARTED.lock().expect("hook") = None;
        let _ = browser.join();
        outcome
    }

    /// `atomic model login`'s browser flow, end to end against a test
    /// issuer: the sign-in it keeps is the issuer's, and no request leaves
    /// the machine. Then Codex's login restrictions: a workspace the issuer
    /// does not sign in to keeps nothing, and a machine that allows only
    /// API keys refuses before the issuer is asked.
    #[cfg(target_os = "linux")]
    #[test]
    fn login_keeps_the_sign_in_the_issuer_gives_as_codex_restrictions_allow() {
        crate::testing::on_codex_runtime(async {
            let (port, seen) = issuer("account-1").await;
            let at = format!("http://127.0.0.1:{port}");
            let dir = auth_dir("allowed");
            let sign_in = SignIn::in_files(&dir).with_issuer(&at);
            browser_login(&sign_in).await.expect("logged in");
            // The address the person is shown, as Codex builds it, comes
            // through whole.
            let url = LOGIN_URLS
                .lock()
                .expect("urls")
                .last()
                .cloned()
                .expect("a login address");
            assert!(url.contains("api.connectors.read"), "{url}");
            assert!(url.contains("code_challenge="), "{url}");
            assert!(
                crate::redact::sign_in_line(&url).contains(&url),
                "the address to sign in at is not written whole: {url}"
            );
            let account = sign_in.account().expect("account").expect("signed in");
            assert_eq!(account.mode, "chatgpt");
            assert_eq!(account.email.as_deref(), Some("dev@example.invalid"));
            let kept = std::fs::read_to_string(dir.join("auth.json")).expect("kept");
            assert!(kept.contains("issued-access-token"));
            assert!(
                seen.lock()
                    .expect("log")
                    .iter()
                    .any(|l| l.starts_with("POST /oauth/token")),
                "the code was exchanged at the test issuer: {seen:?}"
            );

            // Restricted to another workspace: the issuer's sign-in is not
            // kept.
            let dir = auth_dir("workspace");
            std::fs::write(
                dir.join("config.toml"),
                "forced_chatgpt_workspace_id = \"another-workspace\"\n",
            )
            .expect("config");
            let sign_in = SignIn::in_files(&dir).with_issuer(&at);
            let refused = browser_login(&sign_in).await;
            assert!(refused.is_err(), "{refused:?}");
            assert!(
                !dir.join("auth.json").exists(),
                "a refused sign-in was kept"
            );

            // API keys only: refused before any browser or issuer.
            let dir = auth_dir("method");
            std::fs::write(dir.join("config.toml"), "forced_login_method = \"api\"\n")
                .expect("config");
            seen.lock().expect("log").clear();
            let refused = browser_login(&SignIn::in_files(&dir).with_issuer(&at))
                .await
                .expect_err("refused")
                .to_string();
            assert!(
                refused.contains("do not allow a ChatGPT sign-in"),
                "{refused}"
            );
            assert!(seen.lock().expect("log").is_empty(), "{seen:?}");
            for tag in ["allowed", "workspace", "method"] {
                let _ = std::fs::remove_dir_all(auth_dir(tag));
            }
        });
    }

    /// A keyring for tests: the sign-ins it keeps, by the directory Codex
    /// keys them by. Read in place of the OS keyring by tests only.
    /// A directory listed with `None` holds nothing; one not listed falls
    /// through to the OS keyring.
    static KEYRING: std::sync::Mutex<Vec<(PathBuf, Option<String>)>> =
        std::sync::Mutex::new(Vec::new());

    pub(super) fn keyring_holds(
        dir: &Path,
    ) -> Option<std::io::Result<Option<codex_login::AuthDotJson>>> {
        let kept = KEYRING.lock().expect("keyring");
        kept.iter()
            .rev()
            .find(|(d, _)| d == dir)
            .map(|(_, json)| match json.as_deref() {
                Some(UNREACHABLE) => Err(std::io::Error::other("the keyring cannot be reached")),
                Some(j) => Ok(Some(serde_json::from_str(j).expect("a sign-in"))),
                None => Ok(None),
            })
    }

    /// Kept for a directory whose keyring cannot be reached.
    const UNREACHABLE: &str = "unreachable";

    /// Directories removed once, between a create that found them and the
    /// check of what was found, as another invocation's tidy might.
    static REMOVED_BETWEEN: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

    pub(super) fn between_create_and_check(dir: &Path) {
        let mut listed = REMOVED_BETWEEN.lock().expect("removed");
        if let Some(at) = listed.iter().position(|d| d == dir) {
            listed.remove(at);
            let _ = std::fs::remove_dir(dir);
        }
    }

    /// A directory another invocation removes between this one's create,
    /// which found it, and the check of it, is made by this one, which
    /// knows it made it.
    #[test]
    fn a_directory_removed_while_it_is_found_is_made_and_known_to_be() {
        let dir = auth_dir("removed-between");
        REMOVED_BETWEEN.lock().expect("removed").push(dir.clone());
        let sign_in = SignIn::made_at(SignIn::in_files(&dir), None).expect("made");
        assert!(sign_in.made, "taken for found");
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Directories whose logout stops before it reaches the stores.
    static LOGOUT_FAILS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

    pub(super) fn logout_fails(dir: &Path) -> bool {
        LOGOUT_FAILS
            .lock()
            .expect("logout")
            .iter()
            .any(|d| d == dir)
    }

    /// Directories whose keyring the probe finds unusable.
    static PROBE_FAILS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

    /// The test keyring's probe for `dir`, if it is listed there.
    pub(super) fn probe(dir: &Path) -> Option<Result<(), CredentialError>> {
        let _listed = keyring_holds(dir)?;
        if PROBE_FAILS.lock().expect("probe").iter().any(|d| d == dir) {
            return Some(Err(CredentialError::Keyring(
                "the test keyring refuses".into(),
            )));
        }
        Some(Ok(()))
    }

    /// What another login saves for a directory right after this one's
    /// forgets what the store held, as if it saved while this one waited.
    static SAVED_MEANWHILE: std::sync::Mutex<Vec<(PathBuf, String)>> =
        std::sync::Mutex::new(Vec::new());

    /// Forget what the test keyring holds for `dir`, if it is listed there.
    pub(super) fn forget(dir: &Path) -> Option<std::io::Result<()>> {
        let listed = keyring_holds(dir)?;
        // As Codex's logout does, the locks first, then the keyring.
        for lock in ["auth.lock", "auth.json.lock"] {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join(lock));
        }
        if listed.is_err() {
            return Some(Err(std::io::Error::other("the keyring cannot be reached")));
        }
        let meanwhile = SAVED_MEANWHILE
            .lock()
            .expect("saved")
            .iter()
            .rev()
            .find(|(d, _)| d == dir)
            .map(|(_, json)| json.clone());
        KEYRING
            .lock()
            .expect("keyring")
            .push((dir.to_owned(), meanwhile));
        Some(Ok(()))
    }

    /// Keep `json` (or nothing) in the test keyring for `dir`.
    fn keep(dir: &Path, json: Option<String>) {
        KEYRING
            .lock()
            .expect("keyring")
            .push((std::fs::canonicalize(dir).expect("dir"), json));
    }

    /// A ChatGPT sign-in for `account`, as the keyring keeps it.
    fn signed_in(account: &str) -> String {
        let id_token = crate::testing::fake_id_token("dev@example.invalid", account);
        serde_json::json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": {
            "id_token": id_token, "access_token": "", "refresh_token": "",
            "account_id": account}})
        .to_string()
    }

    /// With the keyring store, a live sign-in's directory holds only
    /// Codex's lock files. A login that fails there keeps the directory,
    /// and the sign-in with it. One the login made itself is removed again,
    /// but never while a lock in it is held.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_login_keeps_a_directory_it_did_not_make() {
        crate::testing::on_codex_runtime(async {
            let dir = auth_dir("keyring-kept");
            for lock in ["auth.lock", "auth.json.lock"] {
                std::fs::write(dir.join(lock), "").expect("lock");
            }
            keep(&dir, Some(signed_in("account-1")));
            // The issuer is the discard port on loopback: the login fails
            // there, if the keyring check before it does not.
            // Made as `SignIn::configured` makes it, which finds whether
            // the directory was there.
            let keyring = |dir: &Path| {
                let located = SignIn {
                    store: AuthCredentialsStoreMode::Keyring,
                    ..SignIn::in_files(dir)
                };
                SignIn::made_at(located, None)
                    .expect("made")
                    .with_issuer("http://127.0.0.1:9")
            };
            let sign_in = keyring(&dir);
            assert!(sign_in.account().expect("account").is_some());
            let failed = sign_in.login(true).await;
            assert!(failed.is_err(), "{failed:?}");
            assert!(
                dir.join("auth.lock").exists(),
                "a failed login removed a sign-in"
            );
            let account = sign_in
                .account()
                .expect("account")
                .expect("still signed in");
            assert_eq!(account.mode, "chatgpt");
            // Even with nothing in the store, a directory the login did not
            // make is not its to remove.
            let found = auth_dir("keyring-found");
            std::fs::write(found.join("auth.lock"), "").expect("lock");
            keep(&found, None);
            assert!(keyring(&found).login(true).await.is_err());
            assert!(
                found.join("auth.lock").exists(),
                "a failed login removed a directory it did not make"
            );
            let _ = std::fs::remove_dir_all(&found);

            let made = auth_dir("keyring-made");
            std::fs::remove_dir(&made).expect("not there yet");
            let sign_in = keyring(&made);
            keep(&made, None);
            std::fs::write(made.join("auth.lock"), "").expect("lock");
            let held = std::fs::File::open(made.join("auth.lock")).expect("open");
            held.lock().expect("held");
            assert!(sign_in.login(true).await.is_err());
            assert!(made.join("auth.lock").exists(), "a held lock was removed");
            drop(held);
            assert!(sign_in.login(true).await.is_err());
            assert!(!made.exists(), "a failed login left the directory it made");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// The directory is checked again where it was made: a link that
    /// appeared after the sign-in was located, leading into the checkout
    /// holding the project, is refused there, and nothing is left inside.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_sign_in_is_checked_again_where_its_directory_is_made() {
        let dir = auth_dir("made-at");
        let checkout = dir.join("checkout");
        for sub in [".git", "project", "home"] {
            std::fs::create_dir_all(checkout.join(sub)).expect("checkout");
        }
        std::fs::create_dir_all(dir.join("outside")).expect("outside");
        let link = dir.join("outside/linked-home");
        std::os::unix::fs::symlink(checkout.join("home"), &link).expect("link");
        // Located before the link was there, so as written.
        let located = SignIn::in_files(&link.join("codex-auth"));
        let refused = SignIn::made_at(located, Some(&checkout.join("project")))
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains("inside the checkout holding the project"),
            "{refused}"
        );
        assert!(
            !checkout.join("home/codex-auth").exists(),
            "the sign-in's directory was left inside the checkout"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A linked worktree shares its main checkout's repository, so that
    /// checkout holds its project too.
    #[test]
    fn a_linked_worktrees_main_checkout_holds_its_project() {
        let dir = auth_dir("worktree");
        let main = dir.join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).expect("main");
        std::fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").expect("commondir");
        std::fs::create_dir_all(dir.join("wt/project")).expect("worktree");
        std::fs::write(
            dir.join("wt/.git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        )
        .expect(".git");
        let project = dir.join("wt/project");
        let refused = outside_the_checkout(&main.join("home/codex-auth"), Some(&project))
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains("inside the checkout holding the project"),
            "{refused}"
        );
        assert!(outside_the_checkout(&dir.join("elsewhere/codex-auth"), Some(&project)).is_ok());
        // A commondir git would not write refuses the sign-in wherever it
        // is kept, rather than counting as no checkout.
        std::fs::write(main.join(".git/worktrees/wt/commondir"), "x".repeat(5000))
            .expect("commondir");
        let refused = outside_the_checkout(&dir.join("elsewhere/codex-auth"), Some(&project))
            .expect_err("refused")
            .to_string();
        assert!(refused.contains("is larger than a link file"), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Another login kept a sign-in while this one waited: this one fails
    /// in a directory it made, and the sign-in the other kept stays.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_sign_in_kept_while_a_login_waited_outlives_its_failure() {
        crate::testing::on_codex_runtime(async {
            let dir = auth_dir("kept-meanwhile");
            std::fs::remove_dir(&dir).expect("not there yet");
            let located = SignIn {
                store: AuthCredentialsStoreMode::Keyring,
                ..SignIn::in_files(&dir)
            };
            let waiting = SignIn::made_at(located, None)
                .expect("made")
                .with_issuer("http://127.0.0.1:9");
            // An entry that outlived the directory, then the other login's
            // sign-in, kept once this one has forgotten that entry, and the
            // locks its save left behind.
            keep(&dir, Some(signed_in("account-old")));
            SAVED_MEANWHILE.lock().expect("saved").push((
                std::fs::canonicalize(&dir).expect("dir"),
                signed_in("account-1"),
            ));
            for lock in ["auth.lock", "auth.json.lock"] {
                std::fs::write(dir.join(lock), "").expect("lock");
            }
            assert!(waiting.login(true).await.is_err());
            assert!(
                dir.join("auth.lock").exists(),
                "the other login's sign-in was tidied away"
            );
            let account = waiting
                .account()
                .expect("account")
                .expect("still signed in");
            assert_eq!(account.mode, "chatgpt");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// A login stopped part way, as by Ctrl-C at the browser step, after
    /// it made the directory, leaves no entry that outlived the directory
    /// standing as a sign-in: it was forgotten before the flow began.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_login_stopped_part_way_does_not_revive_an_entry_that_outlived_its_directory() {
        crate::testing::on_codex_runtime(async {
            let dir = auth_dir("stopped");
            keep(&dir, Some(signed_in("account-old")));
            std::fs::remove_dir(&dir).expect("its directory removed");
            let located = SignIn {
                store: AuthCredentialsStoreMode::Keyring,
                ..SignIn::in_files(&dir)
            };
            let sign_in = SignIn::made_at(located, None)
                .expect("made")
                .with_issuer("http://127.0.0.1:9");
            // The browser flow waits for a browser that never comes back.
            let stopped =
                tokio::time::timeout(std::time::Duration::from_secs(2), sign_in.login(false)).await;
            assert!(stopped.is_err(), "the login finished: {stopped:?}");
            assert!(
                sign_in.account().expect("account").is_none(),
                "an outlived entry reads as a sign-in"
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// A keyring entry that outlived its directory is no sign-in, and a
    /// login that makes the directory and fails does not make it one.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_login_does_not_revive_an_entry_that_outlived_its_directory() {
        crate::testing::on_codex_runtime(async {
            let dir = auth_dir("outlived");
            keep(&dir, Some(signed_in("account-old")));
            std::fs::remove_dir(&dir).expect("its directory removed");
            let located = SignIn {
                store: AuthCredentialsStoreMode::Keyring,
                ..SignIn::in_files(&dir)
            };
            let sign_in = SignIn::made_at(located, None)
                .expect("made")
                .with_issuer("http://127.0.0.1:9");
            assert!(sign_in.login(true).await.is_err());
            assert!(
                !dir.exists(),
                "the failed login kept the outlived entry's directory"
            );
            assert!(
                sign_in.account().expect("account").is_none(),
                "an outlived entry reads as a sign-in"
            );
        });
    }

    /// A keyring that cannot be read leaves a directory with a lock file in
    /// it, which a save of another login's may have left, and removes one
    /// with none.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_unreadable_keyring_keeps_a_directory_a_save_may_have_used() {
        crate::testing::on_codex_runtime(async {
            // Forgotten, then unreadable: the locks the forgetting left
            // may as well be a save's, and the directory stays.
            // Unreachable when it would forget: whatever the store reads is
            // no sign-in, and the directory goes, its locks and all.
            for forgotten in [true, false] {
                let dir = auth_dir(&format!("unreadable-{forgotten}"));
                std::fs::remove_dir(&dir).expect("not there yet");
                let located = SignIn {
                    store: AuthCredentialsStoreMode::Keyring,
                    ..SignIn::in_files(&dir)
                };
                let sign_in = SignIn::made_at(located, None)
                    .expect("made")
                    .with_issuer("http://127.0.0.1:9");
                if forgotten {
                    keep(&dir, None);
                    SAVED_MEANWHILE.lock().expect("saved").push((
                        std::fs::canonicalize(&dir).expect("dir"),
                        UNREACHABLE.into(),
                    ));
                } else {
                    keep(&dir, Some(UNREACHABLE.into()));
                }
                assert!(sign_in.login(true).await.is_err());
                assert_eq!(dir.exists(), forgotten, "forgotten: {forgotten}");
                let _ = std::fs::remove_dir_all(&dir);
            }
        });
    }

    /// A login refused before it forgot what the store held, as when the
    /// keyring will not take a value, leaves an entry that outlived its
    /// directory no sign-in: no save's locks are there, and the directory
    /// goes.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_login_refused_before_it_forgot_does_not_revive_an_entry() {
        crate::testing::on_codex_runtime(async {
            let dir = auth_dir("refused-early");
            keep(&dir, Some(signed_in("account-old")));
            std::fs::remove_dir(&dir).expect("its directory removed");
            let located = SignIn {
                store: AuthCredentialsStoreMode::Keyring,
                ..SignIn::in_files(&dir)
            };
            let sign_in = SignIn::made_at(located, None)
                .expect("made")
                .with_issuer("http://127.0.0.1:9");
            PROBE_FAILS
                .lock()
                .expect("probe")
                .push(std::fs::canonicalize(&dir).expect("dir"));
            let refused = sign_in.login(true).await.expect_err("refused");
            assert!(
                refused.to_string().contains("the test keyring refuses"),
                "{refused}"
            );
            assert!(
                !dir.exists(),
                "a login refused early kept the outlived entry's directory"
            );
            assert!(
                sign_in.account().expect("account").is_none(),
                "an outlived entry reads as a sign-in"
            );
        });
    }

    /// A logout that made the directory and stops before the stores leaves
    /// no directory beside an entry that outlived an earlier one.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_logout_that_stops_short_leaves_no_directory_it_made() {
        crate::testing::on_codex_runtime(async {
            let dir = auth_dir("logout-short");
            keep(&dir, Some(signed_in("account-old")));
            std::fs::remove_dir(&dir).expect("its directory removed");
            let located = SignIn {
                store: AuthCredentialsStoreMode::Keyring,
                ..SignIn::in_files(&dir)
            };
            let sign_in = SignIn::made_at(located, None).expect("made");
            LOGOUT_FAILS
                .lock()
                .expect("logout")
                .push(std::fs::canonicalize(&dir).expect("dir"));
            let stopped = sign_in.logout().await.expect_err("stopped short");
            assert!(
                stopped.to_string().contains("stops before the stores"),
                "{stopped}"
            );
            assert!(!dir.exists(), "the logout left the directory it made");
            assert!(
                sign_in.account().expect("account").is_none(),
                "an outlived entry reads as a sign-in"
            );
        });
    }

    /// A logout trusts a sign-in it finds once its own delete went through,
    /// or when the directory was there before it: one it made, whose delete
    /// did not go through, goes.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_logout_trusts_what_it_finds_only_once_its_delete_went_through() {
        for (made, deleted, kept) in [
            (true, false, false),
            (true, true, true),
            (false, false, true),
            (false, true, true),
        ] {
            let dir = auth_dir(&format!("logout-trust-{made}-{deleted}"));
            keep(&dir, Some(signed_in("account-1")));
            let sign_in = SignIn {
                store: AuthCredentialsStoreMode::Keyring,
                made,
                ..SignIn::in_files(&dir)
            };
            sign_in.tidy(Tidy::Logout { made, deleted });
            assert_eq!(dir.exists(), kept, "made {made}, deleted {deleted}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Tidying never goes through a link: it would remove what the link
    /// leads to and leave the link dangling.
    #[cfg(unix)]
    #[test]
    fn tidying_never_goes_through_a_link() {
        let dir = auth_dir("tidy-link");
        let target = dir.join("target");
        std::fs::create_dir_all(&target).expect("target");
        std::fs::write(target.join("auth.lock"), "").expect("lock");
        let link = dir.join("codex-auth");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        SignIn {
            reached: link.clone(),
            made: true,
            ..SignIn::in_files(&target)
        }
        .tidy(Tidy::Login(Forgot::Done));
        assert!(target.join("auth.lock").exists(), "tidied through a link");
        SignIn {
            made: true,
            ..SignIn::in_files(&target)
        }
        .tidy(Tidy::Login(Forgot::Done));
        assert!(!target.exists(), "reached as it is, it is tidied");
        let _ = std::fs::remove_dir_all(&dir);
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

    /// The sign-in store is refused, naming both places, while an older
    /// atomic's credentials are still at their old place and the new one
    /// holds none, also after the denial has made the new directory, empty:
    /// a sign-in kept at the old place would otherwise read as signed out.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_sign_in_is_refused_while_an_older_atomics_credentials_are_stranded() {
        let scratch = crate::state::TestScratch::new("stranded-sign-in");
        for dir in [
            "home",
            "legacy-config/atomic/credentials/codex-auth",
            "state",
        ] {
            std::fs::create_dir_all(scratch.join(dir)).expect("a directory of the case");
        }
        let legacy = scratch.join("legacy-config/atomic/credentials");
        let _process = crate::state::as_process_with_dirs(
            &scratch,
            &[("HOME", "/home")],
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
        let refused = SignIn::located(None).map(|_| ());
        let Err(SignInError::Store(CredentialError::File(why))) = refused else {
            panic!("the sign-in was looked for beside stranded credentials: {refused:?}");
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
        std::fs::rename(legacy.join("codex-auth"), new.join("codex-auth")).expect("moved");
        let located = SignIn::located(None).expect("the moved sign-in store");
        assert_eq!(located.reached, new.join("codex-auth"));
    }
}
