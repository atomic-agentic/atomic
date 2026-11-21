// SPDX-License-Identifier: Apache-2.0
//
// Adapted from OpenAI Codex's codex-rs/thread-manager-sample/src/main.rs
// (Apache-2.0, Copyright 2025 OpenAI) at the revision in deps/codex.toml.

//! Running atoms on Codex: the entry point and one thread turn.
//!
//! In v0.1 atomic drives Codex threads through `ThreadManager` directly. The
//! `Config` below is built in full, as Codex's own sample does, so nothing is
//! read from the user's Codex configuration: Codex's home is the project's
//! state directory (see [`crate::state`]).

use crate::usage::{Call, Exhausted, MeterError, Metering, Outcome, Ticket, Usage, meter_error};
use anyhow::Context;
use anyhow::bail;
use codex_core_api::AbsolutePathBuf;
use codex_core_api::AltScreenMode;
use codex_core_api::ApprovalsReviewer;
use codex_core_api::Arg0DispatchPaths;
use codex_core_api::AskForApproval;
use codex_core_api::AuthKeyringBackendKind;
use codex_core_api::AuthManager;
use codex_core_api::AutoCompactTokenLimitScope;
use codex_core_api::CodexAppsToolsCache;
use codex_core_api::CodexHomeUserInstructionsProvider;
use codex_core_api::CodexThread;
use codex_core_api::Config;
use codex_core_api::ConfigLayerStack;
use codex_core_api::ConfigLoadOptions;
use codex_core_api::ConfigRequirements;
use codex_core_api::ConfigRequirementsToml;
use codex_core_api::Constrained;
use codex_core_api::EnvironmentManager;
use codex_core_api::EventMsg;
use codex_core_api::ExecServerRuntimeOptions;
use codex_core_api::ExtensionRegistryBuilder;
use codex_core_api::Feature;
use codex_core_api::Features;
use codex_core_api::GhostSnapshotConfig;
use codex_core_api::History;
use codex_core_api::MemoriesConfig;
use codex_core_api::ModelAvailabilityNuxConfig;
use codex_core_api::ModelCatalogSettings;
use codex_core_api::MultiAgentV2Config;
use codex_core_api::NewThread;
use codex_core_api::NotSubmittedReason;
use codex_core_api::Notice;
use codex_core_api::OPENAI_PROVIDER_ID;
use codex_core_api::Op;
use codex_core_api::OtelConfig;
use codex_core_api::PermissionProfile;
use codex_core_api::Permissions;
use codex_core_api::ProjectConfig;
use codex_core_api::RealtimeAudioConfig;
use codex_core_api::RealtimeConfig;
use codex_core_api::SessionPickerViewMode;
use codex_core_api::SessionSource;
use codex_core_api::ShellEnvironmentPolicy;
use codex_core_api::SqliteConfig;
use codex_core_api::StartIfIdleSubmission;
use codex_core_api::StartThreadOptions;
use codex_core_api::TerminalResizeReflowConfig;
use codex_core_api::ThreadManager;
use codex_core_api::ThreadStoreBuilder;
use codex_core_api::ThreadStoreConfig;
use codex_core_api::ToolSuggestConfig;
use codex_core_api::TuiKeymap;
use codex_core_api::TuiNotificationSettings;
use codex_core_api::TuiPetAnchor;
use codex_core_api::TurnInputRequest;
use codex_core_api::UriBasedFileOpener;
use codex_core_api::UserInput;
use codex_core_api::WebSearchMode;
use codex_core_api::arg0_dispatch_or_else;
use codex_core_api::bootstrap_auth_config;
use codex_core_api::built_in_model_providers;
use codex_core_api::init_state_db;
use codex_core_api::load_config_toml_with_layer_stack;
use codex_core_api::local_agent_graph_store_from_state_db;
use codex_core_api::passthrough_image_store;
use codex_core_api::resolve_bootstrap_respect_system_proxy;
use codex_core_api::resolve_installation_id;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The provider id atomic registers for its model endpoint.
const ENDPOINT_PROVIDER_ID: &str = "atomic-endpoint";

/// An OpenAI-compatible Responses API endpoint and the model to use there.
#[derive(Clone, Debug)]
pub struct ModelEndpoint {
    /// Base URL of the API, for example `http://127.0.0.1:13305/api/v1`.
    pub base_url: String,
    /// Model id at that endpoint.
    pub model: String,
    /// Environment variable holding the API key, if the endpoint needs one.
    pub api_key_env: Option<String>,
    /// A ChatGPT sign-in (see [`crate::signin`]): Codex atoms call ChatGPT's
    /// Codex backend with it, and `base_url` and `api_key_env` are not
    /// used. Lite atoms cannot use it.
    pub sign_in: Option<crate::signin::SignIn>,
}

/// One turn to run on a fresh thread.
#[derive(Clone, Debug, Default)]
pub struct TurnSpec {
    /// Instructions for the developer role; never holds input data.
    pub developer_instructions: Option<String>,
    /// The user turn.
    pub prompt: String,
    /// A JSON Schema the final answer must follow (structured output).
    pub output_schema: Option<serde_json::Value>,
    /// A model name that overrides the endpoint's.
    pub model: Option<String>,
    /// The sandbox the thread runs in. The network is always restricted.
    pub sandbox: crate::typed::Sandbox,
    /// Environment variables set for every command the thread runs, such as
    /// those that point build tools at its run's caches (see
    /// [`crate::worktree::Caches`]).
    pub env: BTreeMap<String, String>,
    /// Directories a `workspace-write` thread may write besides its working
    /// directory, such as its run's build caches. Absolute paths.
    pub writable_roots: Vec<PathBuf>,
}

/// What a thread's commands are kept out of. It has no default: every
/// caller that starts a thread says which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateGuard {
    /// Deny atomic's private state to the thread's commands.
    Deny(PrivateState),
    /// The thread belongs to no project state: a test runtime, which has no
    /// sandbox helper to enforce a denial with (Codex enforces it through
    /// the helper, even for its own reads). A workspace-write thread whose
    /// writable roots cover atomic's state is refused, and its temporary
    /// directory is made in the system temporary directory.
    NoProjectState,
}

/// What [`StateGuard::Deny`] denies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateState {
    /// The project's state directory, where each thread gets its temporary
    /// directory in its `tmp/`.
    pub state_dir: PathBuf,
    /// Where every project of the atomic home keeps its private state
    /// (`<home>/private`): denied whole, by one entry, so a project made
    /// while a thread runs is covered too.
    pub private: PathBuf,
    /// Other paths denied whole: the context key, the credential file
    /// store, another atomic home's state, the person's own configuration,
    /// which can name a store with its password or an endpoint with its
    /// credential, and the well-known places in the home directory that
    /// hold credentials ([`HOME_SECRETS`]). A path that does not exist when
    /// a command starts has nothing to deny, and is never created; one made
    /// while the command runs is denied to the commands that start after.
    /// Links among them are followed when the list is made, not later.
    pub also: Vec<PathBuf>,
}

/// Where, under the home directory, people and their tools keep
/// credentials: denied to every atom's commands, which a model chooses.
/// Shell startup files and history are among them, since they often export
/// a token or hold one typed at the prompt. Entries under `.config/`,
/// `.local/share/` and `.cache/` are denied where `XDG_CONFIG_HOME`,
/// `XDG_DATA_HOME` and `XDG_CACHE_HOME` move them as well.
pub const HOME_SECRETS: &[&str] = &[
    ".ssh",
    ".aws",
    ".netrc",
    ".codex",
    ".claude",
    ".claude.json",
    ".config/gcloud",
    ".azure",
    ".kube",
    ".docker/config.json",
    ".config/containers/auth.json",
    ".gnupg",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".config/git/credentials",
    ".config/gh",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".m2/settings.xml",
    ".gradle/gradle.properties",
    ".gem/credentials",
    ".pgpass",
    ".vault-token",
    ".terraform.d/credentials.tfrc.json",
    ".cache/huggingface/token",
    ".config/sops/age/keys.txt",
    ".config/rclone/rclone.conf",
    ".password-store",
    ".local/share/keyrings",
    ".profile",
    ".bash_profile",
    ".bash_login",
    ".bashrc",
    ".bashrc.d",
    ".bash_aliases",
    ".bash_logout",
    ".bash_history",
    ".zshenv",
    ".zprofile",
    ".zshrc",
    ".zlogin",
    ".zlogout",
    ".zsh_history",
    ".zsh_sessions",
    ".python_history",
    ".psql_history",
    ".config/fish",
    ".local/share/fish",
];

/// Variables that move a credential file or directory elsewhere, each with
/// what of the place it names is denied: `""` for the place itself.
const MOVED_SECRETS: &[(&str, &[&str])] = &[
    (
        "ZDOTDIR",
        &[
            ".zshenv",
            ".zprofile",
            ".zshrc",
            ".zlogin",
            ".zlogout",
            ".zsh_history",
            ".zsh_sessions",
        ],
    ),
    ("HISTFILE", &[""]),
    ("GNUPGHOME", &[""]),
    ("CARGO_HOME", &["credentials", "credentials.toml"]),
    ("DOCKER_CONFIG", &["config.json"]),
    ("AWS_SHARED_CREDENTIALS_FILE", &[""]),
    ("AWS_CONFIG_FILE", &[""]),
    ("NETRC", &[""]),
    ("NPM_CONFIG_USERCONFIG", &[""]),
    ("npm_config_userconfig", &[""]),
    ("PASSWORD_STORE_DIR", &[""]),
    ("CODEX_HOME", &[""]),
];

/// The home directories whose credential places are denied, as `env` reads
/// `HOME` and with `passwd` the password database's home for this user:
/// both, when they differ, so overriding `HOME` hides nothing. An error,
/// so nothing runs, when neither is an absolute path.
pub fn real_homes(
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    passwd: Option<PathBuf>,
) -> Result<Vec<PathBuf>, String> {
    let mut homes: Vec<PathBuf> = env("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
        .into_iter()
        .collect();
    if let Some(passwd) = passwd.filter(|p| p.is_absolute())
        && !homes.contains(&passwd)
    {
        homes.push(passwd);
    }
    if homes.is_empty() {
        return Err(
            "the home directory is unknown (HOME is unset or not an absolute path, and the user \
             has no home in the password database), so the credentials kept there could not be \
             denied to atoms' commands; set HOME"
                .into(),
        );
    }
    Ok(homes)
}

/// Every credential place denied to atoms' commands, as `env` reads the
/// variables: [`HOME_SECRETS`] under each of [`real_homes`], those under
/// `.config/`, `.local/share/` and `.cache/` where the XDG variables move
/// them too, and what the variables that move a credential name.
pub fn home_secrets(
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    passwd: Option<PathBuf>,
) -> Result<Vec<PathBuf>, String> {
    let absolute = |name: &str| env(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let mut out = Vec::new();
    for home in real_homes(env, passwd)? {
        out.extend(HOME_SECRETS.iter().map(|p| home.join(p)));
    }
    for (prefix, variable) in [
        (".config/", "XDG_CONFIG_HOME"),
        (".local/share/", "XDG_DATA_HOME"),
        (".cache/", "XDG_CACHE_HOME"),
    ] {
        if let Some(moved) = absolute(variable) {
            out.extend(
                HOME_SECRETS
                    .iter()
                    .filter_map(|p| p.strip_prefix(prefix))
                    .map(|rest| moved.join(rest)),
            );
        }
    }
    for (variable, inside) in MOVED_SECRETS {
        if let Some(place) = absolute(variable) {
            out.extend(inside.iter().map(|p| {
                if p.is_empty() {
                    place.clone()
                } else {
                    place.join(p)
                }
            }));
        }
    }
    // KUBECONFIG names a list of files.
    if let Some(list) = env("KUBECONFIG") {
        out.extend(std::env::split_paths(&list).filter(|p| p.is_absolute()));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

impl StateGuard {
    /// The guard for threads of the project whose state is `state`. Creates
    /// the user-wide private directories it denies, so they are denied from
    /// the start.
    pub fn for_project(state: &crate::state::ProjectState) -> anyhow::Result<Self> {
        Self::deny_state_dir(state.dir())
    }

    /// The guard for a state directory at `state_dir`, with the atomic home's
    /// private state and the user-wide private directories as configured.
    pub fn deny_state_dir(state_dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let private = crate::state::base_dir()?.join("private");
        crate::state::private_dir(&private)?;
        let mut also = vec![crate::state::credentials_dir()?];
        also.push(crate::context::default_key_dir().map_err(anyhow::Error::msg)?);
        // Made if missing, like the others, so it is denied from the start
        // and a file put there later is denied too.
        also.extend(crate::state::user_config_dir());
        for dir in &also {
            crate::state::private_dir(dir)?;
        }
        // The person's real home, whatever the shell is given as its own,
        // and the credential places atomic's own environment moves; none is
        // made, and with no home known nothing runs.
        also.extend(
            home_secrets(&person_env, crate::state::passwd_home()).map_err(anyhow::Error::msg)?,
        );
        // With ATOMIC_HOME set, the default home's state, credentials and
        // keys are another atomic's: denied too, where they exist.
        if std::env::var_os("ATOMIC_HOME").is_some_and(|h| !h.is_empty()) {
            let defaults = [
                dirs::data_dir().map(|d| d.join("atomic")),
                dirs::config_dir().map(|d| d.join("atomic")),
            ];
            also.extend(defaults.into_iter().flatten().filter(|d| d.exists()));
        }
        Ok(Self::Deny(PrivateState {
            state_dir: state_dir.into(),
            private,
            also,
        }))
    }
}

/// The person's `CODEX_HOME`, as the process was started with it, before
/// [`dispatch`] points it at atomic's own.
static PERSON_CODEX_HOME: std::sync::OnceLock<Option<std::ffi::OsString>> =
    std::sync::OnceLock::new();

/// The person's environment, as atomic was started with it: `CODEX_HOME`
/// as it was before [`dispatch`] replaced it.
fn person_env(name: &str) -> Option<std::ffi::OsString> {
    match (name, PERSON_CODEX_HOME.get()) {
        ("CODEX_HOME", Some(theirs)) => theirs.clone(),
        _ => std::env::var_os(name),
    }
}

/// How many directory levels inside a denied directory links are followed
/// to the files and directories they point at, which are denied too.
const LINK_DEPTH: usize = 4;

/// The most link targets denied besides the fixed places. The whole list
/// goes to the sandbox helper in one argument, which has a size limit, so
/// past this nothing runs.
const MAX_LINK_TARGETS: usize = 512;

/// Where the system keeps what every command needs: a link into one of
/// these is never followed to deny its target.
const SYSTEM_PREFIXES: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/etc",
    "/opt",
    "/nix/store",
    "/dev",
    "/proc",
    "/sys",
    "/run",
];

/// The links up to [`LINK_DEPTH`] directory levels inside `dir`. Links to
/// directories are not walked into.
fn links_in(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            out.push(entry.path());
        } else if kind.is_dir() {
            links_in(&entry.path(), depth - 1, out);
        }
    }
}

/// What a link or a variable resolved to, if it should be denied: only a
/// regular file or a directory, the person's own, outside the system's
/// directories. A history file linked to `/dev/null`, or a tool linked to
/// the system's `python3`, is left alone, since denying it would break
/// every command.
fn deniable(target: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(target) else {
        return false;
    };
    if !(meta.is_file() || meta.is_dir()) {
        return false;
    }
    if SYSTEM_PREFIXES.iter().any(|p| target.starts_with(p)) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != crate::state::effective_uid() {
            return false;
        }
    }
    true
}

/// Every path a profile denies under `guard`, for a thread whose Codex home
/// is `codex_home`: the state directory's `private/`, the paths in
/// [`PrivateState::also`], and everything in Codex's home but the helper
/// links its shell needs. Each is denied whole, never re-opened below, so
/// the sandbox needs no mount inside a denied directory. A path reached
/// through a linked directory is denied under both spellings; one that is
/// itself a link, such as a `~/.bashrc` kept with a person's dotfiles, is
/// denied as the file or directory it points at, since the sandbox cannot
/// mount on a link, and a read through the link lands on the denied target.
/// The real file or directory behind each link up to [`LINK_DEPTH`] levels
/// inside a denied directory is denied too, such as a `config.yaml` linked
/// to one kept elsewhere.
///
/// A resolved target is denied only if [`deniable`]. Nothing runs when a
/// denied path would cover what the thread needs (its working directory
/// `cwd`, the directories it may write, `writable`, the state directory,
/// its Codex home or atomic's own program), or when a link's target lies in
/// a directory the thread may write, where a command could rename it out
/// from under its denial.
///
/// The list is made when a thread starts. A path that does not exist then
/// is denied only once it does, when a later command starts: one made while
/// a command runs can be read by that command.
fn denied_paths(
    private: &PrivateState,
    codex_home: &Path,
    cwd: Option<&Path>,
    writable: &[PathBuf],
) -> anyhow::Result<(Vec<PathBuf>, Vec<String>)> {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    let writable: Vec<PathBuf> = writable.iter().map(|p| real(p)).collect();
    // atomic's state holds what the thread may write (its worktrees,
    // scratch and temporary directories): no denied link may point there.
    let mut writable = writable;
    writable.push(real(&private.state_dir));
    let mut needed = writable.clone();
    needed.extend(cwd.map(real));
    needed.push(real(codex_home));
    needed.extend(std::env::current_exe().ok().map(|p| real(&p)));
    let covers = |denied: &Path| needed.iter().find(|n| n.starts_with(denied)).cloned();
    // Every project's private state, by one entry, however many projects
    // the home holds.
    let mut denied = vec![private.private.clone()];
    denied.extend(private.also.iter().cloned());
    // Before anything is walked: a place that covers what the thread needs,
    // such as a variable naming the root.
    for path in &denied {
        if let Some(needed) = covers(&real(path)).or_else(|| covers(path)) {
            bail!(
                "{} holds {}, which atoms' commands need, so atomic cannot deny it; check \
                 the variables that name credential places, such as HISTFILE or CODEX_HOME",
                path.display(),
                needed.display()
            );
        }
    }
    let mut links = Vec::new();
    for dir in &private.also {
        links_in(dir, LINK_DEPTH, &mut links);
    }
    if links.len() > MAX_LINK_TARGETS {
        bail!(
            "the places denied to atoms' commands hold {} links, more than the {MAX_LINK_TARGETS} \
             atomic follows; remove some, or keep the files they point at inside those places",
            links.len()
        );
    }
    denied.extend(links);
    let children = |dir: &Path, keep: &str| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name() != keep)
            .map(|e| e.path())
            .collect()
    };
    let arg0 = format!("arg0-{}", std::env::consts::OS);
    let mut all = Vec::new();
    // Shell snapshots are off (see `converse_async`): none is kept open.
    // The Codex home is atomic's own, so what is in it is denied as it is,
    // a link as what it points at.
    let mut own = Vec::new();
    for entry in children(codex_home, "") {
        if entry.file_name().is_some_and(|n| n == "tmp") {
            own.extend(children(&entry, &arg0));
        } else {
            own.push(entry);
        }
    }
    for entry in own {
        if std::fs::symlink_metadata(&entry).is_ok_and(|m| m.file_type().is_symlink()) {
            all.extend(std::fs::canonicalize(&entry).ok());
        } else {
            all.push(entry);
        }
    }
    for path in denied {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            // Nothing there yet: denied once it is.
            all.push(path);
            continue;
        };
        let target = std::fs::canonicalize(&path).ok();
        if meta.file_type().is_symlink() || target.as_ref().is_some_and(|t| *t != path) {
            // A link, or a path through one: what it resolves to, if that
            // is a file or directory of the person's, and never what the
            // thread needs or may write.
            let Some(target) = target.filter(|t| deniable(t)) else {
                continue;
            };
            if let Some(needed) = covers(&target) {
                bail!(
                    "{} resolves to {}, which holds {}, which atoms' commands need; atomic \
                     cannot deny it",
                    path.display(),
                    target.display(),
                    needed.display()
                );
            }
            if let Some(root) = writable.iter().find(|w| target.starts_with(w)) {
                bail!(
                    "{} resolves to {}, inside {}, which atoms' commands may write, so a command \
                     could move it out from under its denial; keep it elsewhere",
                    path.display(),
                    target.display(),
                    root.display()
                );
            }
            all.push(target.clone());
            if !meta.file_type().is_symlink() {
                all.push(path);
            }
            continue;
        }
        if !(meta.is_file() || meta.is_dir()) {
            // A device or socket, such as a variable naming /dev/null.
            continue;
        }
        if let Some(needed) = covers(&path) {
            bail!(
                "{} holds {}, which atoms' commands need, so atomic cannot deny it; check \
                 the variables that name credential places, such as HISTFILE or CODEX_HOME",
                path.display(),
                needed.display()
            );
        }
        all.push(path);
    }
    // A path inside another denied path is denied with it, and the sandbox
    // cannot mount a denial inside one: such as the credential store inside
    // the person's own configuration directory.
    all.sort();
    all.dedup();
    let inside_another = |p: &PathBuf| all.iter().any(|q| q != p && p.starts_with(q));
    let all: Vec<PathBuf> = all.iter().filter(|p| !inside_another(p)).cloned().collect();
    // Codex creates its state database when the first thread starts, after
    // the list above is made: a pattern, expanded when each command starts,
    // covers it.
    let globs = vec![format!("{}/*.sqlite*", codex_home.display())];
    Ok((all, globs))
}

/// What Codex reads from its home at start: `.env` sets variables in
/// atomic's process (none of Codex's own), and `config.toml` its settings.
const CODEX_HOME_FILES: &[&str] = &[".env", "config.toml"];

/// Create `path` empty, readable by its owner only, unless it exists.
fn planted_never(path: &Path) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("create {}", path.display()))),
    }
}

/// The network every sandbox has: none, local sockets included.
const SANDBOX_NETWORK: codex_protocol::permissions::NetworkSandboxPolicy =
    codex_protocol::permissions::NetworkSandboxPolicy::Restricted;

/// Codex's permission profile for a sandbox: read-only, or writes confined
/// to the workspace; the network is restricted either way. Under
/// [`StateGuard::Deny`], atomic's private state is denied as well (see
/// [`denied_paths`]).
pub(crate) fn permission_profile(
    sandbox: crate::typed::Sandbox,
    guard: &StateGuard,
    codex_home: &Path,
    cwd: Option<&Path>,
    writable: &[PathBuf],
) -> anyhow::Result<PermissionProfile> {
    use codex_protocol::permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry,
    };
    let base = match sandbox {
        crate::typed::Sandbox::ReadOnly => PermissionProfile::read_only(),
        // Not /tmp or $TMPDIR: a repository or state directory there would be
        // writable from the turn. A thread gets a temporary directory of its
        // own instead (see `TurnTemp`).
        crate::typed::Sandbox::WorkspaceWrite => PermissionProfile::workspace_write_with(
            &[],
            SANDBOX_NETWORK,
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ true,
        ),
    };
    let StateGuard::Deny(private) = guard else {
        return Ok(base);
    };
    // Files Codex reads from its home when atomic starts: there from the
    // start, so they are denied with the rest, and a command can never plant
    // them for the next run.
    for name in CODEX_HOME_FILES {
        planted_never(&codex_home.join(name))?;
    }
    let mut policy = base.file_system_sandbox_policy();
    // The rest of Codex's home is readable and never writable, even under a
    // writable root that covers it: no command plants a file Codex reads
    // from there (environments.toml, AGENTS.md, rules, hooks) for the next
    // thread.
    let homes = [
        Some(codex_home.to_owned()),
        std::fs::canonicalize(codex_home).ok(),
    ];
    for home in homes.into_iter().flatten() {
        let home = AbsolutePathBuf::from_absolute_path(&home)
            .with_context(|| format!("resolve the Codex home {}", home.display()))?;
        policy.entries.push(FileSystemSandboxEntry::new(
            FileSystemPath::from(home),
            FileSystemAccessMode::Read,
        ));
    }
    let (paths, globs) = denied_paths(private, codex_home, cwd, writable)?;
    for p in paths {
        let p = AbsolutePathBuf::from_absolute_path(&p)
            .with_context(|| format!("resolve the sandbox path {}", p.display()))?;
        policy.entries.push(FileSystemSandboxEntry::new(
            FileSystemPath::from(p),
            FileSystemAccessMode::Deny,
        ));
    }
    for pattern in globs {
        policy.entries.push(FileSystemSandboxEntry::new(
            FileSystemPath::GlobPattern { pattern },
            FileSystemAccessMode::Deny,
        ));
    }
    Ok(PermissionProfile::from_runtime_permissions(
        &policy,
        SANDBOX_NETWORK,
    ))
}

/// Refuse a workspace-write thread under [`StateGuard::NoProjectState`]
/// whose writable roots cover atomic's state or user-wide private
/// directories, or lie inside them: nothing would deny them.
fn refuse_uncovered_store(
    sandbox: crate::typed::Sandbox,
    guard: &StateGuard,
    roots: &[PathBuf],
) -> anyhow::Result<()> {
    if sandbox != crate::typed::Sandbox::WorkspaceWrite || guard != &StateGuard::NoProjectState {
        return Ok(());
    }
    let mut stores = vec![crate::state::base_dir()?, crate::state::credentials_dir()?];
    stores.push(crate::context::default_key_dir().map_err(anyhow::Error::msg)?);
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    for root in roots {
        let root = real(root);
        for store in &stores {
            let store = real(store);
            if store.starts_with(&root) || root.starts_with(&store) {
                bail!(
                    "a workspace-write thread with no project state may not write {}: it covers \
                     atomic's private state at {}, which nothing would deny",
                    root.display(),
                    store.display()
                );
            }
        }
    }
    Ok(())
}

/// atomic's own variables: its configuration and the credentials it holds,
/// such as its database URL, project secrets and telemetry settings. The
/// core inheritance already leaves them out; this keeps them out if it ever
/// widens.
const ATOMIC_OWN: [&str; 3] = ["ATOMIC_*", "DBOS__*", "OTEL_*"];

/// The environment of every command a thread runs: the platform's core
/// variables from this process (`PATH`, `USER` and the like), except any
/// whose name holds `KEY`, `SECRET` or `TOKEN`, any of atomic's own and any
/// in [`SHELL_EXCLUDES`], with `HOME` at [`SHELL_HOME`], and then exactly
/// what the atom declares (`declared`), which the runtime gives the
/// attempt's own `HOME`. Nothing else of atomic's environment reaches the
/// command, and through its output, the model.
pub(crate) fn command_environment(declared: &BTreeMap<String, String>) -> ShellEnvironmentPolicy {
    use codex_protocol::config_types::{EnvironmentVariablePattern, ShellEnvironmentPolicyInherit};
    ShellEnvironmentPolicy {
        inherit: ShellEnvironmentPolicyInherit::Core,
        ignore_default_excludes: false,
        exclude: ATOMIC_OWN
            .iter()
            .chain(SHELL_EXCLUDES)
            .map(|pattern| EnvironmentVariablePattern::new_case_insensitive(pattern))
            .collect(),
        r#set: std::iter::once(("HOME".to_owned(), SHELL_HOME.to_owned()))
            .chain(
                declared
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone())),
            )
            .collect(),
        include_only: Vec::new(),
        use_profile: false,
    }
}

/// The environments a thread's shell and file work may run in: this machine,
/// and nothing else.
///
/// Codex's usual constructors read `CODEX_HOME/environments.toml`, then
/// `CODEX_EXEC_SERVER_URL` and the `CODEX_EXEC_SERVER_NOISE_*` variables
/// from this process's environment, any of which would move a turn's shell
/// to an exec-server the execution backend never chose. At the pinned
/// revision the only public constructor that reads none of them is the one
/// Codex names for tests: with no URL it builds exactly the local
/// environment, and it fails only without runtime paths, which are always
/// given. What it built is checked before any thread uses it.
async fn local_environment(paths: ExecServerRuntimeOptions) -> anyhow::Result<EnvironmentManager> {
    let manager = EnvironmentManager::create_for_tests(/*exec_server_url*/ None, Some(paths)).await;
    let ids = manager.default_environment_ids();
    let local = manager.default_environment();
    if ids != ["local"] || local.as_ref().is_none_or(|e| e.is_remote()) {
        bail!("the turn's shell would not run on this machine: environments {ids:?}");
    }
    Ok(manager)
}

/// What a thread did: its id, how many turns it ran and the tokens it used,
/// as the provider reported them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadReport {
    /// Codex's id for the thread.
    pub thread_id: String,
    /// Turns run on it.
    pub turns: u32,
    /// Input tokens, over all turns.
    pub input_tokens: i64,
    /// Output tokens, over all turns.
    pub output_tokens: i64,
    /// Total tokens, over all turns.
    pub total_tokens: i64,
}

/// Paths Codex needs to re-run this executable as one of its helpers.
#[derive(Clone, Debug)]
pub struct CodexPaths(Arg0DispatchPaths);

impl CodexPaths {
    /// Codex's Linux sandbox helper, when this process has one: atomic's own
    /// executable, run under the helper's name.
    pub fn linux_sandbox(&self) -> Option<&Path> {
        self.0.codex_linux_sandbox_exe.as_deref()
    }

    /// Paths for a process that did not start through [`dispatch`], such as
    /// a test: Codex can re-run this executable, but has no sandbox helper.
    pub fn without_helpers() -> Self {
        Self(Arg0DispatchPaths {
            codex_self_exe: std::env::current_exe().ok(),
            ..Arg0DispatchPaths::default()
        })
    }

    /// Paths for a process that did not start through [`dispatch`] but has
    /// atomic's binary at hand, such as a test: Codex re-runs `atomic` for
    /// its helpers, and runs `linux_sandbox`, a link to `atomic` named
    /// `codex-linux-sandbox`, as its Linux sandbox.
    pub fn with_helpers(atomic: PathBuf, linux_sandbox: PathBuf) -> Self {
        Self(Arg0DispatchPaths {
            codex_self_exe: Some(atomic),
            codex_linux_sandbox_exe: Some(linux_sandbox),
            ..Arg0DispatchPaths::default()
        })
    }
}

/// Harden atomic itself, not one of Codex's helpers: the process that holds
/// a sign-in's token and the operator credential cannot be dumped, read
/// through /proc by a same-user process, or leave a core file. It loses its
/// LD_* variables, and the commands it starts inherit a core-file limit of
/// zero (on macOS it also refuses a debugger). Both entry points call it.
fn harden() {
    if !is_helper(std::env::args_os()) {
        codex_process_hardening::pre_main_hardening();
    }
}

/// Harden `atomic runner` as [`dispatch`] hardens atomic: the commands it
/// runs arrive on its stdin with their environments, and a confined one's
/// with its proxy token, so no process of the same user may read its memory
/// or the /proc files that show it, such as `environ` and `mem`, unless it
/// has `CAP_SYS_PTRACE` over the runner's user namespace; its `cmdline` and
/// `stat` stay readable. Call it first, before the request is read.
pub fn harden_runner() {
    // The runner holds commands' environments, proxy tokens among them.
    codex_process_hardening::pre_main_hardening();
}

/// The process entry point.
///
/// The same executable re-runs itself as Codex's helpers (the Linux sandbox,
/// `apply_patch`); those invocations are dispatched here and never reach
/// `main_fn`. Before anything else, `CODEX_HOME` is pointed at `codex_home`, so
/// Codex never creates or reads files in the user's own Codex home. Call this
/// first in `main`, before any thread starts.
pub fn dispatch<F, Fut>(codex_home: &Path, main_fn: F) -> anyhow::Result<()>
where
    F: FnOnce(CodexPaths) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>>,
{
    keep_sandbox_off_keyrings();
    harden();
    give_the_command_its_own_tmpdir();
    // The person's own Codex home stays denied to atoms' commands.
    let _ = PERSON_CODEX_HOME.set(std::env::var_os("CODEX_HOME"));
    // SAFETY: called at the start of main, while the process has one thread,
    // so no other thread can read the environment concurrently.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("CODEX_HOME", codex_home);
    }
    scrub_overrides();
    arg0_dispatch_or_else(move |paths| main_fn(CodexPaths(paths)))
}

/// The entry point for a command that starts no Codex thread, such as
/// `atomic model`: hardened as [`dispatch`] hardens atomic, with nothing in
/// the environment standing in for a sign-in's tokens, but with no Codex
/// home, so no `.env` of one is loaded. Call this first in `main`, before any
/// thread starts.
pub fn without_threads<Fut>(main_fn: Fut) -> anyhow::Result<()>
where
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    harden();
    scrub_overrides();
    // Codex's own entry point gives its futures this much stack.
    const STACK: usize = 16 * 1024 * 1024;
    let handle = std::thread::Builder::new()
        .name("atomic-main".into())
        .stack_size(STACK)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(STACK)
                .build()?
                .block_on(main_fn)
        })?;
    match handle.join() {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Whether this process was started as one of Codex's helpers rather than
/// as atomic: under a helper's name (the Linux sandbox, `apply_patch`) or with
/// a helper's first argument. A helper must run before atomic touches
/// anything, such as its state directory: it runs inside a turn, where
/// `HOME` is the turn's scratch directory, which the turn may write.
pub fn is_helper(mut args: impl Iterator<Item = std::ffi::OsString>) -> bool {
    let name = args
        .next()
        .and_then(|argv0| {
            Path::new(&argv0)
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    if matches!(
        name.as_str(),
        "codex-linux-sandbox" | "apply_patch" | "applypatch"
    ) {
        return true;
    }
    let first = args.next().unwrap_or_default();
    // Exactly the first arguments Codex's dispatch at the pinned revision
    // answers to (arg0, apply-patch, the exec-server's helpers, the pty
    // setup, the Windows sandboxes).
    matches!(
        first.to_string_lossy().as_ref(),
        "--codex-run-as-apply-patch"
            | "--codex-run-as-arg0-exec-helper"
            | "--codex-run-as-fs-helper"
            | "--codex-run-as-process-setup"
            | "--__codex-windows-mxc"
            | "--run-as-windows-sandbox"
    )
}

/// Run as the Codex helper this process was started as (see [`is_helper`]).
/// A helper never returns; anything else is an error, and nothing was
/// touched.
pub fn run_helper() -> anyhow::Result<()> {
    keep_sandbox_off_keyrings();
    give_the_command_its_own_tmpdir();
    arg0_dispatch_or_else(|_| async {
        bail!("this process was started as a Codex helper Codex does not know")
    })
}

/// The variable that carries a turn's own temporary directory past Codex's
/// sandbox helper to the command (see [`give_the_command_its_own_tmpdir`]).
pub(crate) const TURN_TMPDIR: &str = "ATOMIC_TURN_TMPDIR";

/// When this process is the inner stage of Codex's Linux sandbox helper,
/// just before the command runs, give the command its own `TMPDIR`: the
/// directory in [`TURN_TMPDIR`], private to its turn. The helper's outer
/// stage keeps the backend's shared `TMPDIR`, where Codex keeps its registry
/// of the mount points it makes, which every turn sharing a writable root
/// must share; the command never writes there.
fn give_the_command_its_own_tmpdir() {
    #[cfg(target_os = "linux")]
    if invoked_as_linux_sandbox()
        && is_inner_stage(std::env::args_os())
        && let Some(own) = std::env::var_os(TURN_TMPDIR)
    {
        // SAFETY: the helper is single-threaded here, before it applies its
        // restrictions and executes the command.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TMPDIR", own);
            std::env::remove_var(TURN_TMPDIR);
        }
    }
}

/// Whether the Linux sandbox helper's arguments `args` (the first being its
/// own name) are its inner stage's: `--apply-seccomp-then-exec` among its
/// options, before the `--` that ends them. After that `--` is the command,
/// whose own arguments, the model's to choose, say nothing about the stage;
/// Codex's own launcher reads them the same way.
#[cfg(any(target_os = "linux", test))]
fn is_inner_stage(args: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    args.into_iter()
        .skip(1)
        .take_while(an_option)
        .any(|a| a == "--apply-seccomp-then-exec")
}

/// Whether `arg`, read in order from the helper's arguments, is still one
/// of its options: not the `--` after which the command begins.
#[cfg(any(target_os = "linux", test))]
fn an_option(arg: &std::ffi::OsString) -> bool {
    arg != "--"
}

/// When this process is Codex's Linux sandbox helper, keep the command it
/// runs off the kernel keyrings, or exit: with them in reach the command
/// could read what they hold, or plant a key. The outer stage, which stays
/// outside the sandbox while the command runs, is also made non-dumpable,
/// and only then reads the command's proxy token from the socket it was
/// handed ([`take_secret_env`]): the token is never in its exec
/// environment, and once it is in its memory only a process with
/// `CAP_SYS_PTRACE` over its user namespace can read it through /proc.
/// Between its exec and then, the socket waits unread; it cannot be
/// reopened through /proc, so only a process that may attach to the helper
/// can take it (narrowed, not closed, at Yama's `ptrace_scope` 0).
/// Exec resets it: where the outer stage has no mounts of its own to manage
/// it execs bubblewrap itself, by which time Codex's helper has taken the
/// token out of its environment, and the bridges it forked first keep the
/// token, non-dumpable.
fn keep_sandbox_off_keyrings() {
    #[cfg(target_os = "linux")]
    if invoked_as_linux_sandbox() {
        let inner = is_inner_stage(std::env::args_os());
        let kept_off = if inner {
            // Inside the sandbox, just before the command runs.
            deny_keyrings()
        } else {
            leave_session_keyring()
        };
        if let Err(e) = kept_off {
            crate::log_line!(
                "atomic: cannot keep a sandboxed command off the kernel keyrings: {e}"
            );
            std::process::exit(1);
        }
        if !inner && let Err(e) = not_dumpable() {
            crate::log_line!(
                "atomic: cannot make the sandbox helper, which is about to read the \
                 command's proxy token, non-dumpable: {e}"
            );
            std::process::exit(1);
        }
        if !inner && let Err(e) = take_secret_env() {
            crate::log_line!(
                "atomic: the sandbox helper cannot read the proxy token handed to it: {e}"
            );
            std::process::exit(1);
        }
    }
}

/// Variables, as names and values.
type Vars = Vec<(String, String)>;

/// `env` split into the proxy token's entry and the rest.
fn split_off_token(env: Vars) -> (Vars, Vars) {
    env.into_iter()
        .partition(|(k, _)| k == codex_network_proxy::PROXY_ATTRIBUTION_TOKEN_ENV_KEY)
}

/// When this process is the sandbox helper's outer stage and was handed its
/// command's proxy token on a socket ([`atomic_exec::SecretEnv`]), read it
/// into its environment, where Codex's helper looks for it, and close the
/// socket. Only that one variable is taken, and only from a socket: a
/// descriptor that is not one, or any other variable, fails it. Called once
/// it is non-dumpable, so from then on no other process of its user without
/// `CAP_SYS_PTRACE` over its user namespace can read the token from it.
/// Before that, from its exec, the socket cannot be reopened through
/// /proc as a pipe could, so another process of its user can take it only
/// by attaching to it, which Yama's `ptrace_scope` of 1 or more refuses to
/// one that is not its ancestor; at `ptrace_scope` 0 the window is
/// narrowed, not closed.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn take_secret_env() -> std::io::Result<()> {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    let Some(fd) = std::env::var_os(atomic_exec::SECRET_ENV_FD) else {
        return Ok(());
    };
    let invalid = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, why.to_owned());
    let fd: std::os::fd::RawFd = fd
        .to_str()
        .and_then(|fd| fd.parse().ok())
        .filter(|fd| *fd > 2)
        .ok_or_else(|| invalid("its descriptor is not a number above 2"))?;
    // SAFETY: the descriptor was handed to this process for this alone; it
    // is read and closed here, once.
    let mut socket = unsafe { std::fs::File::from_raw_fd(fd) };
    use std::os::unix::fs::FileTypeExt;
    if !socket.metadata()?.file_type().is_socket() {
        return Err(invalid("its descriptor is not a socket"));
    }
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes)?;
    drop(socket);
    let secret = atomic_exec::SecretEnv::decode(&bytes).map_err(|e| invalid(&e))?;
    let token = the_proxy_token(secret).map_err(|e| invalid(&e))?;
    // SAFETY: the helper is single-threaded here, before Codex's helper
    // starts and reads its environment.
    unsafe {
        std::env::remove_var(atomic_exec::SECRET_ENV_FD);
        std::env::set_var(codex_network_proxy::PROXY_ATTRIBUTION_TOKEN_ENV_KEY, token);
    }
    Ok(())
}

/// The proxy token in `secret`, which must hold it and nothing else: the
/// sandbox helper takes no other variable from its socket.
#[cfg(any(target_os = "linux", test))]
fn the_proxy_token(secret: atomic_exec::SecretEnv) -> Result<String, String> {
    let mut entries = secret.0.into_iter();
    let (Some((name, token)), None) = (entries.next(), entries.next()) else {
        return Err("it was handed other than one variable".to_owned());
    };
    if name != codex_network_proxy::PROXY_ATTRIBUTION_TOKEN_ENV_KEY {
        return Err(format!("it was handed {name}, not the proxy token"));
    }
    Ok(token)
}

/// Make this process non-dumpable: its memory and the /proc files that show
/// it, its environment among them, become unreadable to other processes of
/// its user without `CAP_SYS_PTRACE` over its user namespace; its `cmdline`
/// and `stat` stay readable.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn not_dumpable() -> std::io::Result<()> {
    // SAFETY: PR_SET_DUMPABLE takes an integer and touches no memory.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Confines an exec atom's command under Codex's Linux sandbox helper, as a
/// turn's commands are confined: the whole filesystem readable except
/// atomic's private state, which is denied as it is to turns (see
/// [`denied_paths`]), and the private state of every other project under
/// the same atomic home; only the command's own writable directories, such
/// as its scratch directory and its run's build caches, writable; no network
/// and no sockets, unless its atom declares hosts, and then only those,
/// through a proxy of atomic's own (see [`crate::network`]).
/// The command still runs with exactly its environment, in its directory.
#[cfg_attr(
    not(feature = "exec-loopback-tests"),
    doc = r#"
Only a build with the `exec-loopback-tests` feature, for the sandbox suites,
can let a command reach this machine's loopback interface. (rustdoc does not
check the error code of a `compile_fail` test; the twin below, which runs
with the feature, pins that this fails for want of the method.)

```compile_fail,E0599
fn widen(sandbox: atomic_core::codex::ExecSandbox) -> atomic_core::codex::ExecSandbox {
    sandbox.reaching_loopback_for_tests(vec!["127.0.0.1".into()])
}
```
"#
)]
#[cfg_attr(
    feature = "exec-loopback-tests",
    doc = r#"
With the `exec-loopback-tests` feature, for the sandbox suites, a command
may reach this machine's loopback interface:

```
fn widen(sandbox: atomic_core::codex::ExecSandbox) -> atomic_core::codex::ExecSandbox {
    sandbox.reaching_loopback_for_tests(vec!["127.0.0.1".into()])
}
```
"#
)]
#[derive(Clone, Debug)]
pub struct ExecSandbox {
    helper: PathBuf,
    guard: StateGuard,
    codex_home: PathBuf,
    /// The proxies that limit commands declaring network access to their
    /// hosts, shared by every command this sandbox confines.
    proxies: crate::network::Proxies,
    /// Loopback addresses a command may name as a host, for tests only.
    loopback_for_tests: Vec<String>,
}

impl ExecSandbox {
    /// Confinement through `paths`' sandbox helper, denying what `guard`
    /// denies; `codex_home` is the Codex home atom threads use, whose records
    /// are denied too. Fails where this process has no helper.
    pub fn new(paths: &CodexPaths, guard: StateGuard, codex_home: PathBuf) -> anyhow::Result<Self> {
        let helper = paths.linux_sandbox().map(Path::to_owned).context(
            "this process has no Linux sandbox helper to confine exec atoms' commands with; \
             run atomic itself, which provides one",
        )?;
        Ok(Self {
            helper,
            guard,
            codex_home,
            proxies: crate::network::Proxies::default(),
            loopback_for_tests: Vec::new(),
        })
    }

    /// The same confinement, letting a command name `addrs`, loopback
    /// addresses that atomic's policy refuses, as hosts, on any port. For
    /// tests that serve on this machine; atomic never calls it, and only a
    /// build with the `exec-loopback-tests` feature has it.
    #[cfg(feature = "exec-loopback-tests")]
    #[doc(hidden)]
    pub fn reaching_loopback_for_tests(mut self, addrs: Vec<String>) -> Self {
        self.loopback_for_tests = addrs
            .into_iter()
            .filter(|a| {
                a.parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
            })
            .collect();
        self
    }

    /// The profile for a command that may write `writable`, except the
    /// directories in `read_only` inside them.
    fn profile(
        &self,
        writable: &[PathBuf],
        read_only: &[PathBuf],
    ) -> anyhow::Result<PermissionProfile> {
        use codex_protocol::permissions::{
            FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry,
        };
        let base = permission_profile(
            crate::typed::Sandbox::ReadOnly,
            &self.guard,
            &self.codex_home,
            None,
            writable,
        )?;
        let mut policy = base.file_system_sandbox_policy();
        for dir in writable {
            let dir = AbsolutePathBuf::from_absolute_path(dir)
                .with_context(|| format!("resolve the writable directory {}", dir.display()))?;
            policy.entries.push(FileSystemSandboxEntry::new(
                FileSystemPath::from(dir),
                FileSystemAccessMode::Write,
            ));
        }
        // A narrower entry that only reads carves a writable one out.
        for dir in read_only {
            let dir = AbsolutePathBuf::from_absolute_path(dir)
                .with_context(|| format!("resolve the read-only directory {}", dir.display()))?;
            policy.entries.push(FileSystemSandboxEntry::new(
                FileSystemPath::from(dir),
                FileSystemAccessMode::Read,
            ));
        }
        // No network of its own: a command whose atom declares hosts reaches
        // them only through the proxy its namespace is routed to.
        Ok(PermissionProfile::from_runtime_permissions(
            &policy,
            SANDBOX_NETWORK,
        ))
    }
}

impl atomic_exec::Confine for ExecSandbox {
    /// As [`confine_held`](atomic_exec::Confine::confine_held), for a
    /// command that declares no network. One that does needs its proxy
    /// held while it runs, which this would drop before the command
    /// started, so it is refused.
    fn confine(
        &self,
        command: &atomic_exec::Command,
        tmp: &Path,
    ) -> Result<atomic_exec::Command, atomic_exec::ExecError> {
        let declares_hosts = !command.network.is_empty();
        if declares_hosts {
            return Err(atomic_exec::ExecError::Start {
                program: command.program.clone(),
                message: "it declares hosts, so its proxy must be held while it runs; \
                          confine it with confine_held"
                    .into(),
            });
        }
        self.confine_held(command, tmp).map(|(command, _)| command)
    }

    fn confine_held(
        &self,
        command: &atomic_exec::Command,
        tmp: &Path,
    ) -> Result<(atomic_exec::Command, Option<atomic_exec::Held>), atomic_exec::ExecError> {
        let fail = |message: String| atomic_exec::ExecError::Start {
            program: command.program.clone(),
            message,
        };
        // Resolved here, on this machine's PATH: the command's own
        // environment has none unless it declares one.
        let program = atomic_exec::process::resolve(&command.program).map_err(fail)?;
        let env = atomic_exec::process::resolve("env").map_err(fail)?;
        let text = |p: &Path| {
            p.to_str()
                .map(str::to_owned)
                .ok_or_else(|| fail(format!("{} is not UTF-8", p.display())))
        };
        let program = text(&program)?;
        // `env` would read a program path holding `=` as one more variable.
        if program.contains('=') {
            return Err(fail(format!(
                "{program} has `=` in its path, which a confined command cannot run from"
            )));
        }
        let profile = self
            .profile(&command.writable, &command.read_only)
            .and_then(|p| serde_json::to_string(&p).context("encode the sandbox profile"))
            .map_err(|e| fail(format!("{e:#}")))?;
        let cwd = text(&command.cwd)?;
        let mut args = vec![
            "--sandbox-policy-cwd".to_owned(),
            cwd.clone(),
            "--command-cwd".to_owned(),
            cwd,
            "--permission-profile".to_owned(),
            profile,
        ];
        // The helper needs PATH to find bubblewrap, and keeps its registry of
        // the mount points it makes in TMPDIR: the backend's own, which the
        // turns' helpers share.
        let mut helper_env = vec![("TMPDIR".to_owned(), text(tmp)?)];
        if let Ok(path) = std::env::var("PATH") {
            helper_env.push(("PATH".to_owned(), path));
        }
        // With no hosts declared the command has no network at all. With
        // some, its only way out is a proxy that forwards to those hosts:
        // the command finds it in its proxy variables, and the helper, given
        // the same variables, routes the command's network namespace to it.
        // Without network, `env -i` gives the command exactly its own
        // environment. With it, the helper rewrites the proxy variables it is
        // given to the routes it makes inside the command's namespace, so the
        // command keeps those from the helper and loses the helper's others.
        let mut clear = vec!["-i".to_owned()];
        let mut held = None;
        let mut secret = atomic_exec::SecretEnv::default();
        if !command.network.is_empty() {
            // Checked again here, whoever built the command: only atomic's
            // policy decides what a command may reach, and a loopback
            // address only for a test that asked for it.
            let (loopback, hosts): (Vec<String>, Vec<String>) = command
                .network
                .iter()
                .cloned()
                .partition(|h| self.loopback_for_tests.contains(h));
            let hosts = crate::network::check_hosts(&hosts)
                .map_err(|e| fail(format!("its network: {e}")))?;
            let (prepared, lease) = self
                .proxies
                .prepare(&hosts, &loopback, command.env.iter().cloned().collect())
                .map_err(|e| {
                    fail(format!(
                        "start the proxy for the hosts it declares ({}): {e:#}",
                        command.network.join(", ")
                    ))
                })?;
            held = Some(lease as atomic_exec::Held);
            let context = serde_json::to_string(&prepared.sandbox_context)
                .map_err(|e| fail(format!("encode the network context: {e}")))?;
            args.extend(["--managed-network".to_owned(), context]);
            let mut added: Vec<(String, String)> = prepared
                .env
                .into_iter()
                .filter(|(k, v)| !command.env.iter().any(|(ck, cv)| ck == k && cv == v))
                .collect();
            added.sort();
            // The proxy token goes to the helper on a socket, never in its exec
            // environment, which a process of the same user could read
            // through /proc until the helper has made itself non-dumpable.
            let (token, added) = split_off_token(added);
            if token.is_empty() {
                return Err(fail("the proxy gave the command no token".to_owned()));
            }
            secret = atomic_exec::SecretEnv(token);
            // The helper's outer stage already drops the proxy token from
            // its environment before it starts the sandbox, so the command
            // never inherits it (the sandbox suite's token test checks
            // that); unsetting it here too keeps it out should that change.
            clear = [
                "TMPDIR",
                "PATH",
                codex_network_proxy::PROXY_ATTRIBUTION_TOKEN_ENV_KEY,
                atomic_exec::SECRET_ENV_FD,
            ]
            .into_iter()
            .flat_map(|k| ["-u".to_owned(), k.to_owned()])
            .collect();
            helper_env.extend(added);
        }
        args.extend(["--".to_owned(), text(&env)?]);
        args.extend(clear);
        args.extend(command.env.iter().map(|(k, v)| format!("{k}={v}")));
        args.push(program);
        args.extend(command.args.iter().cloned());
        Ok((
            atomic_exec::Command {
                program: text(&self.helper)?,
                args,
                env: helper_env,
                writable: Vec::new(),
                read_only: Vec::new(),
                network: Vec::new(),
                secret_env: secret,
                ..command.clone()
            },
            held,
        ))
    }

    /// Run `true` confined: the helper, bubblewrap and the user namespaces
    /// it needs either work here or the backend refuses to connect.
    fn check(&self, tmp: &Path) -> Result<(), atomic_exec::ExecError> {
        let probe = atomic_exec::Command {
            program: "true".into(),
            cwd: tmp.to_owned(),
            time_limit: std::time::Duration::from_secs(30),
            output_limit: 4096,
            ..Default::default()
        };
        let confined = self.confine(&probe, tmp)?;
        let done =
            atomic_exec::ExecutionBackend::run(&atomic_exec::LocalBackend::default(), &confined);
        let refuse = |why: String| atomic_exec::ExecError::Unconfinable {
            message: format!(
                "exec atoms' commands run under Codex's Linux sandbox, which cannot start here \
                 ({why}). It needs bubblewrap and unprivileged user namespaces; in a container, \
                 allow them (for docker, `--security-opt seccomp=unconfined` and \
                 `--security-opt systempaths=unconfined`, or a profile that permits unshare)"
            ),
        };
        match done {
            Ok(done) if done.success() => Ok(()),
            Ok(done) => {
                let stderr = String::from_utf8_lossy(&done.stderr);
                let why = format!(
                    "a trial command exited with {}: {}",
                    done.code
                        .map_or_else(|| "a signal".to_owned(), |c| format!("code {c}")),
                    stderr.trim()
                );
                // Only a refusal this machine will repeat until it is fixed
                // stops the run; anything else, such as a full disk under the
                // helper's registry of mount points, may pass, and the run
                // waits.
                if sandbox_refused(&stderr) {
                    Err(refuse(why))
                } else {
                    Err(atomic_exec::ExecError::Unavailable {
                        label: "the sandbox check".into(),
                        message: why,
                    })
                }
            }
            // The trial could not even start: no room or no process to
            // spare, which may pass.
            Err(e) => Err(atomic_exec::ExecError::Unavailable {
                label: "the sandbox check".into(),
                message: e.to_string(),
            }),
        }
    }
}

/// Whether a sandbox trial that failed with `stderr` failed because this
/// machine cannot confine commands at all: bubblewrap missing or not
/// runnable, user namespaces or the mounts bubblewrap makes refused, or the
/// helper unable to drop its capabilities or apply its seccomp filter. Each
/// stays so until the machine changes. Anything else is taken to be passing.
pub(crate) fn sandbox_refused(stderr: &str) -> bool {
    const HELPER: [&str; 5] = [
        "bubblewrap is unavailable",
        "failed to exec system bubblewrap",
        "failed to exec bundled bubblewrap",
        "retained effective or permitted capabilities",
        "error applying Linux sandbox restrictions",
    ];
    const BWRAP: [&str; 6] = [
        "namespace",
        "uid map",
        "gid map",
        "Operation not permitted",
        "Permission denied",
        "Can't mount on symlink destination",
    ];
    stderr.lines().any(|line| {
        HELPER.iter().any(|s| line.contains(s))
            || (line.trim_start().starts_with("bwrap:") && BWRAP.iter().any(|s| line.contains(s)))
    })
}

/// A thread's own temporary directory, `<state>/tmp/turn-…`, writable from
/// its commands and named by their `TMPDIR`. Removed when dropped.
struct TurnTemp(PathBuf);

/// The prefix of a turn's temporary directory, followed by the pid of the
/// process that made it.
const TURN_TEMP_PREFIX: &str = "turn-";

impl TurnTemp {
    /// A new temporary directory for one thread: in the state directory's
    /// `tmp/` under [`StateGuard::Deny`], in the system temporary directory
    /// otherwise. Readable by its owner only.
    fn create(guard: &StateGuard) -> anyhow::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let (parent, prefix) = match guard {
            StateGuard::Deny(private) => (private.state_dir.join("tmp"), TURN_TEMP_PREFIX),
            StateGuard::NoProjectState => (std::env::temp_dir(), "atomic-turn-"),
        };
        std::fs::create_dir_all(&parent).with_context(|| format!("create {}", parent.display()))?;
        let dir = parent.join(format!("{prefix}{}-{n}-{nanos}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(&dir)
            .with_context(|| format!("create the turn's temporary directory {}", dir.display()))?;
        Ok(Self(dir))
    }
}

impl Drop for TurnTemp {
    fn drop(&mut self) {
        // What is left behind is swept at the next start (see
        // `sweep_turn_temps`).
        let _ = remove_all(&self.0);
    }
}

/// Remove `dir` and everything in it, first making each directory in it
/// writable and searchable, so a command that left a mode-000 directory
/// behind cannot keep it.
fn remove_all(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    fn open_up(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                open_up(&entry.path());
            }
        }
    }
    #[cfg(unix)]
    open_up(dir);
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Remove the temporary directories that threads of processes no longer
/// running left in the state directory `state_dir` (a process killed before
/// its threads ended leaves them). Holds the state directory's lock on
/// `tmp/` while it sweeps. Returns how many it removed.
pub fn sweep_turn_temps(state_dir: &Path) -> anyhow::Result<usize> {
    let tmp = state_dir.join("tmp");
    std::fs::create_dir_all(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    let lock = std::fs::File::create(tmp.join(".sweep.lock"))
        .with_context(|| format!("open the sweep lock in {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor this function owns.
        #[allow(unsafe_code)]
        let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
        if locked != 0 {
            return Err(std::io::Error::last_os_error()).context("lock the turn directories");
        }
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(&tmp)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name
            .strip_prefix(TURN_TEMP_PREFIX)
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !process_alive(pid) {
            remove_all(&entry.path())
                .with_context(|| format!("remove {}", entry.path().display()))?;
            removed += 1;
        }
    }
    drop(lock);
    Ok(removed)
}

/// Whether a process with this pid is running. One that is but cannot be
/// signalled counts as running.
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return true;
        };
        // SAFETY: signal 0 delivers nothing; it only checks the pid.
        #[allow(unsafe_code)]
        let r = unsafe { libc::kill(pid, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Variables Codex reads that would replace a sign-in's token, or send its
/// refresh or revocation elsewhere.
const OVERRIDES: &[&str] = &[
    codex_login::CODEX_ACCESS_TOKEN_ENV_VAR,
    codex_login::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR,
    codex_login::REVOKE_TOKEN_URL_OVERRIDE_ENV_VAR,
];

/// Remove [`OVERRIDES`] from the environment: nothing in it stands in for,
/// or redirects, a sign-in's tokens. Codex's own `.env` loading never sets a
/// `CODEX_` variable, so none comes back.
pub fn scrub_overrides() {
    scrub(|name| {
        // SAFETY: called at the start of main, while the process has one
        // thread, so no other thread can read the environment concurrently.
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var(name);
        }
    });
}

/// Remove each of [`OVERRIDES`] with `remove`.
fn scrub(mut remove: impl FnMut(&str)) {
    for name in OVERRIDES {
        remove(name);
    }
}

/// Whether this process is Codex's Linux sandbox helper: this executable,
/// run under the helper's name, which [`dispatch`] hands to Codex.
#[cfg(target_os = "linux")]
fn invoked_as_linux_sandbox() -> bool {
    std::env::args_os()
        .next()
        .as_deref()
        .map(Path::new)
        .and_then(Path::file_name)
        .is_some_and(|name| name == "codex-linux-sandbox")
}

/// Join a new, empty session keyring, so the command the helper sandboxes
/// cannot read the keys of the session that started atomic, such as the
/// cached operator credential (see [`crate::credential`]).
///
/// Codex's sandbox gives a command a new user namespace, and with it a new
/// user keyring, but the session keyring is inherited, and the user's own
/// keyring is linked into it. Keys found through a keyring the process holds
/// are readable to it; an anonymous session keyring holds nothing. Where
/// `keyctl` itself is refused, as under a container's default seccomp
/// profile, it is refused to the command too, and there is nothing to leave.
#[cfg(target_os = "linux")]
fn leave_session_keyring() -> std::io::Result<()> {
    const KEYCTL_JOIN_SESSION_KEYRING: libc::c_long = 1;
    // SAFETY: keyctl(KEYCTL_JOIN_SESSION_KEYRING, NULL) reads no memory: a
    // null name asks for a new anonymous keyring.
    #[allow(unsafe_code)]
    let joined = unsafe {
        libc::syscall(
            libc::SYS_keyctl,
            KEYCTL_JOIN_SESSION_KEYRING,
            std::ptr::null::<libc::c_char>(),
        )
    };
    if joined >= 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        // keyctl itself is refused here only if reading the session keyring
        // is refused too; then it is refused to the command as well.
        Some(libc::ENOSYS | libc::EPERM) if !session_keyring_readable() => Ok(()),
        _ => Err(error),
    }
}

/// Whether this process can find its session keyring at all.
#[cfg(target_os = "linux")]
fn session_keyring_readable() -> bool {
    const KEYCTL_GET_KEYRING_ID: libc::c_long = 0;
    const KEY_SPEC_SESSION_KEYRING: libc::c_long = -3;
    // SAFETY: keyctl(KEYCTL_GET_KEYRING_ID, id, create = 0) reads no memory.
    #[allow(unsafe_code)]
    let id = unsafe {
        libc::syscall(
            libc::SYS_keyctl,
            KEYCTL_GET_KEYRING_ID,
            KEY_SPEC_SESSION_KEYRING,
            0 as libc::c_long,
        )
    };
    id >= 0
}

/// Deny `keyctl`, `add_key` and `request_key` to this process and what it
/// runs, with `EPERM`. A key's serial is global and the sandbox maps the
/// user's uid onto itself, so a command that can call `keyctl` can link any
/// keyring the user owns, its own session keyring included, into its own,
/// and read or plant keys there; leaving the session keyring takes away
/// possession only. Installed in the sandbox's inner stage, under
/// `no_new_privs` as Codex's own filter is, before Codex installs that.
#[cfg(target_os = "linux")]
fn deny_keyrings() -> std::io::Result<()> {
    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};
    #[cfg(target_arch = "x86_64")]
    let arch = TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")]
    let arch = TargetArch::aarch64;
    #[cfg(target_arch = "riscv64")]
    let arch = TargetArch::riscv64;
    let rules = [libc::SYS_keyctl, libc::SYS_add_key, libc::SYS_request_key]
        .into_iter()
        .map(|nr| (nr, Vec::new()))
        .collect();
    let other = |e: &dyn std::fmt::Display| std::io::Error::other(e.to_string());
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|e| other(&e))?;
    let program: BpfProgram = filter
        .try_into()
        .map_err(|e: seccompiler::BackendError| other(&e))?;
    // SAFETY: prctl(PR_SET_NO_NEW_PRIVS, 1) takes no pointers.
    #[allow(unsafe_code)]
    let set = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if set != 0 {
        return Err(std::io::Error::last_os_error());
    }
    seccompiler::apply_filter(&program).map_err(|e| other(&e))
}

/// Run one turn on a fresh, ephemeral Codex thread and return the model's
/// final message. The thread runs with read-only permissions and never asks
/// for approval.
///
/// This and the entry points below take a directory, not a workspace, so
/// only the runtime calls them, once its backend has opened one (see
/// [`crate::runtime::run_turn`]).
#[cfg(test)]
pub(crate) async fn one_turn(
    paths: &CodexPaths,
    codex_home: &Path,
    cwd: &Path,
    endpoint: &ModelEndpoint,
    guard: &StateGuard,
    prompt: &str,
) -> anyhow::Result<String> {
    let spec = TurnSpec {
        prompt: prompt.to_owned(),
        ..TurnSpec::default()
    };
    run(paths, codex_home, cwd, endpoint, guard, &spec).await
}

/// Run `spec` as one turn on a fresh, ephemeral thread and return the final
/// message.
#[cfg(test)]
pub(crate) async fn run(
    paths: &CodexPaths,
    codex_home: &Path,
    cwd: &Path,
    endpoint: &ModelEndpoint,
    guard: &StateGuard,
    spec: &TurnSpec,
) -> anyhow::Result<String> {
    converse(paths, codex_home, cwd, endpoint, guard, spec, |_| None)
        .await
        .map(|(answer, _)| answer)
}

/// Run `first` on a fresh, ephemeral thread, then, after each answer, the
/// follow-up turn `next` returns on the same thread, until it returns `None`.
/// Returns the last answer and how many turns ran. Follow-up turns use the
/// thread's instructions and model; their own are ignored.
#[cfg(test)]
pub(crate) async fn converse<F>(
    paths: &CodexPaths,
    codex_home: &Path,
    cwd: &Path,
    endpoint: &ModelEndpoint,
    guard: &StateGuard,
    first: &TurnSpec,
    mut next: F,
) -> anyhow::Result<(String, ThreadReport)>
where
    F: FnMut(&str) -> Option<TurnSpec>,
{
    converse_async(
        paths,
        codex_home,
        cwd,
        endpoint,
        &Metering::none(),
        guard,
        first,
        |answer| std::future::ready(next(&answer)),
    )
    .await
}

/// Conversation with asynchronous checks between turns. Each turn is
/// admitted by `meter` before it is submitted and settled once after it.
#[allow(clippy::too_many_arguments)] // each is one thing every thread needs
pub(crate) async fn converse_async<F, Fut>(
    paths: &CodexPaths,
    codex_home: &Path,
    cwd: &Path,
    endpoint: &ModelEndpoint,
    meter: &Metering,
    guard: &StateGuard,
    first: &TurnSpec,
    mut next: F,
) -> anyhow::Result<(String, ThreadReport)>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Option<TurnSpec>>,
{
    let spec = first;
    let mut endpoint = endpoint.clone();
    if let Some(m) = &spec.model {
        endpoint.model = m.clone();
    }
    let mut config = new_config(&endpoint, codex_home, cwd, paths.0.clone()).await?;
    config.developer_instructions = spec.developer_instructions.clone();
    config.permissions = Permissions::from_approval_and_profile(
        Constrained::allow_any(AskForApproval::Never),
        Constrained::allow_any(permission_profile(
            spec.sandbox,
            guard,
            codex_home,
            Some(cwd),
            &match spec.sandbox {
                crate::typed::Sandbox::ReadOnly => Vec::new(),
                crate::typed::Sandbox::WorkspaceWrite => {
                    let mut roots = spec.writable_roots.clone();
                    roots.push(cwd.to_owned());
                    roots
                }
            },
        )?),
    )?;
    let mut writable_roots = spec.writable_roots.clone();
    writable_roots.push(cwd.to_owned());
    refuse_uncovered_store(spec.sandbox, guard, &writable_roots)?;
    writable_roots.pop();
    // Under workspace-write the system temporary directories are not
    // writable (see `permission_profile`): the thread gets its own, removed
    // when the thread ends.
    // A workspace that names a TMPDIR (the backend's shared one, see
    // `crate::runtime`) keeps it: Codex's sandbox keeps its registry of
    // protected mount points there, and turns that share a writable root
    // must share it.
    let temp = match spec.sandbox {
        crate::typed::Sandbox::WorkspaceWrite if !spec.env.contains_key("TMPDIR") => {
            Some(TurnTemp::create(guard)?)
        }
        _ => None,
    };
    let mut env = spec.env.clone();
    if let Some(temp) = &temp {
        writable_roots.push(temp.0.clone());
        env.entry("TMPDIR".into())
            .or_insert_with(|| temp.0.display().to_string());
    }
    if !writable_roots.is_empty() {
        let mut roots = vec![config.cwd.clone()];
        for root in &writable_roots {
            let root = AbsolutePathBuf::from_absolute_path(root)
                .with_context(|| format!("resolve the writable root {}", root.display()))?;
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        config.permissions.set_workspace_roots(roots.clone());
        config.workspace_roots = roots;
        config.workspace_roots_explicit = true;
    }
    config.permissions.shell_environment_policy = command_environment(&env);
    // No login shell, and no snapshot taken from one: a command never runs
    // the developer's or the system's shell startup files.
    config.permissions.allow_login_shell = false;
    config
        .features
        .set_enabled(Feature::ShellSnapshot, false)
        .context("turn shell snapshots off")?;
    let state_db = init_state_db(&config).await;

    if let Some(sign_in) = &endpoint.sign_in {
        crate::signin::protected()?;
        // Codex's login restrictions hold for the sign-in as for its own.
        sign_in.allowed_by(&config.auth_config())?;
    }
    let auth_manager = match &endpoint.sign_in {
        // The developer's own sign-in, kept apart from every project (see
        // `crate::signin`); never a key from the environment. Codex loads it
        // only from the workspaces its restrictions allow.
        Some(sign_in) => Arc::new(
            AuthManager::new(
                sign_in.auth_home().to_path_buf(),
                /*enable_codex_api_key_env*/ false,
                sign_in.store(),
                config.auth_config().effective_chatgpt_workspaces(),
                Some(config.chatgpt_base_url.clone()),
                sign_in.keyring_kind(),
                config.auth_route_config(),
            )
            .await,
        ),
        None => {
            AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ false).await?
        }
    };
    if endpoint.sign_in.is_some() && auth_manager.auth().await.is_none() {
        bail!(
            "no one is signed in with ChatGPT: sign in with `atomic model login`, or configure a \
             model endpoint"
        );
    }
    let local_runtime_paths = ExecServerRuntimeOptions::from_optional_paths(
        config.codex_self_exe.clone(),
        config.codex_linux_sandbox_exe.clone(),
    )?;
    let thread_store = ThreadStoreBuilder::from_config(&config, state_db.clone()).build();
    let environment_manager = Arc::new(local_environment(local_runtime_paths).await?);
    let installation_id = resolve_installation_id(&config.codex_home).await?;
    let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
        config.codex_home.clone(),
    ));
    // No extensions: an atom's tools are the ones its file declares.
    let extensions = ExtensionRegistryBuilder::<Config>::new();
    let startup = codex_core_api::build_startup_models(
        &config,
        Arc::clone(&auth_manager),
        installation_id,
        Arc::new(ModelCatalogSettings::disk(config.codex_home.to_path_buf())),
    );
    let thread_manager = ThreadManager::new(
        &config,
        startup,
        CodexAppsToolsCache::default(),
        SessionSource::Exec,
        environment_manager,
        Arc::new(extensions.build()),
        user_instructions_provider,
        /*analytics_events_client*/ None,
        passthrough_image_store(),
        Arc::clone(&thread_store),
        local_agent_graph_store_from_state_db(state_db.as_ref()),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );

    let NewThread {
        thread_id, thread, ..
    } = thread_manager
        .start_thread(StartThreadOptions::new(config))
        .await
        .context("start Codex thread")?;

    let mut current = spec.clone();
    let mut report = ThreadReport {
        thread_id: thread_id.to_string(),
        ..ThreadReport::default()
    };
    // The thread's running totals as last reported: each turn's usage is
    // what they grew by, so each reported token is charged once.
    let mut seen = (0_i64, 0_i64);
    let outcome = loop {
        // Admitted before the turn is submitted, settled once after it.
        let (ticket, limit) = match meter
            .admit(Call {
                index: report.turns,
                model: endpoint.model.clone(),
                input_bound: None,
                turn: true,
                work: false,
            })
            .await
        {
            Ok(admitted) => admitted,
            Err(e) => break Err(meter_error(e)),
        };
        report.turns += 1;
        // One span per turn, which may make several requests; it ends before
        // the turn is settled.
        let span = crate::spans::hold(crate::usage::call_span(
            &endpoint.model,
            crate::usage::provider_name(&endpoint.base_url),
        ));
        let bounded_by = ticket.as_ref().and_then(|t| t.bounded_by.clone());
        // A turn is cut off when its reserved time runs out; a failed or cut
        // off turn still reports the usage seen before it stopped.
        let mut recorder = ProgressLog {
            meter,
            ticket: ticket.as_ref(),
            seen,
            recorded: seen,
            records: 0,
            warnings: 0,
            said: None,
        };
        let (turn, progress) = span
            .run(run_turn(&thread, &current, limit.time, &mut recorder))
            .await;
        if let (Err(e), Some(span)) = (&turn, span.span()) {
            crate::usage::record_error(&span, e);
        }
        let totals = progress.totals(seen);
        // Codex repeats the last totals in every token count, so a turn
        // whose requests reported nothing shows them unchanged; a request
        // that reports usage always grows them. A turn with nothing reported
        // spent something unknown and stays charged at its reservation.
        let fresh = totals.filter(|(input, output, _)| *input > seen.0 || *output > seen.1);
        // A turn stopped with a provider request in flight, which has
        // reported nothing yet, spent more than the thread grew by: it is
        // charged the growth and one more request's bound (see
        // [`Progress::in_flight`]).
        let cut_off = matches!(&turn, Err(e) if e.downcast_ref::<CutOff>().is_some());
        let in_flight = progress.in_flight(cut_off, turn.is_err());
        let outcome = match fresh {
            Some((input, output, total)) => {
                let grown = Usage {
                    input_tokens: (input - seen.0).max(0),
                    output_tokens: (output - seen.1).max(0),
                };
                seen = (input, output);
                if let Some(span) = span.span() {
                    crate::usage::record_usage(&span, &grown);
                }
                report.input_tokens = input;
                report.output_tokens = output;
                report.total_tokens = total;
                if in_flight {
                    Outcome::Partial {
                        reported: grown,
                        last: progress.last.map(|(input_tokens, output_tokens)| Usage {
                            input_tokens,
                            output_tokens,
                        }),
                    }
                } else {
                    Outcome::Reported(grown)
                }
            }
            None => Outcome::Unknown,
        };
        span.end();
        if let Err(e) = meter.settle(ticket, outcome).await {
            break Err(meter_error(e));
        }
        match turn {
            // A turn cut off at its reserved time used up the active time
            // its atom had left: the run parks, as at any other cap.
            Err(e) if e.downcast_ref::<CutOff>().is_some() => {
                break Err(meter_error(MeterError::Exhausted(Exhausted(format!(
                    "{e}: all the active time the allowance {} had left",
                    bounded_by.as_deref().unwrap_or("of its atom")
                )))));
            }
            Err(e) => break Err(e),
            Ok(answer) => match next(answer.clone()).await {
                Some(follow_up) => current = follow_up,
                None => break Ok((answer, report.clone())),
            },
        }
    };
    let shutdown_result = thread.shutdown_and_wait().await;
    let _ = thread_manager.remove_thread(&thread_id).await;

    let outcome = outcome?;
    shutdown_result.context("shut down Codex thread")?;
    Ok(outcome)
}

async fn new_config(
    endpoint: &ModelEndpoint,
    codex_home: &Path,
    cwd: &Path,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<Config> {
    let codex_home =
        AbsolutePathBuf::from_absolute_path(codex_home).context("resolve the Codex home")?;
    let cwd = AbsolutePathBuf::from_absolute_path(cwd).context("resolve the working directory")?;
    let model = Some(endpoint.model.clone());
    // Resolve auth through the shared bootstrap path so cargo-run builds retain the configured
    // secure store instead of the full Config loader's local-development file-store override.
    let bootstrap = load_config_toml_with_layer_stack(
        codex_home.as_path(),
        // No project layer: atom threads never load the project's own Codex
        // configuration.
        None,
        Vec::new(),
        ConfigLoadOptions::default(),
    )
    .await
    .context("load authentication configuration")?;
    let auth_config = bootstrap_auth_config(codex_home.as_path(), &bootstrap)?;
    let requirements = bootstrap.config_layer_stack.requirements();
    let requirements_toml = bootstrap.config_layer_stack.requirements_toml();
    // Preserve auth restrictions for every manager rebuilt from Config without importing
    // execution settings, plugins, or hooks from the bootstrap configuration.
    let config_layer_stack = ConfigLayerStack::new(
        Vec::new(),
        ConfigRequirements {
            allowed_login_methods: requirements.allowed_login_methods.clone(),
            allowed_chatgpt_workspaces: requirements.allowed_chatgpt_workspaces.clone(),
            ..Default::default()
        },
        ConfigRequirementsToml {
            allowed_login_methods: requirements_toml.allowed_login_methods.clone(),
            allowed_chatgpt_workspaces: requirements_toml.allowed_chatgpt_workspaces.clone(),
            ..Default::default()
        },
    )?;
    let mcp_oauth_credentials_store_mode = bootstrap
        .config_toml
        .mcp_oauth_credentials_store
        .unwrap_or_default();
    let respect_system_proxy = resolve_bootstrap_respect_system_proxy(
        &bootstrap.config_toml,
        bootstrap
            .config_layer_stack
            .requirements()
            .feature_requirements
            .as_ref(),
    )?;
    let mut model_providers = built_in_model_providers(/*openai_base_url*/ None);
    let mut model_provider = model_providers
        .get(OPENAI_PROVIDER_ID)
        .context("OpenAI model provider should be available")?
        .clone();
    let model_provider_id = match &endpoint.sign_in {
        // The built-in OpenAI provider as it is, with the sign-in's tokens:
        // ChatGPT's Codex backend.
        Some(sign_in) => {
            // For tests: the backend's Codex route.
            model_provider.base_url = sign_in.backend().map(|b| format!("{b}/codex"));
            model_provider.supports_websockets = false;
            // The organization and project headers are for OpenAI's API
            // keys, never for a ChatGPT sign-in.
            model_provider.env_http_headers = None;
            OPENAI_PROVIDER_ID.to_string()
        }
        // An OpenAI-compatible Responses endpoint: the built-in OpenAI
        // provider, pointed at another base URL, with no OpenAI sign-in and
        // no websockets.
        None => {
            model_provider.name = "atomic endpoint".to_string();
            model_provider.base_url = Some(endpoint.base_url.clone());
            model_provider.env_key = endpoint.api_key_env.clone();
            model_provider.requires_openai_auth = false;
            model_provider.supports_websockets = false;
            env_headers_for(&endpoint.base_url, &mut model_provider.env_http_headers);
            ENDPOINT_PROVIDER_ID.to_string()
        }
    };
    model_providers.insert(model_provider_id.clone(), model_provider.clone());

    let mut config = Config {
        application_network_policy: Default::default(),
        application_auth_route_config: Some(auth_config.auth_route_config.clone()),
        config_layer_stack,
        plugins: Default::default(),
        startup_warnings: Vec::new(),
        bypass_hook_trust: false,
        model,
        daybreak_enabled: false,
        service_tier: None,
        review_model: None,
        model_context_window: None,
        model_auto_compact_token_limit: None,
        model_auto_compact_token_limit_scope: AutoCompactTokenLimitScope::Total,
        model_post_turn_compact_threshold_percent: 0,
        async_compaction_threshold_percent: 65,
        model_provider_id,
        model_provider,
        personality: None,
        permissions: Permissions::from_approval_and_profile(
            Constrained::allow_any(AskForApproval::Never),
            Constrained::allow_any(PermissionProfile::read_only()),
        )?,
        explicit_permission_profile_mode: false,
        custom_permission_profiles: Vec::new(),
        approvals_reviewer: ApprovalsReviewer::User,
        requested_approvals_reviewer: None,
        enforce_residency: Constrained::allow_any(/*initial_value*/ None),
        hide_agent_reasoning: false,
        show_raw_agent_reasoning: false,
        base_instructions: None,
        base_instructions_provenance: None,
        developer_instructions: None,
        guardian_policy_revision: 0,
        guardian_policy_config: None,
        guardian_extra_policy: None,
        guardian_policy_template: None,
        guardian_transcript_mode: Default::default(),
        guardian_sync_elastic_budget: None,
        guardian_conversation_history_prompt: None,
        guardian_conversation_history_max_output_tokens: None,
        guardian_circuit_break_action: Default::default(),
        guardian_review_timeout: Some(std::time::Duration::from_secs(90)),
        include_permissions_instructions: false,
        include_apps_instructions: false,
        include_collaboration_mode_instructions: false,
        include_skill_instructions: false,
        skill_max_context_tokens: None,
        cloud_skill_enabled: false,
        orchestrator_mcp_enabled: false,
        include_environment_context: false,
        include_environment_context_time: true,
        compact_prompt: None,
        notify: None,
        tui_notifications: TuiNotificationSettings::default(),
        animations: true,
        tui_effects: Default::default(),
        tui_rendering: Default::default(),
        show_tooltips: true,
        tui_show_server_version_notice: true,
        tui_auto_recap: true,
        model_availability_nux: ModelAvailabilityNuxConfig::default(),
        tui_fullscreen_transcript: false,
        tui_collapse_turn_details: false,
        tui_sidebar_width_percent: None,
        tui_mouse_scroll_speed: None,
        tui_copy_on_select: Default::default(),
        tui_copy_on_select_clear_selection: false,
        tui_right_click_paste: Default::default(),
        tui_alternate_screen: AltScreenMode::Auto,
        tui_status_line: None,
        tui_status_line_use_colors: true,
        tui_terminal_title: None,
        tui_theme: None,
        tui_raw_output_mode: false,
        tui_pet: None,
        tui_pet_anchor: TuiPetAnchor::Composer,
        terminal_resize_reflow: TerminalResizeReflowConfig::default(),
        tui_keymap: TuiKeymap::default(),
        tui_quick_dials: BTreeMap::new(),
        tui_session_picker_view: SessionPickerViewMode::Dense,
        tui_agents_overview_grouping: Default::default(),
        tui_resume_cwd: None,
        tui_vim_mode_default: false,
        tui_question_esc_back: true,
        cwd: cwd.clone(),
        workspace_roots: vec![cwd],
        workspace_roots_explicit: false,
        cli_auth_credentials_store_mode: auth_config.auth_credentials_store_mode,
        mcp_servers: Constrained::allow_any(HashMap::new()),
        mcp_enterprise_managed_auth: None,
        non_prefixed_mcp_tool_servers: None,
        mcp_oauth_credentials_store_mode,
        mcp_oauth_callback_port: None,
        mcp_oauth_callback_url: None,
        mcp_optional_startup_grace: std::time::Duration::from_secs(1),
        model_providers,
        project_doc_max_bytes: 32 * 1024,
        project_doc_fallback_filenames: Vec::new(),
        tool_output_token_limit: None,
        agents_enabled: true,
        agent_max_threads: Some(6),
        agent_default_subagent_model: None,
        agent_default_subagent_reasoning_effort: None,
        agent_interrupt_message_enabled: false,
        agent_max_depth: 1,
        agent_roles: BTreeMap::new(),
        memories: MemoriesConfig::default(),
        sqlite: SqliteConfig::from_sqlite_home_with_nfs_enforcement(
            codex_home.clone(),
            /*enforce_delete_on_nfs*/ false,
        ),
        log_dir: codex_home.join("log").to_path_buf(),
        codex_home,
        history: History::default(),
        ephemeral: true,
        allow_persistent_dynamic_tool_updates: false,
        extra_config: None,
        file_opener: UriBasedFileOpener::VsCode,
        codex_self_exe: arg0_paths.codex_self_exe,
        codex_linux_sandbox_exe: arg0_paths.codex_linux_sandbox_exe,
        main_execve_wrapper_exe: None,
        zsh_path: None,
        model_reasoning_effort: None,
        plan_mode_reasoning_effort: None,
        model_reasoning_summary: None,
        model_catalog: None,
        model_verbosity: None,
        chatgpt_base_url: auth_config
            .chatgpt_base_url
            .clone()
            .unwrap_or_else(|| "https://chatgpt.com/backend-api/".to_string()),
        chatgpt_base_url_is_explicit: auth_config.chatgpt_base_url.is_some(),
        respect_system_proxy,
        apps_mcp_product_sku: None,
        responses_api_metadata: BTreeMap::new(),
        realtime_audio: RealtimeAudioConfig::default(),
        experimental_realtime_ws_base_url: None,
        experimental_realtime_webrtc_call_base_url: None,
        experimental_realtime_ws_model: None,
        realtime: RealtimeConfig::default(),
        experimental_realtime_ws_backend_prompt: None,
        experimental_realtime_ws_startup_context: None,
        experimental_realtime_start_instructions: None,
        experimental_thread_store: ThreadStoreConfig::Local,
        forced_chatgpt_workspace_id: auth_config.forced_chatgpt_workspace_id.clone(),
        forced_login_method: auth_config.forced_login_method,
        web_search_mode: Constrained::allow_any(WebSearchMode::Disabled),
        web_search_config: None,
        experimental_request_user_input_enabled: true,
        update_plan_enabled: true,
        tool_registry: Default::default(),
        code_mode: Default::default(),
        background_terminal_max_timeout: 300_000,
        thread_unload_delay: std::time::Duration::from_secs(/*secs*/ 60),
        ghost_snapshot: GhostSnapshotConfig::default(),
        multi_agent_v2: MultiAgentV2Config::default(),
        max_goal_token_budget: None,
        token_budget: None,
        token_budget_startup_config: None,
        rollout_budget: None,
        current_time_reminder: None,
        sleep_tool_mode: Default::default(),
        runtime_auth_mode: Default::default(),
        features: Default::default(),
        prefer_mxc: false,

        runtime_feature_defaults: Default::default(),
        suppress_unstable_features_warning: false,
        active_project: ProjectConfig { trust_level: None },
        notices: Notice::default(),
        check_for_update_on_startup: false,
        disable_paste_burst: false,
        analytics_enabled: Some(false),
        feedback_enabled: false,
        tool_suggest: ToolSuggestConfig::default(),
        otel: OtelConfig::default(),
    };
    if let Some(backend) = endpoint.sign_in.as_ref().and_then(|s| s.backend()) {
        // Every ChatGPT route, not only the model's: nothing reaches the
        // real backend from a test.
        config.chatgpt_base_url = format!("{backend}/");
        config.chatgpt_base_url_is_explicit = true;
    }
    config
        .features
        .set(Features::with_defaults())
        .context("configure default features")?;
    // Nothing that reaches a ChatGPT account's connectors, plugins, apps or
    // hosted tools, or acts outside the sandbox: with a sign-in those would
    // carry its token, and read-only connector tools need no approval. An
    // atom's tools are the ones its file declares.
    for feature in ATOMIC_OFF {
        config
            .features
            .set_enabled(*feature, false)
            .with_context(|| format!("turn off Codex feature {feature:?}"))?;
    }
    // AuthManager and MCP both derive their credential backend from Config.
    config.features.set_enabled(
        Feature::SecretAuthStorage,
        auth_config.keyring_backend_kind == AuthKeyringBackendKind::Secrets,
    )?;
    Ok(config)
}

/// Codex's login settings for a sign-in kept in `codex_home`, as a
/// thread's configuration resolves them: how it reaches the network, and
/// the login restrictions of the machine and of `codex_home`'s own
/// configuration.
pub(crate) async fn auth_policy(codex_home: &Path) -> anyhow::Result<codex_login::AuthConfig> {
    let endpoint = ModelEndpoint {
        base_url: "http://127.0.0.1:9/unused".into(),
        model: "unused".into(),
        api_key_env: None,
        sign_in: None,
    };
    let config = new_config(
        &endpoint,
        codex_home,
        codex_home,
        Arg0DispatchPaths::default(),
    )
    .await?;
    Ok(config.auth_config())
}

/// Codex features atomic's threads never have: ChatGPT apps and connectors
/// (the codex_apps MCP server), plugins and their suggestions, hosted image
/// generation, browser and computer use, realtime and in-app surfaces.
const ATOMIC_OFF: &[Feature] = &[
    Feature::Apps,
    Feature::EnableMcpApps,
    Feature::CodexAppsMcp20260728,
    Feature::AppsEarlyResourceDiscovery,
    Feature::Plugins,
    Feature::RemotePlugin,
    Feature::RecommendedPlugins,
    Feature::PluginSharing,
    Feature::PluginDiscoveryPrefetch,
    Feature::PluginHooks,
    Feature::ToolSuggest,
    Feature::SkillMcpDependencyInstall,
    Feature::ImageGeneration,
    Feature::ImageGenerationAnimation,
    Feature::InAppBrowser,
    Feature::BrowserAnnotationApi,
    Feature::BrowserUse,
    Feature::BrowserUseFullCdpAccess,
    Feature::BrowserUseExternal,
    Feature::ComputerUse,
    Feature::InAppChat,
    Feature::InAppDictation,
    Feature::InAppVoice,
    Feature::InAppLocalAutomation,
    Feature::InAppUpdates,
    Feature::RealtimeConversation,
    Feature::RemoteControl,
    Feature::WebSearchRequest,
    Feature::WebSearchCached,
    Feature::StandaloneWebSearch,
];

/// OpenAI's API, the one endpoint the OpenAI provider's organization and
/// project headers are for.
const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Whether `base_url` is exactly OpenAI's API.
fn is_openai_default(base_url: &str) -> bool {
    base_url.trim_end_matches('/') == OPENAI_DEFAULT_BASE_URL
}

/// The OpenAI provider sends `OPENAI_ORGANIZATION` and `OPENAI_PROJECT` as
/// headers: kept for OpenAI's own API only, never sent to another endpoint.
fn env_headers_for(
    base_url: &str,
    headers: &mut Option<std::collections::HashMap<String, String>>,
) {
    if !is_openai_default(base_url) {
        *headers = None;
    }
}

/// Variables an atom's shell never sees, whatever else it inherits: atomic's
/// and Codex's own settings, model and cloud credentials, and database
/// connections. Matched without regard to case.
pub const SHELL_EXCLUDES: &[&str] = &[
    "ATOMIC_*",
    "OPENAI_*",
    "CODEX_*",
    "DATABASE_URL",
    "PG*",
    "AWS_*",
    "*_API_KEY",
];

/// The home directory an atom's shell is given: one that does not exist.
/// Codex runs commands in the person's own shell, which reads startup files
/// from the home directory even when it is not a login shell (bash's
/// `.bashrc` when its input is a socket, zsh's `.zshenv` always), and those
/// can export anything.
pub const SHELL_HOME: &str = "/nonexistent";

/// How long a turn waits for the thread to finish settling the previous one.
const NOT_IDLE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

/// Run one turn; return its final message and the thread's token totals
/// (input, output, total) as last reported.
/// A turn cut off when its reserved time ran out.
#[derive(Debug, thiserror::Error)]
#[error("the turn was cut off after {} ms, its reserved time", .0.as_millis())]
struct CutOff(std::time::Duration);

/// How long an interrupted turn has to wind down.
const INTERRUPT_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

/// Where a turn got to, as its events show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Progress {
    /// The thread's running token totals (input, output, total), as last
    /// reported.
    usage: Option<(i64, i64, i64)>,
    /// What this turn's completed responses reported, input and output,
    /// summed: Codex sends a response's totals only after its tools drain,
    /// so a turn stopped before then has them only here.
    responses: (i64, i64),
    /// What the last completed response reported, input and output: the
    /// size of the turn's requests at its end.
    last: Option<(i64, i64)>,
    /// Whether a provider response has begun streaming and not completed.
    streaming: bool,
    /// Whether the turn completed.
    completed: bool,
}

impl Progress {
    /// Follow one event of the turn.
    fn observe(&mut self, msg: &EventMsg) {
        match msg {
            EventMsg::TokenCount(count) => {
                // Sent after every response, broken ones too, repeating the
                // totals when nothing new was reported.
                if let Some(info) = &count.info {
                    let t = &info.total_token_usage;
                    self.usage = Some((t.input_tokens, t.output_tokens, t.total_tokens));
                }
            }
            // A response completed, with its own usage.
            EventMsg::RawResponseCompleted(done) => {
                self.streaming = false;
                if let Some(u) = &done.token_usage {
                    self.responses.0 += u.input_tokens.max(0);
                    self.responses.1 += u.output_tokens.max(0);
                    self.last = Some((u.input_tokens.max(0), u.output_tokens.max(0)));
                }
            }
            EventMsg::AgentMessageContentDelta(_)
            | EventMsg::ReasoningContentDelta(_)
            | EventMsg::ReasoningRawContentDelta(_)
            | EventMsg::PlanDelta(_)
            | EventMsg::StreamError(_) => self.streaming = true,
            EventMsg::TurnComplete(_) => self.completed = true,
            _ => {}
        }
    }

    /// The thread's input and output totals, the larger of what its token
    /// counts said and what the turn's responses reported on top of `seen`,
    /// the totals before the turn.
    fn totals(&self, seen: (i64, i64)) -> Option<(i64, i64, i64)> {
        let from_responses = (seen.0 + self.responses.0, seen.1 + self.responses.1);
        match self.usage {
            Some((input, output, _)) => {
                let (input, output) = (input.max(from_responses.0), output.max(from_responses.1));
                Some((input, output, input + output))
            }
            None if self.responses != (0, 0) => Some((
                from_responses.0,
                from_responses.1,
                from_responses.0 + from_responses.1,
            )),
            None => None,
        }
    }

    /// Whether the turn stopped with a provider request in flight: one sent
    /// whose usage is not in the totals. A turn cut off at its time is taken
    /// to be waiting on one unless it had completed: a tool that yields to
    /// the model while it runs, as a unified exec session does, leaves no
    /// sign of the requests made meanwhile, so a turn cut off while a tool
    /// ran may be charged one request too many, never one too few. A turn
    /// that failed was, only if a response had begun streaming and broke
    /// off, not if the provider refused the request outright.
    fn in_flight(&self, cut_off: bool, failed: bool) -> bool {
        if self.completed {
            return false;
        }
        cut_off || (failed && self.streaming)
    }
}

/// Where a running turn sends a line it has to say: production gives the
/// recorder none, and the line goes to the log; a test gives it one and
/// reads the line back.
type Said = Arc<dyn Fn(&str) + Send + Sync>;

/// Where a running turn's reported usage is recorded as it grows, so that a
/// turn whose process is stopped or killed before it settles stays charged
/// at least what it reported (see [`crate::usage::Meter::progress`]).
struct ProgressLog<'a> {
    meter: &'a Metering,
    ticket: Option<&'a Ticket>,
    /// The thread's totals before the turn.
    seen: (i64, i64),
    /// The totals as last recorded.
    recorded: (i64, i64),
    /// How many times it recorded.
    records: u32,
    /// How many times a failed record was said to, which is at most once a
    /// turn. Counted rather than flagged, so that a second one trying to
    /// follow the first is visible to a test instead of merely unseen.
    warnings: u32,
    /// Where what it says goes (see [`Said`]).
    said: Option<Said>,
}

/// How long a turn waits on one record of its progress before it goes on
/// without it: a store held by another writer must not hold up the turn.
const PROGRESS_PATIENCE: std::time::Duration = std::time::Duration::from_secs(1);

impl ProgressLog<'_> {
    /// Record `progress` if the turn reported more since it last recorded:
    /// what it reported, and one request in flight, since it may be making
    /// one. A record that fails, or takes longer than [`PROGRESS_PATIENCE`],
    /// leaves the turn charged as it was, at its reservation or its last
    /// record, is retried at the next token event, and is said once a turn.
    async fn note(&mut self, progress: &Progress) {
        let Some((input, output, _)) = progress.totals(self.seen) else {
            return;
        };
        if input <= self.recorded.0 && output <= self.recorded.1 {
            return;
        }
        let outcome = Outcome::Partial {
            reported: Usage {
                input_tokens: (input - self.seen.0).max(0),
                output_tokens: (output - self.seen.1).max(0),
            },
            last: progress.last.map(|(input_tokens, output_tokens)| Usage {
                input_tokens,
                output_tokens,
            }),
        };
        let recorded =
            tokio::time::timeout(PROGRESS_PATIENCE, self.meter.progress(self.ticket, outcome))
                .await;
        match recorded {
            Ok(Ok(())) => {
                self.recorded = (input, output);
                self.records += 1;
                if crate::failpoint::armed()
                    && let Some(ticket) = self.ticket
                {
                    // A crash here leaves the turn charged what it recorded.
                    crate::failpoint::failpoint(&format!(
                        "usage:after-progress:{}:{}",
                        ticket.request, self.records
                    ));
                }
            }
            failed if self.warnings == 0 => {
                self.warnings += 1;
                let why = match failed {
                    Ok(Err(e)) => e.to_string(),
                    _ => format!("the store did not answer within {PROGRESS_PATIENCE:?}"),
                };
                let (run, request) = match self.ticket {
                    Some(t) => (run_of_request(&t.request), t.request.as_str()),
                    None => ("?", "?"),
                };
                let line = progress_warning(run, request, &why, self.records > 0);
                match &self.said {
                    Some(said) => said(&line),
                    None => crate::log_line!("{line}"),
                }
            }
            _ => {}
        }
    }
}

/// The run a meter's request id names. The ledger meter in the `atomic`
/// binary makes each one as `<run>:<step>:<call>`, so the run is what is
/// left after two fields are cut off the end: a run's id may itself hold
/// colons. An id of any other shape names no run and is said as it is.
fn run_of_request(request: &str) -> &str {
    request.rsplitn(3, ':').nth(2).unwrap_or(request)
}

/// What a turn says when a record of its progress fails. `recorded` is
/// whether any record of it was ever written: with none, a stop before the
/// turn settles leaves it charged what its admission reserved, and only
/// with one is there a last record to be charged at (see
/// [`ProgressLog::note`]).
fn progress_warning(run: &str, request: &str, why: &str, recorded: bool) -> String {
    let charged = if recorded {
        "charged at its last record"
    } else {
        "charged at the whole bound its request reserved"
    };
    format!(
        "run {run}: a Codex turn's usage so far was not recorded for request \
         {request} ({why}); until it is, a stop before the turn settles leaves \
         it {charged}"
    )
}

/// Run one turn, cut off after `time` if one is given, recording its
/// reported usage through `recorder` as it grows. Returns how it ended and
/// where it got to, the thread's running totals as last reported among it,
/// whether or not it succeeded.
async fn run_turn(
    thread: &CodexThread,
    spec: &TurnSpec,
    time: Option<std::time::Duration>,
    recorder: &mut ProgressLog<'_>,
) -> (anyhow::Result<String>, Progress) {
    let mut progress = Progress::default();
    let ended = match time {
        None => turn(thread, spec, &mut progress, recorder).await,
        Some(time) => {
            match tokio::time::timeout(time, turn(thread, spec, &mut progress, recorder)).await {
                Ok(ended) => ended,
                Err(_) => {
                    interrupt(thread, &mut progress).await;
                    Err(anyhow::Error::new(CutOff(time)))
                }
            }
        }
    };
    (ended, progress)
}

/// Interrupt the running turn and wait, briefly, for it to stop, keeping
/// any usage it reports on the way. A turn that completed before the
/// interrupt took says so; one aborted keeps where it was when stopped.
async fn interrupt(thread: &CodexThread, progress: &mut Progress) {
    if thread.submit(Op::Interrupt).await.is_err() {
        return;
    }
    let _ = tokio::time::timeout(INTERRUPT_PATIENCE, async {
        while let Ok(event) = thread.next_event().await {
            match &event.msg {
                EventMsg::TokenCount(_)
                | EventMsg::RawResponseCompleted(_)
                | EventMsg::TurnComplete(_) => progress.observe(&event.msg),
                _ => {}
            }
            if matches!(
                event.msg,
                EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_)
            ) {
                break;
            }
        }
    })
    .await;
}

async fn turn(
    thread: &CodexThread,
    spec: &TurnSpec,
    progress: &mut Progress,
    recorder: &mut ProgressLog<'_>,
) -> anyhow::Result<String> {
    // Codex sends TurnComplete before it marks the thread idle, so a follow-up
    // turn submitted straight after one can be declined as NotIdle. That
    // window is short: retry for a bounded time, and only for that reason.
    let until = std::time::Instant::now() + NOT_IDLE_PATIENCE;
    loop {
        let mut request = TurnInputRequest::user_input(vec![UserInput::Text {
            text: spec.prompt.clone(),
            text_elements: Vec::new(),
        }]);
        request.start.final_output_json_schema = spec.output_schema.clone();
        match thread
            .start_turn_if_idle(request)
            .await
            .context("submit user input")?
        {
            StartIfIdleSubmission::Started { .. } => break,
            StartIfIdleSubmission::NotSubmitted {
                reason: NotSubmittedReason::NotIdle,
            } if std::time::Instant::now() < until => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            StartIfIdleSubmission::NotSubmitted { reason } => {
                bail!("turn input was not submitted: {reason:?}")
            }
        }
    }

    loop {
        let event = thread.next_event().await.context("read Codex event")?;
        progress.observe(&event.msg);
        if matches!(
            event.msg,
            EventMsg::TokenCount(_) | EventMsg::RawResponseCompleted(_)
        ) {
            recorder.note(progress).await;
        }
        match event.msg {
            EventMsg::TurnComplete(done) => {
                return done
                    .last_agent_message
                    .context("the turn completed without a message");
            }
            // A provider's error can echo what it was sent.
            EventMsg::Error(event) => bail!("{}", crate::redact::excerpt(&event.message, 4096)),
            EventMsg::TurnAborted(_) => bail!("turn aborted"),
            EventMsg::ExecApprovalRequest(_) => bail!("turn requested exec approval"),
            EventMsg::ApplyPatchApprovalRequest(_) => bail!("turn requested patch approval"),
            EventMsg::RequestPermissions(_) => bail!("turn requested permissions"),
            EventMsg::RequestUserInput(_) => bail!("turn requested user input"),
            EventMsg::DynamicToolCallRequest(_) => bail!("turn requested a dynamic tool call"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeModel, Reply};

    #[cfg(unix)]
    #[test]
    fn the_file_behind_a_link_in_a_denied_directory_is_denied_too() {
        let dir = scratch("linked-deny");
        let config = dir.join("config/atomic");
        std::fs::create_dir_all(&config).expect("config dir");
        std::fs::create_dir_all(dir.join("kept")).expect("kept");
        std::fs::create_dir_all(dir.join("codex")).expect("codex home");
        std::fs::write(dir.join("kept/config.yaml"), "x").expect("kept file");
        let _ = std::fs::remove_file(config.join("config.yaml"));
        std::os::unix::fs::symlink(dir.join("kept/config.yaml"), config.join("config.yaml"))
            .expect("link");
        let private = PrivateState {
            state_dir: dir.join("state"),
            private: dir.join("private"),
            also: vec![config.clone()],
        };
        let (denied, _) = denied_paths(&private, &dir.join("codex"), None, &[]).expect("denied");
        let kept = std::fs::canonicalize(dir.join("kept/config.yaml")).expect("real");
        assert!(denied.contains(&kept), "{denied:?}");
        assert!(
            !denied.contains(&std::fs::canonicalize(dir.join("kept")).expect("real")),
            "only the file the link names: {denied:?}"
        );
    }

    #[test]
    fn the_credential_places_are_found_wherever_the_environment_moves_them() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| std::ffi::OsString::from(v))
            }
        };
        let has = |secrets: &[PathBuf], path: &str| secrets.contains(&PathBuf::from(path));
        // HOME and the password database's home, when they differ.
        let secrets =
            home_secrets(&env(&[("HOME", "/srv/home")]), Some("/home/dev".into())).expect("homes");
        for path in [
            "/srv/home/.ssh",
            "/home/dev/.ssh",
            "/home/dev/.cargo/credentials.toml",
        ] {
            assert!(has(&secrets, path), "{path}: {secrets:?}");
        }
        // Cargo reads its config in every directory above a project, so it
        // stays readable; the tokens are in its credentials.
        assert!(
            !has(&secrets, "/home/dev/.cargo/config.toml"),
            "{secrets:?}"
        );
        // With no home at all, nothing runs.
        assert!(home_secrets(&env(&[]), None).is_err());
        assert!(home_secrets(&env(&[("HOME", "relative")]), None).is_err());
        // What the variables move.
        let secrets = home_secrets(
            &env(&[
                ("HOME", "/home/dev"),
                ("XDG_CONFIG_HOME", "/cfg"),
                ("XDG_CACHE_HOME", "/cch"),
                ("XDG_DATA_HOME", "/dat"),
                ("CODEX_HOME", "/cdx"),
                ("AWS_CONFIG_FILE", "/aws/config"),
                ("npm_config_userconfig", "/npm/rc"),
                ("CARGO_HOME", "/opt/cargo"),
                ("ZDOTDIR", "/zdot"),
                ("HISTFILE", "/hist/bash"),
                ("GNUPGHOME", "/gpg"),
                ("KUBECONFIG", "/k/a:/k/b:relative"),
                ("DOCKER_CONFIG", "/dock"),
                ("AWS_SHARED_CREDENTIALS_FILE", "/aws/creds"),
                ("NETRC", "/n/netrc"),
                ("PASSWORD_STORE_DIR", "/pass"),
            ]),
            None,
        )
        .expect("homes");
        for path in [
            "/cfg/gh",
            "/cfg/git/credentials",
            "/cch/huggingface/token",
            "/opt/cargo/credentials.toml",
            "/dat/keyrings",
            "/dat/fish",
            "/cdx",
            "/aws/config",
            "/npm/rc",
            "/zdot/.zshrc",
            "/hist/bash",
            "/gpg",
            "/k/a",
            "/k/b",
            "/dock/config.json",
            "/aws/creds",
            "/n/netrc",
            "/pass",
        ] {
            assert!(has(&secrets, path), "{path}: {secrets:?}");
        }
        assert!(!has(&secrets, "relative"));
        assert!(!has(&secrets, "/opt/cargo/config.toml"), "{secrets:?}");
    }

    /// A link at `link` to `target`, replacing what was there.
    #[cfg(unix)]
    fn link(target: &Path, link: &Path) {
        let _ = std::fs::remove_file(link);
        std::fs::create_dir_all(link.parent().expect("parent")).expect("parent");
        std::os::unix::fs::symlink(target, link).expect("link");
    }

    #[cfg(unix)]
    #[test]
    fn only_the_persons_own_files_and_directories_are_denied_behind_links() {
        let dir = scratch("deniable");
        let home = dir.join("home");
        std::fs::create_dir_all(dir.join("codex")).expect("codex home");
        std::fs::create_dir_all(home.join(".ssh")).expect("ssh");
        // A history kept nowhere, a known-hosts file thrown away, and a
        // tool linked to the system's: denying any would break commands.
        link(Path::new("/dev/null"), &home.join(".bash_history"));
        link(Path::new("/dev/null"), &home.join(".ssh/known_hosts"));
        link(Path::new("/usr/bin/env"), &home.join(".ssh/bin/env"));
        std::fs::write(home.join(".ssh/kept"), "x").expect("kept");
        link(&home.join(".ssh/kept"), &home.join(".ssh/deep/key"));
        let private = PrivateState {
            state_dir: dir.join("state"),
            private: dir.join("private"),
            // A variable naming /dev/null, too.
            also: vec![
                home.join(".bash_history"),
                home.join(".ssh"),
                PathBuf::from("/dev/null"),
            ],
        };
        let (denied, _) = denied_paths(&private, &dir.join("codex"), None, &[]).expect("denied");
        for system in ["/dev/null", "/usr/bin/env"] {
            assert!(
                !denied.iter().any(|d| Path::new(system).starts_with(d)),
                "{system}: {denied:?}"
            );
        }
        assert!(denied.contains(&home.join(".ssh")), "{denied:?}");
    }

    #[cfg(unix)]
    #[test]
    fn nothing_runs_when_a_denial_would_cover_what_commands_need_or_may_write() {
        let dir = scratch("covering");
        let home = dir.join("home");
        let codex = dir.join("codex");
        let work = dir.join("work");
        for d in [&codex, &work, &home.join(".aws"), &home.join("project")] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let refused = |also: Vec<PathBuf>, cwd: Option<&Path>, writable: &[PathBuf]| {
            let private = PrivateState {
                state_dir: dir.join("state"),
                private: dir.join("private"),
                also,
            };
            denied_paths(&private, &codex, cwd, writable)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        // A variable naming the root, or a directory the thread works in,
        // or the thread's Codex home.
        assert!(refused(vec!["/".into()], None, &[]).contains("need"));
        let project = home.join("project");
        assert!(refused(vec![home.clone()], Some(&project), &[]).contains("need"));
        assert!(refused(vec![dir.clone()], None, &[]).contains("need"));
        // A secret linked into a directory the thread may write.
        std::fs::write(work.join("creds"), "x").expect("creds");
        link(&work.join("creds"), &home.join(".aws/credentials"));
        let message = refused(vec![home.join(".aws")], None, std::slice::from_ref(&work));
        assert!(message.contains("may write"), "{message}");
        // Too many links to follow.
        std::fs::remove_file(home.join(".aws/credentials")).expect("unlink");
        let many = home.join(".kube");
        std::fs::create_dir_all(&many).expect("kube");
        std::fs::write(dir.join("one"), "x").expect("one");
        for i in 0..=MAX_LINK_TARGETS {
            link(&dir.join("one"), &many.join(format!("l{i}")));
        }
        assert!(refused(vec![many], None, &[]).contains("links"));
        // And the same places, safely, run.
        assert_eq!(refused(vec![home.join(".aws")], Some(&project), &[]), "");
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_secret_is_denied_as_what_it_points_at_and_links_are_followed_deep() {
        let dir = scratch("linked-secrets");
        let home = dir.join("home");
        std::fs::create_dir_all(home.join("dotfiles")).expect("dotfiles");
        std::fs::create_dir_all(home.join("keys")).expect("keys");
        std::fs::create_dir_all(home.join(".aws/deep")).expect("aws");
        std::fs::create_dir_all(dir.join("elsewhere")).expect("elsewhere");
        std::fs::create_dir_all(dir.join("codex")).expect("codex home");
        std::fs::write(home.join("dotfiles/bashrc"), "x").expect("bashrc");
        std::fs::write(dir.join("elsewhere/credentials"), "x").expect("creds");
        let _ = std::fs::remove_file(home.join(".bashrc"));
        std::os::unix::fs::symlink(home.join("dotfiles/bashrc"), home.join(".bashrc"))
            .expect("link");
        let _ = std::fs::remove_file(home.join(".ssh"));
        std::os::unix::fs::symlink(home.join("keys"), home.join(".ssh")).expect("link");
        let _ = std::fs::remove_file(home.join(".aws/deep/credentials"));
        std::os::unix::fs::symlink(
            dir.join("elsewhere/credentials"),
            home.join(".aws/deep/credentials"),
        )
        .expect("link");
        let private = PrivateState {
            state_dir: dir.join("state"),
            private: dir.join("private"),
            also: vec![home.join(".bashrc"), home.join(".ssh"), home.join(".aws")],
        };
        let (denied, _) = denied_paths(&private, &dir.join("codex"), None, &[]).expect("denied");
        let real = |p: &Path| std::fs::canonicalize(p).expect("real");
        assert!(
            denied.contains(&real(&home.join("dotfiles/bashrc"))),
            "{denied:?}"
        );
        assert!(denied.contains(&real(&home.join("keys"))), "{denied:?}");
        // No mount on a link: the links themselves are not in the list.
        assert!(!denied.contains(&home.join(".bashrc")), "{denied:?}");
        assert!(!denied.contains(&home.join(".ssh")), "{denied:?}");
        // A link two levels down, to a file outside the denied directory.
        assert!(
            denied.contains(&real(&dir.join("elsewhere/credentials"))),
            "{denied:?}"
        );
    }

    #[test]
    fn a_denied_path_inside_another_is_left_to_it() {
        let dir = scratch("nested-deny");
        let config = dir.join("config/atomic");
        let credentials = config.join("credentials");
        std::fs::create_dir_all(&credentials).expect("dirs");
        std::fs::create_dir_all(dir.join("codex")).expect("codex home");
        let private = PrivateState {
            state_dir: dir.join("state"),
            private: dir.join("private"),
            also: vec![credentials.clone(), dir.join("key"), config.clone()],
        };
        let (denied, _) = denied_paths(&private, &dir.join("codex"), None, &[]).expect("denied");
        assert!(denied.contains(&config), "{denied:?}");
        assert!(denied.contains(&dir.join("key")), "{denied:?}");
        assert!(
            !denied.iter().any(|p| p.starts_with(&credentials)),
            "the sandbox cannot deny inside a denied directory: {denied:?}"
        );
    }

    #[test]
    fn an_atoms_shell_inherits_no_secret() {
        let policy = command_environment(&BTreeMap::from([(
            "NOTE_DIR".to_owned(),
            "/w/notes".to_owned(),
        )]));
        let around = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/dev"),
            ("LANG", "C.UTF-8"),
            ("OPENAI_API_KEY", "canary"),
            ("OPENAI_ORGANIZATION", "canary"),
            ("ATOMIC_DATABASE_URL", "canary"),
            ("CODEX_HOME", "canary"),
            ("DATABASE_URL", "canary"),
            ("PGPASSWORD", "canary"),
            ("AWS_ACCESS_KEY_ID", "canary"),
            ("ANTHROPIC_API_KEY", "canary"),
            ("GITHUB_TOKEN", "canary"),
            ("MY_SECRET", "canary"),
            ("UNRELATED", "canary"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let env = codex_protocol::shell_environment::populate_env(around.clone(), &policy, None);
        let mut kept: Vec<&str> = env.keys().map(String::as_str).collect();
        kept.sort_unstable();
        assert_eq!(kept, ["HOME", "LANG", "NOTE_DIR", "PATH"], "{env:?}");
        assert_eq!(env["HOME"], SHELL_HOME, "never the person's home");
        // The exclusions hold even for what the platform's core list might
        // one day include.
        let mut wide = policy.clone();
        wide.inherit = codex_protocol::config_types::ShellEnvironmentPolicyInherit::All;
        let env = codex_protocol::shell_environment::populate_env(around, &wide, None);
        for name in [
            "OPENAI_ORGANIZATION",
            "CODEX_HOME",
            "DATABASE_URL",
            "PGPASSWORD",
            "AWS_ACCESS_KEY_ID",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(!env.contains_key(name), "{name}: {env:?}");
        }
    }

    #[test]
    fn only_openais_api_gets_the_organization_headers() {
        let built_in = built_in_model_providers(None)
            .get(OPENAI_PROVIDER_ID)
            .expect("the OpenAI provider")
            .env_http_headers
            .clone();
        let names = |h: &Option<std::collections::HashMap<String, String>>| {
            h.as_ref()
                .map(|h| h.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        };
        assert!(names(&built_in).contains(&"OPENAI_ORGANIZATION".to_owned()));
        let mut kept = built_in.clone();
        env_headers_for("https://api.openai.com/v1", &mut kept);
        assert_eq!(kept, built_in);
        let mut elsewhere = built_in.clone();
        env_headers_for("https://example.com/v1", &mut elsewhere);
        assert_eq!(elsewhere, None);
        assert!(is_openai_default("https://api.openai.com/v1"));
        assert!(is_openai_default("https://api.openai.com/v1/"));
        for other in [
            "http://127.0.0.1:11434/v1",
            "https://api.openai.com.example/v1",
            "https://api.openai.com@example.com/v1",
            "https://example.com/v1",
        ] {
            assert!(!is_openai_default(other), "{other}");
        }
    }

    #[test]
    fn a_sign_in_sends_no_organization_or_project_headers() {
        on_codex_runtime(async {
            let (home, auth) = (scratch("home-headers"), scratch("auth-headers"));
            let signed_in = ModelEndpoint {
                base_url: crate::signin::CODEX_BACKEND.into(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: Some(crate::signin::SignIn::in_files(&auth)),
            };
            let config = new_config(&signed_in, &home, &home, Arg0DispatchPaths::default())
                .await
                .expect("config");
            assert_eq!(config.model_provider_id, OPENAI_PROVIDER_ID);
            assert_eq!(config.model_provider.env_http_headers, None);
            // OpenAI's API with a key keeps them: the provider has them.
            let api = ModelEndpoint {
                base_url: "https://api.openai.com/v1".into(),
                api_key_env: Some("OPENAI_API_KEY".into()),
                sign_in: None,
                ..signed_in.clone()
            };
            let config = new_config(&api, &home, &home, Arg0DispatchPaths::default())
                .await
                .expect("config");
            assert!(config.model_provider.env_http_headers.is_some());
        });
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("atomic-codex-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    /// Codex runs on threads with large stacks, as its entry point sets up.
    fn on_codex_runtime<F: std::future::Future<Output = ()> + Send + 'static>(f: F) {
        const STACK: usize = 16 * 1024 * 1024;
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn(move || {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_stack_size(STACK)
                    .enable_all()
                    .build()
                    .expect("runtime")
                    .block_on(f)
            })
            .expect("thread")
            .join()
            .expect("the test thread panicked");
    }

    #[test]
    fn a_scripted_model_drives_a_real_codex_thread_with_an_exact_call_count() {
        on_codex_runtime(async {
            let model = FakeModel::start(vec![Reply::Text("pong".into())])
                .await
                .expect("fake model");
            let (home, cwd) = (scratch("home"), scratch("cwd"));
            let endpoint = ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: None,
            };
            let answer = one_turn(
                &CodexPaths::without_helpers(),
                &home,
                &cwd,
                &endpoint,
                &StateGuard::NoProjectState,
                "ping",
            )
            .await
            .expect("one turn");
            assert_eq!(answer, "pong");
            assert_eq!(model.calls(), 1, "requests: {:#?}", model.requests());
            // The prompt reached the model.
            assert!(model.requests()[0].to_string().contains("ping"));
        });
    }

    /// Codex sends TurnComplete before it marks the thread idle; a follow-up
    /// submitted in that window must wait, not fail as NotIdle.
    #[test]
    fn follow_up_turns_submitted_back_to_back_all_run() {
        const TURNS: usize = 100;
        on_codex_runtime(async {
            let replies = (0..TURNS).map(|i| Reply::Text(format!("r{i}"))).collect();
            let model = FakeModel::start(replies).await.expect("fake model");
            let (home, cwd) = (scratch("home3"), scratch("cwd3"));
            let endpoint = ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: None,
            };
            let first = TurnSpec {
                prompt: "t0".into(),
                ..TurnSpec::default()
            };
            let mut seen = Vec::new();
            let (last, report) = converse(
                &CodexPaths::without_helpers(),
                &home,
                &cwd,
                &endpoint,
                &StateGuard::NoProjectState,
                &first,
                |answer| {
                    seen.push(answer.to_owned());
                    (seen.len() < TURNS).then(|| TurnSpec {
                        prompt: format!("t{}", seen.len()),
                        ..TurnSpec::default()
                    })
                },
            )
            .await
            .expect("every follow-up turn runs");
            assert_eq!(last, format!("r{}", TURNS - 1));
            assert_eq!(report.turns as usize, TURNS);
            assert_eq!(model.calls(), TURNS);
        });
    }

    /// An exec command, confined, is the sandbox helper running `env -i`
    /// with exactly the command's environment, then the command: its
    /// profile denies atomic's private state and lets it write only its own
    /// directories and those it is given, such as its run's caches.
    #[test]
    fn an_exec_command_is_confined_under_the_sandbox_helper() {
        use atomic_exec::Confine;
        let state = scratch("exec-sandbox");
        let scratch_dir = state.join("scratch/a1");
        std::fs::create_dir_all(&scratch_dir).expect("scratch");
        let paths = CodexPaths::with_helpers(
            PathBuf::from("/opt/atomic"),
            PathBuf::from("/opt/bin/codex-linux-sandbox"),
        );
        let guard = StateGuard::Deny(PrivateState {
            state_dir: state.clone(),
            private: state.join("private"),
            also: vec![state.join("keys")],
        });
        let sandbox = ExecSandbox::new(&paths, guard, state.join("codex")).expect("a helper");
        let tmp = state.join("scratch/.tmp");
        let confined = sandbox
            .confine(
                &atomic_exec::Command {
                    program: "sh".into(),
                    args: vec!["-c".into(), "true".into()],
                    cwd: state.clone(),
                    env: vec![("HOME".into(), scratch_dir.display().to_string())],
                    writable: vec![scratch_dir.clone(), state.join("cache/run-1")],
                    ..Default::default()
                },
                &tmp,
            )
            .expect("confined");
        assert_eq!(confined.program, "/opt/bin/codex-linux-sandbox");
        // The helper gets its registry's TMPDIR and PATH, nothing else: no
        // variable the command declares loads into it.
        let names: Vec<&str> = confined.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["TMPDIR", "PATH"]);
        assert_eq!(confined.env[0].1, tmp.display().to_string());
        let at = |flag: &str| {
            confined
                .args
                .iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("{flag} in {:?}", confined.args))
        };
        let profile = &confined.args[at("--permission-profile") + 1];
        let rest = &confined.args[at("--") + 1..];
        assert!(rest[0].ends_with("/env") && rest[1] == "-i", "{rest:?}");
        assert_eq!(rest[2], format!("HOME={}", scratch_dir.display()));
        assert!(rest[3].ends_with("/sh"), "resolved: {rest:?}");
        assert_eq!(&rest[4..], ["-c", "true"]);
        assert!(confined.writable.is_empty());
        let profile: serde_json::Value = serde_json::from_str(profile).expect("json");
        let text = profile.to_string();
        for denied in [state.join("private"), state.join("keys")] {
            assert!(
                text.contains(&denied.display().to_string()),
                "{denied:?}: {text}"
            );
        }
        // Its scratch and its run's caches, as the command names them.
        for writable in [&scratch_dir, &state.join("cache/run-1")] {
            assert!(
                text.contains(&writable.display().to_string()),
                "{writable:?}: {text}"
            );
        }
        assert!(text.contains("restricted"), "no network: {text}");
    }

    /// A command that declares hosts is confined only with its proxy held:
    /// `confine`, which would drop the proxy before the command starts,
    /// refuses it, and `confine_held` routes it to the proxy and hands back
    /// the hold.
    #[test]
    fn a_command_with_hosts_is_confined_only_with_its_proxy_held() {
        use atomic_exec::Confine;
        let state = scratch("exec-held");
        let paths = CodexPaths::with_helpers(
            PathBuf::from("/opt/atomic"),
            PathBuf::from("/opt/bin/codex-linux-sandbox"),
        );
        let sandbox = ExecSandbox::new(&paths, StateGuard::NoProjectState, state.join("codex"))
            .expect("a helper");
        let command = atomic_exec::Command {
            program: "sh".into(),
            cwd: state.clone(),
            network: vec!["allowed.invalid".into()],
            ..Default::default()
        };
        let error = sandbox
            .confine(&command, &state)
            .expect_err("its proxy would stop before it starts");
        assert!(error.to_string().contains("confine_held"), "{error}");
        let (confined, held) = sandbox.confine_held(&command, &state).expect("confined");
        assert!(held.is_some(), "the proxy is held");
        assert!(
            confined.args.iter().any(|a| a == "--managed-network"),
            "{:?}",
            confined.args
        );
    }

    /// A confined command's proxy token reaches its sandbox helper on a
    /// pipe only: it is in neither the helper's exec environment nor its
    /// arguments, both of which other processes can read before the helper
    /// can protect itself.
    #[test]
    fn a_commands_proxy_token_reaches_its_helper_on_a_pipe_only() {
        use atomic_exec::Confine;
        let state = scratch("exec-token-pipe");
        let paths = CodexPaths::with_helpers(
            PathBuf::from("/opt/atomic"),
            PathBuf::from("/opt/bin/codex-linux-sandbox"),
        );
        let sandbox = ExecSandbox::new(&paths, StateGuard::NoProjectState, state.join("codex"))
            .expect("a helper");
        let command = atomic_exec::Command {
            program: "sh".into(),
            cwd: state.clone(),
            network: vec!["allowed.invalid".into()],
            ..Default::default()
        };
        let (confined, _held) = sandbox.confine_held(&command, &state).expect("confined");
        let key = codex_network_proxy::PROXY_ATTRIBUTION_TOKEN_ENV_KEY;
        let [(secret_key, token)] = confined.secret_env.0.as_slice() else {
            panic!("one secret variable: {:?}", confined.secret_env);
        };
        assert_eq!(secret_key, key);
        assert_eq!(token.len(), 64, "a token of 32 bytes, in hexadecimal");
        assert!(
            confined
                .env
                .iter()
                .all(|(k, v)| k != key && !v.contains(token.as_str())),
            "the token is in the helper's environment"
        );
        assert!(
            confined.args.iter().all(|a| !a.contains(token.as_str())),
            "the token is in the helper's arguments"
        );
        // The proxy variables the helper routes still are in its environment.
        assert!(
            confined.env.iter().any(|(k, _)| k == "HTTP_PROXY"),
            "{:?}",
            confined.env
        );
    }

    /// The sandbox helper takes only the proxy token from its socket: a
    /// second variable, or another one in its place, is refused.
    #[test]
    fn the_helper_takes_only_the_proxy_token_from_its_socket() {
        let key = codex_network_proxy::PROXY_ATTRIBUTION_TOKEN_ENV_KEY;
        let token = |entries: &[(&str, &str)]| {
            the_proxy_token(atomic_exec::SecretEnv(
                entries
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            ))
        };
        assert_eq!(token(&[(key, "abc")]).as_deref(), Ok("abc"));
        for refused in [
            &[][..],
            &[("LD_PRELOAD", "/tmp/x.so")][..],
            &[(key, "abc"), ("LD_PRELOAD", "/tmp/x.so")][..],
        ] {
            assert!(token(refused).is_err(), "{refused:?}");
        }
    }

    /// However many projects the atomic home holds, their private state is
    /// denied by one entry: the profile a command carries on its command
    /// line stays the same size.
    #[test]
    fn the_profile_stays_bounded_however_many_projects_there_are() {
        use atomic_exec::Confine;
        let home = scratch("many-projects");
        let paths =
            CodexPaths::with_helpers("/opt/atomic".into(), "/opt/codex-linux-sandbox".into());
        let profile_len = |home: &Path| {
            let guard = StateGuard::Deny(PrivateState {
                state_dir: home.join("projects/p0"),
                private: home.join("private"),
                also: Vec::new(),
            });
            let sandbox =
                ExecSandbox::new(&paths, guard, home.join("projects/p0/codex")).expect("helper");
            let confined = sandbox
                .confine(
                    &atomic_exec::Command {
                        program: "sh".into(),
                        cwd: home.to_owned(),
                        ..Default::default()
                    },
                    home,
                )
                .expect("confined");
            confined.args.iter().map(String::len).sum::<usize>()
        };
        std::fs::create_dir_all(home.join("private/p0")).expect("one project");
        let one = profile_len(&home);
        for n in 1..1000 {
            std::fs::create_dir_all(home.join(format!("private/p{n}"))).expect("project");
            std::fs::create_dir_all(home.join(format!("projects/p{n}"))).expect("state");
        }
        assert_eq!(
            profile_len(&home),
            one,
            "the profile grew with the projects"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn only_codex_helper_invocations_are_helpers() {
        let args = |a: &[&str]| a.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        for helper in [
            &["/usr/bin/codex-linux-sandbox", "--sandbox-policy-cwd", "/x"][..],
            &["/opt/apply_patch"][..],
            &["atomic", "--codex-run-as-apply-patch", "p"][..],
            &["atomic", "--codex-run-as-fs-helper"][..],
            &["atomic", "--codex-run-as-process-setup"][..],
        ] {
            assert!(is_helper(args(helper).into_iter()), "{helper:?}");
        }
        for not in [
            &["atomic", "run", "x"][..],
            &["atomic", "--codex-run-as-anything"][..],
            &["atomic", "--__codex-whatever"][..],
            &["codex-linux-sandbox-x"][..],
        ] {
            assert!(!is_helper(args(not).into_iter()), "{not:?}");
        }
    }

    /// The helper's stage is read from its own options only: a command that
    /// is, or holds, the inner stage's flag leaves the outer stage outer.
    #[test]
    fn only_the_helpers_own_options_name_its_inner_stage() {
        let args = |a: &[&str]| a.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        let helper = "/x/codex-linux-sandbox";
        for inner in [
            &[
                helper,
                "--sandbox-policy-cwd",
                "/w",
                "--apply-seccomp-then-exec",
                "--",
                "ls",
            ][..],
            &[
                helper,
                "--apply-seccomp-then-exec",
                "--",
                "--apply-seccomp-then-exec",
            ][..],
        ] {
            assert!(is_inner_stage(args(inner)), "{inner:?}");
        }
        for outer in [
            &[
                helper,
                "--sandbox-policy-cwd",
                "/w",
                "--",
                "--apply-seccomp-then-exec",
            ][..],
            &[helper, "--", "/bin/bash", "-c", "--apply-seccomp-then-exec"][..],
            &[helper, "--sandbox-policy-cwd", "/w", "--", "ls"][..],
            &["--apply-seccomp-then-exec"][..],
        ] {
            assert!(!is_inner_stage(args(outer)), "{outer:?}");
        }
    }

    #[test]
    fn commands_get_only_core_and_declared_variables() {
        let inherited = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/dev"),
            ("USER", "dev"),
            ("ATOMIC_DATABASE_URL", "postgres://atomic:pw@db/atomic"),
            ("ATOMIC_SECRET_DEPLOY", "s3cret"),
            ("GITHUB_TOKEN", "ghp_x"),
            ("AWS_SECRET_ACCESS_KEY", "aws"),
            ("OPENAI_API_KEY", "sk"),
            ("DBOS__APPVERSION", "1"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "authorization=Bearer x"),
            ("SOME_TOOL_SETTING", "x"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let declared = BTreeMap::from([
            ("ATOMIC_CACHE_DIR".to_owned(), "/cache".to_owned()),
            ("CARGO_TARGET_DIR".to_owned(), "/cache/target".to_owned()),
        ]);
        let env = codex_protocol::shell_environment::create_env_from_vars(
            inherited,
            &command_environment(&declared),
            None,
        );
        let names: std::collections::BTreeSet<&str> = env.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "ATOMIC_CACHE_DIR",
                "CARGO_TARGET_DIR",
                "HOME",
                "PATH",
                "USER"
            ]
            .into(),
            "{env:?}"
        );
        assert_eq!(env["CARGO_TARGET_DIR"], "/cache/target");
    }

    /// Should the inheritance ever widen, atomic's own variables still stay
    /// out: the exporter's settings among them.
    #[test]
    fn atomics_own_variables_stay_out_whatever_is_inherited() {
        use codex_protocol::config_types::ShellEnvironmentPolicyInherit;
        let inherited = [
            ("PATH", "/usr/bin"),
            ("ATOMIC_DATABASE_URL", "postgres://atomic:pw@db/atomic"),
            ("DBOS__APPVERSION", "1"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "authorization=Bearer x"),
            ("SOME_TOOL_SETTING", "x"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let mut policy = command_environment(&BTreeMap::new());
        policy.inherit = ShellEnvironmentPolicyInherit::All;
        let env = codex_protocol::shell_environment::create_env_from_vars(inherited, &policy, None);
        let names: std::collections::BTreeSet<&str> = env.keys().map(String::as_str).collect();
        // HOME is atomic's own, a directory that does not exist, never one
        // inherited.
        assert_eq!(
            names,
            ["HOME", "PATH", "SOME_TOOL_SETTING"].into(),
            "{env:?}"
        );
        assert_eq!(env["HOME"], SHELL_HOME);
    }

    #[test]
    fn sandboxes_map_to_codex_permission_profiles() {
        use crate::typed::Sandbox;
        let home = Path::new("/state/codex");
        let profile = |sandbox| {
            permission_profile(sandbox, &StateGuard::NoProjectState, home, None, &[])
                .expect("profile")
        };
        assert_eq!(profile(Sandbox::ReadOnly), PermissionProfile::read_only());
        assert_eq!(
            profile(Sandbox::WorkspaceWrite),
            PermissionProfile::workspace_write_with(
                &[],
                codex_protocol::permissions::NetworkSandboxPolicy::Restricted,
                true,
                true
            )
        );
        assert_ne!(
            profile(Sandbox::WorkspaceWrite),
            PermissionProfile::workspace_write(),
            "the system temporary directories are not writable"
        );
        assert_ne!(profile(Sandbox::ReadOnly), profile(Sandbox::WorkspaceWrite));
    }

    /// What a profile lets a command at `path` do, with `cwd` and `roots` as
    /// the thread's working directory and writable roots.
    fn access(
        profile: &PermissionProfile,
        cwd: &str,
        roots: &[&str],
        path: &str,
    ) -> codex_protocol::permissions::FileSystemAccessMode {
        let mut workspace: Vec<AbsolutePathBuf> =
            vec![AbsolutePathBuf::from_absolute_path(cwd).expect("cwd")];
        workspace.extend(
            roots
                .iter()
                .map(|r| AbsolutePathBuf::from_absolute_path(r).expect("root")),
        );
        let policy = profile
            .clone()
            .materialize_project_roots_with_workspace_roots(&workspace)
            .file_system_sandbox_policy();
        let can_read = policy.can_read_local_path_with_cwd(Path::new(path), Path::new(cwd));
        let can_write = policy.can_write_local_path_with_cwd(Path::new(path), Path::new(cwd));
        use codex_protocol::permissions::FileSystemAccessMode::*;
        match (can_read, can_write) {
            (_, true) => Write,
            (true, false) => Read,
            (false, false) => Deny,
        }
    }

    #[test]
    fn the_environment_cannot_stand_in_for_or_redirect_a_sign_ins_tokens() {
        let mut removed = Vec::new();
        scrub(|name| removed.push(name.to_owned()));
        assert_eq!(
            removed,
            [
                "CODEX_ACCESS_TOKEN",
                "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
                "CODEX_REVOKE_TOKEN_URL_OVERRIDE",
            ]
        );
    }

    #[test]
    fn private_state_is_denied_and_the_rest_of_the_state_directory_is_not() {
        use crate::typed::Sandbox;
        use codex_protocol::permissions::FileSystemAccessMode::*;
        let dir = scratch("private");
        let state = dir.join("state");
        let home = state.join("codex");
        for sub in [
            "private",
            "codex/shell_snapshots",
            "codex/tmp/arg0-linux",
            "codex/log",
        ] {
            std::fs::create_dir_all(state.join(sub)).expect("layout");
        }
        std::fs::write(home.join("auth.json"), "{}").expect("codex file");
        std::fs::create_dir_all(home.join("tmp/other")).expect("codex tmp");
        let guard = StateGuard::Deny(PrivateState {
            state_dir: state.clone(),
            private: state.join("private"),
            also: vec![dir.join("keys"), dir.join("credentials")],
        });
        let write =
            permission_profile(Sandbox::WorkspaceWrite, &guard, &home, None, &[]).expect("rw");
        let read = permission_profile(Sandbox::ReadOnly, &guard, &home, None, &[]).expect("ro");
        let p = |rel: &str| state.join(rel).display().to_string();
        let worktree = p("worktrees/run/step");
        let cache = p("cache");
        for (profile, own) in [(&write, Write), (&read, Read)] {
            let at = |path: &str| access(profile, &worktree, &[&cache], path);
            assert_eq!(at(&p("private/atomic.db")), Deny);
            assert_eq!(at(&p("private")), Deny);
            assert_eq!(
                at(&dir.join("keys/context.key").display().to_string()),
                Deny
            );
            assert_eq!(at(&dir.join("credentials/x").display().to_string()), Deny);
            assert_eq!(at(&p("codex/auth.json")), Deny);
            // There from the start, so a command cannot plant them.
            assert_eq!(at(&p("codex/.env")), Deny);
            assert_eq!(at(&p("codex/config.toml")), Deny);
            // Nothing else Codex reads from its home can be planted, even
            // under a writable root over the whole state directory.
            let state_dir = state.display().to_string();
            for planted in [
                "environments.toml",
                "AGENTS.md",
                "AGENTS.override.md",
                "hooks.json",
                "rules/default.rules",
                "skills/x/SKILL.md",
            ] {
                let at_all = access(
                    profile,
                    &worktree,
                    &[&state_dir],
                    &p(&format!("codex/{planted}")),
                );
                assert_ne!(at_all, Write, "{planted} could be planted");
            }
            assert_eq!(at(&p("codex/log/x")), Deny);
            assert_eq!(at(&p("codex/tmp/other/x")), Deny);
            // Shell snapshots are off, and none is readable.
            assert_eq!(at(&p("codex/shell_snapshots/t.sh")), Deny);
            // What Codex's shell needs, and the thread's own checkout.
            assert_eq!(at(&p("codex/tmp/arg0-linux/codex-linux-sandbox")), Read);
            assert_eq!(at(&format!("{worktree}/src/lib.rs")), own);
            assert_eq!(at("/usr/bin/git"), Read);
            // The system temporary directory is not writable, nor a project in it.
            assert_eq!(at("/tmp/x"), Read);
            assert_eq!(at("/tmp/project/.git/HEAD"), Read);
        }
        assert_eq!(
            access(
                &write,
                &worktree,
                &[&cache],
                &format!("{cache}/cargo-target/x")
            ),
            Write
        );
        // Every denied path is denied whole: nothing below one is re-opened,
        // so the sandbox needs no mount inside a denied directory.
        let policy = write
            .clone()
            .materialize_project_roots_with_workspace_roots(&[AbsolutePathBuf::from_absolute_path(
                &worktree,
            )
            .expect("cwd")])
            .file_system_sandbox_policy();
        let denied: Vec<_> = policy
            .entries
            .iter()
            .filter(|e| e.access == Deny)
            .filter_map(|e| match &e.path {
                codex_protocol::permissions::FileSystemPath::Path { path } => {
                    path.to_abs_path().ok()
                }
                _ => None,
            })
            .collect();
        for entry in policy.entries.iter().filter(|e| e.access != Deny) {
            if let codex_protocol::permissions::FileSystemPath::Path { path } = &entry.path
                && let Ok(path) = path.to_abs_path()
            {
                assert!(
                    !denied
                        .iter()
                        .any(|d| path.as_path().starts_with(d.as_path())),
                    "{} re-opens a denied path",
                    path.display()
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_writing_thread_with_no_project_state_may_not_cover_atomic_state() {
        use crate::typed::Sandbox;
        let base = crate::state::base_dir().expect("base");
        let inside = base.join("projects/p");
        let refused = |roots: &[PathBuf]| {
            refuse_uncovered_store(Sandbox::WorkspaceWrite, &StateGuard::NoProjectState, roots)
        };
        assert!(refused(std::slice::from_ref(&inside)).is_err());
        assert!(
            refused(&[PathBuf::from("/")]).is_err(),
            "a root above the store"
        );
        assert!(refused(&[PathBuf::from("/nowhere/project")]).is_ok());
        assert!(
            refuse_uncovered_store(Sandbox::ReadOnly, &StateGuard::NoProjectState, &[inside])
                .is_ok(),
            "a read-only thread writes nothing"
        );
    }

    #[test]
    fn turn_directories_of_dead_processes_are_swept() {
        let state = scratch("sweep");
        let tmp = state.join("tmp");
        std::fs::create_dir_all(&tmp).expect("tmp");
        // A pid that no process has: above the kernel's pid limit.
        let dead = tmp.join("turn-4194305-0-1");
        std::fs::create_dir_all(dead.join("locked/inner")).expect("dead");
        std::fs::write(dead.join("locked/inner/f"), "x").expect("file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dead.join("locked"), std::fs::Permissions::from_mode(0o000))
                .expect("mode 000");
        }
        let mine = TurnTemp::create(&StateGuard::Deny(PrivateState {
            state_dir: state.clone(),
            private: state.join("private"),
            also: Vec::new(),
        }))
        .expect("live");
        std::fs::write(tmp.join("unrelated"), "x").expect("other");
        assert_eq!(sweep_turn_temps(&state).expect("sweep"), 1);
        assert!(!dead.exists());
        assert!(mine.0.exists(), "a live process's directory stays");
        assert!(tmp.join("unrelated").exists());
        let kept = mine.0.clone();
        drop(mine);
        assert!(!kept.exists());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_tool_call_costs_one_more_model_call() {
        on_codex_runtime(async {
            let model = FakeModel::start(vec![
                Reply::Tool {
                    name: "no_such_tool".into(),
                    arguments: serde_json::json!({}),
                },
                Reply::Text("done".into()),
            ])
            .await
            .expect("fake model");
            let (home, cwd) = (scratch("home2"), scratch("cwd2"));
            let endpoint = ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: None,
            };
            let answer = one_turn(
                &CodexPaths::without_helpers(),
                &home,
                &cwd,
                &endpoint,
                &StateGuard::NoProjectState,
                "go",
            )
            .await
            .expect("one turn");
            assert_eq!(answer, "done");
            assert_eq!(model.calls(), 2);
            // The second call carries the failed tool call's result.
            assert!(model.requests()[1].to_string().contains("call_synthetic"));
        });
    }

    /// A sandbox trial stops the run only for what the machine repeats until
    /// it is fixed; a failure that may pass, such as a full disk, waits.
    #[test]
    fn only_a_refusal_the_machine_repeats_makes_a_trial_unconfinable() {
        for refused in [
            "bwrap: No permissions to create a new namespace, likely because the kernel does not \
             allow non-privileged user namespaces.",
            "bwrap: Creating new namespace failed: Operation not permitted",
            "bwrap: Can't mount proc on /newroot/proc: Operation not permitted",
            "bwrap: setting up uid map: Permission denied",
            "bwrap: Can't mount on symlink destination /home/dev/.bashrc: Invalid argument",
            "thread 'main' panicked at linux-sandbox/src/launcher.rs:60:13:\nbubblewrap is \
             unavailable: no system bwrap was found on PATH",
            "thread 'main' panicked at x.rs:1:1:\nfailed to exec system bubblewrap /usr/bin/bwrap: \
             Permission denied (os error 13)",
            "thread 'main' panicked at x.rs:1:1:\nLinux sandbox retained effective or permitted \
             capabilities",
        ] {
            assert!(sandbox_refused(refused), "{refused}");
        }
        for passing in [
            "",
            "thread 'main' panicked at linux-sandbox/src/bwrap.rs:721:13:\nerror building \
             bubblewrap command: No space left on device (os error 28)",
            "bwrap: Can't make directory /tmp/x: No space left on device",
            "bwrap: Can't create file at /tmp/x: Disk quota exceeded",
            "error building bubblewrap command: Permission denied (os error 13)",
            "fork: Resource temporarily unavailable",
        ] {
            assert!(!sandbox_refused(passing), "{passing}");
        }
    }
    // -----------------------------------------------------------------------
    // Metering: each turn admitted first and settled once, each reported
    // token charged once.

    use crate::usage::{Limit, Meter, MeterFuture, Ticket};
    use std::sync::Mutex;

    /// Admits every turn with `limit` and records each settlement.
    #[derive(Default)]
    struct Recorder {
        limit: Limit,
        settled: Mutex<Vec<Outcome>>,
    }

    impl Meter for Recorder {
        fn admit(&self, call: Call) -> MeterFuture<'_, Ticket> {
            Box::pin(async move {
                Ok(Ticket {
                    request: format!("r:{}", call.index),
                    attempt: 1,
                    model: call.model,
                    admitted: std::time::Instant::now(),
                    limit: self.limit,
                    bounded_by: None,
                })
            })
        }

        fn settle(&self, _: Ticket, outcome: Outcome) -> MeterFuture<'_, ()> {
            Box::pin(async move {
                self.settled.lock().expect("settled").push(outcome);
                Ok(())
            })
        }
    }

    /// A meter whose progress records fail `failing` times, or never
    /// answer, counting each record it is asked for.
    #[derive(Default)]
    struct Flaky {
        failing: std::sync::atomic::AtomicUsize,
        hangs: bool,
        records: std::sync::atomic::AtomicUsize,
    }

    impl Meter for Flaky {
        fn admit(&self, _: Call) -> MeterFuture<'_, Ticket> {
            Box::pin(async { Err(MeterError::Store("not used".into())) })
        }

        fn settle(&self, _: Ticket, _: Outcome) -> MeterFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn progress<'a>(&'a self, _: &'a Ticket, _: Outcome) -> MeterFuture<'a, ()> {
            use std::sync::atomic::Ordering::SeqCst;
            Box::pin(async move {
                self.records.fetch_add(1, SeqCst);
                if self.hangs {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                }
                if self.failing.load(SeqCst) > 0 {
                    self.failing.fetch_sub(1, SeqCst);
                    return Err(MeterError::Store("the store is down".into()));
                }
                Ok(())
            })
        }
    }

    fn progress_ticket() -> Ticket {
        Ticket {
            request: "run-1:2:0".into(),
            attempt: 1,
            model: "synthetic".into(),
            admitted: std::time::Instant::now(),
            limit: Limit::default(),
            bounded_by: None,
        }
    }

    /// A sink for a [`ProgressLog`] to say its warnings into, and the lines
    /// it keeps: a test hands the sink to a recorder as `said` and reads
    /// back what it wrote, instead of what reached the log.
    fn warning_sink() -> (Said, Arc<std::sync::Mutex<Vec<String>>>) {
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = {
            let lines = lines.clone();
            move |line: &str| lines.lock().expect("said").push(line.to_owned())
        };
        (Arc::new(sink), lines)
    }

    #[tokio::test]
    async fn a_progress_record_advances_only_once_written_and_a_failure_is_said_once() {
        use std::sync::atomic::Ordering::SeqCst;
        let meter = Arc::new(Flaky {
            failing: 2.into(),
            ..Flaky::default()
        });
        let metering = Metering::new(meter.clone());
        let ticket = progress_ticket();
        let (sink, said) = warning_sink();
        let mut log = ProgressLog {
            meter: &metering,
            ticket: Some(&ticket),
            seen: (0, 0),
            recorded: (0, 0),
            records: 0,
            warnings: 0,
            said: Some(sink),
        };
        let grown = Progress {
            usage: Some((3, 2, 5)),
            ..Progress::default()
        };
        // A failed record does not advance, so the next token event, though
        // it reports nothing new, tries it again; the failure is said once.
        log.note(&grown).await;
        assert_eq!((log.recorded, log.records, log.warnings), ((0, 0), 0, 1));
        log.note(&grown).await;
        assert_eq!(meter.records.load(SeqCst), 2);
        assert_eq!((log.recorded, log.records, log.warnings), ((0, 0), 0, 1));
        // What was written, not what was meant: two failures asked for, one
        // line said. Drop the guard that says it once and this is two.
        let lines = said.lock().expect("said").clone();
        assert_eq!(lines.len(), 1, "said once, of two failures: {lines:?}");
        // Nothing was recorded, so a stop would charge the reservation, and
        // the line names it instead of a record that does not exist.
        assert!(lines[0].contains("reserved"), "{lines:?}");
        assert!(!lines[0].contains("last record"), "{lines:?}");
        // The run it names is the run the request id belongs to.
        assert!(lines[0].starts_with("run run-1: "), "{lines:?}");
        // Written, it advances; with nothing new, nothing is asked again.
        log.note(&grown).await;
        assert_eq!((log.recorded, log.records), ((3, 2), 1));
        log.note(&grown).await;
        assert_eq!(meter.records.load(SeqCst), 3);
    }

    #[tokio::test]
    async fn a_progress_warning_of_a_turn_that_had_recorded_names_its_last_record() {
        let meter = Arc::new(Flaky {
            failing: 1.into(),
            ..Flaky::default()
        });
        let metering = Metering::new(meter.clone());
        let ticket = progress_ticket();
        let (sink, said) = warning_sink();
        let mut log = ProgressLog {
            meter: &metering,
            ticket: Some(&ticket),
            seen: (0, 0),
            recorded: (3, 2),
            records: 1,
            warnings: 0,
            said: Some(sink),
        };
        // One record is kept, the turn reports more, and the store refuses
        // it: what a stop would charge is that last record, not a bound a
        // record was meant to replace.
        log.note(&Progress {
            usage: Some((5, 4, 9)),
            ..Progress::default()
        })
        .await;
        let lines = said.lock().expect("said").clone();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("last record"), "{lines:?}");
        assert!(!lines[0].contains("reserved"), "{lines:?}");
        assert_eq!((log.recorded, log.records), ((3, 2), 1));
    }

    #[test]
    fn the_run_a_request_id_names_keeps_the_colons_it_has() {
        // The ledger meter makes every request id `<run>:<step>:<call>`, so
        // only the last two fields are cut off (see `metering` in the
        // `atomic` binary).
        assert_eq!(run_of_request("run-1:2:0"), "run-1");
        assert_eq!(run_of_request("wf:7:ab:2:0"), "wf:7:ab");
        // An id of any other shape names no run and is said as it is, as
        // the ids of a meter that keeps nothing between admit and settle.
        assert_eq!(run_of_request("r:0"), "r:0");
        assert_eq!(run_of_request("0"), "0");
        assert_eq!(run_of_request(""), "");
    }

    #[tokio::test]
    async fn a_progress_record_the_store_does_not_answer_is_given_up_after_its_patience() {
        let meter = Arc::new(Flaky {
            hangs: true,
            ..Flaky::default()
        });
        let metering = Metering::new(meter.clone());
        let ticket = progress_ticket();
        let mut log = ProgressLog {
            meter: &metering,
            ticket: Some(&ticket),
            seen: (0, 0),
            recorded: (0, 0),
            records: 0,
            warnings: 0,
            said: None,
        };
        let started = std::time::Instant::now();
        log.note(&Progress {
            usage: Some((3, 2, 5)),
            ..Progress::default()
        })
        .await;
        let waited = started.elapsed();
        assert!(
            waited >= PROGRESS_PATIENCE && waited < PROGRESS_PATIENCE * 3,
            "{waited:?}"
        );
        assert_eq!((log.records, log.warnings), (0, 1));
    }

    async fn metered_conversation(
        tag: &str,
        replies: Vec<Reply>,
        turns: usize,
        limit: Limit,
    ) -> (anyhow::Result<(String, ThreadReport)>, Vec<Outcome>, usize) {
        let model = FakeModel::start(replies).await.expect("fake model");
        let (home, cwd) = (
            scratch(&format!("home-{tag}")),
            scratch(&format!("cwd-{tag}")),
        );
        let endpoint = ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
            sign_in: None,
        };
        let recorder = Arc::new(Recorder {
            limit,
            ..Recorder::default()
        });
        let meter = Metering::new(recorder.clone());
        let mut asked = 1;
        let ended = converse_async(
            &CodexPaths::without_helpers(),
            &home,
            &cwd,
            &endpoint,
            &meter,
            &StateGuard::NoProjectState,
            &TurnSpec {
                prompt: "t0".into(),
                ..TurnSpec::default()
            },
            |_| {
                let more = asked < turns;
                asked += 1;
                std::future::ready(more.then(|| TurnSpec {
                    prompt: format!("t{asked}"),
                    ..TurnSpec::default()
                }))
            },
        )
        .await;
        let settled = recorder.settled.lock().expect("settled").clone();
        (ended, settled, model.calls())
    }

    const ONE_USAGE: Usage = Usage {
        input_tokens: 1,
        output_tokens: 1,
    };
    const ONE: Outcome = Outcome::Reported(ONE_USAGE);

    #[test]
    fn each_turn_settles_with_what_the_thread_grew_by_and_a_silent_turn_as_unknown() {
        on_codex_runtime(async {
            let (ended, settled, calls) = metered_conversation(
                "meter",
                vec![
                    Reply::Text("a".into()),
                    Reply::WithoutUsage("b".into()),
                    Reply::Text("c".into()),
                ],
                3,
                Limit::default(),
            )
            .await;
            let (_, report) = ended.expect("three turns");
            assert_eq!(calls, 3);
            // The thread's totals grow by one and one per reported turn; the
            // turn that reported nothing is unknown, never free, and the
            // next turn is charged only its own growth.
            assert_eq!(settled, [ONE, Outcome::Unknown, ONE]);
            assert_eq!((report.input_tokens, report.output_tokens), (2, 2));
        });
    }

    #[test]
    fn a_failed_turn_settles_with_the_usage_seen_before_it_failed() {
        on_codex_runtime(async {
            // The turn's first request reports its usage and calls a tool;
            // the second is refused, and the turn fails.
            let (ended, settled, calls) = metered_conversation(
                "failed",
                vec![
                    Reply::Tool {
                        name: "no_such_tool".into(),
                        arguments: serde_json::json!({}),
                    },
                    Reply::Http {
                        status: 400,
                        body:
                            r#"{"error": {"message": "refused", "type": "invalid_request_error"}}"#
                                .into(),
                    },
                ],
                1,
                Limit::default(),
            )
            .await;
            assert!(ended.is_err(), "the turn failed");
            assert!(calls >= 2, "{calls}");
            assert_eq!(settled, [ONE], "the failed turn kept the usage it reported");
        });
    }

    #[test]
    fn a_turn_is_cut_off_when_its_reserved_time_runs_out() {
        on_codex_runtime(async {
            let began = std::time::Instant::now();
            let (ended, settled, _) = metered_conversation(
                "slow",
                vec![Reply::Delayed {
                    after: std::time::Duration::from_secs(30),
                    reply: Box::new(Reply::Text("late".into())),
                }],
                1,
                Limit {
                    output_tokens: None,
                    time: Some(std::time::Duration::from_millis(500)),
                },
            )
            .await;
            let e = ended.expect_err("cut off");
            assert!(format!("{e:#}").contains("cut off"), "{e:#}");
            assert!(began.elapsed() < std::time::Duration::from_secs(20));
            assert_eq!(settled, [Outcome::Unknown]);
        });
    }

    #[test]
    fn a_turn_whose_last_response_breaks_off_is_charged_as_one_in_flight() {
        on_codex_runtime(async {
            // The first request reports its usage and calls a tool; every
            // later response begins and breaks off, Codex's retries too.
            let mut replies = vec![Reply::Tool {
                name: "no_such_tool".into(),
                arguments: serde_json::json!({}),
            }];
            replies.extend((0..12).map(|_| Reply::Broken("par".into())));
            let (ended, settled, calls) =
                metered_conversation("broken", replies, 1, Limit::default()).await;
            assert!(ended.is_err(), "the turn failed");
            assert!((3..=13).contains(&calls), "{calls}");
            assert_eq!(
                settled,
                [Outcome::Partial {
                    reported: ONE_USAGE,
                    last: Some(ONE_USAGE),
                }]
            );
        });
    }

    #[test]
    fn a_request_is_in_flight_unless_the_turn_completed_or_was_refused() {
        let at = |streaming, completed| Progress {
            streaming,
            completed,
            ..Progress::default()
        };
        // Cut off at any point but after completing: waiting on a request,
        // as a response streamed, or while a tool ran, which may have
        // yielded to further requests.
        assert!(at(false, false).in_flight(true, true));
        assert!(at(true, false).in_flight(true, true));
        assert!(!at(false, true).in_flight(true, true));
        // A failure is in flight only if a response broke off midway, not
        // when a request was refused outright.
        assert!(at(true, false).in_flight(false, true));
        assert!(!at(false, false).in_flight(false, true));
        // A completed turn never is.
        assert!(!at(false, true).in_flight(false, false));
    }

    #[test]
    fn a_turns_totals_count_responses_whose_token_count_never_came() {
        // A response completed, and the turn stopped before its totals.
        let p = Progress {
            usage: Some((10, 4, 14)),
            responses: (30, 9),
            ..Progress::default()
        };
        assert_eq!(p.totals((10, 4)), Some((40, 13, 53)));
        // The totals already count it: nothing twice.
        let p = Progress {
            usage: Some((40, 13, 53)),
            responses: (30, 9),
            ..Progress::default()
        };
        assert_eq!(p.totals((10, 4)), Some((40, 13, 53)));
        assert_eq!(Progress::default().totals((10, 4)), None);
    }

    #[test]
    fn a_turn_cut_off_with_a_request_in_flight_is_settled_in_part() {
        on_codex_runtime(async {
            // The turn's first request reports its usage and calls a tool;
            // the second is still in flight when its time runs out.
            let (ended, settled, calls) = metered_conversation(
                "inflight",
                vec![
                    Reply::Tool {
                        name: "no_such_tool".into(),
                        arguments: serde_json::json!({}),
                    },
                    Reply::Delayed {
                        after: std::time::Duration::from_secs(30),
                        reply: Box::new(Reply::Text("late".into())),
                    },
                ],
                1,
                Limit {
                    output_tokens: None,
                    time: Some(std::time::Duration::from_secs(3)),
                },
            )
            .await;
            let e = ended.expect_err("cut off");
            assert!(format!("{e:#}").contains("cut off"), "{e:#}");
            assert_eq!(calls, 2, "the second request was made");
            // The first request's usage and one more request's bound: the
            // second spent something no one has reported.
            assert_eq!(
                settled,
                [Outcome::Partial {
                    reported: ONE_USAGE,
                    last: Some(ONE_USAGE),
                }]
            );
        });
    }
}
