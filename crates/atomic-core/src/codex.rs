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
use codex_core_api::LoadInstructionsFuture;
use codex_core_api::LoadedUserInstructions;
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
use codex_core_api::UserInstructionsProvider;
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
use codex_extension_api::SkillInvocationContributor;
use codex_protocol::config_types::TrustLevel;
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
    /// Directories the thread's commands may neither read nor write, apart
    /// from those inside them it may write: the scratch roots of the
    /// project's backends, a `workspace-write` thread's own directory
    /// excepted. Absolute paths. Codex's Linux sandbox reopens only writable
    /// directories inside a denied one, so a `read-only` thread's own scratch
    /// directory, which it may not write, is out of its reach too: it cannot
    /// even list it.
    pub denied: Vec<PathBuf>,
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
    ("CLOUDSDK_CONFIG", &[""]),
];

/// Where workload identities put their token files on Kubernetes when no
/// variable says otherwise: AWS's (IAM roles for service accounts, and EKS
/// Pod Identity), the pod's own service account, and Azure workload
/// identity's. Denied whether or not atomic's environment names them.
const WORKLOAD_TOKEN_DIRS: &[&str] = &[
    "/var/run/secrets/eks.amazonaws.com/serviceaccount",
    "/var/run/secrets/pods.eks.amazonaws.com/serviceaccount",
    // `scripts/controls/k8s-token-dir.row` switches this off.
    "/var/run/secrets/kubernetes.io/serviceaccount",
    // `scripts/controls/azure-token-dir.row` switches this off.
    "/var/run/secrets/azure/tokens",
];

/// The variables that name a workload credential file an SDK reads, each
/// with what the file is called in a refusal: AWS's token files, Azure
/// workload identity's, and Google's application credentials, which may be
/// a key or a configuration naming a token file of its own (see
/// [`google_token_files`]).
const WORKLOAD_TOKEN_FILES: &[(&str, &str)] = &[
    ("AWS_WEB_IDENTITY_TOKEN_FILE", "token file"),
    ("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", "token file"),
    // `scripts/controls/azure-token-file.row` switches this off.
    ("AZURE_FEDERATED_TOKEN_FILE", "token file"),
    // `scripts/controls/gcp-credentials-file.row` switches this off.
    (GOOGLE_CREDENTIALS, "credentials file"),
];

/// The variable that names Google's application credentials.
const GOOGLE_CREDENTIALS: &str = "GOOGLE_APPLICATION_CREDENTIALS";

/// The most of a credential configuration atomic reads to find the token
/// files it names: far more than any holds.
const CONFIG_BYTES_MAX: u64 = 1 << 20;

/// The projected volume `file` is in, if it is in one, as Kubernetes mounts
/// a service account's token: the volume holds a `..data` link to its
/// current version, a hidden `..<time>` directory beside it, and each item
/// is a link through `..data`, which Kubernetes swaps for a new version each
/// time it rotates the token. Denying the volume, not only the version the
/// token resolves to now, keeps the next token denied too.
///
/// Found by what `file` resolves to, so a link to the token, through any
/// number of links, and an item nested in a directory of the volume are
/// found too: the volume is the directory above an ancestor of that path
/// which its `..data` resolves to, or which is hidden as a version is,
/// beside a `..data` link. A file that does not resolve through such a
/// version, even beside a `..data`, is in no volume. Where `..data`, or the
/// file, cannot be read for any reason but that it is not there (nor any
/// directory it could be in), such as a link that leads back to itself, the
/// directory holding it is denied whole.
fn projected_volume(file: PathBuf) -> Option<PathBuf> {
    let missing = |e: &std::io::Error| {
        matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        )
    };
    let target = match std::fs::canonicalize(&file) {
        Ok(target) => target,
        Err(e) if missing(&e) => return None,
        Err(_) => return file.parent().map(Path::to_owned),
    };
    for version in target.ancestors().skip(1) {
        let Some(volume) = version.parent() else {
            break;
        };
        let data = volume.join("..data");
        match std::fs::symlink_metadata(&data) {
            Ok(meta) if meta.file_type().is_symlink() => {}
            Ok(_) => continue,
            Err(e) if missing(&e) => continue,
            Err(_) => return Some(volume.to_owned()),
        }
        let hidden = version
            .file_name()
            .is_some_and(|n| n.as_encoded_bytes().starts_with(b".."));
        match std::fs::canonicalize(&data) {
            Ok(current) if current == version || hidden => return Some(volume.to_owned()),
            Ok(_) => {}
            // One that leads nowhere beside a version, or cannot be resolved.
            Err(_) if hidden => return Some(volume.to_owned()),
            Err(e) if !missing(&e) => return Some(volume.to_owned()),
            Err(_) => {}
        }
    }
    None
}

/// The workload credential files the variables in `env` name, a relative
/// one resolved against the working directory `here` gives, as an SDK
/// started there resolves it, and, for one in a projected volume, the
/// volume itself; with the token files Google's application credentials
/// name. An error, so nothing runs, when a name is relative and the working
/// directory is unknown, or when Google's credentials cannot be read whole.
fn token_files(
    env: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    here: &dyn Fn() -> std::io::Result<PathBuf>,
) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for (name, what) in WORKLOAD_TOKEN_FILES {
        let Some(file) = env(name).filter(|v| !v.is_empty()).map(PathBuf::from) else {
            continue;
        };
        // Named as plainly as it reads, so a trailing `/` is no directory it
        // must be: only one above the file can fail as not a directory.
        let file: PathBuf = file.components().collect();
        let file = if file.is_absolute() {
            file
        } else {
            let refused = |e: std::io::Error| {
                format!(
                    "{name} names the {what} {} relative to the working directory, which \
                     is unknown ({e}), so it could not be denied to atoms' commands",
                    file.display()
                )
            };
            here().map_err(refused)?.join(&file)
        };
        if *name == GOOGLE_CREDENTIALS {
            // `scripts/controls/gcp-external-token.row` switches this off.
            out.extend(google_token_files(&file, here)?);
        }
        out.extend(projected_volume(file.clone()));
        out.push(file);
    }
    Ok(out)
}

/// `file` read as plainly as a name in a configuration reads, resolved
/// against the working directory `here` gives when it is relative.
fn named_path(file: &str, here: &dyn Fn() -> std::io::Result<PathBuf>) -> Result<PathBuf, String> {
    let file: PathBuf = Path::new(file).components().collect();
    if file.is_absolute() {
        return Ok(file);
    }
    here().map(|here| here.join(&file)).map_err(|e| {
        format!(
            "it names {} relative to the working directory, which is unknown ({e})",
            file.display()
        )
    })
}

/// The bytes of the credential configuration at `path`, followed through
/// links as its SDK follows them, read without waiting, as opening a fifo
/// would, and no further than [`CONFIG_BYTES_MAX`]: `None` when it is not
/// there, or is a device, as `/dev/null` is when a variable names it to
/// turn the configuration off, which is never opened; and an error for
/// anything else that keeps it from being read whole.
fn read_config(path: &Path) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read as _;
    let unreadable = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::PermissionDenied => format!("this user may not read it: {e}"),
        _ => e.to_string(),
    };
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        // Only a file that is not there names nothing: any other error
        // refuses. `scripts/controls/workload-config-not-there.row` switches
        // this off.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e)),
    };
    // `scripts/controls/workload-config-device.row` switches this off.
    let device = is_device(&meta);
    if device {
        return Ok(None);
    }
    let mut open = std::fs::OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut open, libc::O_NONBLOCK);
    let file = open.open(path).map_err(unreadable)?;
    // What was opened, which may not be what was looked at.
    let meta = file.metadata().map_err(|e| e.to_string())?;
    // `scripts/controls/workload-config-plain.row` switches this off.
    let plain = meta.is_file();
    if !plain {
        return Err("it is not a plain file".into());
    }
    let mut bytes = Vec::new();
    file.take(CONFIG_BYTES_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > CONFIG_BYTES_MAX {
        return Err(format!("it holds more than {CONFIG_BYTES_MAX} bytes"));
    }
    Ok(Some(bytes))
}

/// Whether `meta` is a device's: a character or a block device.
fn is_device(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        meta.file_type().is_char_device() || meta.file_type().is_block_device()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

/// The token files the Google credentials file at `config` names, each
/// with its projected volume where it is in one: an external account's
/// `credential_source.file`, and the `output_file` its executable keeps a
/// token in, as for the `source_credentials` an impersonated account is
/// configured with. A key, or an account that names no file, names none,
/// and so does a file that is not there. Anything that cannot be read whole
/// and as JSON is refused, naming the file, so nothing runs rather than a
/// token it may name being left readable.
fn google_token_files(
    config: &Path,
    here: &dyn Fn() -> std::io::Result<PathBuf>,
) -> Result<Vec<PathBuf>, String> {
    let refused = |why: String| {
        format!(
            "{} could not be read as Google credentials ({why}), so a token file they name \
             could not be denied to atoms' commands; fix it, or point {GOOGLE_CREDENTIALS} at \
             a file that can be read",
            config.display()
        )
    };
    let Some(bytes) = read_config(config).map_err(refused)? else {
        return Ok(Vec::new());
    };
    // `scripts/controls/gcp-config-refused.row` switches this off.
    let value = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) => value,
        Err(e) => return Err(refused(format!("it is not JSON: {e}"))),
    };
    let mut names = Vec::new();
    google_named(&value, 0, &mut names).map_err(refused)?;
    let mut out = Vec::new();
    for name in names {
        let file = named_path(&name, here).map_err(refused)?;
        out.extend(projected_volume(file.clone()));
        out.push(file);
    }
    Ok(out)
}

/// What Google credentials `value` name as files holding a token, into
/// `out`, and those of the credentials they are configured with, no deeper
/// than a few: an error when a field that names a file is not text, or one
/// that holds others is not an object.
fn google_named(
    value: &serde_json::Value,
    depth: usize,
    out: &mut Vec<String>,
) -> Result<(), String> {
    use serde_json::Value;
    let Some(object) = value.as_object() else {
        return Err("they are not a JSON object".into());
    };
    if depth > 4 {
        return Err("their source credentials nest too deeply".into());
    }
    let named = |value: Option<&Value>, field: &str| match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => Ok(Some(name.clone()).filter(|n| !n.is_empty())),
        Some(_) => Err(format!("{field} is not text")),
    };
    let fields = |value: &Value, field: &str| {
        value
            .as_object()
            .cloned()
            .ok_or_else(|| format!("{field} is not an object"))
    };
    if let Some(source) = object.get("credential_source") {
        let source = fields(source, "credential_source")?;
        out.extend(named(source.get("file"), "credential_source.file")?);
        if let Some(executable) = source.get("executable") {
            let executable = fields(executable, "credential_source.executable")?;
            out.extend(named(
                executable.get("output_file"),
                "credential_source.executable.output_file",
            )?);
        }
    }
    if let Some(inner) = object.get("source_credentials") {
        google_named(inner, depth + 1, out)?;
    }
    Ok(())
}

/// The web identity token files the AWS configuration file at `config`
/// names in any of its profiles, each with its projected volume where it is
/// in one: one named relative to the working directory `here` gives, as an
/// AWS SDK reads it, and one under `~/` in each of `homes` as well. A file
/// that is not there names none; one that cannot be read whole as text is
/// refused, naming it, so nothing runs.
fn aws_profile_token_files(
    config: &Path,
    homes: &[PathBuf],
    here: &dyn Fn() -> std::io::Result<PathBuf>,
) -> Result<Vec<PathBuf>, String> {
    let refused = |why: String| {
        format!(
            "{} could not be read as an AWS configuration file ({why}), so a web identity \
             token file it names could not be denied to atoms' commands; make it a file this \
             user can read, or move it",
            config.display()
        )
    };
    let Some(bytes) = read_config(config).map_err(refused)? else {
        return Ok(Vec::new());
    };
    let text = String::from_utf8(bytes).map_err(|_| refused("it is not UTF-8 text".into()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        // A key ends at `=` or, as Python's configuration parser reads it,
        // at `:`, whichever comes first.
        // `scripts/controls/aws-profile-colon.row` switches this off.
        let Some(at) = line.find(['=', ':']) else {
            continue;
        };
        let (key, value) = (&line[..at], line[at + 1..].trim());
        if !key.trim().eq_ignore_ascii_case("web_identity_token_file") || value.is_empty() {
            continue;
        }
        // Some SDKs end a value at a `#` or `;` after white space, as a
        // comment, and others keep it: both readings are denied.
        // `scripts/controls/aws-profile-comment.row` switches this off.
        let comment = comment_start(value);
        let mut values = vec![value];
        values.extend(
            comment
                .map(|at| value[..at].trim_end())
                .filter(|v| !v.is_empty()),
        );
        for value in values {
            let mut files = vec![named_path(value, here).map_err(refused)?];
            if let Some(rest) = value.strip_prefix("~/") {
                files.extend(homes.iter().map(|home| home.join(rest)));
            }
            for file in files {
                out.extend(projected_volume(file.clone()));
                out.push(file);
            }
        }
    }
    Ok(out)
}

/// Where a comment starts in a configuration value, as the SDKs that allow
/// one read it: at the first `#` or `;` that follows white space.
fn comment_start(value: &str) -> Option<usize> {
    value
        .char_indices()
        .find(|&(at, c)| matches!(c, '#' | ';') && value[..at].ends_with(char::is_whitespace))
        .map(|(at, _)| at)
}

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
    let homes = real_homes(env, passwd)?;
    for home in &homes {
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
    // Workload token files: where they usually are, those the variables
    // name, a relative one resolved against atomic's working directory as
    // an SDK resolves it, and, for one in a projected volume, the volume
    // itself.
    out.extend(WORKLOAD_TOKEN_DIRS.iter().map(PathBuf::from));
    out.extend(token_files(env, &std::env::current_dir)?);
    // Those named inside a configuration rather than by a variable: an AWS
    // profile's web identity token file, and what the application default
    // credentials gcloud writes name.
    let mut aws: Vec<PathBuf> = homes
        .iter()
        .flat_map(|home| [home.join(".aws/config"), home.join(".aws/credentials")])
        .collect();
    aws.extend(absolute("AWS_CONFIG_FILE"));
    aws.extend(absolute("AWS_SHARED_CREDENTIALS_FILE"));
    let here = &std::env::current_dir;
    for config in &aws {
        // `scripts/controls/aws-profile-token.row` switches this off.
        out.extend(aws_profile_token_files(config, &homes, here)?);
    }
    let mut gcloud: Vec<PathBuf> = homes
        .iter()
        .map(|home| home.join(".config/gcloud"))
        .collect();
    gcloud.extend(absolute("CLOUDSDK_CONFIG"));
    for dir in &gcloud {
        let defaults = dir.join("application_default_credentials.json");
        // `scripts/controls/gcp-default-credentials.row` switches this off.
        out.extend(google_token_files(&defaults, &std::env::current_dir)?);
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
        // keys are another atomic's: denied too, where they exist. The entry
        // from `dirs` is where that other atomic would keep its credentials
        // (see [`crate::state::credentials_dir`]), which is not always the
        // directory denied above: that one comes from
        // [`crate::state::user_config_dir`], the same function
        // [`crate::state::outside_user_config`] reads, so the directory a
        // state directory is refused for sitting inside is denied outright.
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

/// Where the system keeps the programs and libraries every command needs,
/// and its devices: a link into one of these is never followed to deny its
/// target. Secrets kept elsewhere by the system, under `/run`, `/etc` or
/// `/opt`, are denied when a credential place links to them.
const SYSTEM_PREFIXES: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/nix/store",
    "/dev",
    "/proc",
    "/sys",
];

/// Why a deny list could not be made: nothing runs. Not the model's doing.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct DenyRefused(pub String);

/// Whether `meta` is a character or block device, such as `/dev/null`:
/// never denied, since every command may need it.
fn device(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let kind = meta.file_type();
        kind.is_char_device() || kind.is_block_device()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

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

/// Directories a credential place that is a link may point at and still not
/// be denied, since every command needs them: the root and the system's
/// top-level directories, each exactly.
const NEVER_DENIED: &[&str] = &[
    "/", "/bin", "/boot", "/dev", "/etc", "/home", "/lib", "/lib32", "/lib64", "/libx32", "/media",
    "/mnt", "/nix", "/opt", "/proc", "/root", "/run", "/sbin", "/srv", "/sys", "/tmp", "/usr",
    "/var",
];

/// Whether `target` is one of [`NEVER_DENIED`], or, in a test, the
/// directory [`with_never_denied`] adds.
fn never_denied(target: &Path) -> bool {
    #[cfg(test)]
    if ALSO_NEVER_DENIED.with(|d| d.borrow().as_deref() == Some(target)) {
        return true;
    }
    NEVER_DENIED.iter().any(|p| target == Path::new(p))
}

#[cfg(test)]
thread_local! {
    /// A directory a test has [`never_denied`] treat as one of
    /// [`NEVER_DENIED`], on its own thread.
    static ALSO_NEVER_DENIED: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Have [`never_denied`] treat `dir` as one of [`NEVER_DENIED`] on this
/// thread, until what it returns is dropped: a test's stand-in for a
/// top-level directory, without walking a real one.
#[cfg(test)]
pub(crate) fn with_never_denied(dir: &Path) -> NeverDeniedGuard {
    ALSO_NEVER_DENIED.with(|d| *d.borrow_mut() = Some(dir.to_owned()));
    NeverDeniedGuard
}

/// Ends [`with_never_denied`] when dropped.
#[cfg(test)]
#[must_use]
pub(crate) struct NeverDeniedGuard;

#[cfg(test)]
impl Drop for NeverDeniedGuard {
    fn drop(&mut self) {
        ALSO_NEVER_DENIED.with(|d| *d.borrow_mut() = None);
    }
}

/// What a link resolved to, if it should be denied.
///
/// Never a device, nor anything under the system's program, library and
/// device directories ([`SYSTEM_PREFIXES`]). A credential place that is
/// itself a link (`listed`), such as `~/.netrc` kept with a person's
/// dotfiles, is otherwise denied as what it points at, whoever owns it and
/// whatever its mode, but for the system's top-level directories
/// ([`NEVER_DENIED`]). A link found inside a credential place is also left
/// alone when it leads to a program (a file with an execute bit that starts
/// as one does) or to a directory someone else owns: a virtual
/// environment's `python` linked to a toolchain's keeps working. A key
/// linked to one a secrets manager keeps under `/run`, or a cluster's
/// credentials under `/etc`, are denied whoever owns them.
fn deniable(target: &Path, listed: bool) -> bool {
    let Ok(meta) = std::fs::metadata(target) else {
        return false;
    };
    if device(&meta) || SYSTEM_PREFIXES.iter().any(|p| target.starts_with(p)) {
        return false;
    }
    if listed {
        return !never_denied(target);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.is_file() && meta.permissions().mode() & 0o111 != 0 && program(target) {
            return false;
        }
        if meta.is_dir() && meta.uid() != crate::state::effective_uid() {
            return false;
        }
    }
    true
}

/// Whether the file at `path` starts as a program does: an ELF or Mach-O
/// binary, or a script with `#!`.
fn program(path: &Path) -> bool {
    use std::io::Read;
    let mut start = [0u8; 4];
    let Ok(read) = std::fs::File::open(path).and_then(|mut f| f.read(&mut start)) else {
        return false;
    };
    let start = &start[..read];
    start.starts_with(b"#!")
        || start.starts_with(b"\x7fELF")
        || [
            [0xfe, 0xed, 0xfa, 0xce],
            [0xfe, 0xed, 0xfa, 0xcf],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xca, 0xfe, 0xba, 0xbe],
        ]
        .iter()
        .any(|magic| start == magic)
}

/// Every path a profile denies under `guard`, for a thread whose Codex home
/// is `codex_home`: the state directory's `private/`, the paths in
/// [`PrivateState::also`], and everything in Codex's home but the helper
/// links its shell needs. Each is denied whole, never re-opened below, so
/// the sandbox needs no mount inside a denied directory. A path reached
/// through a linked directory is denied under its real spelling, which
/// denies it under every other; one that is itself a link, such as a
/// `~/.bashrc` kept with a person's dotfiles, is denied as the file or
/// directory it points at, since the sandbox cannot mount on a link, and a
/// read through the link lands on the denied target.
/// The real file or directory behind each link up to [`LINK_DEPTH`] levels
/// inside a denied directory is denied too, such as a `config.yaml` linked
/// to one kept elsewhere.
///
/// A resolved target is denied only if [`deniable`]. Nothing runs when a
/// denied path would cover what the thread needs (its working directory
/// `cwd`, the directories it may write, `writable`, the state directory,
/// its Codex home or atomic's own program); when a link's target lies in
/// a directory the thread may write, where a command could rename it out
/// from under its denial; when a place is named through a link inside a
/// directory the thread may write, which a command could point elsewhere;
/// and when a place lies in a directory the thread may write that it is
/// given through a link, where the sandbox, which knows that directory by
/// the spelling it was given, would not keep the place denied.
///
/// Each is denied too wherever else the mount table shows it, such as a
/// home bound at a second path (see [`crate::mounts`]); nothing runs when
/// the table cannot be read, or when such a place would cover what the
/// thread needs or lies two or more levels inside a directory it may
/// write.
///
/// The list is made when a thread starts. A path that does not exist then
/// is denied only once it does, when a later command starts: one made while
/// a command runs can be read by that command.
#[cfg(test)]
fn denied_paths(
    private: &PrivateState,
    codex_home: &Path,
    cwd: Option<&Path>,
    writable: &[PathBuf],
) -> anyhow::Result<(Vec<PathBuf>, Vec<String>)> {
    denied_paths_with(private, codex_home, cwd, writable, &[])
}

/// [`denied_paths`], with `also` denied besides by [`deny_also`]: not in the
/// list, but counted in its cap as each command counts them.
fn denied_paths_with(
    private: &PrivateState,
    codex_home: &Path,
    cwd: Option<&Path>,
    writable: &[PathBuf],
    also: &[PathBuf],
) -> anyhow::Result<(Vec<PathBuf>, Vec<String>)> {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    let given = writable;
    let mut roots: Vec<PathBuf> = writable.iter().map(|p| real(p)).collect();
    // atomic's state holds what the thread may write (its worktrees,
    // scratch and temporary directories): no denied link may point there.
    let state = real(&private.state_dir);
    // The same directories wherever else they are mounted, such as a bind of
    // the working directory: a command may write them there too.
    let seen_again = crate::mounts::second_mounts(&[&roots, std::slice::from_ref(&state)])
        .map_err(|e| anyhow::Error::new(DenyRefused(format!("{e:#}"))))?;
    roots.extend(seen_again[0].iter().cloned());
    let mut writable = roots.clone();
    writable.push(state);
    writable.extend(seen_again[1].iter().cloned());
    let mut needed = writable.clone();
    needed.extend(cwd.map(real));
    needed.push(real(codex_home));
    needed.extend(std::env::current_exe().ok().map(|p| real(&p)));
    let covers = |denied: &Path| needed.iter().find(|n| n.starts_with(denied)).cloned();
    let refuse = |why: String| anyhow::Error::new(DenyRefused(why));
    let holds = |path: &Path, needed: &Path| {
        refuse(format!(
            "{} holds {}, which atoms' commands need, so atomic cannot deny it; keep the \
             project and atomic's state outside the places that hold credentials, and point \
             any variable that names such a place (HISTFILE, CODEX_HOME, \
             AWS_WEB_IDENTITY_TOKEN_FILE and the like) elsewhere",
            path.display(),
            needed.display()
        ))
    };
    // A denied place two or more levels inside a directory the thread may
    // write could be moved out from under its denial by a command renaming
    // a directory above it; one directly inside cannot, since the sandbox's
    // mount on it is busy.
    let movable = |path: &Path, real_path: &Path, within: &[PathBuf]| {
        within
            .iter()
            .find(|w| {
                real_path
                    .strip_prefix(w)
                    .is_ok_and(|rest| rest.components().count() >= 2)
            })
            .map(|root| {
                refuse(format!(
                    "{} is inside {}, which atoms' commands may write, so a command could move \
                     it out from under its denial; run with a working directory that does not \
                     hold it",
                    path.display(),
                    root.display()
                ))
            })
    };
    // Every project's private state, by one entry, however many projects
    // the home holds.
    let mut denied = vec![private.private.clone()];
    denied.extend(private.also.iter().cloned());
    // Before anything is walked: a place that covers what the thread needs,
    // such as a variable naming the root.
    for path in &denied {
        if let Some(needed) = covers(&real(path)).or_else(|| covers(path)) {
            return Err(holds(path, &needed));
        }
    }
    // The places themselves, resolved, which what is found inside them
    // needs no denial of its own beside: a place as it is, or what a place
    // that is a link points at, when that is denied.
    let base = denied.len();
    let places = as_built(&denied);
    let mut links = Vec::new();
    for dir in &private.also {
        links_in(dir, LINK_DEPTH, &mut links);
    }
    // A link inside a place, walked as the kernel walks it: one whose chain
    // runs through a link a command may point, or that leads nowhere yet into
    // what the thread may write, refuses the thread.
    for link in &links {
        if let Some(refused) = link_planted(link, &writable, &places) {
            return Err(refused);
        }
    }
    // Only the links that lead somewhere a denial is needed count against
    // the cap.
    links.retain(|link| {
        std::fs::canonicalize(link)
            .is_ok_and(|t| deniable(&t, false) && !places.iter().any(|p| t.starts_with(p)))
    });
    if links.len() > MAX_LINK_TARGETS {
        return Err(refuse(format!(
            "the places denied to atoms' commands hold {} links to files elsewhere, more than \
             the {MAX_LINK_TARGETS} atomic follows; remove some, or keep the files they point at \
             inside those places",
            links.len()
        )));
    }
    let link_count = links.len();
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
            // Codex's links, and atomic's own link to itself under the
            // helper's name, which every sandbox runs; they stay readable.
            let helpers = children(&entry, &arg0)
                .into_iter()
                .filter(|p| not_the_helpers_link(p));
            own.extend(helpers);
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
    for (i, path) in denied.into_iter().enumerate() {
        let found = i >= base;
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            // Nothing there yet: denied once it is. A place, but for
            // atomic's own private state, that would be made in what the
            // thread may write, or through a link there, refuses it: a
            // command could make it for what reads it to find.
            if !found
                && i != 0
                && let Some(refused) = missing_into(&path, &writable, &places)
            {
                return Err(refused);
            }
            all.push(path);
            continue;
        };
        if meta.file_type().is_symlink() {
            // A link: the sandbox cannot mount on one, so what it resolves
            // to, if that should be denied, and never what the thread
            // needs or may write. A target inside another denied place is
            // denied with it. A place that is a link atomic cannot resolve
            // for a reason other than leading nowhere yet refuses the
            // thread here, as each of its commands would where the sandbox
            // helper walks the places again (Linux); one that leads nowhere
            // yet refuses it when where it leads lies in what the thread may
            // write, where a command could put a file for what reads the
            // place to find.
            let unresolved = |e: anyhow::Error| {
                refuse(format!(
                    "{e:#}; remove the link, or point it at something atomic can reach"
                ))
            };
            let target = if found {
                std::fs::canonicalize(&path).ok()
            } else {
                match resolved(&path) {
                    Ok(Some(target)) => {
                        // Its own chain, which a command may repoint where
                        // it runs through what the thread may write.
                        if let Some(refused) = rewritable_chain(&path, &writable, &places) {
                            return Err(refused);
                        }
                        Some(target)
                    }
                    Ok(None) => {
                        if let Some(refused) = dangling_into(&path, &writable, &places) {
                            return Err(refused);
                        }
                        None
                    }
                    Err(e) => return Err(unresolved(e)),
                }
            };
            let Some(target) = target.filter(|t| deniable(t, !found)) else {
                continue;
            };
            if found && places.iter().any(|p| target.starts_with(p)) {
                continue;
            }
            if let Some(needed) = covers(&target) {
                return Err(refuse(format!(
                    "{} resolves to {}, which holds {}, which atoms' commands need; atomic \
                     cannot deny it",
                    path.display(),
                    target.display(),
                    needed.display()
                )));
            }
            if let Some(refused) = movable(&path, &target, &writable) {
                return Err(refused);
            }
            all.push(target);
            continue;
        }
        // A place as it is; only a device, such as a variable naming
        // /dev/null, is left out. A pipe or socket is denied like a file.
        if device(&meta) {
            continue;
        }
        let real_path = real(&path);
        if let Some(needed) = covers(&real_path) {
            return Err(holds(&path, &needed));
        }
        if let Some(refused) = movable(&path, &real_path, &roots) {
            return Err(refused);
        }
        all.push(path);
    }
    // Each place goes to the sandbox under its real spelling alone, the
    // directories above it resolved: an older bubblewrap, such as 0.10,
    // cannot mount on a path that crosses an absolute link, as
    // /var/run/secrets crosses /var/run -> /run, and no command would start.
    // A mount on the real path denies it under every spelling. Through a
    // link a command may rewrite, the path could later lead elsewhere, so
    // that is refused, as Codex refuses it.
    let mut spelled = Vec::with_capacity(all.len());
    for path in all {
        if let Some(link) = rewritable_link(&path, &roots) {
            return Err(refuse(format!(
                "{} is reached through the link {}, which atoms' commands may write, so a \
                 command could point it elsewhere; run with a working directory that does not \
                 hold it",
                path.display(),
                link.display()
            )));
        }
        spelled.push(real_spelling(&path));
    }
    let mut all = spelled;
    // The same places wherever else they are mounted, such as a home bound
    // at a second path or the host's files under /run/host: denied there
    // too, or nothing runs (see `crate::mounts`).
    // And Codex's home, whose databases a pattern denies (see below).
    let home = [real_spelling(codex_home)];
    let found =
        crate::mounts::second_mounts(&[&all, &home]).map_err(|e| refuse(format!("{e:#}")))?;
    let (aliases, home_aliases) = (&found[0], &found[1]);
    // One cap for both, as both go to the sandbox helper in one argument.
    let second = aliases.len() + home_aliases.len();
    if link_count + second > MAX_LINK_TARGETS {
        return Err(refuse(format!(
            "the places denied to atoms' commands hold {link_count} links to files elsewhere and \
             are mounted at {second} other places, more than the {MAX_LINK_TARGETS} atomic \
             denies besides the places themselves"
        )));
    }
    for alias in aliases.iter().chain(home_aliases) {
        if let Some(needed) = covers(alias) {
            return Err(refuse(format!(
                "{} is a second mount of a place denied to atoms' commands, and holds {}, which \
                 they need, so atomic cannot deny it there",
                alias.display(),
                needed.display()
            )));
        }
        if deep_inside(&writable, alias).is_some() {
            return Err(refuse(format!(
                "{} is a second mount of a place denied to atoms' commands, two or more levels \
                 inside a directory they may write, so a command could move it out from under \
                 its denial; mount it elsewhere",
                alias.display()
            )));
        }
    }
    all.extend(aliases.iter().cloned());
    // A path inside another denied path is denied with it, and the sandbox
    // cannot mount a denial inside one: such as the credential store inside
    // the person's own configuration directory.
    all.sort();
    all.dedup();
    let inside_another = |p: &PathBuf| all.iter().any(|q| q != p && p.starts_with(q));
    let all: Vec<PathBuf> = all.iter().filter(|p| !inside_another(p)).cloned().collect();
    // The sandbox finds the denials inside a writable directory by the
    // spelling it is given that directory under: one given through a link
    // would not keep a denial inside it, under its real spelling, in force.
    let codex = real_spelling(codex_home);
    for (named, root) in given.iter().zip(&roots) {
        if named.as_path() == root.as_path() {
            continue;
        }
        // Codex's home, whose databases a pattern denies, is weighed with the
        // places listed. The files planted in it at start are listed as well,
        // so this is defence in depth for a home that holds them.
        // `scripts/controls/deny-linked-codex-home.row` switches this off.
        let mut weighed = all.iter().chain([&codex]);
        if let Some(place) = weighed.find(|p| p.starts_with(root)) {
            return Err(refuse(format!(
                "{} is inside {}, which atoms' commands may write, named {} through a link, so \
                 its denial would not hold; point ATOMIC_HOME (or XDG_DATA_HOME, or HOME) and \
                 the project at their real paths, with no link above them",
                place.display(),
                root.display(),
                named.display()
            )));
        }
    }
    // Codex creates its state database when the first thread starts, after
    // the list above is made: a pattern, expanded when each command starts,
    // covers it, under the home's real spelling, as every place is, and so
    // at each second mount of Codex's home.
    let home_spelled = real_spelling(codex_home);
    let mut globs = vec![sqlite_glob(&home_spelled).map_err(|e| refuse(format!("{e:#}")))?];
    for alias in home_aliases {
        globs.push(sqlite_glob(alias).map_err(|e| refuse(format!("{e:#}")))?);
    }
    // The cap again, counted as each command's sandbox helper counts it
    // (see `late_targets`): the second mounts of all the list denies, its
    // second mounts among them and what `deny_also` will deny besides (the
    // backend's scratch directories), and of each directory whose databases
    // it denies; so a list a thread starts with does not refuse its commands.
    let homes: Vec<PathBuf> = std::iter::once(home_spelled)
        .chain(home_aliases.iter().cloned())
        .collect();
    let mut as_commands_see = all.clone();
    as_commands_see.extend(also.iter().cloned());
    let counted = crate::mounts::second_mounts(&[&as_commands_see, &homes])
        .map_err(|e| refuse(format!("{e:#}")))?;
    let (mounted, home_mounts) = (counted[0].len(), counted[1].len());
    let as_commands_count = link_count + mounted + home_mounts;
    if as_commands_count > MAX_LINK_TARGETS {
        return Err(refuse(format!(
            "the places denied to atoms' commands hold {link_count} links to files elsewhere, \
             with {mounted} second mounts of what they deny, counted from each side, and \
             {home_mounts} of Codex's home, more than the {MAX_LINK_TARGETS} atomic follows"
        )));
    }
    Ok((all, globs))
}

/// The pattern that denies the databases Codex keeps in `home`; refused
/// when `home` holds a character a pattern gives a meaning to, or is not
/// UTF-8, since then the pattern may not match it and the databases would
/// go undenied.
fn sqlite_glob(home: &Path) -> anyhow::Result<String> {
    let Some(text) = home.to_str() else {
        bail!(
            "{} is not UTF-8, so atomic cannot deny Codex's databases there",
            home.display()
        );
    };
    let special = text.matches(GLOB_SPECIAL).next();
    if let Some(c) = special {
        bail!(
            "{text} holds `{c}`, which a pattern gives a meaning to, so atomic cannot deny \
             Codex's databases there; keep Codex's home, and any second mount of it, at a path \
             without one"
        );
    }
    Ok(format!("{text}/{SQLITE_FILES}"))
}

/// The characters a sandbox pattern gives a meaning to.
const GLOB_SPECIAL: &[char] = &['*', '?', '[', ']', '{', '}', '\\'];

/// The databases Codex keeps in its home, as a pattern inside it.
const SQLITE_FILES: &str = "*.sqlite*";

/// Where [`walk_link`] found a link leads.
enum Landing {
    /// There now, as what it resolves to.
    There(PathBuf),
    /// Not there yet: where it would be once something is made there.
    NotYet(PathBuf),
}

/// Walk `path`, a link, as the kernel walks it, a directory at a time, each
/// link on the way followed where it is, and say where it leads. Refused
/// when a link on the way lies in one of `writable`, where a command could
/// point it elsewhere; when it climbs back out of one of them with `..`,
/// where a command could make what it climbs out of a link; or when where
/// it leads cannot be told. What lies inside a denied place of `masked` is
/// hidden from every command, and no link or directory there can be changed.
fn walk_link(path: &Path, writable: &[PathBuf], masked: &[PathBuf]) -> anyhow::Result<Landing> {
    use std::path::Component;
    let refuse = |why: String| Err(anyhow::Error::new(DenyRefused(why)));
    // What is walked so far, every link in it followed, and what is left.
    let mut done = PathBuf::from("/");
    let mut rest: std::collections::VecDeque<std::ffi::OsString> = path
        .components()
        .filter(|c| !matches!(c, Component::RootDir))
        .map(|c| c.as_os_str().to_owned())
        .collect();
    let mut hops = 0;
    while let Some(part) = rest.pop_front() {
        if part == "." {
            continue;
        }
        if part == ".." {
            // Up out of a directory a command may write: what it holds,
            // that this climbs back out of, a command can make a link.
            if let Some(root) = climbing_out(writable, masked, &done) {
                return refuse(format!(
                    "{} leads by way of {}, and climbs back out of it, in {}, which atoms' \
                     commands may write, so a command could make it a link that leads \
                     elsewhere; point {} without `..`",
                    path.display(),
                    done.display(),
                    root.display(),
                    path.display()
                ));
            }
            done.pop();
            continue;
        }
        let next = done.join(&part);
        let link = match std::fs::symlink_metadata(&next) {
            Ok(meta) => meta.file_type().is_symlink(),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                // Not there: where it would lead is this, and the rest.
                if rest.iter().any(|p| p == ".." || p == ".") {
                    return refuse(format!(
                        "{} is a link that leads nowhere yet, by way of {}, and atomic cannot \
                         tell where it would lead; point the link elsewhere",
                        path.display(),
                        next.display()
                    ));
                }
                return Ok(Landing::NotYet(
                    rest.iter().fold(next, |p, part| p.join(part)),
                ));
            }
            Err(e) => {
                return refuse(format!(
                    "{} leads by way of {}, which cannot be looked at ({e}); point the link at \
                     something atomic can reach",
                    path.display(),
                    next.display()
                ));
            }
        };
        if !link {
            done = next;
            continue;
        }
        // A link on the way: one a command may point elsewhere decides
        // where the place leads, whatever it points at now.
        if let Some(root) = changeable(writable, masked, &next) {
            if next == path {
                return refuse(format!(
                    "{} is a link in {}, which atoms' commands may write, so a command could \
                     point it elsewhere or put something in its place; keep it elsewhere, or \
                     run the project from a directory that does not hold it",
                    path.display(),
                    root.display()
                ));
            }
            return refuse(format!(
                "{} leads by way of {}, a link in {}, which atoms' commands may write, so a \
                 command could point it elsewhere; point {} past it",
                path.display(),
                next.display(),
                root.display(),
                path.display()
            ));
        }
        hops += 1;
        if hops > LINK_HOPS {
            return refuse(format!(
                "{} leads through more than {LINK_HOPS} links; atomic cannot tell where",
                path.display()
            ));
        }
        let to = match std::fs::read_link(&next) {
            Ok(to) => to,
            Err(e) => {
                return refuse(format!(
                    "{} leads by way of {}, a link atomic cannot read ({e}); point it at \
                     something atomic can reach",
                    path.display(),
                    next.display()
                ));
            }
        };
        if to.is_absolute() {
            done = PathBuf::from("/");
        }
        for part in to
            .components()
            .filter(|c| !matches!(c, Component::RootDir))
            .rev()
        {
            rest.push_front(part.as_os_str().to_owned());
        }
    }
    Ok(Landing::There(done))
}

/// Why a denied place that is a link leading nowhere yet cannot be left as
/// it is ([`walk_link`]): a link on the way a command may point, or where it
/// would lead lies in one of `writable`, where a command could put a file for
/// what reads the place to find. `None` when it leads elsewhere.
fn dangling_into(path: &Path, writable: &[PathBuf], masked: &[PathBuf]) -> Option<anyhow::Error> {
    let lands = match walk_link(path, writable, masked) {
        Ok(Landing::NotYet(lands)) => lands,
        // There after all, as when it was made a moment ago.
        Ok(Landing::There(lands)) => {
            let root = inside_of(writable, &lands)?;
            return Some(anyhow::Error::new(DenyRefused(format!(
                "{} leads to {}, in {}, which atoms' commands may write; point the link \
                 elsewhere",
                path.display(),
                lands.display(),
                root.display()
            ))));
        }
        Err(e) => return Some(e),
    };
    let root = inside_of(writable, &lands)?;
    Some(anyhow::Error::new(DenyRefused(format!(
        "{} is a link to {}, which is not there yet and lies in {}, which atoms' commands may \
         write, so a command could put a file there for what reads {} to find; point the link \
         elsewhere, or make what it leads to first",
        path.display(),
        lands.display(),
        root.display(),
        path.display()
    ))))
}

/// Why a denied place that is not there cannot be left so: its path, walked
/// as [`walk_link`] walks it, would be made in one of `writable`, or runs
/// through a link a command may point elsewhere, so a command could make it
/// for what reads it to find. `None` when it would be made elsewhere.
fn missing_into(path: &Path, writable: &[PathBuf], masked: &[PathBuf]) -> Option<anyhow::Error> {
    let lands = match walk_link(path, writable, masked) {
        Ok(Landing::NotYet(lands) | Landing::There(lands)) => lands,
        Err(e) => return Some(e),
    };
    let root = inside_of(writable, &lands)?;
    Some(anyhow::Error::new(DenyRefused(format!(
        "{}, which is not there yet, would be made at {}, in {}, which atoms' commands may \
         write, so a command could make it for what reads {} to find; make it first, or run the \
         project from a directory that does not hold it",
        path.display(),
        lands.display(),
        root.display(),
        path.display()
    ))))
}

/// Why a link that leads somewhere now cannot be denied as what it points at
/// ([`walk_link`]): a link on the way a command may point elsewhere, as for
/// a place whose own chain runs through what the thread may write. `None`
/// when its chain is the thread's to change nowhere.
fn rewritable_chain(
    path: &Path,
    writable: &[PathBuf],
    masked: &[PathBuf],
) -> Option<anyhow::Error> {
    walk_link(path, writable, masked).err()
}

/// Why a link found inside a denied place cannot be left as it is: its chain
/// runs through a link a command may point elsewhere, or it leads nowhere
/// yet into what the thread may write, where a command could put a file for
/// what reads it to find ([`walk_link`]). `None` otherwise.
fn link_planted(link: &Path, writable: &[PathBuf], masked: &[PathBuf]) -> Option<anyhow::Error> {
    let lands = match walk_link(link, writable, masked) {
        Ok(Landing::NotYet(lands)) => lands,
        Ok(Landing::There(_)) => return None,
        Err(e) => return Some(e),
    };
    let root = inside_of(writable, &lands)?;
    Some(anyhow::Error::new(DenyRefused(format!(
        "{}, inside a place denied to atoms' commands, is a link to {}, which is not there yet \
         and lies in {}, which they may write, so a command could put a file there for what \
         reads {} to find; point the link elsewhere, or make what it leads to first",
        link.display(),
        lands.display(),
        root.display(),
        link.display()
    ))))
}

/// The first of `writable` that holds `path`, where a command may change it,
/// unless a denied place of `masked` holds it too: the sandbox hides what is
/// inside one, so no command can change it there.
fn changeable<'a>(writable: &'a [PathBuf], masked: &[PathBuf], path: &Path) -> Option<&'a PathBuf> {
    if inside_of(masked, path).is_some() {
        return None;
    }
    inside_of(writable, path)
}

/// The first of `writable` that `dir` lies inside, not at, where a command
/// may change what `..` from it climbs out of ([`changeable`]).
fn climbing_out<'a>(
    writable: &'a [PathBuf],
    masked: &[PathBuf],
    dir: &Path,
) -> Option<&'a PathBuf> {
    changeable(writable, masked, dir).filter(|root| dir != root.as_path())
}

/// The first of `dirs` that holds `path`, or is it.
fn inside_of<'a>(dirs: &'a [PathBuf], path: &Path) -> Option<&'a PathBuf> {
    dirs.iter().find(|d| path.starts_with(d))
}

/// The most links [`walk_link`] follows, as the kernel does.
const LINK_HOPS: usize = 40;

/// `path` with the links among the directories above it, and itself,
/// resolved as far as it exists, and the rest as it is named.
fn real_spelling(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            return rest.iter().rev().fold(real, |p, part| p.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

/// The first of the directories above `path` that is a link inside one of
/// `writable`, which a command could point elsewhere.
fn rewritable_link(path: &Path, writable: &[PathBuf]) -> Option<PathBuf> {
    path.ancestors().skip(1).find_map(|dir| {
        let (parent, name) = (dir.parent()?, dir.file_name()?);
        std::fs::symlink_metadata(dir)
            .is_ok_and(|m| m.file_type().is_symlink())
            .then(|| real_spelling(parent).join(name))
            .filter(|at| writable.iter().any(|w| at.starts_with(w)))
    })
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
    permission_profile_with(sandbox, guard, codex_home, cwd, writable, &[])
}

/// [`permission_profile`], for a profile that [`deny_also`] will deny
/// `also` in as well: counted toward the cap as each command's sandbox
/// helper will count them, though denied by `deny_also`.
pub(crate) fn permission_profile_with(
    sandbox: crate::typed::Sandbox,
    guard: &StateGuard,
    codex_home: &Path,
    cwd: Option<&Path>,
    writable: &[PathBuf],
    also: &[PathBuf],
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
    let (paths, globs) = denied_paths_with(private, codex_home, cwd, writable, also)?;
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

/// `profile` with `denied` denied as well: the backend's scratch
/// directories. What the thread may write inside a denied directory, such as
/// its own scratch directory, stays writable, as a writable root of its own.
///
/// These go into the profile spelled as they were handed over: their cap is
/// weighed with the list's (`denied_paths_with`), but no link in them is
/// resolved here. So a scratch or state root that is itself a link reaches
/// each command's sandbox helper as a denial that is a link, and that walk
/// denies what it points at instead (see `late_targets`). It refuses the
/// command, and the command does not run, when what it points at holds a
/// place the command needs to run, such as the command's own working
/// directory: a turn whose root lies inside such a linked root is refused
/// every command of its turn. That fails closed, and is what supporting
/// scratch or state directories moved elsewhere and linked back would have to
/// settle -- by resolving them here, before they are denied.
fn deny_also(profile: PermissionProfile, denied: &[PathBuf]) -> anyhow::Result<PermissionProfile> {
    use codex_protocol::permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry,
    };
    if denied.is_empty() {
        return Ok(profile);
    }
    let mut policy = profile.file_system_sandbox_policy();
    for dir in denied {
        let dir = AbsolutePathBuf::from_absolute_path(dir)
            .with_context(|| format!("resolve the sandbox path {}", dir.display()))?;
        policy.entries.push(FileSystemSandboxEntry::new(
            FileSystemPath::from(dir),
            FileSystemAccessMode::Deny,
        ));
    }
    Ok(PermissionProfile::from_runtime_permissions(
        &policy,
        SANDBOX_NETWORK,
    ))
}

/// Where Codex keeps what it records of a thread under `guard`, its state
/// databases and its logs, made if missing: the project's own directory in
/// the atomic home's `private/` (`<home>/private/<id>/codex`), which every
/// project's commands are denied whole, by one entry, so no command reads
/// another project's records, nor its own. Its Codex home keeps only what a
/// turn's shell needs. `None` under [`StateGuard::NoProjectState`]: the
/// records stay in the Codex home.
fn codex_records(guard: &StateGuard) -> anyhow::Result<Option<PathBuf>> {
    let StateGuard::Deny(private) = guard else {
        return Ok(None);
    };
    let id = private
        .state_dir
        .file_name()
        .context("the state directory has no name to keep its Codex records by")?;
    let records = private.private.join(id).join("codex");
    crate::state::private_dir(&records).with_context(|| format!("create {}", records.display()))?;
    Ok(Some(records))
}

/// Refuse a thread whose commands must be kept out of `denied` when `paths`
/// has no Linux sandbox helper, except on macOS (`macos`), whose own sandbox
/// denies paths itself. On Linux, Codex's sandbox reads a profile that
/// denies paths only through that helper, and nowhere else has atomic seen a
/// denial hold, so without one the thread could not be kept out of other
/// attempts' scratch directories; atomic refuses it before the thread starts
/// rather than run it with less denied. On macOS the denial is not verified.
fn refuse_denied_without_helper(
    paths: &CodexPaths,
    denied: &[PathBuf],
    macos: bool,
) -> anyhow::Result<()> {
    if !macos && !denied.is_empty() && paths.linux_sandbox().is_none() {
        let denied: Vec<_> = denied.iter().map(|d| d.display().to_string()).collect();
        bail!(
            "this turn's commands must be kept out of {}, and apart from macOS only \
             Codex's Linux sandbox helper can do that, which this process does not have; \
             run atomic itself, which provides one",
            denied.join(", ")
        );
    }
    Ok(())
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
/// a sign-in's token and the operator credential cannot be dumped, or leave
/// a core file, and no process of the same user may read its memory or the
/// /proc files that show it, such as `environ` and `mem`, unless it has
/// `CAP_SYS_PTRACE` over atomic's user namespace; its `cmdline` and `stat`
/// stay readable. It loses its LD_* variables, and the commands it starts inherit a core-file limit of
/// zero (on macOS it also refuses a debugger). Both entry points call it.
fn harden() {
    if !is_helper(std::env::args_os()) {
        codex_process_hardening::pre_main_hardening();
    }
}

/// Harden `atomic runner` as [`dispatch`] hardens atomic: the commands it
/// runs arrive on its stdin with their environments, and a confined one's
/// proxy token on a socket beside it, so no process of the same user may
/// read its memory or the /proc files that show it, such as `environ` and
/// `mem`, unless it has `CAP_SYS_PTRACE` over the runner's user namespace;
/// its `cmdline` and `stat` stay readable. Call it first, before the request
/// or the socket is read; the socket's reader refuses while the runner is
/// still dumpable. Until it has run, the request is exposed in the stdin
/// pipe: another process of the same user can reopen that pipe through /proc
/// and read the request, its program, arguments, directory and whole
/// environment among it, or replace it. When a secret was handed, the
/// socket's digest refuses a replaced request; it does not stop one being
/// read, and with no secret handed there is no digest to refuse with.
pub fn harden_runner() {
    // The runner holds commands' environments, proxy tokens among them.
    codex_process_hardening::pre_main_hardening();
}

/// Point this process's `TMPDIR` at `tmp`, the shared temporary directory of
/// the backend its turns run on ([`atomic_exec::LocalBackend::shared_tmp`]).
///
/// Codex's sandbox keeps a registry of the mount points it makes in a turn's
/// writable roots, such as the empty `.git` it mounts read-only where there
/// is none, in its helper's `TMPDIR`, so that a sandbox never removes one
/// another sandbox is still using. A turn's commands give their helper the
/// backend's shared directory. Codex's file helper, which applies a turn's
/// patches under the same sandbox, takes its `TMPDIR` from this process
/// instead: pointed here, the two keep one registry. Elsewhere than Linux
/// Codex keeps no such registry, and this does nothing.
///
/// Whatever this process and its helpers keep in their temporary directory
/// is then kept there, which connecting the backend empties: give it only
/// for a command that runs turns on that backend, before it connects.
///
/// Call it in `main` before [`dispatch`], while the process has one thread.
pub fn share_sandbox_registry(tmp: &Path) {
    #[cfg(target_os = "linux")]
    // SAFETY: called at the start of main, while the process has one thread,
    // so no other thread can read the environment concurrently.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("TMPDIR", tmp);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = tmp;
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
    // Before anything that could re-execute this process: see [`as_any_helper`].
    as_any_helper();
    harden();
    // The person's own Codex home stays denied to atoms' commands.
    let _ = PERSON_CODEX_HOME.set(std::env::var_os("CODEX_HOME"));
    // SAFETY: called at the start of main, while the process has one thread,
    // so no other thread can read the environment concurrently.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("CODEX_HOME", codex_home);
    }
    scrub_overrides();
    let links = hold_helper_links(codex_home);
    #[cfg(target_os = "linux")]
    let helper = own_helper_link(codex_home);
    arg0_dispatch_or_else(move |paths| {
        drop(links);
        // Codex's sandbox runs atomic's own link to itself as its helper,
        // which every bubblewrap can run by its path; without one, Codex's.
        #[cfg(target_os = "linux")]
        let paths = match helper {
            Some(helper) => Arg0DispatchPaths {
                codex_linux_sandbox_exe: Some(helper),
                ..paths
            },
            None => paths,
        };
        main_fn(CodexPaths(paths))
    })
}

/// Whether `path`, in a Codex home's `tmp/`, is anything but the real directory
/// of atomic's own link to itself, which every sandbox must run.
/// `scripts/controls/sandbox-helper-link-readable.row` switches this off.
fn not_the_helpers_link(path: &Path) -> bool {
    let named = path.file_name() == Some(HELPER_DIR.as_ref());
    // Only a real directory by that name: a link planted under it is denied
    // as what it points at, as every other entry is.
    // `scripts/controls/sandbox-helper-link-real-dir.row` switches this off.
    let real_dir = std::fs::symlink_metadata(path)
        .ok()
        .as_ref()
        .map(std::fs::Metadata::is_dir)
        == Some(true);
    !(named && real_dir)
}

/// The directory below a Codex home's `tmp/` where atomic keeps its own
/// link to itself under the Linux sandbox helper's name.
const HELPER_DIR: &str = "atomic-helper";

/// atomic's own link to this executable under the Linux sandbox helper's
/// name, `<codex_home>/tmp/atomic-helper/<key>/codex-linux-sandbox`, made
/// or mended now, while this process has one thread: `None` when it cannot
/// be, and when what stands on the way is not what atomic made.
///
/// Codex runs it as the helper, by that absolute path, for every turn's and
/// exec atom's command. A bubblewrap without `--argv0` runs the helper's
/// inner stage by the name its outer stage was started under, and this one
/// is a path it can run inside the sandbox, never a name it looks for along
/// PATH. The directory is readable by every sandbox and writable by none:
/// atoms' commands never write the Codex home, and this directory, like
/// Codex's own links, is left out of what is denied them (see
/// [`denied_paths`]). `<key>` names this executable's path, so processes run
/// from two paths sharing a Codex home never run each other's.
///
/// `tmp`, `atomic-helper` and `<key>` are opened one at a time, never
/// through a link, each made only when it is not there, and each, like the
/// Codex home, must be a directory of this user's that no one else may
/// write: one of this user's that others may write, as a umask such as 002
/// leaves one, is narrowed first, and anything else falls back to Codex's
/// own helper. The link is made in the `<key>` directory held open, beside
/// its place, and renamed into it.
#[cfg(target_os = "linux")]
fn own_helper_link(codex_home: &Path) -> Option<PathBuf> {
    use sha2::Digest as _;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let exe = std::env::current_exe().ok()?;
    let key = sha2::Sha256::digest(exe.as_os_str().as_bytes());
    let key: String = key[..8].iter().map(|b| format!("{b:02x}")).collect();
    let home = std::ffi::CString::new(codex_home.as_os_str().as_bytes()).ok()?;
    // SAFETY: `home` is valid for the call; the result is owned below.
    #[allow(unsafe_code)]
    let fd = unsafe {
        libc::open(
            home.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` was just returned open, and nothing else owns it.
    #[allow(unsafe_code)]
    let mut dir = unsafe { OwnedFd::from_raw_fd(fd) };
    if !tightened(&dir) || !private_dir(&dir) {
        return None;
    }
    for part in ["tmp", HELPER_DIR, key.as_str()] {
        let name = std::ffi::CString::new(part).ok()?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        // SAFETY: `name` is valid for the calls, and `dir` is open.
        #[allow(unsafe_code)]
        let mut fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        let mut made = false;
        if fd < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            // SAFETY: as above.
            #[allow(unsafe_code)]
            let r = unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) };
            made = r == 0;
            // SAFETY: as above.
            #[allow(unsafe_code)]
            {
                fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
            }
        }
        if fd < 0 {
            return None;
        }
        // SAFETY: `fd` was just returned open, and nothing else owns it.
        #[allow(unsafe_code)]
        let next = unsafe { OwnedFd::from_raw_fd(fd) };
        if made {
            // SAFETY: fchmod takes an open descriptor and a mode.
            #[allow(unsafe_code)]
            unsafe {
                libc::fchmod(next.as_raw_fd(), 0o700);
            }
        }
        if !tightened(&next) || !private_dir(&next) {
            return None;
        }
        dir = next;
    }
    let link = codex_home
        .join("tmp")
        .join(HELPER_DIR)
        .join(&key)
        .join("codex-linux-sandbox");
    let name = std::ffi::CString::new("codex-linux-sandbox").ok()?;
    let mut leads = vec![0u8; 4096];
    // SAFETY: `leads` is valid for its length, and `dir` is open.
    #[allow(unsafe_code)]
    let n = unsafe {
        libc::readlinkat(
            dir.as_raw_fd(),
            name.as_ptr(),
            leads.as_mut_ptr().cast(),
            leads.len(),
        )
    };
    if n >= 0 && &leads[..n as usize] == exe.as_os_str().as_bytes() {
        return Some(link);
    }
    // Made beside it and renamed over it, so another process never finds
    // it half made, and one leading elsewhere is replaced.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp = std::ffi::CString::new(format!(
        ".codex-linux-sandbox.{}.{nanos}",
        std::process::id()
    ))
    .ok()?;
    let target = std::ffi::CString::new(exe.as_os_str().as_bytes()).ok()?;
    // SAFETY: both strings are valid for the calls, and `dir` is open.
    #[allow(unsafe_code)]
    unsafe {
        if libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), temp.as_ptr()) != 0 {
            return None;
        }
        if libc::renameat(
            dir.as_raw_fd(),
            temp.as_ptr(),
            dir.as_raw_fd(),
            name.as_ptr(),
        ) != 0
        {
            libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0);
            return None;
        }
    }
    Some(link)
}

/// Take group and other write off the directory held open as `dir` when it
/// is this user's own, as a umask such as 002 leaves one another program
/// made: `false` only when that fails. One that is not this user's is left
/// as it is, for [`private_dir`] to refuse.
/// `scripts/controls/sandbox-helper-link-tighten.row` switches this off.
#[cfg(target_os = "linux")]
fn tightened(dir: &std::os::fd::OwnedFd) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: a zeroed stat is a valid value of a plain C struct.
    #[allow(unsafe_code)]
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is valid for the call, and `dir` is open.
    #[allow(unsafe_code)]
    if unsafe { libc::fstat(dir.as_raw_fd(), &mut st) } != 0 {
        return false;
    }
    // SAFETY: geteuid takes nothing and cannot fail.
    #[allow(unsafe_code)]
    let ours = st.st_uid == unsafe { libc::geteuid() };
    let narrow = ours && st.st_mode & 0o022 != 0;
    if !narrow {
        return true;
    }
    // SAFETY: fchmod takes an open descriptor and a mode.
    #[allow(unsafe_code)]
    let r = unsafe { libc::fchmod(dir.as_raw_fd(), st.st_mode & 0o7777 & !0o022) };
    r == 0
}

/// Whether the directory held open as `dir` is this user's own, which no
/// one else may write. `scripts/controls/sandbox-helper-link-private.row`
/// switches this off.
#[cfg(target_os = "linux")]
fn private_dir(dir: &std::os::fd::OwnedFd) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: a zeroed stat is a valid value of a plain C struct.
    #[allow(unsafe_code)]
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is valid for the call, and `dir` is open.
    #[allow(unsafe_code)]
    if unsafe { libc::fstat(dir.as_raw_fd(), &mut st) } != 0 {
        return false;
    }
    // SAFETY: geteuid takes nothing and cannot fail.
    #[allow(unsafe_code)]
    let me = unsafe { libc::geteuid() };
    st.st_mode & libc::S_IFMT == libc::S_IFDIR && st.st_uid == me && st.st_mode & 0o022 == 0
}

/// How long one atomic process waits for another to finish making Codex's
/// links in the same Codex home before it makes its own regardless.
const HELPER_LINKS_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// A lock on making Codex's links to atomic under its helpers' names, in a
/// file of `codex_home` that atoms' commands are denied under
/// [`StateGuard::Deny`], held from just
/// before Codex's dispatch until the command itself starts: or none, when
/// it cannot be taken within [`HELPER_LINKS_WAIT`] or made at all.
///
/// Codex makes those links in a new directory of its home, after sweeping
/// away every such directory no running process holds, and a directory
/// another process is still making is not yet held: two atomic processes
/// starting at once in one Codex home can take each other's away, and the
/// one that loses warns that it "could not create PATH aliases" and runs
/// atomic itself as its helper (see [`run_as_linux_sandbox`]). One at a time
/// they cannot. The same warning has other causes this does not touch: a
/// Codex home under the system's temporary directory, which a release build
/// of Codex refuses to make links in, and a home that cannot be written or
/// is full.
fn hold_helper_links(codex_home: &Path) -> Option<std::fs::File> {
    let dir = codex_home.join("tmp");
    // Private, as atomic's own link to itself requires of it, whatever the
    // umask (see `own_helper_link`).
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
        .create(&dir)
        .ok()?;
    #[cfg(not(unix))]
    std::fs::create_dir_all(&dir).ok()?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("atomic-helper-links.lock"))
        .ok()?;
    let deadline = std::time::Instant::now() + HELPER_LINKS_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => return Some(lock),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
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
    // Codex's Linux sandbox helper run as this executable under its own
    // name, as Codex runs it when it could not make its link to atomic
    // under the helper's name (see [`run_as_linux_sandbox`]).
    #[cfg(target_os = "linux")]
    if helper_arguments(&first, args.next().as_deref()) {
        return true;
    }
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
    // The outer stage, started as atomic itself or by the helper's bare
    // name, as Codex starts it when it could not link atomic under that
    // name, runs again under one that is a path: a bubblewrap without
    // `--argv0` runs the inner stage by the outer stage's own name, and a
    // bare one it would look for along PATH. The inner stage is left as it
    // is, already past bubblewrap.
    #[cfg(target_os = "linux")]
    if linux_sandbox_args(std::env::args_os())
        && !is_inner_stage(std::env::args_os())
        && (!invoked_as_linux_sandbox() || started_by_relative_name())
    {
        return Err(run_as_linux_sandbox());
    }
    // A helper of another system's, which Codex's dispatch here would not
    // take, goes no further: nothing of a command's start is made for it.
    if let Some(first) = std::env::args_os().nth(1)
        && OTHER_SYSTEMS_HELPERS.iter().any(|h| first == *h)
    {
        bail!(
            "{} names a Codex helper of another operating system",
            first.to_string_lossy()
        );
    }
    as_any_helper();
    arg0_dispatch_or_else(|_| async {
        bail!("this process was started as a Codex helper Codex does not know")
    })
}

/// When this process is the outer stage of Codex's Linux sandbox helper,
/// starting one command, deny what links made since its profile's deny list
/// was built point at (see [`late_targets`]), or exit. A turn's list
/// is built when its thread starts, and each of its commands starts the
/// helper with it: a link a later process made inside a denied place, such
/// as `~/.aws/credentials` leading to a file kept elsewhere, would leave
/// that file readable by its own path. When there is anything to add, the
/// helper starts again with the larger list, and finds nothing more.
///
/// What it cannot see: a link made between this walk and the sandbox's
/// start, a moment later, which that command may then read through; and a
/// link made while a long-lived command, such as a turn's unified-exec
/// session, keeps running, since the walk is made once as each command
/// starts. Codex's own file helper, which it starts for a turn's file
/// operations with only PATH, TMPDIR, TMP and TEMP, is not handed
/// [`PLACES`]: it walks the places inferred from the list's entries, which
/// hold no place that was a link when the list was built, so for it such a
/// place is not followed again: one pointed elsewhere since, one that led
/// nowhere then and leads somewhere now, and new links inside one that
/// leads to a directory never denied.
fn deny_links_made_since_the_list() {
    #[cfg(target_os = "linux")]
    if invoked_as_linux_sandbox() && !is_inner_stage(std::env::args_os()) {
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        // Started again once already: what it adds is its last chance.
        let again_already = take_env(RELISTED).is_some();
        // Started again, it must not have been handed the proxy token in its
        // environment: the token goes to the helper only on a socket, read
        // after this walk, so a token here means it was read before a start
        // again put it in the environment of a process that was dumpable
        // until its first step. The command does not run.
        if again_already
            && std::env::var_os(codex_network_proxy::PROXY_ATTRIBUTION_TOKEN_ENV_KEY).is_some()
        {
            crate::log_line!(
                "atomic: the sandbox helper was started again with the proxy token in its \
                 environment, where another process could have read it; the command does not run"
            );
            std::process::exit(1);
        }
        let carried = take_env(PLACES);
        let places = match carried.as_ref().map(|v| {
            v.to_str()
                .and_then(|t| serde_json::from_str::<Vec<PathBuf>>(t).ok())
        }) {
            None => None,
            Some(Some(places)) => Some(places),
            Some(None) => {
                crate::log_line!(
                    "atomic: the places denied to atoms' commands came to the sandbox helper in a \
                     form it cannot read; the command does not run"
                );
                std::process::exit(1);
            }
        };
        match relisted(&args, places.as_deref()) {
            Ok(None) => {}
            Ok(Some(_)) if again_already => {
                crate::log_line!(
                    "atomic: links in the places denied to atoms' commands, or their mounts, kept \
                     changing while the sandbox started; the command does not run"
                );
                std::process::exit(1);
            }
            Ok(Some(again)) => {
                use std::os::unix::process::CommandExt;
                let started = std::env::current_exe().map(|exe| {
                    let mut again_cmd = std::process::Command::new(exe);
                    again_cmd
                        .arg0(&again[0])
                        .args(&again[1..])
                        .env(RELISTED, "1");
                    if let Some(carried) = &carried {
                        again_cmd.env(PLACES, carried);
                    }
                    again_cmd.exec()
                });
                // `exec` returns only when it failed.
                crate::log_line!(
                    "atomic: cannot start the sandbox helper again with the links made since its \
                     deny list was built denied: {}",
                    match started {
                        Ok(e) | Err(e) => e,
                    }
                );
                std::process::exit(1);
            }
            Err(e) => {
                crate::log_line!("atomic: {e:#}");
                std::process::exit(1);
            }
        }
    }
}

/// Set on the sandbox helper started again with more denied, and taken off
/// before anything else sees it. An atom cannot declare it.
pub(crate) const RELISTED: &str = "ATOMIC_SANDBOX_RELISTED";

/// Carries the places a command's deny list was built with, as JSON, to the
/// sandbox helper that starts the command, which walks them again (see
/// [`late_targets`]) and takes it off before anything else sees it. An
/// atom cannot declare it.
pub(crate) const PLACES: &str = "ATOMIC_SANDBOX_PLACES";

/// The places of `guard`'s list, as [`PLACES`] carries them; `None` when it
/// denies nothing, and an error when a place is not UTF-8.
fn places_text(guard: &StateGuard) -> anyhow::Result<Option<String>> {
    let StateGuard::Deny(private) = guard else {
        return Ok(None);
    };
    let places: Vec<&str> = private
        .also
        .iter()
        .map(|p| {
            p.to_str()
                .with_context(|| format!("{} is not UTF-8", p.display()))
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(Some(serde_json::to_string(&places)?))
}

/// When this process is the outer stage of Codex's Linux sandbox helper,
/// make it non-dumpable, or exit. The outer stage stays outside the sandbox
/// while the command runs, and reads the command's proxy token from the
/// socket it was handed once this is done ([`take_secret_env`]): from then
/// on no process of the same user without `CAP_SYS_PTRACE` over its user
/// namespace can read its memory or environment through /proc. Exec resets
/// it, so a helper started again comes here again, before it reads the
/// token; where the outer stage has no mounts of its own to manage it execs
/// bubblewrap itself, by which time Codex's helper has taken the token out
/// of its environment, and the bridges it forked first keep the token,
/// non-dumpable.
fn helper_not_dumpable() {
    #[cfg(target_os = "linux")]
    if invoked_as_linux_sandbox()
        && !is_inner_stage(std::env::args_os())
        && let Err(e) = not_dumpable()
    {
        crate::log_line!(
            "atomic: cannot make the sandbox helper, which is about to read the command's proxy \
             token, non-dumpable: {e}"
        );
        std::process::exit(1);
    }
}

/// Take `name` out of this process's environment, and what it held.
#[cfg(target_os = "linux")]
fn take_env(name: &str) -> Option<std::ffi::OsString> {
    let value = std::env::var_os(name)?;
    // SAFETY: the helper is single-threaded here, before it starts anything.
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var(name);
    }
    Some(value)
}

/// The helper's arguments `args`, with what links made since its profile's
/// deny list was built point at denied in it; `None` when there is nothing
/// to add, or no profile to add it to.
#[cfg(any(target_os = "linux", test))]
fn relisted(
    args: &[std::ffi::OsString],
    places: Option<&[PathBuf]>,
) -> anyhow::Result<Option<Vec<std::ffi::OsString>>> {
    use codex_protocol::permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxKind,
    };
    let options: Vec<&std::ffi::OsString> =
        args.iter().skip(1).take_while(|a| an_option(a)).collect();
    let value = |flag: &str| {
        options
            .iter()
            .position(|a| *a == flag)
            .and_then(|at| options.get(at + 1))
            .map(|v| (*v).clone())
    };
    let Some(profile) = value("--permission-profile") else {
        return Ok(None);
    };
    let at = 1
        + options
            .iter()
            .position(|a| *a == "--permission-profile")
            .unwrap_or_default()
        + 1;
    let text = profile
        .to_str()
        .context("the sandbox helper's profile is not UTF-8")?;
    let profile: PermissionProfile =
        serde_json::from_str(text).context("read the sandbox helper's profile")?;
    let (mut policy, network) = profile.to_runtime_permissions();
    if policy.kind != FileSystemSandboxKind::Restricted {
        return Ok(None);
    }
    let paths = |mode: FileSystemAccessMode| -> Vec<PathBuf> {
        policy
            .entries
            .iter()
            .filter(|e| e.access == mode)
            .filter_map(|e| match &e.path {
                FileSystemPath::Path { path } => Some(path.to_path_buf()),
                _ => None,
            })
            .collect()
    };
    let denied = paths(FileSystemAccessMode::Deny);
    let mut writable = paths(FileSystemAccessMode::Write);
    let cwd: Vec<PathBuf> = ["--command-cwd", "--sandbox-policy-cwd"]
        .iter()
        .filter_map(|flag| value(flag).map(PathBuf::from))
        .collect();
    // The command's directory is writable when the profile lets it write
    // the workspace, which it names only as such.
    let workspace_writable = policy.entries.iter().any(|e| {
        e.access == FileSystemAccessMode::Write
            && matches!(&e.path, FileSystemPath::Special { value }
                if matches!(value, codex_protocol::permissions::FileSystemSpecialPath::ProjectRoots { .. }))
    });
    if workspace_writable {
        writable.extend(cwd.iter().cloned());
    }
    // What the command needs: what it may write, where it works, what the
    // profile lets it read by name (Codex's home), and atomic itself.
    let mut needed = writable.clone();
    needed.extend(cwd);
    needed.extend(paths(FileSystemAccessMode::Read));
    needed.extend(std::env::current_exe().ok());
    // The directories whose databases the list denies by a pattern.
    let homes: Vec<PathBuf> = policy
        .entries
        .iter()
        .filter(|e| e.access == FileSystemAccessMode::Deny)
        .filter_map(|e| match &e.path {
            FileSystemPath::GlobPattern { pattern } => pattern
                .strip_suffix(SQLITE_FILES)
                .and_then(|dir| dir.strip_suffix('/'))
                .map(PathBuf::from),
            _ => None,
        })
        .collect();
    let late = late_targets(&denied, places, &homes, &needed, &writable)?;
    if late.targets.is_empty() && late.replaced.is_empty() && late.globs.is_empty() {
        return Ok(None);
    }
    // A place that has become a link is denied as what it points at, as
    // when the list is built: the sandbox cannot mount on a link. A denial
    // inside one now made is denied with it: the sandbox cannot mount one
    // inside another.
    let gone = |path: &Path| {
        let replaced = late.replaced.iter().any(|r| r == path);
        let inside_new = inside_any(path, &late.targets);
        replaced || inside_new
    };
    policy.entries.retain(|e| {
        !(e.access == FileSystemAccessMode::Deny
            && matches!(&e.path, FileSystemPath::Path { path } if gone(&path.to_path_buf())))
    });
    for target in late.targets {
        let target = AbsolutePathBuf::from_absolute_path(&target)
            .with_context(|| format!("resolve the sandbox path {}", target.display()))?;
        policy.entries.push(FileSystemSandboxEntry::new(
            FileSystemPath::from(target),
            FileSystemAccessMode::Deny,
        ));
    }
    for pattern in late.globs {
        policy.entries.push(FileSystemSandboxEntry::new(
            FileSystemPath::GlobPattern { pattern },
            FileSystemAccessMode::Deny,
        ));
    }
    let profile = PermissionProfile::from_runtime_permissions(&policy, network);
    let mut again = args.to_vec();
    again[at] = serde_json::to_string(&profile)
        .context("encode the sandbox profile")?
        .into();
    Ok(Some(again))
}

/// What the link `link` leads to; `None` when that is not there, as for a
/// link made before what it points at, and refused when it cannot be
/// resolved for any other reason, such as a loop or a filesystem that has
/// gone away, since then what it leads to is unknown.
fn resolved(link: &Path) -> anyhow::Result<Option<PathBuf>> {
    match std::fs::canonicalize(link) {
        Ok(target) => Ok(Some(target)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(DenyRefused(format!(
            "{} is a link atomic cannot resolve ({e}), so it cannot tell what to deny",
            link.display()
        ))
        .into()),
    }
}

/// What [`late_targets`] found to deny since the list was built.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Default, PartialEq, Eq)]
struct Late {
    /// What links made since point at, to deny.
    targets: Vec<PathBuf>,
    /// Denied places that have become links, whose denial is now that of
    /// what they point at.
    replaced: Vec<PathBuf>,
    /// Patterns for Codex's databases at the second mounts of its home made
    /// since, to deny.
    globs: Vec<String>,
}

/// [`late_targets`] with no directories of databases to watch: what the
/// tests of the walk itself call.
#[cfg(test)]
fn late_link_targets(
    denied: &[PathBuf],
    places: Option<&[PathBuf]>,
    needed: &[PathBuf],
    writable: &[PathBuf],
) -> anyhow::Result<Late> {
    late_targets(denied, places, &[], needed, writable)
}

/// What links made since a list was built point at, by the rules the list
/// is built with. The list's places are `places`, as its builder carried
/// them, or, where it carried none, the paths of `denied` other than those
/// it denies as what a link points at, or as a second mount of one. A link
/// up to [`LINK_DEPTH`] levels inside a place, or a place that is now a
/// link, is followed; one that a denial covers already, or that
/// [`deniable`] leaves alone, needs nothing, and what is found is denied at
/// its second mounts too, as is each second mount of what `denied` holds
/// made since the list was built, and the databases at each new second
/// mount of `homes`, the directories whose databases the list denies by a
/// pattern (Codex's home and its second mounts as the list was built)
/// ([`Late::globs`]), from one read of the mount table
/// (two where the places are inferred, which needs the table too). What
/// `denied` holds is counted from each side, as when the list is built: the
/// second mounts of its second mounts among them, which is the count
/// `denied_paths_with` weighs a list against the cap with, so that a list a
/// thread starts with is not refused by its own commands. Nothing
/// runs when a place, or a new second mount, now leads to what the command
/// needs (`needed`), or two or more levels inside a directory of
/// `writable`, where a command could move it out from under its denial;
/// when a link inside a place does; when a denied entry or a place has
/// become a link it cannot resolve for a reason other than leading nowhere
/// ([`resolved`]); or when the links the places hold and those second
/// mounts come to more than [`MAX_LINK_TARGETS`].
#[cfg(any(target_os = "linux", test))]
fn late_targets(
    denied: &[PathBuf],
    places: Option<&[PathBuf]>,
    homes: &[PathBuf],
    needed: &[PathBuf],
    writable: &[PathBuf],
) -> anyhow::Result<Late> {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    // A denied link covers nothing: the list never holds one, so it is a
    // place that has become one since, whose denial is replaced below.
    let covered: Vec<PathBuf> = denied
        .iter()
        .filter(|p| !std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
        .map(|p| real(p))
        .collect();
    let needed: Vec<PathBuf> = needed.iter().map(|p| real(p)).collect();
    let writable: Vec<PathBuf> = writable.iter().map(|p| real(p)).collect();
    let places: Vec<PathBuf> = match places {
        Some(places) => places.to_vec(),
        None => {
            // Entries the list denies as what a link points at, or as a
            // second mount of one, were never its places, and are not walked.
            let followed: Vec<PathBuf> = followed_from(denied).into_iter().collect();
            let aliases = crate::mounts::second_mounts(&[&followed])
                .map_err(|e| DenyRefused(format!("{e:#}")))?;
            denied
                .iter()
                .filter(|p| {
                    let p = real(p);
                    let found = followed.contains(&p);
                    let shown = aliases[0].contains(&p);
                    !(found || shown)
                })
                .cloned()
                .collect()
        }
    };
    let refuse = |link: &Path, target: &Path| -> anyhow::Result<()> {
        if let Some(holds) = held_in(&needed, target) {
            bail!(DenyRefused(format!(
                "{} now leads to {}, which holds {}, which this command needs; atomic cannot \
                 deny it",
                link.display(),
                target.display(),
                holds.display()
            )));
        }
        if let Some(root) = deep_inside(&writable, target) {
            bail!(DenyRefused(format!(
                "{} now leads to {}, two or more levels inside {}, which this command may \
                 write, so it could move it out from under its denial",
                link.display(),
                target.display(),
                root.display()
            )));
        }
        Ok(())
    };
    let mut late = Late::default();
    // A denied entry that is now a link, however it is spelled: the list
    // never holds one, and the sandbox cannot mount on one. Its denial is
    // replaced by that of what it points at, held to the same rules.
    for entry in denied {
        if !std::fs::symlink_metadata(entry).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        late.replaced.push(entry.clone());
        let Some(target) = resolved(entry)? else {
            continue;
        };
        refuse(entry, &target)?;
        if deniable(&target, true) && !covered.iter().any(|c| target.starts_with(c)) {
            late.targets.push(target);
        }
    }
    // The places as the list's builder takes them, which what is found
    // inside needs no denial of its own beside.
    let inside = as_built(&places);
    // Every link target the places hold outside them, old and new: what the
    // cap counts, as the list's builder counts it.
    let mut held = std::collections::BTreeSet::new();
    for place in &places {
        let Ok(meta) = std::fs::symlink_metadata(place) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            // A place that is a link: refused when it leads to what the
            // command needs, whatever that is, or deep inside a directory it
            // may write, as when the list is built.
            let Some(target) = resolved(place)? else {
                continue;
            };
            refuse(place, &target)?;
            if deniable(&target, true) && !covered.iter().any(|c| target.starts_with(c)) {
                late.targets.push(target.clone());
            }
            // Its links are followed through it, as the list follows a
            // place's.
            if !target.is_dir() {
                continue;
            }
        } else if !meta.is_dir() {
            continue;
        }
        let mut links = Vec::new();
        links_in(place, LINK_DEPTH, &mut links);
        for link in links {
            let Ok(target) = std::fs::canonicalize(&link) else {
                continue;
            };
            if !deniable(&target, false) || inside.iter().any(|p| target.starts_with(p)) {
                continue;
            }
            held.insert(target.clone());
            if covered.iter().any(|c| target.starts_with(c)) {
                continue;
            }
            refuse(&link, &target)?;
            late.targets.push(target);
        }
    }
    collapse(&mut late.targets);
    // One read of the mount table as the command starts: the second mounts
    // of what is new, those of what the list denies already, which a mount
    // made since the list was built may have added to, and those of Codex's
    // home, whose databases a pattern denies at each. Each is held to the
    // rules the list's builder holds a second mount to.
    let found = crate::mounts::second_mounts(&[&late.targets, &covered, homes])
        .map_err(|e| DenyRefused(format!("{e:#}")))?;
    let refuse_mount = |alias: &Path| -> anyhow::Result<()> {
        if let Some(holds) = held_in(&needed, alias) {
            bail!(DenyRefused(format!(
                "{} is a second mount of what this command is denied, and holds {}, which it \
                 needs; atomic cannot deny it there",
                alias.display(),
                holds.display()
            )));
        }
        if let Some(root) = deep_inside(&writable, alias) {
            bail!(DenyRefused(format!(
                "{} is a second mount of what this command is denied, two or more levels inside \
                 {}, which it may write, so it could move it out from under its denial",
                alias.display(),
                root.display()
            )));
        }
        Ok(())
    };
    // Counted as the list's builder counts them (see `denied_paths`).
    let aliases = found[0].len();
    let mounted = found[1].len();
    let home_mounts = found[2].len();
    // A second mount a denial covers already needs none of its own, and one
    // the list was built with was held to these rules then: only what is new
    // is held to them now.
    let uncovered = |a: &PathBuf| !covered.iter().any(|c| real(a).starts_with(c));
    let mut found = found.into_iter();
    let mut of_new: Vec<PathBuf> = found.next().unwrap_or_default();
    of_new.retain(uncovered);
    let mut since: Vec<PathBuf> = found.next().unwrap_or_default();
    since.retain(uncovered);
    let made_since = since.len();
    // A second mount of Codex's home its databases are not denied at yet.
    let known: Vec<PathBuf> = homes.iter().map(|h| real(h)).collect();
    let new_homes: Vec<PathBuf> = found
        .next()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| !known.contains(&real(a)))
        .collect();
    for alias in &of_new {
        refuse_mount(alias)?;
    }
    for alias in &since {
        refuse_mount(alias)?;
    }
    for alias in &new_homes {
        refuse_mount(alias)?;
    }
    let mut new_globs = Vec::new();
    for home in &new_homes {
        new_globs.push(sqlite_glob(home).map_err(|e| DenyRefused(format!("{e:#}")))?);
    }
    late.globs.extend(new_globs);
    late.targets.extend(of_new);
    late.targets.extend(since);
    collapse(&mut late.targets);
    let count = held.len() + aliases + mounted + home_mounts;
    if count > MAX_LINK_TARGETS {
        bail!(DenyRefused(format!(
            "the places denied to atoms' commands now hold {} links to files elsewhere, with \
             {aliases} second mounts of what the new ones point at, {mounted} second mounts of \
             what they deny ({made_since} made since the list was built) and {home_mounts} of \
             Codex's home, more than the {MAX_LINK_TARGETS} atomic follows",
            held.len()
        )));
    }
    Ok(late)
}

/// `paths` sorted, each once, with any inside another dropped: the sandbox
/// cannot mount a denial inside another, and the outer one denies it.
#[cfg(any(target_os = "linux", test))]
fn collapse(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
    let all = std::mem::take(paths);
    for p in &all {
        if !inside_any(p, &all) {
            paths.push(p.clone());
        }
    }
}

/// The places a list is built with, resolved, which what is found inside
/// them needs no denial of its own beside: a place as it is, or what a
/// place that is a link points at, when that is denied. A place not there
/// is none.
fn as_built(places: &[PathBuf]) -> Vec<PathBuf> {
    places
        .iter()
        .filter_map(|p| {
            let meta = std::fs::symlink_metadata(p).ok()?;
            let target = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
            (!meta.file_type().is_symlink() || deniable(&target, true)).then_some(target)
        })
        .collect()
}

/// Whether `path` lies inside another of `all`.
#[cfg(any(target_os = "linux", test))]
fn inside_any(path: &Path, all: &[PathBuf]) -> bool {
    all.iter().any(|q| q != path && path.starts_with(q))
}

/// What links up to [`LINK_DEPTH`] levels inside any directory of `denied`
/// point at.
#[cfg(any(target_os = "linux", test))]
fn followed_from(denied: &[PathBuf]) -> std::collections::BTreeSet<PathBuf> {
    let mut followed = std::collections::BTreeSet::new();
    for path in denied {
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            let mut links = Vec::new();
            links_in(path, LINK_DEPTH, &mut links);
            followed.extend(links.iter().filter_map(|l| std::fs::canonicalize(l).ok()));
        }
    }
    followed
}

/// The first of `dirs` that `path` holds.
#[cfg(any(target_os = "linux", test))]
fn held_in<'a>(dirs: &'a [PathBuf], path: &Path) -> Option<&'a PathBuf> {
    dirs.iter().find(|d| d.starts_with(path))
}

/// The first of `dirs` that holds `path` two or more levels inside it,
/// where a command that may write it could rename a directory above `path`.
fn deep_inside<'a>(dirs: &'a [PathBuf], path: &Path) -> Option<&'a PathBuf> {
    dirs.iter().find(|d| {
        path.strip_prefix(d)
            .is_ok_and(|rest| rest.components().count() >= 2)
    })
}

/// The first arguments [`is_helper`] knows that name a helper Codex's dispatch
/// runs only on another operating system than this one.
const OTHER_SYSTEMS_HELPERS: &[&str] = if cfg!(windows) {
    &["--codex-run-as-process-setup"]
} else if cfg!(target_os = "linux") {
    &["--__codex-windows-mxc", "--run-as-windows-sandbox"]
} else {
    &[
        "--__codex-windows-mxc",
        "--run-as-windows-sandbox",
        "--codex-run-as-process-setup",
    ]
};

/// What [`dispatch`] and [`run_helper`] both do first, whichever of Codex's
/// helpers this process turns out to be. First, and before the walk, the
/// sandbox helper's outer stage is made non-dumpable (which reads nothing),
/// so no other process of the user's reads it while it walks; a start again
/// through `exec` makes it dumpable, and comes here again. Then the walk for
/// links made since its deny list was built, which may start it again, and
/// so comes before anything is read into this process: the socket that
/// carries the command's proxy token passes through a start again unread.
/// Then keep the sandbox off keyrings, which reads that token
/// ([`take_secret_env`]) once nothing more starts the helper again, so it is
/// never left in a process that is dumpable again; and give a sandboxed
/// command its own `TMPDIR`.
fn as_any_helper() {
    helper_not_dumpable();
    // The walk may start the helper again, so it comes before the token is
    // read: read first, the token would pass through `exec` in the
    // environment of a process that is dumpable until its first step, which
    // the walk then refuses.
    deny_links_made_since_the_list();
    keep_sandbox_off_keyrings();
    give_the_command_its_own_tmpdir();
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
/// outside the sandbox while the command runs, was made non-dumpable first
/// ([`helper_not_dumpable`]), and only now reads the command's proxy token
/// from the socket it was handed ([`take_secret_env`]): the token is never
/// in its exec environment, and once it is in its memory no process of the
/// same user without `CAP_SYS_PTRACE` over its user namespace can newly
/// read it through /proc or attach to it. Between its exec and then, the
/// socket waits unread; it cannot be reopened through /proc, so only a
/// process that may attach to the helper can take it, and one that
/// attached then, as Yama's `ptrace_scope` 0 allows, keeps its access
/// afterwards: the window is narrowed, not closed, at `ptrace_scope` 0.
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
/// descriptor that is not one, any other variable, or an empty token fails
/// it. Called once it is non-dumpable, so from then on no other process of
/// its user without `CAP_SYS_PTRACE` over its user namespace can newly read
/// the token from it. Before that, from its exec, the socket cannot be
/// reopened through /proc as a pipe could, so another process of its user
/// can take it only by attaching to it, which Yama's `ptrace_scope` of 1 or
/// more refuses to one that is not its ancestor; at `ptrace_scope` 0 one
/// can attach then and keep its access after, so the window is narrowed,
/// not closed.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn take_secret_env() -> std::io::Result<()> {
    let Some(fd) = std::env::var_os(atomic_exec::SECRET_ENV_FD) else {
        return Ok(());
    };
    let invalid = |why: String| std::io::Error::new(std::io::ErrorKind::InvalidData, why);
    // SAFETY: the helper is single-threaded here, and the descriptor was
    // handed to it for this alone.
    let secret = unsafe { atomic_exec::process::read_secret_socket(&fd) }?;
    let token = the_proxy_token(secret).map_err(invalid)?;
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
    let (Some((name, handed)), None) = (entries.next(), entries.next()) else {
        return Err("it was handed other than one variable".to_owned());
    };
    if name != codex_network_proxy::PROXY_ATTRIBUTION_TOKEN_ENV_KEY {
        return Err(format!("it was handed {name}, not the proxy token"));
    }
    if handed.is_empty() {
        return Err("it was handed an empty proxy token".to_owned());
    }
    Ok(handed)
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
/// the same atomic home, and every other attempt's scratch directory (see
/// [`atomic_exec::Command::denied`]); only the command's own writable directories, such
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

    /// The profile for a command run in `cwd` that may write `writable`,
    /// except the directories in `read_only` inside them, and may neither
    /// read nor write `denied`, apart from the writable directories inside
    /// them.
    fn profile(
        &self,
        cwd: &Path,
        writable: &[PathBuf],
        read_only: &[PathBuf],
        denied: &[PathBuf],
    ) -> anyhow::Result<PermissionProfile> {
        use codex_protocol::permissions::{
            FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry,
        };
        let base = permission_profile(
            crate::typed::Sandbox::ReadOnly,
            &self.guard,
            &self.codex_home,
            Some(cwd),
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
        // Codex's Linux sandbox reopens the writable directories inside a
        // denied one, such as the command's own scratch directory.
        for dir in denied {
            let dir = AbsolutePathBuf::from_absolute_path(dir)
                .with_context(|| format!("resolve the denied directory {}", dir.display()))?;
            policy.entries.push(FileSystemSandboxEntry::new(
                FileSystemPath::from(dir),
                FileSystemAccessMode::Deny,
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
            .profile(
                &command.cwd,
                &command.writable,
                &command.read_only,
                &command.denied,
            )
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
        // The places its list was built with, which the helper walks again.
        if let Some(places) = places_text(&self.guard).map_err(|e| fail(format!("{e:#}")))? {
            helper_env.push((PLACES.to_owned(), places));
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
                PLACES,
                RELISTED,
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
                denied: Vec::new(),
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
                if not_the_helper(&stderr) {
                    Err(atomic_exec::ExecError::Unconfinable {
                        message: format!(
                            "Codex's Linux sandbox helper could not be set up: what ran in \
                             its place was not the helper ({why}); run atomic again, and if \
                             this repeats, install a bubblewrap that supports --argv0"
                        ),
                    })
                } else if sandbox_refused(&stderr) {
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

/// Whether a sandbox trial that failed with `stderr` failed because what
/// ran in the helper's place was not the helper: atomic's own command line,
/// refusing the helper's arguments, or bubblewrap, unable to run the helper
/// by its name, as one without `--argv0` tries to. Waiting would not change
/// either.
pub(crate) fn not_the_helper(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|line| refused_helper_arguments(line) || ran_by_helper_name(line))
}

/// Whether `line` is atomic's own command line refusing the helper's first
/// argument. `scripts/controls/sandbox-helper-not-atomic.row` switches this
/// off.
fn refused_helper_arguments(line: &str) -> bool {
    line.contains("unexpected argument '--sandbox-policy-cwd'")
}

/// Whether `line` is bubblewrap failing to run the helper by its name, the
/// bare one or a path that ends in it.
/// `scripts/controls/sandbox-helper-not-by-name.row` switches this off.
fn ran_by_helper_name(line: &str) -> bool {
    let Some((said, rest)) = line.split_once("execvp ") else {
        return false;
    };
    let run = rest.split(':').next().unwrap_or_default();
    let named = run == "codex-linux-sandbox" || run.ends_with("/codex-linux-sandbox");
    said.trim_start().starts_with("bwrap:") && named
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

/// The first argument Codex passes its Linux sandbox helper at the pinned
/// revision, to its outer stage and to its inner one alike, whenever it
/// does not first pass `--inherit-pid-namespace`, as it does only for a
/// command of its own. No command line of atomic's own starts with it.
#[cfg(target_os = "linux")]
const LINUX_SANDBOX_ARG1: &str = "--sandbox-policy-cwd";

/// Whether this process was started under a name that is not an absolute
/// path: a bare one, which a program run by that name would be looked for
/// along PATH, or a relative one, which would be resolved against whatever
/// directory it is run from, the workspace a command writes among them.
/// `scripts/controls/sandbox-helper-bare-name.row` switches this off.
#[cfg(target_os = "linux")]
fn started_by_relative_name() -> bool {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    !Path::new(&argv0).is_absolute()
}

/// The option Codex may give its Linux sandbox helper ahead of
/// [`LINUX_SANDBOX_ARG1`], for a command of its own at the pinned revision.
#[cfg(target_os = "linux")]
const INHERIT_PID_NAMESPACE: &str = "--inherit-pid-namespace";

/// Whether `first` and `second`, the first two arguments after the
/// program's name, are how Codex starts its Linux sandbox helper: with
/// [`LINUX_SANDBOX_ARG1`], alone or after [`INHERIT_PID_NAMESPACE`].
/// `scripts/controls/sandbox-helper-pid-namespace.row` switches the second
/// form off.
#[cfg(target_os = "linux")]
fn helper_arguments(first: &std::ffi::OsStr, second: Option<&std::ffi::OsStr>) -> bool {
    let after_inherit = first == INHERIT_PID_NAMESPACE;
    first == LINUX_SANDBOX_ARG1 || (after_inherit && second == Some(LINUX_SANDBOX_ARG1.as_ref()))
}

/// Whether this process was given the Linux sandbox helper's arguments,
/// under a name that is not one of Codex's other helpers'.
#[cfg(target_os = "linux")]
fn linux_sandbox_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    let mut args = args.into_iter();
    let patch = args
        .next()
        .as_deref()
        .map(Path::new)
        .and_then(Path::file_name)
        .is_some_and(|name| name == "apply_patch" || name == "applypatch");
    let first = args.next().unwrap_or_default();
    !patch && helper_arguments(&first, args.next().as_deref())
}

/// The name this executable runs again under as Codex's Linux sandbox
/// helper: its file name is the helper's, which Codex's dispatch and
/// atomic's own know it by, but as a path it can never exist, since
/// `/dev/null` is no directory. A bubblewrap without `--argv0` runs the
/// inner stage by this name: an absolute one, which it cannot find, so it
/// fails, rather than the bare name, which it would look for along `PATH`,
/// where a command could have put a program by that name.
/// `scripts/controls/sandbox-helper-unfindable.row` switches it off.
#[cfg(target_os = "linux")]
const HELPER_BY_NAME_ONLY: &str = "/dev/null/codex-linux-sandbox";

/// Run this executable again, with the same arguments, under the Linux
/// sandbox helper's name, which Codex's dispatch and atomic's own helper
/// entry know it by. Codex runs atomic itself as its helper when it could
/// not make its link to atomic under that name, such as when another
/// atomic process sweeping the same Codex home took the directory away as
/// it was being made; it warns that it "could not create PATH aliases".
/// Returns only why the executable could not be run.
#[cfg(target_os = "linux")]
fn run_as_linux_sandbox() -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    // Its environment may already hold a command's proxy token, before the
    // helper this becomes makes itself non-dumpable.
    if let Err(e) = not_dumpable() {
        return anyhow::Error::new(e).context("make the sandbox helper non-dumpable");
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return anyhow::Error::new(e).context("find atomic's own executable"),
    };
    let error = std::process::Command::new(&exe)
        .arg0(HELPER_BY_NAME_ONLY)
        .args(std::env::args_os().skip(1))
        .exec();
    anyhow::Error::new(error).context(format!(
        "run {} as Codex's Linux sandbox helper",
        exe.display()
    ))
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

/// The instructions a Codex host declares for itself: none, and read from
/// nowhere.
///
/// At the pinned rev Codex's own host provider reads `AGENTS.override.md` and
/// then `AGENTS.md` straight out of the Codex home and sends them to every
/// thread as global user instructions. atomic's Codex home is the project's
/// state directory (see [`crate::state`]), so those files are not a
/// checkout's; they are still input no atom declared, no pin recorded and
/// nothing checked. An atom's inputs are what its file declares, so the host
/// declares none: this answers "nothing" and opens no file doing it.
struct NoHostInstructions;

impl UserInstructionsProvider for NoHostInstructions {
    fn load_user_instructions(&self) -> LoadInstructionsFuture<'_> {
        Box::pin(std::future::ready(LoadedUserInstructions::default()))
    }
}

/// Whether atomic needs a snapshot of the skills Codex finds in the host. It
/// has no skill catalog of its own, so it needs none.
///
/// That answer is what `Feature::SkipHostSkillDiscovery` (`new_config`) acts
/// on: Codex asks its installed skill contributors, and a registry with no
/// contributor at all is taken for the old behavior of searching anyway
/// (`ext/extension-api/src/registry.rs`). Answering here is the only way the
/// feature reaches an atom's thread; with nothing discovered, a `$name` in a
/// turn's text names no skill, so no `SKILL.md` of the checkout is loaded.
const ATOMIC_NEEDS_HOST_SKILLS: bool = false;

struct NoSkillCatalog;

impl SkillInvocationContributor for NoSkillCatalog {
    fn requires_host_skill_discovery(&self) -> bool {
        ATOMIC_NEEDS_HOST_SKILLS
    }
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
    refuse_denied_without_helper(paths, &spec.denied, cfg!(target_os = "macos"))?;
    let mut endpoint = endpoint.clone();
    if let Some(m) = &spec.model {
        endpoint.model = m.clone();
    }
    let mut config = new_config(&endpoint, codex_home, cwd, paths.0.clone()).await?;
    // What Codex records of its threads goes with the project's private
    // state, which every project's commands are denied by one entry.
    if let Some(records) = codex_records(guard)? {
        config.sqlite = SqliteConfig::from_sqlite_home_with_nfs_enforcement(
            AbsolutePathBuf::from_absolute_path(&records)
                .context("resolve the Codex records' directory")?,
            /*enforce_delete_on_nfs*/ false,
        );
        config.log_dir = records.join("log");
    }
    config.developer_instructions = spec.developer_instructions.clone();
    config.permissions = Permissions::from_approval_and_profile(
        Constrained::allow_any(AskForApproval::Never),
        Constrained::allow_any(deny_also(
            permission_profile_with(
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
                &spec.denied,
            )?,
            &spec.denied,
        )?),
    )?;
    let mut writable_roots = spec.writable_roots.clone();
    writable_roots.push(cwd.to_owned());
    refuse_uncovered_store(spec.sandbox, guard, &writable_roots)?;
    writable_roots.pop();
    // Under workspace-write the system temporary directories are not
    // writable (see `permission_profile`): the thread gets its own, removed
    // when the thread ends.
    // A workspace that names a TMPDIR keeps it (see `crate::runtime`): on
    // Linux the backend's shared one, where Codex's sandbox keeps its
    // registry of protected mount points, which turns that share a writable
    // root must share; elsewhere the turn's own.
    let temp = match spec.sandbox {
        crate::typed::Sandbox::WorkspaceWrite if !spec.env.contains_key("TMPDIR") => {
            Some(TurnTemp::create(guard)?)
        }
        _ => None,
    };
    let mut env = spec.env.clone();
    // The places its list was built with, which the Linux sandbox helper
    // walks again as each command starts, and takes off before the command
    // runs. Elsewhere no helper would take it off, and the command would
    // see the list.
    if cfg!(target_os = "linux")
        && let Some(places) = places_text(guard)?
    {
        env.insert(PLACES.to_owned(), places);
    }
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
    // The host declares no instructions of its own, so it reads none from
    // the Codex home (see [`NoHostInstructions`]).
    let user_instructions_provider: Arc<dyn UserInstructionsProvider> =
        Arc::new(NoHostInstructions);
    // No extension gives a thread tools or context: an atom's tools are the
    // ones its file declares. The one entry answers the question Codex asks
    // before it searches the checkout for `.agents/skills` (see
    // [`ATOMIC_NEEDS_HOST_SKILLS`]).
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.skill_invocation_contributor(Arc::new(NoSkillCatalog));
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
                longest: None,
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
            // What this turn says of itself, for this step's span and its
            // run's, which never hear this error (see [`crate::usage::Why`]).
            crate::usage::note_step_failure(e);
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
                break Err(meter_error(MeterError::Exhausted(Exhausted {
                    reason: format!(
                        "{e}: all the active time the allowance {} had left",
                        bounded_by.as_deref().unwrap_or("of its atom")
                    ),
                    scope: bounded_by.clone(),
                })));
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

/// Codex finds repository instructions on its own: it walks up from a
/// thread's working directory to the project root and, in each directory on
/// the way, reads the first file it finds named `AGENTS.override.md` or
/// `AGENTS.md` (a configured fallback name next, and atomic configures
/// none), sending their text to the model as user instructions. An
/// atom's inputs are what its file declares, so no thread atomic starts gets
/// any of that text: a checkout's or worktree's instruction files are
/// untrusted input that nothing has pinned or recorded the provenance of. At
/// the pinned rev Codex stops discovery on either of the two values below
/// (`core/src/agents_md.rs`), so both are set: a zero-byte project-doc budget,
/// and the project explicitly untrusted. Repository skills and the Codex
/// home's own instruction file come to Codex by other routes and are switched
/// off there: `new_config` sets `Feature::SkipHostSkillDiscovery`, which acts
/// on the answer in [`ATOMIC_NEEDS_HOST_SKILLS`], and [`NoHostInstructions`]
/// replaces Codex's host provider. With nothing read, a step's repair and
/// resume turns see the same instructions as its first.
fn no_repository_instructions() -> (usize, Option<TrustLevel>) {
    (0, Some(TrustLevel::Untrusted))
}

async fn new_config(
    endpoint: &ModelEndpoint,
    codex_home: &Path,
    cwd: &Path,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<Config> {
    new_config_reading(endpoint, codex_home, cwd, arg0_paths, |name| {
        std::env::var(name).ok()
    })
    .await
}

/// [`new_config`], with the proxy variables a key's refusal reads from
/// `var` rather than this process's environment.
async fn new_config_reading(
    endpoint: &ModelEndpoint,
    codex_home: &Path,
    cwd: &Path,
    arg0_paths: Arg0DispatchPaths,
    var: fn(&str) -> Option<String>,
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
            // A key to an endpoint on this machine over plain http would go
            // to a proxy in cleartext, and Codex's client cannot be told to
            // skip proxies: the turn is refused, saying how to fix it.
            // Elsewhere than Linux, or told to respect the system's proxy
            // settings, Codex's client finds its proxy by other rules.
            let rules =
                crate::lite::ProxyRules::for_codex(cfg!(target_os = "linux"), respect_system_proxy);
            if endpoint.api_key_env.is_some()
                && let Some(why) = crate::lite::proxied_key(&endpoint.base_url, &var, rules)
            {
                anyhow::bail!(why);
            }
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

    // Codex finds a checkout's or worktree's own instruction files unless
    // both these say otherwise (see [`no_repository_instructions`]).
    let (project_doc_max_bytes, project_trust) = no_repository_instructions();

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
        project_doc_max_bytes,
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
        active_project: ProjectConfig {
            trust_level: project_trust,
        },
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
    // Codex would otherwise search every directory from the project root down
    // to the thread's working directory for a `.agents/skills/` folder and
    // load each skill it finds there, with neither the project's trust level
    // nor the instruction budget in the way (`ext/skills/src/host_roots.rs`).
    // A `$name` in a turn's text then sends that skill's `SKILL.md` to the
    // model. The feature stops that search once Codex has asked whether
    // anything needs the host's skills, which is what the one contributor
    // registered in `converse_async` answers (see [`ATOMIC_NEEDS_HOST_SKILLS`]).
    config
        .features
        .set_enabled(Feature::SkipHostSkillDiscovery, true)
        .context("skip the checkout's skills")?;
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
        let dir_scratch = Scratch::new("linked-deny");
        let dir = dir_scratch.0.clone();
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
                ("AWS_WEB_IDENTITY_TOKEN_FILE", "/aws/web-identity-token"),
                (
                    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
                    "/aws/pod-identity-token",
                ),
                ("NETRC", "/n/netrc"),
                ("PASSWORD_STORE_DIR", "/pass"),
                ("AZURE_FEDERATED_TOKEN_FILE", "/az/federated-token"),
                ("GOOGLE_APPLICATION_CREDENTIALS", "/gcp/credentials.json"),
                ("CLOUDSDK_CONFIG", "/gcloud"),
            ]),
            None,
        )
        .expect("homes");
        // Where workload identities keep their tokens on Kubernetes.
        let pod = "/var/run/secrets/kubernetes.io/serviceaccount";
        let azure = "/var/run/secrets/azure/tokens";
        assert!(has(&secrets, pod), "{pod} is not among the denied places");
        assert!(
            has(&secrets, azure),
            "{azure} is not among the denied places"
        );
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
            "/aws/web-identity-token",
            "/aws/pod-identity-token",
            "/var/run/secrets/eks.amazonaws.com/serviceaccount",
            "/var/run/secrets/pods.eks.amazonaws.com/serviceaccount",
            "/n/netrc",
            "/pass",
            "/az/federated-token",
            "/gcp/credentials.json",
            "/gcloud",
        ] {
            assert!(has(&secrets, path), "{path}: {secrets:?}");
        }
        assert!(!has(&secrets, "relative"));
        assert!(!has(&secrets, "/opt/cargo/config.toml"), "{secrets:?}");
    }

    /// A workload token file a variable names in a projected volume, as
    /// Kubernetes mounts one, is denied with the volume that holds it, so the
    /// token it rotates in next is denied too; an ordinary token file is
    /// denied alone.
    #[cfg(unix)]
    #[test]
    fn a_projected_token_volume_is_denied_whole() {
        let dir_scratch = Scratch::new("projected-token");
        let dir = dir_scratch.0.clone();
        let volume = dir.join("serviceaccount");
        std::fs::create_dir_all(volume.join("..2026_01_01_00_00_00.1")).expect("version");
        std::fs::write(volume.join("..2026_01_01_00_00_00.1/token"), "t").expect("token");
        std::os::unix::fs::symlink("..2026_01_01_00_00_00.1", volume.join("..data"))
            .expect("..data");
        std::os::unix::fs::symlink("..data/token", volume.join("token")).expect("token link");
        std::fs::write(dir.join("plain-token"), "t").expect("plain");
        let projected = volume.join("token").display().to_string();
        let plain = dir.join("plain-token").display().to_string();
        let home = dir.display().to_string();
        let env = |name: &str| {
            match name {
                "HOME" => Some(home.clone()),
                "AWS_WEB_IDENTITY_TOKEN_FILE" => Some(projected.clone()),
                "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE" => Some(plain.clone()),
                _ => None,
            }
            .map(std::ffi::OsString::from)
        };
        let places = home_secrets(&env, None).expect("homes");
        assert!(places.contains(&volume), "the volume is denied");
        assert!(
            places.contains(&volume.join("token")),
            "the token link is denied"
        );
        assert!(
            places.contains(&dir.join("plain-token")),
            "the ordinary token is denied"
        );
        assert!(
            !places.contains(&dir),
            "an ordinary token's directory stays readable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A token file Google's application credentials name is denied with the
    /// credentials: an external account's source file, with its projected
    /// volume, the file its executable keeps a token in, and the source file
    /// of the credentials an impersonated account is configured with.
    #[cfg(unix)]
    #[test]
    fn the_token_files_googles_credentials_name_are_denied() {
        let dir = std::fs::canonicalize(scratch("gcp-external")).expect("real");
        let volume = dir.join("gke");
        std::fs::create_dir_all(volume.join("..2026_01_01_00_00_00.1")).expect("version");
        std::fs::write(volume.join("..2026_01_01_00_00_00.1/token"), "t").expect("token");
        link(Path::new("..2026_01_01_00_00_00.1"), &volume.join("..data"));
        link(Path::new("..data/token"), &volume.join("token"));
        let external = serde_json::json!({
            "type": "external_account",
            "credential_source": {
                "file": volume.join("token"),
                "executable": {"command": "/bin/true", "output_file": dir.join("cached.json")},
            },
        });
        std::fs::write(dir.join("external.json"), external.to_string()).expect("config");
        let impersonated = serde_json::json!({
            "type": "impersonated_service_account",
            "source_credentials": {
                "type": "external_account",
                "credential_source": {"file": "relative/inner-token"},
            },
        });
        std::fs::write(dir.join("impersonated.json"), impersonated.to_string()).expect("config");
        let here = || -> std::io::Result<PathBuf> { Ok(PathBuf::from("/work")) };
        for (config, wanted) in [
            (
                "external.json",
                vec![
                    volume.clone(),
                    volume.join("token"),
                    dir.join("cached.json"),
                ],
            ),
            (
                "impersonated.json",
                vec![PathBuf::from("/work/relative/inner-token")],
            ),
        ] {
            let named = dir.join(config).display().to_string();
            let env =
                |name: &str| (name == GOOGLE_CREDENTIALS).then(|| std::ffi::OsString::from(&named));
            let found = token_files(&env, &here).expect("found");
            assert!(
                found.contains(&dir.join(config)),
                "the credentials: {found:?}"
            );
            for file in &wanted {
                assert!(found.contains(file), "{}: {found:?}", file.display());
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The web identity token file an AWS profile names, and not a variable,
    /// is denied, with its projected volume: one under `~/` in each home as
    /// well as from the working directory, in any profile.
    #[cfg(unix)]
    #[test]
    fn the_web_identity_token_file_an_aws_profile_names_is_denied() {
        let dir = std::fs::canonicalize(scratch("aws-profile")).expect("real");
        let volume = dir.join("eks");
        std::fs::create_dir_all(volume.join("..2026_01_01_00_00_00.1")).expect("version");
        std::fs::write(volume.join("..2026_01_01_00_00_00.1/token"), "t").expect("token");
        link(Path::new("..2026_01_01_00_00_00.1"), &volume.join("..data"));
        link(Path::new("..data/token"), &volume.join("token"));
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".aws")).expect("home");
        std::fs::write(
            home.join(".aws/config"),
            format!(
                "[default]\nregion = eu-west-1\n[profile pod]\nweb_identity_token_file = {}\n",
                volume.join("token").display()
            ),
        )
        .expect("config");
        std::fs::write(
            dir.join("moved-config"),
            format!(
                "[profile other]\n  Web_Identity_Token_File=~/tokens/web\n\
                 [profile colon]\nweb_identity_token_file: {d}/colon-token\n\
                 [profile commented]\nWEB_IDENTITY_TOKEN_FILE = {d}/commented-token  # eks\n\
                 [profile both]\nweb_identity_token_file\t:\t{d}/both-token ;note\n",
                d = dir.display()
            ),
        )
        .expect("moved");
        let (home_s, moved) = (home.display().to_string(), dir.join("moved-config"));
        let moved_s = moved.display().to_string();
        let env = |name: &str| {
            match name {
                "HOME" => Some(home_s.clone()),
                "AWS_CONFIG_FILE" => Some(moved_s.clone()),
                _ => None,
            }
            .map(std::ffi::OsString::from)
        };
        let places = home_secrets(&env, None).expect("homes");
        for file in [
            volume.clone(),
            volume.join("token"),
            home.join("tokens/web"),
            dir.join("colon-token"),
            dir.join("commented-token"),
            dir.join("commented-token  # eks"),
            dir.join("both-token"),
        ] {
            assert!(
                places.contains(&file),
                "{} is not among the denied places",
                file.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The application default credentials gcloud writes are read for the
    /// token files they name, as Google's libraries read them with no
    /// variable set.
    #[cfg(unix)]
    #[test]
    fn the_default_google_credentials_are_read_for_the_token_files_they_name() {
        let dir = std::fs::canonicalize(scratch("gcp-default")).expect("real");
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".config/gcloud")).expect("gcloud");
        let config = serde_json::json!({
            "type": "external_account",
            "credential_source": {"file": dir.join("adc-token")},
        });
        std::fs::write(
            home.join(".config/gcloud/application_default_credentials.json"),
            config.to_string(),
        )
        .expect("config");
        let home_s = home.display().to_string();
        let env = |name: &str| (name == "HOME").then(|| std::ffi::OsString::from(&home_s));
        let places = home_secrets(&env, None).expect("homes");
        assert!(
            places.contains(&dir.join("adc-token")),
            "adc-token is not among the denied places"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A credential configuration that cannot be read whole is refused,
    /// naming it, so nothing runs: only one that is not there names nothing.
    /// Google's credentials that are not JSON, or whose file field is not
    /// text; a directory, a fifo (without waiting on it), a file past the
    /// bound, a path through a file, and an AWS file that is not text.
    #[cfg(unix)]
    #[test]
    fn a_credential_configuration_that_cannot_be_read_whole_is_refused() {
        let dir = std::fs::canonicalize(scratch("config-refused")).expect("real");
        let here = || -> std::io::Result<PathBuf> { Ok(PathBuf::from("/work")) };
        let google = |name: &str| {
            google_token_files(&dir.join(name), &here)
                .err()
                .unwrap_or_default()
        };
        let aws = |name: &str| {
            aws_profile_token_files(&dir.join(name), &[], &here)
                .err()
                .unwrap_or_default()
        };
        assert_eq!(
            google_token_files(&dir.join("missing"), &here),
            Ok(Vec::new())
        );
        // A device, as /dev/null is when a variable turns a configuration
        // off, names nothing and is not opened.
        let null = Path::new("/dev/null");
        assert_eq!(aws_profile_token_files(null, &[], &here), Ok(Vec::new()));
        assert_eq!(google_token_files(null, &here), Ok(Vec::new()));
        // One this user may not read is refused, saying so.
        std::fs::write(dir.join("theirs"), "[default]\n").expect("theirs");
        std::fs::set_permissions(
            dir.join("theirs"),
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .expect("mode");
        assert!(
            aws("theirs").contains("this user may not read it"),
            "{}",
            aws("theirs")
        );
        std::fs::write(dir.join("broken.json"), "{\"credential_source\": ").expect("broken");
        assert!(
            google("broken.json").contains("it is not JSON"),
            "{}",
            google("broken.json")
        );
        std::fs::write(
            dir.join("number.json"),
            r#"{"credential_source": {"file": 7}}"#,
        )
        .expect("number");
        assert!(
            google("number.json").contains("credential_source.file is not text"),
            "{}",
            google("number.json")
        );
        std::fs::create_dir_all(dir.join("a-directory")).expect("dir");
        assert!(google("a-directory").contains("not a plain file"));
        let fifo = std::ffi::CString::new(dir.join("fifo").into_os_string().into_encoded_bytes())
            .expect("name");
        // SAFETY: mkfifo reads a NUL-terminated path and makes a pipe there.
        #[allow(unsafe_code)]
        let made = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo");
        assert!(aws("fifo").contains("not a plain file"), "{}", aws("fifo"));
        let big = vec![b' '; CONFIG_BYTES_MAX as usize + 1];
        std::fs::write(dir.join("big"), big).expect("big");
        assert!(aws("big").contains("more than"), "{}", aws("big"));
        // An error other than not being there: a path through a file.
        std::fs::write(dir.join("plain"), "").expect("plain");
        let through = google("plain/credentials.json");
        assert!(through.contains("Not a directory"), "{through}");
        std::fs::write(dir.join("latin1"), b"web_identity_token_file = /t\xff\n").expect("bytes");
        assert!(
            aws("latin1").contains("not UTF-8 text"),
            "{}",
            aws("latin1")
        );
        assert!(
            aws("latin1").starts_with(&format!(
                "{} could not be read",
                dir.join("latin1").display()
            )),
            "the file is named"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A projected volume is found by where its token resolves to: through
    /// a link to a link to the token, and for an item nested in one of the
    /// volume's directories. A file beside a `..data` that does not resolve
    /// through it is in no volume.
    #[cfg(unix)]
    #[test]
    fn a_projected_volume_is_found_through_links_and_for_nested_items() {
        let dir_scratch = Scratch::new("projected-links");
        let dir = dir_scratch.0.clone();
        let version = "..2026_01_01_00_00_00.1";
        let volume = |name: &str| {
            let volume = dir.join(name);
            std::fs::create_dir_all(volume.join(version).join("aws")).expect("version");
            std::fs::write(volume.join(version).join("token"), "t").expect("token");
            std::fs::write(volume.join(version).join("aws/token"), "t").expect("nested");
            link(Path::new(version), &volume.join("..data"));
            link(Path::new("..data/token"), &volume.join("token"));
            link(Path::new("..data/aws"), &volume.join("aws"));
            volume
        };
        let (linked, nested) = (volume("linked"), volume("nested"));
        // A token named through a link to the volume's link to it.
        link(&linked.join("token"), &dir.join("etc/token"));
        assert_eq!(
            projected_volume(dir.join("etc/token")),
            Some(linked.clone())
        );
        assert_eq!(
            projected_volume(nested.join("aws/token")),
            Some(nested.clone())
        );
        // A plain file beside the `..data` link, and one nowhere near one.
        std::fs::write(linked.join("plain"), "p").expect("plain");
        assert_eq!(projected_volume(linked.join("plain")), None);
        std::fs::write(dir.join("ordinary"), "o").expect("ordinary");
        assert_eq!(projected_volume(dir.join("ordinary")), None);
        assert_eq!(projected_volume(dir.join("missing")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What cannot be read is denied whole: a token that is a link leading
    /// back to itself denies its directory, and a `..data` that does denies
    /// its volume. A token below a file, which can be in no directory, is in
    /// no volume.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_token_or_data_link_denies_its_directory() {
        let dir = scratch("projected-unreadable");
        link(&dir.join("loop"), &dir.join("loop"));
        assert_eq!(projected_volume(dir.join("loop")), Some(dir.clone()));
        let volume = dir.join("volume");
        std::fs::create_dir_all(volume.join("current")).expect("version");
        std::fs::write(volume.join("current/token"), "t").expect("token");
        link(&volume.join("..data"), &volume.join("..data"));
        assert_eq!(projected_volume(volume.join("current/token")), Some(volume));
        std::fs::write(dir.join("plain"), "p").expect("plain");
        assert_eq!(projected_volume(dir.join("plain/token")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A token file named with a trailing `/` is found in its projected
    /// volume all the same: the name is read plainly, so only a directory
    /// above the file can fail as not a directory.
    #[cfg(unix)]
    #[test]
    fn a_token_named_with_a_trailing_slash_is_still_found_in_its_volume() {
        let dir = scratch("trailing-slash");
        let volume = dir.join("volume");
        std::fs::create_dir_all(volume.join("..2026_01_01_00_00_00.1")).expect("version");
        std::fs::write(volume.join("..2026_01_01_00_00_00.1/token"), "t").expect("token");
        link(Path::new("..2026_01_01_00_00_00.1"), &volume.join("..data"));
        link(Path::new("..data/token"), &volume.join("token"));
        let named = format!("{}/token/", volume.display());
        let env = |name: &str| {
            (name == "AWS_WEB_IDENTITY_TOKEN_FILE").then(|| std::ffi::OsString::from(&named))
        };
        let here = || -> std::io::Result<PathBuf> { Ok(PathBuf::from("/")) };
        let found = token_files(&env, &here).expect("found");
        assert!(found.contains(&volume), "the volume is denied");
        assert!(found.contains(&volume.join("token")), "the token is denied");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A relative token file with no working directory to resolve it
    /// against is refused, naming the variable, so nothing runs.
    #[test]
    fn a_relative_token_file_with_no_working_directory_is_refused() {
        let env = |name: &str| {
            (name == "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE")
                .then(|| std::ffi::OsString::from("tokens/pod"))
        };
        let gone = || -> std::io::Result<PathBuf> {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        };
        let refused = token_files(&env, &gone).expect_err("refused");
        assert!(
            refused.starts_with(
                "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE names the token file tokens/pod"
            ),
            "{refused}"
        );
    }

    /// A token file named relative to atomic's working directory is denied
    /// where an AWS SDK started there would read it.
    #[test]
    fn a_relative_token_file_is_denied_from_the_working_directory() {
        let env = |name: &str| {
            match name {
                "HOME" => Some("/srv/home"),
                "AWS_WEB_IDENTITY_TOKEN_FILE" => Some("tokens/web"),
                _ => None,
            }
            .map(std::ffi::OsString::from)
        };
        let places = home_secrets(&env, None).expect("homes");
        let here = std::env::current_dir().expect("cwd");
        assert!(
            places.contains(&here.join("tokens/web")),
            "the relative token is denied from the working directory"
        );
    }

    /// A place named through a directory that is an absolute link goes to
    /// the sandbox under its real spelling alone, existing or not: an older
    /// bubblewrap cannot mount on the other, and so does Codex's database
    /// pattern. One reached through a link a command may write is refused,
    /// and so is one inside a writable directory given through a link.
    #[cfg(unix)]
    #[test]
    fn a_place_behind_a_linked_directory_is_denied_by_its_real_path() {
        let dir_scratch = Scratch::new("real-spelling");
        let dir = dir_scratch.0.clone();
        std::fs::create_dir_all(dir.join("real/gnupg")).expect("gnupg");
        std::fs::create_dir_all(dir.join("codex")).expect("codex home");
        link(&dir.join("real"), &dir.join("linked"));
        let private = |also: Vec<PathBuf>| PrivateState {
            state_dir: dir.join("state"),
            private: dir.join("private"),
            also,
        };
        let named = private(vec![dir.join("linked/gnupg"), dir.join("linked/missing")]);
        let (denied, _) = denied_paths(&named, &dir.join("codex"), None, &[]).expect("denied");
        assert!(denied.contains(&dir.join("real/gnupg")), "{denied:?}");
        assert!(denied.contains(&dir.join("real/missing")), "{denied:?}");
        assert!(
            !denied.iter().any(|d| d.starts_with(dir.join("linked"))),
            "a spelling through the link: {denied:?}"
        );
        // Codex's database pattern, too, is under the home's real spelling.
        link(&dir.join("codex"), &dir.join("linked-codex"));
        let (_, globs) =
            denied_paths(&named, &dir.join("linked-codex"), None, &[]).expect("denied");
        assert_eq!(
            globs,
            vec![format!("{}/*.sqlite*", dir.join("codex").display())]
        );
        let work = dir.join("work");
        link(&dir.join("real"), &work.join("link"));
        let through = private(vec![work.join("link/gnupg")]);
        let refused = denied_paths(&through, &dir.join("codex"), None, &[work])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(refused.contains("may write"), "{refused}");
        // A place inside a directory the thread may write, given through a
        // link: the sandbox would not keep it denied.
        std::fs::write(dir.join("real/.secret"), "s").expect("secret");
        let inside = private(vec![dir.join("real/.secret")]);
        let refused = denied_paths(&inside, &dir.join("codex"), None, &[dir.join("linked")])
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            refused.contains("through a link, so its denial would not hold"),
            "{refused}"
        );
        // And so is Codex's home there, whose databases a pattern denies.
        std::fs::create_dir_all(dir.join("real/codex-home")).expect("codex home");
        let refused = denied_paths(
            &private(vec![]),
            &dir.join("real/codex-home"),
            None,
            &[dir.join("linked")],
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(
            refused.contains("through a link, so its denial would not hold"),
            "{refused}"
        );
        let given_real = denied_paths(&inside, &dir.join("codex"), None, &[dir.join("real")]);
        assert!(given_real.is_ok(), "{given_real:?}");
        let _ = std::fs::remove_dir_all(&dir);
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
        let dir_scratch = Scratch::new("deniable");
        let dir = dir_scratch.0.clone();
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

    /// What a link made inside a denied place since the list was built
    /// points at is found, unless a denied path covers it already; one that
    /// would cover what the command needs, or lies two levels inside a
    /// directory it may write, refuses the command.
    #[cfg(unix)]
    #[test]
    fn a_late_link_is_found_and_one_that_cannot_be_denied_refuses() {
        let _table = crate::mounts::with_table(ONE_MOUNT);
        let dir_scratch = Scratch::new("late-links");
        let dir = dir_scratch.0.clone();
        let place = dir.join("home/.aws");
        let work = dir.join("work");
        for d in [&place, &work.join("deep"), &dir.join("elsewhere")] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let real = |p: &Path| std::fs::canonicalize(p).expect("real");
        let creds = dir.join("elsewhere/creds");
        std::fs::write(&creds, "x").expect("creds");
        let places = [place.clone()];
        let targets = |denied: &[PathBuf], needed: &[PathBuf], writable: &[PathBuf]| {
            late_link_targets(denied, Some(&places), needed, writable).map(|l| l.targets)
        };
        assert!(targets(&places, &[], &[]).expect("none").is_empty());
        link(&creds, &place.join("credentials"));
        assert_eq!(targets(&places, &[], &[]).expect("found"), [real(&creds)]);
        // Denied already: nothing more.
        let covered = [place.clone(), dir.join("elsewhere")];
        assert!(targets(&covered, &[], &[]).expect("none").is_empty());
        // Two levels inside a directory the command may write.
        std::fs::write(work.join("deep/creds"), "x").expect("creds");
        std::fs::remove_file(place.join("credentials")).expect("unlink");
        link(&work.join("deep/creds"), &place.join("credentials"));
        let refused = targets(&places, &[], std::slice::from_ref(&work))
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains("credentials now leads to") && refused.contains("two or more levels"),
            "{refused}"
        );
        // Leading to what the command needs.
        std::fs::remove_file(place.join("credentials")).expect("unlink");
        link(&work, &place.join("credentials"));
        let refused = targets(&places, &[work.join("deep")], &[])
            .expect_err("refused")
            .to_string();
        assert!(refused.contains("which this command needs"), "{refused}");
    }

    /// The walk follows only the places a list was built with: what the
    /// list denies as a link's target is not walked again, so a link inside
    /// a linked directory, there when the list was built, starts nothing
    /// again, however deep the chain, whether the places came with the
    /// profile or are read from it.
    #[cfg(unix)]
    #[test]
    fn a_static_chain_of_links_finds_nothing_new() {
        let _table = crate::mounts::with_table(ONE_MOUNT);
        let dir_scratch = Scratch::new("late-static");
        let dir = dir_scratch.0.clone();
        let place = dir.join("home/.codex");
        let skills = dir.join("dev/skills");
        let store = dir.join("dev/store");
        for d in [&place, &skills.join("pkg"), &store.join("lib")] {
            std::fs::create_dir_all(d).expect("dir");
        }
        link(&skills, &place.join("skills"));
        // A workspace's links, under the linked directory.
        link(&store.join("lib"), &skills.join("pkg/node_modules"));
        std::fs::create_dir_all(store.join("lib/deeper")).expect("dir");
        link(&dir.join("dev"), &store.join("lib/deeper/up"));
        let real = |p: &Path| std::fs::canonicalize(p).expect("real");
        // As the list was built: the place, and what its link points at.
        let built = [place.clone(), real(&skills)];
        for places in [None, Some(std::slice::from_ref(&place))] {
            let late = late_link_targets(&built, places, &[], &[]).expect("walked");
            assert_eq!(late, Late::default(), "{places:?}: {late:?}");
        }
    }

    /// A place that has become a link is denied as what it points at, its
    /// own entry replaced, and refuses the command when it leads to what the
    /// command needs, whatever that is, or two levels inside a directory it
    /// may write.
    #[cfg(unix)]
    #[test]
    fn a_place_that_became_a_link_is_replaced_by_its_target() {
        let _table = crate::mounts::with_table(ONE_MOUNT);
        let dir_scratch = Scratch::new("late-place");
        let dir = dir_scratch.0.clone();
        let home = dir.join("home");
        std::fs::create_dir_all(dir.join("elsewhere")).expect("dir");
        std::fs::create_dir_all(dir.join("work/deep")).expect("dir");
        std::fs::create_dir_all(&home).expect("home");
        let netrc = home.join(".netrc");
        let kept = dir.join("elsewhere/netrc");
        std::fs::write(&kept, "machine x").expect("netrc");
        link(&kept, &netrc);
        let only = std::slice::from_ref(&netrc);
        let late = late_link_targets(only, Some(only), &[], &[]).expect("walked");
        assert_eq!(
            late,
            Late {
                targets: vec![std::fs::canonicalize(&kept).expect("real")],
                replaced: vec![netrc.clone()],
                globs: Vec::new(),
            }
        );
        // As the list was built while the place was a link: what it pointed
        // at, so that only the place, walked again, can refuse.
        let built = [std::fs::canonicalize(&kept).expect("real")];
        // Now leading to the root, which holds what the command needs.
        std::fs::remove_file(&netrc).expect("unlink");
        link(Path::new("/"), &netrc);
        let refused = late_link_targets(&built, Some(only), &[dir.join("work")], &[])
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains(&format!("{} now leads to /,", netrc.display()))
                && refused.contains("which this command needs"),
            "{refused}"
        );
        // Now two levels inside a directory the command may write.
        std::fs::write(dir.join("work/deep/netrc"), "machine y").expect("netrc");
        std::fs::remove_file(&netrc).expect("unlink");
        link(&dir.join("work/deep/netrc"), &netrc);
        let refused = late_link_targets(&built, Some(only), &[], &[dir.join("work")])
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains(&format!("{} now leads to", netrc.display()))
                && refused.contains("two or more levels"),
            "{refused}"
        );
    }

    /// What the late walk finds is denied once, the outer of nested ones
    /// only, as the sandbox cannot mount a denial inside another.
    #[cfg(unix)]
    #[test]
    fn nested_late_targets_collapse_to_the_outer() {
        let _table = crate::mounts::with_table(ONE_MOUNT);
        let dir_scratch = Scratch::new("late-nested");
        let dir = dir_scratch.0.clone();
        let home = dir.join("home");
        let kept = dir.join("dev/kept");
        std::fs::create_dir_all(kept.join("b")).expect("dir");
        std::fs::create_dir_all(&home).expect("home");
        let place = home.join(".aws");
        link(&kept, &place);
        link(&kept.join("b"), &kept.join("a"));
        link(&dir.join("dev"), &kept.join("up"));
        let only = std::slice::from_ref(&place);
        let late = late_link_targets(only, Some(only), &[], &[]).expect("walked");
        assert_eq!(
            late.targets,
            [std::fs::canonicalize(dir.join("dev")).expect("real")],
            "{late:?}"
        );
    }

    /// The cap counts what the places' links lead to outside them, as the
    /// builder does: links within a place, however many, count nothing; more
    /// than the cap leading out refuses.
    #[cfg(unix)]
    #[test]
    fn the_late_cap_counts_only_links_that_lead_out() {
        let _table = crate::mounts::with_table(ONE_MOUNT);
        // Over a thousand links: removed when the test ends, however.
        let scratch = Scratch::new("late-cap");
        let dir = scratch.0.clone();
        let place = dir.join("home/.password-store");
        std::fs::create_dir_all(place.join("files")).expect("dir");
        std::fs::create_dir_all(dir.join("out")).expect("dir");
        for i in 0..=MAX_LINK_TARGETS {
            let inside = place.join(format!("files/{i}"));
            std::fs::write(&inside, "x").expect("file");
            link(&inside, &place.join(format!("in-{i}")));
        }
        let only = std::slice::from_ref(&place);
        assert!(
            late_link_targets(only, Some(only), &[], &[])
                .expect("links within the place count nothing")
                .targets
                .is_empty()
        );
        // As many leading out, all denied already as the list was built.
        let out = std::fs::canonicalize(dir.join("out")).expect("real");
        let mut built = vec![place.clone()];
        for i in 0..=MAX_LINK_TARGETS {
            let there = out.join(format!("{i}"));
            std::fs::write(&there, "x").expect("file");
            link(&there, &place.join(format!("out-{i}")));
            built.push(there);
        }
        let refused = late_link_targets(&built, Some(only), &[], &[])
            .expect_err("refused")
            .to_string();
        assert!(refused.contains("more than the 512"), "{refused}");
        // As many leading out, none denied yet: refused too, with the new
        // ones' second mounts counted.
        let refused = late_link_targets(only, Some(only), &[], &[])
            .expect_err("refused")
            .to_string();
        assert!(
            refused
                .contains("links to files elsewhere, with 0 second mounts of what the new ones point at, 0 second mounts of what they deny (0 made since the list was built) and 0 of Codex's home, more than the 512"),
            "{refused}"
        );
    }

    /// A place carried from the builder is walked even where the profile
    /// alone would take it for a link's target: a place that is a link into
    /// another place's linked directory.
    #[cfg(unix)]
    #[test]
    fn a_carried_place_is_walked_through_a_link() {
        let _table = crate::mounts::with_table(ONE_MOUNT);
        let dir_scratch = Scratch::new("late-carried");
        let dir = dir_scratch.0.clone();
        let ssh = dir.join("home/.ssh");
        let gnupg = dir.join("dev/gnupg");
        std::fs::create_dir_all(&ssh).expect("dir");
        std::fs::create_dir_all(&gnupg).expect("dir");
        std::fs::create_dir_all(dir.join("secret")).expect("dir");
        link(&gnupg, &ssh.join("gnupg"));
        let real = |p: &Path| std::fs::canonicalize(p).expect("real");
        // GNUPGHOME names the link: the list walks it as a place.
        let places = [ssh.clone(), ssh.join("gnupg")];
        let built = [ssh.clone(), real(&gnupg)];
        // A key linked in since.
        std::fs::write(dir.join("secret/key"), "x").expect("key");
        link(&dir.join("secret/key"), &gnupg.join("private-keys"));
        let late = late_link_targets(&built, Some(&places), &[], &[]).expect("walked");
        assert_eq!(late.targets, [real(&dir.join("secret/key"))], "{late:?}");
    }

    /// What the late walk finds is denied at its second mounts too.
    #[cfg(unix)]
    #[test]
    fn a_late_target_is_denied_at_its_second_mounts() {
        let dir_scratch = Scratch::new("late-aliases");
        let dir = dir_scratch.0.clone();
        let place = dir.join("home/.aws");
        std::fs::create_dir_all(&place).expect("dir");
        std::fs::create_dir_all(dir.join("elsewhere")).expect("dir");
        std::fs::write(dir.join("elsewhere/creds"), "x").expect("creds");
        link(&dir.join("elsewhere/creds"), &place.join("credentials"));
        let creds = std::fs::canonicalize(dir.join("elsewhere/creds")).expect("real");
        let _table = crate::mounts::with_table(
            b"1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 8:1 / /again rw - ext4 /dev/sda1 rw\n",
        );
        let only = std::slice::from_ref(&place);
        let late = late_link_targets(only, Some(only), &[], &[]).expect("walked");
        let again = Path::new("/again").join(creds.strip_prefix("/").expect("absolute"));
        assert!(late.targets.contains(&creds), "{late:?}");
        assert!(late.targets.contains(&again), "{late:?}");
        // A second mount a denial covers already needs none of its own.
        // As the builder would: the place at its own second mount too.
        let again_place = Path::new("/again").join(place.strip_prefix("/").expect("absolute"));
        let built = [place.clone(), again_place, again.clone()];
        let late = late_link_targets(&built, Some(only), &[], &[]).expect("walked");
        assert_eq!(late.targets, [creds], "{late:?}");
    }

    /// A mount table of the root alone, so that what a test finds does not
    /// hang on the host's mounts.
    const ONE_MOUNT: &[u8] = b"1 0 8:1 / / rw - ext4 /dev/sda1 rw\n";

    /// A denied entry that has become a link is replaced by what it points
    /// at however it is spelled, as when the place is named through a linked
    /// directory and the list holds it by its real path, and is held to the
    /// same rules.
    #[cfg(unix)]
    #[test]
    fn a_denied_entry_that_became_a_link_is_replaced_however_it_is_spelled() {
        let _table = crate::mounts::with_table(ONE_MOUNT);
        let dir_scratch = Scratch::new("late-spelled");
        let dir = dir_scratch.0.clone();
        let real_home = dir.join("data/u");
        std::fs::create_dir_all(&real_home).expect("dir");
        std::fs::create_dir_all(dir.join("elsewhere")).expect("dir");
        std::fs::create_dir_all(dir.join("work")).expect("dir");
        link(&real_home, &dir.join("home"));
        let real = |p: &Path| std::fs::canonicalize(p).expect("real");
        let entry = real(&real_home).join(".netrc");
        std::fs::write(&entry, "machine x").expect("netrc");
        // The place as named, through the linked directory; the list's
        // entry by its real path.
        let places = [dir.join("home/.netrc")];
        let denied = [entry.clone()];
        assert_eq!(
            late_link_targets(&denied, Some(&places), &[], &[]).expect("walked"),
            Late::default()
        );
        let kept = dir.join("elsewhere/netrc");
        std::fs::write(&kept, "machine y").expect("netrc");
        std::fs::remove_file(&entry).expect("unlink");
        link(&kept, &entry);
        assert_eq!(
            late_link_targets(&denied, Some(&places), &[], &[]).expect("walked"),
            Late {
                targets: vec![real(&kept)],
                replaced: vec![entry.clone()],
                globs: Vec::new(),
            }
        );
        // Leading to what the command needs, with no place to walk: the
        // entry alone refuses.
        std::fs::remove_file(&entry).expect("unlink");
        link(&dir.join("work"), &entry);
        let refused = late_link_targets(&denied, Some(&[]), &[dir.join("work")], &[])
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains(&format!("{} now leads to", entry.display()))
                && refused.contains("which this command needs"),
            "{refused}"
        );
    }

    /// A denied entry, or a carried place, that has become a link that
    /// cannot be resolved for a reason other than leading nowhere, such as
    /// a loop, refuses the command; one that leads nowhere is dropped.
    #[cfg(unix)]
    #[test]
    fn a_link_that_cannot_be_resolved_refuses() {
        let _table = crate::mounts::with_table(ONE_MOUNT);
        let dir = Scratch::new("late-unresolved");
        let home = dir.0.join("home");
        std::fs::create_dir_all(&home).expect("dir");
        let netrc = home.join(".netrc");
        // A link to itself: ELOOP.
        link(&netrc, &netrc);
        let only = std::slice::from_ref(&netrc);
        // As a denied entry, with no place to walk.
        let refused = late_link_targets(only, Some(&[]), &[], &[])
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains(&format!(
                "{} is a link atomic cannot resolve",
                netrc.display()
            )),
            "{refused}"
        );
        // As a carried place, not among the entries.
        let refused = late_link_targets(&[], Some(only), &[], &[])
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains(&format!(
                "{} is a link atomic cannot resolve",
                netrc.display()
            )),
            "{refused}"
        );
        // Leading nowhere: the entry is dropped, nothing to deny.
        std::fs::remove_file(&netrc).expect("unlink");
        link(&home.join("not-there"), &netrc);
        assert_eq!(
            late_link_targets(only, Some(only), &[], &[]).expect("walked"),
            Late {
                targets: Vec::new(),
                replaced: vec![netrc.clone()],
                globs: Vec::new(),
            }
        );
    }

    /// A second mount of what the list denies, made after it was built, is
    /// denied at its new place for the next command, held to the same rules
    /// as the rest.
    #[cfg(unix)]
    #[test]
    fn a_mount_made_since_the_list_is_denied_for_the_next_command() {
        use codex_protocol::permissions::{FileSystemAccessMode, FileSystemPath};
        let dir = Scratch::new("mounted-since");
        let home = dir.0.join("home");
        let place = home.join(".aws");
        let work = dir.0.join("work");
        let codex = dir.0.join("codex");
        for d in [&place, &work, &codex] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let guard = StateGuard::Deny(PrivateState {
            state_dir: dir.0.join("state"),
            private: dir.0.join("private"),
            also: vec![place.clone()],
        });
        // The list, built while the home is mounted once.
        let built = crate::mounts::with_table(ONE_MOUNT);
        let profile = permission_profile(
            crate::typed::Sandbox::ReadOnly,
            &guard,
            &codex,
            Some(&work),
            &[],
        )
        .expect("profile");
        let args: Vec<std::ffi::OsString> = [
            "codex-linux-sandbox".to_owned(),
            "--command-cwd".to_owned(),
            work.display().to_string(),
            "--permission-profile".to_owned(),
            serde_json::to_string(&profile).expect("json"),
            "--".to_owned(),
            "true".to_owned(),
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        let places = [place.clone()];
        assert_eq!(relisted(&args, Some(&places)).expect("nothing new"), None);
        drop(built);
        // The home bound at a second path since.
        let table = format!(
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 8:1 {} /again rw - ext4 /dev/sda1 rw\n",
            home.display()
        );
        let _since = crate::mounts::with_table(table.as_bytes());
        let again = relisted(&args, Some(&places))
            .expect("relisted")
            .expect("more denied");
        let at = again
            .iter()
            .position(|a| a == "--permission-profile")
            .expect("flag");
        let profile: PermissionProfile =
            serde_json::from_str(again[at + 1].to_str().expect("text")).expect("profile");
        let (policy, _) = profile.to_runtime_permissions();
        let denied: Vec<PathBuf> = policy
            .entries
            .iter()
            .filter(|e| e.access == FileSystemAccessMode::Deny)
            .filter_map(|e| match &e.path {
                FileSystemPath::Path { path } => Some(path.to_path_buf()),
                _ => None,
            })
            .collect();
        assert!(denied.contains(&PathBuf::from("/again/.aws")), "{denied:?}");
        // Where the new mount of it holds what the command needs: refused.
        let refused = late_link_targets(
            &places,
            Some(&places),
            &[PathBuf::from("/again/.aws/needed")],
            &[],
        )
        .expect_err("refused")
        .to_string();
        assert!(
            refused.contains(
                "/again/.aws is a second mount of what this command is denied, and holds"
            ) && refused.contains("which it needs"),
            "{refused}"
        );
    }

    /// A table of the root and `n` binds of `dir`, at /again0 and on.
    fn binds_of(dir: &Path, n: usize) -> Vec<u8> {
        let mut table = String::from("1 0 8:1 / / rw - ext4 /dev/sda1 rw\n");
        for i in 0..n {
            table.push_str(&format!(
                "{} 1 8:1 {} /again{i} rw - ext4 /dev/sda1 rw\n",
                i + 2,
                dir.display()
            ));
        }
        table.into_bytes()
    }

    /// A second mount of Codex's home made since the list was built has its
    /// databases denied there for the next command, as the list denies them
    /// at each second mount of the home it was built with.
    #[cfg(unix)]
    #[test]
    fn a_mount_of_codex_home_made_since_has_its_databases_denied() {
        use codex_protocol::permissions::{FileSystemAccessMode, FileSystemPath};
        let dir = Scratch::new("home-mounted-since");
        let place = dir.0.join("home/.aws");
        let work = dir.0.join("work");
        let codex = dir.0.join("codex");
        for d in [&place, &work, &codex] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let guard = StateGuard::Deny(PrivateState {
            state_dir: dir.0.join("state"),
            private: dir.0.join("private"),
            also: vec![place.clone()],
        });
        let built = crate::mounts::with_table(ONE_MOUNT);
        let profile = permission_profile(
            crate::typed::Sandbox::ReadOnly,
            &guard,
            &codex,
            Some(&work),
            &[],
        )
        .expect("profile");
        let args: Vec<std::ffi::OsString> = [
            "codex-linux-sandbox".to_owned(),
            "--command-cwd".to_owned(),
            work.display().to_string(),
            "--permission-profile".to_owned(),
            serde_json::to_string(&profile).expect("json"),
            "--".to_owned(),
            "true".to_owned(),
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        let places = [place.clone()];
        assert_eq!(relisted(&args, Some(&places)).expect("nothing new"), None);
        drop(built);
        // The directory that holds both bound at a second path since.
        let _since = crate::mounts::with_table(&binds_of(&dir.0, 1));
        let again = relisted(&args, Some(&places))
            .expect("relisted")
            .expect("more denied");
        let at = again
            .iter()
            .position(|a| a == "--permission-profile")
            .expect("flag");
        let profile: PermissionProfile =
            serde_json::from_str(again[at + 1].to_str().expect("text")).expect("profile");
        let (policy, _) = profile.to_runtime_permissions();
        let globs: Vec<String> = policy
            .entries
            .iter()
            .filter(|e| e.access == FileSystemAccessMode::Deny)
            .filter_map(|e| match &e.path {
                FileSystemPath::GlobPattern { pattern } => Some(pattern.clone()),
                _ => None,
            })
            .collect();
        assert!(
            globs.contains(&"/again0/codex/*.sqlite*".to_owned()),
            "{globs:?}"
        );
        // Started again with it, the helper finds nothing more.
        assert_eq!(relisted(&again, Some(&places)).expect("nothing more"), None);
    }

    /// A list built with Codex's home seen at two paths, as inside toolbox
    /// or distrobox, refuses none of its commands: what the list was built
    /// with was held to the rules then, and only what is new is now.
    #[cfg(unix)]
    #[test]
    fn a_home_mounted_twice_when_the_list_was_built_refuses_no_command() {
        let dir = Scratch::new("home-twice");
        let place = dir.0.join("home/.aws");
        let work = dir.0.join("work");
        let codex = dir.0.join("codex");
        for d in [&place, &work, &codex] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let guard = StateGuard::Deny(PrivateState {
            state_dir: dir.0.join("state"),
            private: dir.0.join("private"),
            also: vec![place.clone()],
        });
        let _table = crate::mounts::with_table(&binds_of(&dir.0, 1));
        for sandbox in [
            crate::typed::Sandbox::ReadOnly,
            crate::typed::Sandbox::WorkspaceWrite,
        ] {
            let writable = match sandbox {
                crate::typed::Sandbox::WorkspaceWrite => vec![work.clone()],
                crate::typed::Sandbox::ReadOnly => Vec::new(),
            };
            let profile = permission_profile(sandbox, &guard, &codex, Some(&work), &writable)
                .expect("profile");
            let args: Vec<std::ffi::OsString> = [
                "codex-linux-sandbox".to_owned(),
                "--command-cwd".to_owned(),
                work.display().to_string(),
                "--permission-profile".to_owned(),
                serde_json::to_string(&profile).expect("json"),
                "--".to_owned(),
                "true".to_owned(),
            ]
            .into_iter()
            .map(Into::into)
            .collect();
            let places = [place.clone()];
            assert_eq!(relisted(&args, Some(&places)).expect("nothing new"), None);
        }
    }

    /// A second mount of Codex's home made since, two or more levels inside
    /// what the command may write, refuses it; and so does a second mount of
    /// what a new link points at there.
    #[cfg(unix)]
    #[test]
    fn a_new_home_or_target_mount_deep_inside_what_the_command_may_write_refuses() {
        let dir = Scratch::new("home-deep");
        let codex = dir.0.join("codex");
        let work = dir.0.join("work");
        std::fs::create_dir_all(&codex).expect("dir");
        std::fs::create_dir_all(work.join("a/b")).expect("dir");
        let table = format!(
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 8:1 {} {} rw - ext4 /dev/sda1 rw\n",
            codex.display(),
            work.join("a/b").display()
        );
        let home_table = crate::mounts::with_table(table.as_bytes());
        let refused = late_targets(
            &[],
            Some(&[]),
            std::slice::from_ref(&codex),
            &[],
            std::slice::from_ref(&work),
        )
        .expect_err("refused")
        .to_string();
        assert!(
            refused.contains(&format!(
                "{} is a second mount of what this command is denied, two or more levels inside",
                work.join("a/b").display()
            )),
            "{refused}"
        );
        drop(home_table);
        // A new link's target, seen again deep inside what it may write.
        let place = dir.0.join("home/.aws");
        let elsewhere = dir.0.join("elsewhere");
        std::fs::create_dir_all(&place).expect("dir");
        std::fs::create_dir_all(&elsewhere).expect("dir");
        std::fs::write(elsewhere.join("creds"), "x").expect("creds");
        link(&elsewhere.join("creds"), &place.join("credentials"));
        let table = format!(
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 8:1 {} {} rw - ext4 /dev/sda1 rw\n",
            elsewhere.display(),
            work.join("a/b").display()
        );
        let _table = crate::mounts::with_table(table.as_bytes());
        let only = std::slice::from_ref(&place);
        let refused = late_link_targets(only, Some(only), &[], std::slice::from_ref(&work))
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains(&format!(
                "{} is a second mount of what this command is denied, two or more levels inside",
                work.join("a/b/creds").display()
            )),
            "{refused}"
        );
    }

    /// A Codex home, or a second mount of it, at a path holding a character
    /// a pattern gives a meaning to refuses: the pattern that denies its
    /// databases might not match it.
    #[cfg(unix)]
    #[test]
    fn a_home_whose_path_a_pattern_would_misread_refuses() {
        let dir = Scratch::new("home-special");
        let codex = dir.0.join("co[dex]");
        std::fs::create_dir_all(&codex).expect("dir");
        let private = PrivateState {
            state_dir: dir.0.join("state"),
            private: dir.0.join("private"),
            also: Vec::new(),
        };
        let one = crate::mounts::with_table(ONE_MOUNT);
        let refused = denied_paths(&private, &codex, None, &[])
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains("holds `[`, which a pattern gives a meaning to"),
            "{refused}"
        );
        drop(one);
        // A second mount of a plain one, made since at such a path.
        let plain = dir.0.join("codex");
        std::fs::create_dir_all(&plain).expect("dir");
        let table = format!(
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 8:1 {} /again* rw - ext4 /dev/sda1 rw\n",
            plain.display()
        );
        let _table = crate::mounts::with_table(table.as_bytes());
        let refused = late_targets(&[], Some(&[]), std::slice::from_ref(&plain), &[], &[])
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains("holds `*`, which a pattern gives a meaning to"),
            "{refused}"
        );
    }

    /// The cap counts the second mounts of what the list denies and those
    /// of Codex's home, each part on its own, as the builder counts them.
    #[cfg(unix)]
    #[test]
    fn the_cap_counts_the_second_mounts_of_what_is_denied_and_of_codex_home() {
        let dir = Scratch::new("cap-mounts");
        let home = dir.0.join("home");
        let place = home.join(".aws");
        let codex = dir.0.join("codex");
        for d in [&place, &codex] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let only = std::slice::from_ref(&place);
        {
            let _table = crate::mounts::with_table(&binds_of(&home, MAX_LINK_TARGETS + 1));
            let refused = late_link_targets(only, Some(only), &[], &[])
                .expect_err("refused")
                .to_string();
            assert!(
                refused.contains(
                    "513 second mounts of what they deny (513 made since the list was built)"
                ),
                "{refused}"
            );
        }
        let _table = crate::mounts::with_table(&binds_of(&codex, MAX_LINK_TARGETS + 1));
        let refused = late_targets(&[], Some(&[]), std::slice::from_ref(&codex), &[], &[])
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains("and 513 of Codex's home, more than the 512"),
            "{refused}"
        );
    }

    /// A mount table the helper cannot read as one refuses the command,
    /// whatever else it finds: it cannot tell where what it denies is
    /// mounted now.
    #[cfg(unix)]
    #[test]
    fn a_mount_table_that_cannot_be_read_refuses_the_command() {
        let dir = Scratch::new("table-unread");
        let place = dir.0.join("home/.aws");
        std::fs::create_dir_all(&place).expect("dir");
        let _table = crate::mounts::with_table(b"not a mount table\n");
        let only = std::slice::from_ref(&place);
        assert!(
            late_link_targets(only, Some(only), &[], &[])
                .is_err_and(|e| e.downcast_ref::<DenyRefused>().is_some())
        );
    }

    /// A second mount made since, two or more levels inside a directory the
    /// command may write, refuses it: it could move it out from under its
    /// denial.
    #[cfg(unix)]
    #[test]
    fn a_mount_made_since_deep_inside_what_the_command_may_write_refuses() {
        let dir = Scratch::new("mounted-deep");
        let home = dir.0.join("home");
        let place = home.join(".aws");
        let work = dir.0.join("work");
        std::fs::create_dir_all(&place).expect("dir");
        std::fs::create_dir_all(work.join("a/b")).expect("dir");
        let table = format!(
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 8:1 {} {} rw - ext4 /dev/sda1 rw\n",
            home.display(),
            work.join("a/b").display()
        );
        let _table = crate::mounts::with_table(table.as_bytes());
        let only = std::slice::from_ref(&place);
        let refused = late_link_targets(only, Some(only), &[], std::slice::from_ref(&work))
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains(&format!(
                "{} is a second mount of what this command is denied, two or more levels inside",
                work.join("a/b/.aws").display()
            )),
            "{refused}"
        );
    }

    /// What the profile will deny besides the list, the backend's scratch
    /// directories, counts toward the cap as each command counts it: the
    /// builder refuses the list the thread would start with, and so does each
    /// command's helper, over the same denied.
    #[cfg(unix)]
    #[test]
    fn the_builder_counts_the_scratch_it_will_deny_as_each_command_does() {
        let dir = Scratch::new("builder-cap-scratch");
        let codex = dir.0.join("codex");
        let scratch = dir.0.join("scratch");
        for d in [&codex, &scratch] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let private = PrivateState {
            state_dir: dir.0.join("state"),
            private: dir.0.join("private"),
            also: Vec::new(),
        };
        let _table = crate::mounts::with_table(&binds_of(&scratch, MAX_LINK_TARGETS + 1));
        assert!(denied_paths(&private, &codex, None, &[]).is_ok());
        let refused =
            denied_paths_with(&private, &codex, None, &[], std::slice::from_ref(&scratch))
                .expect_err("refused")
                .to_string();
        assert!(
            refused.contains("with 513 second mounts of what they deny, counted from each side"),
            "{refused}"
        );
        // The scratch directory as one of the paths a command's profile
        // denies, counted by the walk each command's helper makes before it
        // starts the command: its own 513 second mounts, and nothing new
        // found besides.
        let refused = late_targets(std::slice::from_ref(&scratch), Some(&[]), &[], &[], &[])
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains("0 second mounts of what the new ones point at"),
            "{refused}"
        );
        assert!(
            refused.contains("513 second mounts of what they deny"),
            "{refused}"
        );
        assert!(
            refused.contains("more than the 512 atomic follows"),
            "{refused}"
        );
    }

    /// The builder counts second mounts as each command does: from each
    /// side, so a list it starts a thread with never refuses its commands for
    /// the cap alone. A list whose second mounts come to the cap from each
    /// side both accept; the same list with one more of them, both refuse --
    /// the builder counting the list's own second mounts among what it
    /// counts, and each command's helper counting the same ones again from
    /// what the list denies.
    #[cfg(unix)]
    #[test]
    fn the_builder_counts_second_mounts_as_each_command_does() {
        use codex_protocol::permissions::{FileSystemAccessMode, FileSystemPath};
        let dir = Scratch::new("builder-cap");
        let home = dir.0.join("home");
        let place = home.join(".aws");
        let work = dir.0.join("work");
        let codex = dir.0.join("codex");
        for d in [&place, &work, &codex] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let private = PrivateState {
            state_dir: dir.0.join("state"),
            private: dir.0.join("private"),
            also: vec![place.clone()],
        };
        let guard = StateGuard::Deny(private.clone());
        let places = [place.clone()];
        // Binds one short of the cap: the place's 511 second mounts and the
        // place itself as theirs come to 512, which is the cap, so neither
        // the builder nor a command of a thread it starts with refuses.
        let table = crate::mounts::with_table(&binds_of(&home, MAX_LINK_TARGETS - 1));
        denied_paths(&private, &codex, Some(&work), &[]).expect("the cap counted from one side");
        // The list as a command's helper is handed it: the paths its profile
        // denies, the directories whose databases its patterns deny, and the
        // places the builder carried.
        let profile = permission_profile(
            crate::typed::Sandbox::ReadOnly,
            &guard,
            &codex,
            Some(&work),
            &[],
        )
        .expect("profile");
        let (policy, _) = profile.to_runtime_permissions();
        let paths = |mode: FileSystemAccessMode| -> Vec<PathBuf> {
            policy
                .entries
                .iter()
                .filter(|e| e.access == mode)
                .filter_map(|e| match &e.path {
                    FileSystemPath::Path { path } => Some(path.to_path_buf()),
                    _ => None,
                })
                .collect()
        };
        let denied = paths(FileSystemAccessMode::Deny);
        let homes: Vec<PathBuf> = policy
            .entries
            .iter()
            .filter(|e| e.access == FileSystemAccessMode::Deny)
            .filter_map(|e| match &e.path {
                FileSystemPath::GlobPattern { pattern } => pattern
                    .strip_suffix(SQLITE_FILES)
                    .and_then(|dir| dir.strip_suffix('/'))
                    .map(PathBuf::from),
                _ => None,
            })
            .collect();
        assert_eq!(homes, vec![codex.clone()], "{homes:?}");
        let under_cap = |name: &str| -> Vec<std::ffi::OsString> {
            [
                "codex-linux-sandbox".to_owned(),
                "--sandbox-policy-cwd".to_owned(),
                work.display().to_string(),
                "--command-cwd".to_owned(),
                work.display().to_string(),
                "--permission-profile".to_owned(),
                serde_json::to_string(&profile).expect("json"),
                "--".to_owned(),
                name.to_owned(),
            ]
            .into_iter()
            .map(Into::into)
            .collect()
        };
        let under = late_targets(&denied, Some(&places), &homes, &[], &[]).expect("512 accepted");
        assert_eq!(under, Late::default(), "{under:?}");
        assert_eq!(
            relisted(&under_cap("one"), Some(&places)).expect("nothing new"),
            None
        );
        drop(table);
        // As many binds as the cap: the place's 512 second mounts and the
        // place itself as theirs make 513, counted from each side.
        let _table = crate::mounts::with_table(&binds_of(&home, MAX_LINK_TARGETS));
        let refused = denied_paths(&private, &codex, Some(&work), &[])
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains("513 second mounts of what they deny, counted from each side"),
            "{refused}"
        );
        assert!(
            refused.contains("more than the 512 atomic follows"),
            "{refused}"
        );
        // The same list, counted as each of the thread's commands counts it,
        // is over the cap for the helper too, which is what the builder's
        // count is held to.
        let refused = late_targets(&denied, Some(&places), &homes, &[], &[])
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains("513 second mounts of what they deny"),
            "{refused}"
        );
        assert!(
            refused.contains("more than the 512 atomic follows"),
            "{refused}"
        );
        // And the helper's own entry point, which every command's start
        // calls, is refused by the same walk; its caller logs that and leaves
        // the command unrun.
        let refused = relisted(&under_cap("two"), Some(&places))
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains("513 second mounts of what they deny"),
            "{refused}"
        );
        assert!(
            refused.contains("more than the 512 atomic follows"),
            "{refused}"
        );
    }

    /// The helper, given a profile for a place named through a linked
    /// directory, which the list denies by its real spelling, leaves no link
    /// denied once the place has become one: the sandbox cannot mount on a
    /// link.
    #[cfg(unix)]
    #[test]
    fn a_place_under_a_linked_directory_that_became_a_link_leaves_no_link_denied() {
        use codex_protocol::permissions::{FileSystemAccessMode, FileSystemPath};
        let _table = crate::mounts::with_table(ONE_MOUNT);
        let dir_scratch = Scratch::new("relisted-spelled");
        let dir = dir_scratch.0.clone();
        let real_home = dir.join("data/u");
        let work = dir.join("work");
        let codex = dir.join("codex");
        for d in [&real_home, &work, &codex, &dir.join("elsewhere")] {
            std::fs::create_dir_all(d).expect("dir");
        }
        link(&real_home, &dir.join("home"));
        let place = dir.join("home/.netrc");
        std::fs::write(&place, "machine x").expect("netrc");
        let guard = StateGuard::Deny(PrivateState {
            state_dir: dir.join("state"),
            private: dir.join("private"),
            also: vec![place.clone()],
        });
        let profile = permission_profile(
            crate::typed::Sandbox::ReadOnly,
            &guard,
            &codex,
            Some(&work),
            &[],
        )
        .expect("profile");
        let args: Vec<std::ffi::OsString> = [
            "codex-linux-sandbox".to_owned(),
            "--sandbox-policy-cwd".to_owned(),
            work.display().to_string(),
            "--command-cwd".to_owned(),
            work.display().to_string(),
            "--permission-profile".to_owned(),
            serde_json::to_string(&profile).expect("json"),
            "--".to_owned(),
            "true".to_owned(),
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        let places = [place.clone()];
        assert_eq!(relisted(&args, Some(&places)).expect("nothing new"), None);
        let kept = dir.join("elsewhere/netrc");
        std::fs::write(&kept, "machine y").expect("netrc");
        let real_place = std::fs::canonicalize(&real_home)
            .expect("real")
            .join(".netrc");
        std::fs::remove_file(&real_place).expect("unlink");
        link(&kept, &real_place);
        let again = relisted(&args, Some(&places))
            .expect("relisted")
            .expect("more denied");
        let at = again
            .iter()
            .position(|a| a == "--permission-profile")
            .expect("flag");
        let profile: PermissionProfile =
            serde_json::from_str(again[at + 1].to_str().expect("text")).expect("profile");
        let (policy, _) = profile.to_runtime_permissions();
        let denied: Vec<PathBuf> = policy
            .entries
            .iter()
            .filter(|e| e.access == FileSystemAccessMode::Deny)
            .filter_map(|e| match &e.path {
                FileSystemPath::Path { path } => Some(path.to_path_buf()),
                _ => None,
            })
            .collect();
        let links: Vec<&PathBuf> = denied
            .iter()
            .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
            .collect();
        assert!(
            links.is_empty(),
            "links left denied: {links:?} in {denied:?}"
        );
        assert!(
            denied.contains(&std::fs::canonicalize(&kept).expect("real")),
            "{denied:?}"
        );
    }

    /// A denial the list holds inside what a new link points at is taken
    /// out, as the sandbox cannot mount one denial inside another.
    #[cfg(unix)]
    #[test]
    fn a_denial_inside_a_late_target_is_taken_out() {
        use codex_protocol::permissions::{FileSystemAccessMode, FileSystemPath};
        let _table = crate::mounts::with_table(ONE_MOUNT);
        let dir_scratch = Scratch::new("relisted-inner");
        let dir = dir_scratch.0.clone();
        let place = dir.join("home/.aws");
        let kept = dir.join("dev/kept");
        let work = dir.join("work");
        let codex = dir.join("codex");
        for d in [&place, &kept.join("inner"), &work, &codex] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let kept = std::fs::canonicalize(&kept).expect("real");
        let inner = kept.join("inner");
        let guard = StateGuard::Deny(PrivateState {
            state_dir: dir.join("state"),
            private: dir.join("private"),
            also: vec![place.clone(), inner.clone()],
        });
        let profile = permission_profile(
            crate::typed::Sandbox::ReadOnly,
            &guard,
            &codex,
            Some(&work),
            &[],
        )
        .expect("profile");
        let args: Vec<std::ffi::OsString> = [
            "codex-linux-sandbox".to_owned(),
            "--command-cwd".to_owned(),
            work.display().to_string(),
            "--permission-profile".to_owned(),
            serde_json::to_string(&profile).expect("json"),
            "--".to_owned(),
            "true".to_owned(),
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        let places = [place.clone(), inner.clone()];
        link(&kept, &place.join("kept"));
        let again = relisted(&args, Some(&places))
            .expect("relisted")
            .expect("more denied");
        let at = again
            .iter()
            .position(|a| a == "--permission-profile")
            .expect("flag");
        let profile: PermissionProfile =
            serde_json::from_str(again[at + 1].to_str().expect("text")).expect("profile");
        let (policy, _) = profile.to_runtime_permissions();
        let denied: Vec<PathBuf> = policy
            .entries
            .iter()
            .filter(|e| e.access == FileSystemAccessMode::Deny)
            .filter_map(|e| match &e.path {
                FileSystemPath::Path { path } => Some(path.to_path_buf()),
                _ => None,
            })
            .collect();
        assert!(denied.contains(&kept), "{denied:?}");
        assert!(!denied.contains(&inner), "{denied:?}");
    }

    /// Where no places come with the profile, what the list denies as a
    /// second mount of a link's target is not walked as a place: a link in
    /// it, as in what it shows, was never followed.
    #[cfg(unix)]
    #[test]
    fn a_second_mount_in_the_list_is_not_walked_as_a_place() {
        let dir_scratch = Scratch::new("late-inferred-alias");
        let dir = dir_scratch.0.clone();
        let place = dir.join("home/.config/gh");
        let shown = dir.join("dev/gh");
        let again = dir.join("again/gh");
        for d in [&place, &shown, &again, &dir.join("secret")] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let real = |p: &Path| std::fs::canonicalize(p).expect("real");
        link(&shown, &place.join("hosts"));
        let (shown, again) = (real(&shown), real(&again));
        // `again` is a second mount of what the place's link points at.
        let table = format!(
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 8:1 {} {} rw - ext4 /dev/sda1 rw\n",
            shown.display(),
            again.display()
        );
        let _table = crate::mounts::with_table(table.as_bytes());
        std::fs::write(dir.join("secret/key"), "x").expect("key");
        link(&dir.join("secret/key"), &again.join("key"));
        let built = [place.clone(), shown.clone(), again.clone()];
        let late = late_link_targets(&built, None, &[], &[]).expect("walked");
        assert_eq!(late, Late::default());
    }

    /// A place that is a link to a directory the list never denies, such as
    /// one of the system's top-level ones, is no place to hold what a new
    /// link elsewhere points at: that is denied, as the builder would.
    #[cfg(unix)]
    #[test]
    fn a_place_linked_to_a_never_denied_directory_covers_nothing() {
        let _table = crate::mounts::with_table(ONE_MOUNT);
        let dir = Scratch::new("late-top");
        // The test's own stand-in for one of the system's top-level
        // directories, so the walk through the linked place stays in it.
        let top = dir.0.join("top");
        std::fs::create_dir_all(top.join("elsewhere")).expect("dir");
        let _never = with_never_denied(&top);
        let place = dir.0.join("home/.aws");
        std::fs::create_dir_all(&place).expect("dir");
        let linked = dir.0.join("home/.docker");
        link(&top, &linked);
        let places = [place.clone(), linked];
        let creds = top.join("elsewhere/creds");
        std::fs::write(&creds, "x").expect("creds");
        link(&creds, &place.join("credentials"));
        let late = late_link_targets(std::slice::from_ref(&place), Some(&places), &[], &[])
            .expect("walked");
        assert_eq!(late.targets, [creds], "{late:?}");
    }

    /// The helper's own reading of its arguments: a turn's workspace-write
    /// profile counts its directory as writable and a read-only one does
    /// not; what is added replaces a place that became a link, and takes
    /// out what it holds.
    #[cfg(unix)]
    #[test]
    fn the_helper_relists_a_turns_profile() {
        let _table = crate::mounts::with_table(ONE_MOUNT);
        let dir_scratch = Scratch::new("relisted");
        let dir = dir_scratch.0.clone();
        let place = dir.join("home/.aws");
        let work = dir.join("work");
        let codex = dir.join("codex");
        for d in [&place, &work.join("deep"), &codex] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let guard = StateGuard::Deny(PrivateState {
            state_dir: dir.join("state"),
            private: dir.join("private"),
            also: vec![place.clone()],
        });
        use crate::typed::Sandbox;
        let args = |sandbox: Sandbox| -> Vec<std::ffi::OsString> {
            let writable = match sandbox {
                Sandbox::WorkspaceWrite => vec![work.clone()],
                Sandbox::ReadOnly => Vec::new(),
            };
            let profile = permission_profile(sandbox, &guard, &codex, Some(&work), &writable)
                .expect("profile");
            [
                "codex-linux-sandbox".to_owned(),
                "--sandbox-policy-cwd".to_owned(),
                work.display().to_string(),
                "--command-cwd".to_owned(),
                work.display().to_string(),
                "--permission-profile".to_owned(),
                serde_json::to_string(&profile).expect("json"),
                "--".to_owned(),
                "true".to_owned(),
            ]
            .into_iter()
            .map(Into::into)
            .collect()
        };
        let places = [place.clone()];
        let (rw, ro) = (args(Sandbox::WorkspaceWrite), args(Sandbox::ReadOnly));
        assert_eq!(relisted(&rw, Some(&places)).expect("nothing new"), None);
        // A key two levels inside the command's directory, linked in since.
        std::fs::write(work.join("deep/key"), "x").expect("key");
        link(&work.join("deep/key"), &place.join("credentials"));
        let refused = relisted(&rw, Some(&places))
            .expect_err("refused")
            .to_string();
        assert!(refused.contains("two or more levels"), "{refused}");
        let again = relisted(&ro, Some(&places))
            .expect("relisted")
            .expect("more denied");
        let profile = again[again
            .iter()
            .position(|a| a == "--permission-profile")
            .expect("flag")
            + 1]
        .to_str()
        .expect("text")
        .to_owned();
        let key = std::fs::canonicalize(work.join("deep/key")).expect("real");
        assert!(profile.contains(&key.display().to_string()), "{profile}");
    }

    #[cfg(unix)]
    #[test]
    fn a_second_mount_that_cannot_be_denied_refuses_the_list() {
        let dir_scratch = Scratch::new("second-mounts");
        let dir = dir_scratch.0.clone();
        let codex = dir.join("codex");
        for d in [&codex, &dir.join("home/.aws")] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let private = PrivateState {
            state_dir: dir.join("state"),
            private: dir.join("private"),
            also: vec![dir.join("home/.aws")],
        };
        let real = std::fs::canonicalize(&dir).expect("real");
        let rel = real.strip_prefix("/").expect("absolute");
        // The whole filesystem bound again at `at`.
        let bound = |at: &str| {
            format!("1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 8:1 / {at} rw - ext4 /dev/sda1 rw\n")
        };
        let refused = |table: String, cwd: Option<&Path>, writable: &[PathBuf]| {
            let _table = crate::mounts::with_table(table.as_bytes());
            denied_paths(&private, &codex, cwd, writable)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        // Bound where nothing needs it: denied there too, with Codex's
        // databases.
        {
            let _table = crate::mounts::with_table(bound("/again").as_bytes());
            let (denied, globs) = denied_paths(&private, &codex, None, &[]).expect("denied");
            assert!(
                denied.contains(&Path::new("/again").join(rel).join("home/.aws")),
                "{denied:?}"
            );
            assert!(
                globs.contains(&format!("/again/{}/codex/*.sqlite*", rel.display())),
                "{globs:?}"
            );
        }
        // Where its second mount would cover the thread's directory.
        let cwd = Path::new("/again").join(rel).join("home/.aws/work");
        let message = refused(bound("/again"), Some(&cwd), &[]);
        assert!(
            message.contains("is a second mount") && message.contains("which they need"),
            "{message}"
        );
        // Where it lies two levels inside a directory the thread may write:
        // the whole filesystem, bound there, is the thread's to write at `/`
        // too, and so is the place.
        let message = refused(bound("/w/a"), None, &[PathBuf::from("/w")]);
        assert!(
            message.contains("home/.aws is inside /, which atoms' commands may write"),
            "{message}"
        );
        // Codex's home bound two levels inside a writable directory: its
        // databases' pattern there is held to the same checks.
        let codex_real = std::fs::canonicalize(&codex).expect("real");
        let message = refused(
            format!(
                "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 8:1 {} /w/a/b rw - ext4 /dev/sda1 rw\n",
                codex_real.display()
            ),
            None,
            &[PathBuf::from("/w")],
        );
        assert!(
            message.contains("/w/a/b is a second mount") && message.contains("two or more levels"),
            "{message}"
        );
        // Too many second mounts of Codex's home alone: they count too.
        let mut many = String::from("1 0 8:1 / / rw - ext4 /dev/sda1 rw\n");
        for i in 0..=MAX_LINK_TARGETS {
            many.push_str(&format!(
                "{} 1 8:1 {} /h{i} rw - ext4 /dev/sda1 rw\n",
                i + 2,
                codex_real.display()
            ));
        }
        let message = refused(many, None, &[]);
        assert!(message.contains("other places, more than"), "{message}");
        // Too many second mounts to deny.
        let mut many = String::from("1 0 8:1 / / rw - ext4 /dev/sda1 rw\n");
        for i in 0..=MAX_LINK_TARGETS {
            many.push_str(&format!("{} 1 8:1 / /m{i} rw - ext4 /dev/sda1 rw\n", i + 2));
        }
        let message = refused(many, None, &[]);
        assert!(message.contains("other places, more than"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn nothing_runs_when_a_denial_would_cover_what_commands_need_or_may_write() {
        let dir_scratch = Scratch::new("covering");
        let dir = dir_scratch.0.clone();
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
        // A secret linked two levels into a directory the thread may write,
        // where a command could rename the directory above it; directly
        // inside, the sandbox's mount on it cannot be moved.
        std::fs::create_dir_all(work.join("deep")).expect("deep");
        std::fs::write(work.join("deep/creds"), "x").expect("creds");
        link(&work.join("deep/creds"), &home.join(".aws/credentials"));
        let message = refused(vec![home.join(".aws")], None, std::slice::from_ref(&work));
        assert!(message.contains("may write"), "{message}");
        std::fs::write(work.join("creds"), "x").expect("creds");
        link(&work.join("creds"), &home.join(".aws/credentials"));
        assert_eq!(
            refused(vec![home.join(".aws")], None, std::slice::from_ref(&work)),
            ""
        );
        // Too many links to follow.
        std::fs::remove_file(home.join(".aws/credentials")).expect("unlink");
        let many = home.join(".kube");
        std::fs::create_dir_all(&many).expect("kube");
        std::fs::write(dir.join("one"), "x").expect("one");
        for i in 0..=MAX_LINK_TARGETS {
            link(&dir.join("one"), &many.join(format!("l{i}")));
        }
        assert!(refused(vec![many.clone()], None, &[]).contains("links"));
        // Links inside the place they are in count for nothing.
        std::fs::remove_dir_all(&many).expect("unlink");
        std::fs::create_dir_all(&many).expect("kube");
        std::fs::write(many.join("one"), "x").expect("one");
        for i in 0..=MAX_LINK_TARGETS {
            link(&many.join("one"), &many.join(format!("l{i}")));
        }
        assert_eq!(refused(vec![many.clone()], None, &[]), "");
        // A place inside a directory the thread may write could be moved
        // away from its denial; a link inside a place, to the place, is
        // no reason to refuse.
        link(&home.join(".aws"), &home.join(".aws/current"));
        let message = refused(vec![home.join(".aws")], None, std::slice::from_ref(&dir));
        assert!(message.contains("may write"), "{message}");
        // With the home itself the working directory a turn may write, as a
        // project at the home's top is, its places directly inside stay.
        assert_eq!(
            refused(vec![home.join(".aws")], None, std::slice::from_ref(&home)),
            ""
        );
        std::fs::remove_file(home.join(".aws/current")).expect("unlink");
        // And the same places, safely, run.
        assert_eq!(refused(vec![home.join(".aws")], Some(&project), &[]), "");
        // A refusal is the machine's, not the model's.
        let private = PrivateState {
            state_dir: dir.join("state"),
            private: dir.join("private"),
            also: vec!["/".into()],
        };
        let refusal = denied_paths(&private, &codex, None, &[]).expect_err("refused");
        assert!(refusal.downcast_ref::<DenyRefused>().is_some(), "{refusal}");
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_is_denied_and_only_programs_devices_and_others_directories_are_left() {
        use std::os::unix::fs::PermissionsExt;
        let dir_scratch = Scratch::new("pipes");
        let dir = dir_scratch.0.clone();
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".ssh")).expect("ssh");
        std::fs::create_dir_all(dir.join("codex")).expect("codex home");
        let fifo = home.join(".netrc");
        let _ = std::fs::remove_file(&fifo);
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(made.success());
        // A program of the person's, a key and a directory elsewhere.
        std::fs::create_dir_all(dir.join("tools")).expect("tools");
        std::fs::write(dir.join("tools/python3"), "#!/bin/sh\n").expect("program");
        std::fs::set_permissions(
            dir.join("tools/python3"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("mode");
        std::fs::write(dir.join("key"), "x").expect("key");
        link(&dir.join("tools/python3"), &home.join(".ssh/bin/python3"));
        link(&dir.join("key"), &home.join(".ssh/id"));
        // Files with an execute bit that are no programs, as on a file
        // system that marks everything executable: one found by the walk,
        // and one a credential place itself links to.
        for name in ["marked-key", "marked-pgpass"] {
            std::fs::write(dir.join(name), "machine example.com password x\n").expect("file");
            std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o777))
                .expect("mode");
        }
        link(&dir.join("marked-key"), &home.join(".ssh/marked"));
        link(&dir.join("marked-pgpass"), &home.join(".pgpass"));
        // A credential place that links to a program is still denied.
        link(&dir.join("tools/python3"), &home.join(".vault-token"));
        let private = PrivateState {
            state_dir: dir.join("state"),
            private: dir.join("private"),
            also: vec![
                fifo.clone(),
                home.join(".ssh"),
                home.join(".pgpass"),
                home.join(".vault-token"),
            ],
        };
        let (denied, _) = denied_paths(&private, &dir.join("codex"), None, &[]).expect("denied");
        assert!(denied.contains(&fifo), "{denied:?}");
        let real = |p: &Path| std::fs::canonicalize(p).expect("real");
        assert!(denied.contains(&real(&dir.join("key"))), "{denied:?}");
        for marked in ["marked-key", "marked-pgpass"] {
            assert!(
                denied.contains(&real(&dir.join(marked))),
                "{marked}: {denied:?}"
            );
        }
        // Found by the walk, the program is left; linked to by a credential
        // place, it is denied.
        assert!(
            denied.contains(&real(&dir.join("tools/python3"))),
            "{denied:?}"
        );
        assert!(!deniable(&real(&dir.join("tools/python3")), false));
        // Whoever owns them: what the system keeps outside its program and
        // device directories, such as a cluster's credentials under /etc.
        assert!(
            Path::new("/etc/hostname").is_file(),
            "a file the system keeps"
        );
        assert!(deniable(Path::new("/etc/hostname"), false));
        assert!(!deniable(Path::new("/usr/bin/env"), false));
        assert!(!deniable(Path::new("/dev/null"), true));
        // A directory someone else owns, found by the walk, stays; one a
        // credential place links to is denied, but for the system's own.
        if crate::state::effective_uid() != 0 {
            assert!(
                !deniable(Path::new("/etc"), false),
                "root's, found by the walk"
            );
        }
        assert!(!deniable(Path::new("/etc"), true), "the system's own");
        // A directory under them that a credential place links to, such as
        // a cluster's, is denied whoever owns it.
        if Path::new("/etc/ssl").is_dir() {
            let ssl = std::fs::canonicalize("/etc/ssl").expect("real");
            if !SYSTEM_PREFIXES.iter().any(|p| ssl.starts_with(p)) {
                assert!(deniable(&ssl, true), "{ssl:?}");
            }
        }
        assert!(!deniable(Path::new("/"), true), "the root");
    }

    #[cfg(unix)]
    #[test]
    fn a_place_link_that_cannot_be_resolved_refuses_the_thread() {
        let dir = Scratch::new("place-unresolved");
        let home = dir.0.join("home");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(dir.0.join("codex")).expect("codex home");
        let aws = home.join(".aws");
        // A link to itself: ELOOP, for anyone.
        std::os::unix::fs::symlink(&aws, &aws).expect("link");
        let private = PrivateState {
            state_dir: dir.0.join("state"),
            private: dir.0.join("private"),
            also: vec![aws.clone()],
        };
        let refused = denied_paths(&private, &dir.0.join("codex"), None, &[])
            .expect_err("refused")
            .to_string();
        assert!(
            refused.contains(&format!(
                "{} is a link atomic cannot resolve",
                aws.display()
            )) && refused.contains("remove the link, or point it at something atomic can reach"),
            "{refused}"
        );
        // One that leads nowhere yet is passed over, as it always was.
        std::fs::remove_file(&aws).expect("unlink");
        std::os::unix::fs::symlink(home.join("not-there"), &aws).expect("link");
        denied_paths(&private, &dir.0.join("codex"), None, &[]).expect("passed over");
    }

    /// What a command could repoint refuses the thread: a place whose own
    /// chain, there now, runs through a link in what the thread may write; a
    /// link inside a place that does, or that leads nowhere yet into what
    /// it may write; and a place that leads into a second mount of what it
    /// may write, as much the thread's to write as the directory itself.
    #[cfg(unix)]
    #[test]
    fn a_place_or_a_link_in_one_a_command_could_repoint_refuses_the_thread() {
        let dir = Scratch::new("place-chains");
        let home = dir.0.join("home");
        let work = dir.0.join("work");
        let codex = dir.0.join("codex");
        let elsewhere = dir.0.join("elsewhere");
        for d in [&home, &work, &codex, &elsewhere, &dir.0.join("state")] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let place = home.join(".aws");
        let private = PrivateState {
            state_dir: dir.0.join("state"),
            private: dir.0.join("private"),
            also: vec![place.clone()],
        };
        let writable = std::slice::from_ref(&work);
        let refused = || {
            denied_paths(&private, &codex, Some(&work), writable)
                .err()
                .map(|e| e.to_string())
        };
        let one = crate::mounts::with_table(ONE_MOUNT);
        // (b) The place, there now, through a link in what it may write.
        std::fs::write(elsewhere.join("aws"), "[default]").expect("aws");
        link(&elsewhere.join("aws"), &work.join("hop"));
        link(&work.join("hop"), &place);
        let why = refused().expect("refused");
        assert!(
            why.contains(&format!(
                "{} leads by way of {}, a link in {}",
                place.display(),
                work.join("hop").display(),
                work.display()
            )),
            "{why}"
        );
        // (a) A link inside the place, leading nowhere yet into what it may
        // write, and one whose chain runs through a link there.
        std::fs::remove_file(&place).expect("unlink");
        std::fs::create_dir_all(&place).expect("dir");
        let config = place.join("config");
        link(&work.join("aws-config"), &config);
        let why = refused().expect("refused");
        assert!(
            why.contains(&format!(
                "{}, inside a place denied to atoms' commands, is a link to {}, which is not \
                 there yet and lies in {}",
                config.display(),
                work.join("aws-config").display(),
                work.display()
            )),
            "{why}"
        );
        std::fs::remove_file(&config).expect("unlink");
        link(&work.join("hop"), &config);
        let why = refused().expect("refused");
        assert!(
            why.contains(&format!(
                "{} leads by way of {}, a link in {}",
                config.display(),
                work.join("hop").display(),
                work.display()
            )),
            "{why}"
        );
        std::fs::remove_file(&config).expect("unlink");
        std::fs::remove_dir(&place).expect("rmdir");
        drop(one);
        // (c) Into a second mount of what it may write.
        let table = format!(
            "1 0 8:1 / / rw - ext4 /dev/sda1 rw\n2 1 8:1 {} /again rw - ext4 /dev/sda1 rw\n",
            work.display()
        );
        let _table = crate::mounts::with_table(table.as_bytes());
        link(Path::new("/again/aws"), &place);
        let why = refused().expect("refused");
        assert!(
            why.contains(&format!(
                "{} is a link to /again/aws, which is not there yet and lies in /again",
                place.display()
            )),
            "{why}"
        );
        drop(_table);
        let one = crate::mounts::with_table(ONE_MOUNT);
        // A place not there yet, under a directory that is a link into what
        // it may write, leading nowhere yet.
        std::fs::remove_file(&place).expect("unlink");
        let config = home.join(".config");
        link(&work.join("cfg"), &config);
        let gh = config.join("gh");
        let listed = PrivateState {
            also: vec![gh.clone()],
            ..private.clone()
        };
        let why = denied_paths(&listed, &codex, Some(&work), writable)
            .expect_err("refused")
            .to_string();
        assert!(
            why.contains(&format!(
                "{}, which is not there yet, would be made at {}, in {}",
                gh.display(),
                work.join("cfg/gh").display(),
                work.display()
            )),
            "{why}"
        );
        // A place that is a link in what the thread may write, its target
        // there now: a command could point it elsewhere.
        link(&elsewhere.join("aws"), &work.join(".aws"));
        let in_work = PrivateState {
            also: vec![work.join(".aws")],
            ..private.clone()
        };
        let why = denied_paths(&in_work, &codex, Some(&work), writable)
            .expect_err("refused")
            .to_string();
        assert!(
            why.contains(&format!(
                "{} is a link in {}, which atoms' commands may write",
                work.join(".aws").display(),
                work.display()
            )),
            "{why}"
        );
        drop(one);
        let _table = crate::mounts::with_table(ONE_MOUNT);
        // A place whose chain stays out of what it may write: denied.
        link(&elsewhere.join("aws"), &place);
        assert_eq!(refused(), None);
    }

    /// A place that is a link leading nowhere yet refuses the thread when
    /// where it would lead, through each further link, lies in what the
    /// thread may write or in atomic's state, where a command could put a
    /// file for what reads the place to find, or cannot be told; one that
    /// leads elsewhere is passed over.
    #[cfg(unix)]
    #[test]
    fn a_dangling_place_link_into_what_the_thread_may_write_refuses_it() {
        let dir = Scratch::new("place-dangling");
        let home = dir.0.join("home");
        let work = dir.0.join("work");
        let codex = dir.0.join("codex");
        let state = dir.0.join("state");
        for d in [&home, &work, &codex, &state] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let _table = crate::mounts::with_table(ONE_MOUNT);
        let place = home.join(".aws");
        let private = PrivateState {
            state_dir: state.clone(),
            private: dir.0.join("private"),
            also: vec![place.clone()],
        };
        let refused = |writable: &[PathBuf]| {
            denied_paths(&private, &codex, Some(&work), writable)
                .err()
                .map(|e| e.to_string())
        };
        let writable = std::slice::from_ref(&work);
        // Into what the thread may write.
        link(&work.join("aws"), &place);
        let why = refused(writable).expect("refused");
        assert!(
            why.contains(&format!(
                "{} is a link to {}, which is not there yet and lies in {}",
                place.display(),
                work.join("aws").display(),
                work.display()
            )),
            "{why}"
        );
        // Into atomic's state, for a thread that writes nothing.
        std::fs::remove_file(&place).expect("unlink");
        link(&state.join("aws"), &place);
        let why = refused(&[]).expect("refused");
        assert!(
            why.contains(&format!("lies in {}", state.display())),
            "{why}"
        );
        // Through a further link, also leading nowhere yet.
        std::fs::remove_file(&place).expect("unlink");
        link(&work.join("aws"), &dir.0.join("hop"));
        link(&dir.0.join("hop"), &place);
        let why = refused(writable).expect("refused");
        assert!(
            why.contains(&format!("lies in {}", work.display())),
            "{why}"
        );
        // Through a link partway along, itself leading nowhere yet, into
        // what the thread may write.
        std::fs::remove_file(&place).expect("unlink");
        std::fs::create_dir_all(dir.0.join("elsewhere")).expect("dir");
        link(&work.join("d"), &dir.0.join("elsewhere/d"));
        link(&dir.0.join("elsewhere/d/aws"), &place);
        let why = refused(writable).expect("refused");
        assert!(
            why.contains(&format!(
                "is a link to {}, which is not there yet and lies in {}",
                work.join("d/aws").display(),
                work.display()
            )),
            "{why}"
        );
        // Through a link a command may point elsewhere: one partway along in
        // what the thread may write, leading out of it now.
        std::fs::remove_file(&place).expect("unlink");
        link(&dir.0.join("elsewhere"), &work.join("lnk"));
        link(&work.join("lnk/aws"), &place);
        let why = refused(writable).expect("refused");
        assert!(
            why.contains(&format!(
                "leads by way of {}, a link in {}",
                work.join("lnk").display(),
                work.display()
            )),
            "{why}"
        );
        // And a further link that is itself in what the thread may write.
        std::fs::remove_file(&place).expect("unlink");
        link(&dir.0.join("elsewhere/aws"), &work.join("hop"));
        link(&work.join("hop"), &place);
        let why = refused(writable).expect("refused");
        assert!(
            why.contains(&format!(
                "leads by way of {}, a link in {}",
                work.join("hop").display(),
                work.display()
            )),
            "{why}"
        );
        // Up through a directory of what the thread may write, back out of
        // it: a command could make that directory a link.
        std::fs::remove_file(&place).expect("unlink");
        std::fs::create_dir_all(work.join("sub")).expect("dir");
        link(&work.join("sub/../../elsewhere/aws"), &place);
        let why = refused(writable).expect("refused");
        assert!(
            why.contains(&format!(
                "leads by way of {}, and climbs back out of it, in {}",
                work.join("sub").display(),
                work.display()
            )),
            "{why}"
        );
        // Somewhere it cannot tell: up from what is not there.
        std::fs::remove_file(&place).expect("unlink");
        link(&dir.0.join("missing/../aws"), &place);
        let why = refused(writable).expect("refused");
        assert!(
            why.contains("atomic cannot tell where it would lead"),
            "{why}"
        );
        // Elsewhere: passed over, as before.
        std::fs::remove_file(&place).expect("unlink");
        link(&dir.0.join("elsewhere/aws"), &place);
        assert_eq!(refused(writable), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_secret_is_denied_as_what_it_points_at_and_links_are_followed_deep() {
        let dir_scratch = Scratch::new("linked-secrets");
        let dir = dir_scratch.0.clone();
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
        let dir_scratch = Scratch::new("nested-deny");
        let dir = dir_scratch.0.clone();
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
            let (home_scratch, auth_scratch) =
                (Scratch::new("home-headers"), Scratch::new("auth-headers"));
            let (home, auth) = (home_scratch.0.clone(), auth_scratch.0.clone());
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

    /// A scratch directory of the test's own, removed when dropped,
    /// whatever the test came to.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        /// A new directory with a name not to guess, made only where
        /// nothing is.
        fn new(tag: &str) -> Self {
            use std::hash::{BuildHasher, Hasher};
            for _ in 0..16 {
                let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
                hasher.write_u32(std::process::id());
                let d = std::env::temp_dir()
                    .join(format!("atomic-codex-{tag}-{:016x}", hasher.finish()));
                match std::fs::create_dir(&d) {
                    Ok(()) => return Self(std::fs::canonicalize(d).expect("real")),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("scratch: {e}"),
                }
            }
            panic!("scratch: no new name");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("atomic-codex-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("mkdir");
        // Under its real spelling, as the deny list spells what it finds.
        std::fs::canonicalize(&d).expect("real")
    }

    /// A scratch directory this call alone has: named by a counter and the
    /// clock as well as the pid, and made with `create_dir`, so a second run
    /// says so instead of planting its canaries where the first left them.
    fn scratch_once(tag: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "atomic-codex-{tag}-{}-{n}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir(&d).expect("this test's own scratch directory");
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
            let (home_scratch, cwd_scratch) = (Scratch::new("home"), Scratch::new("cwd"));
            let (home, cwd) = (home_scratch.0.clone(), cwd_scratch.0.clone());
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

    /// A Codex atom's key to an endpoint on this machine over plain http, in
    /// an environment that names no proxy and exempts nothing, with Codex's
    /// `respect_system_proxy` feature off and on. On Linux, where the feature
    /// finds no platform settings, the configuration is made either way.
    /// Elsewhere the platform's settings may name a proxy atomic cannot read:
    /// with the feature off the key is refused unless NO_PROXY exempts the
    /// host, and with it on, where NO_PROXY changes nothing, it is refused
    /// with the feature named.
    #[test]
    fn a_key_the_systems_proxy_settings_may_carry_is_refused_where_they_apply() {
        on_codex_runtime(async {
            for name in [
                "HTTP_PROXY",
                "http_proxy",
                "ALL_PROXY",
                "all_proxy",
                "NO_PROXY",
                "no_proxy",
            ] {
                assert!(
                    std::env::var_os(name).is_none(),
                    "{name} is set in this test's environment"
                );
            }
            let endpoint = ModelEndpoint {
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "synthetic".into(),
                api_key_env: Some("PATH".into()),
                sign_in: None,
            };
            let plain = scratch("home-no-system-proxy");
            let default = new_config(&endpoint, &plain, &plain, Arg0DispatchPaths::default()).await;
            let respecting = scratch("home-system-proxy");
            std::fs::write(
                respecting.join("config.toml"),
                "[features]\nrespect_system_proxy = true\n",
            )
            .expect("config");
            let respected = new_config(
                &endpoint,
                &respecting,
                &respecting,
                Arg0DispatchPaths::default(),
            )
            .await;
            if cfg!(target_os = "linux") {
                default.expect("no proxy is named");
                respected.expect("no proxy is named, and Linux has no platform settings");
                // A proxy value only Codex's own resolver takes, with a space
                // before it: reqwest's default reads it as none, and with the
                // feature on Codex sends the key to it, so the key is refused.
                let spaced = |name: &str| {
                    (name == "HTTP_PROXY").then(|| " http://proxy.example:3128".to_owned())
                };
                new_config_reading(
                    &endpoint,
                    &plain,
                    &plain,
                    Arg0DispatchPaths::default(),
                    spaced,
                )
                .await
                .expect("reqwest's default takes no proxy from it");
                let message = format!(
                    "{:#}",
                    new_config_reading(
                        &endpoint,
                        &respecting,
                        &respecting,
                        Arg0DispatchPaths::default(),
                        spaced,
                    )
                    .await
                    .expect_err("refused")
                );
                assert!(message.contains("HTTP_PROXY names a proxy"), "{message}");
            } else {
                let message = format!("{:#}", default.expect_err("refused"));
                assert!(
                    message.contains("the system's proxy settings")
                        && message.contains(
                            "set NO_PROXY to include 127.0.0.1, or use an https endpoint"
                        ),
                    "{message}"
                );
                let message = format!("{:#}", respected.expect_err("refused"));
                assert!(
                    message.contains("turn respect_system_proxy off, or use an https endpoint"),
                    "{message}"
                );
            }
        });
    }

    /// The configuration every Codex thread is started with leaves Codex's
    /// own repository-instruction discovery switched off, on both of the
    /// settings Codex gates it on.
    #[test]
    fn no_threads_configuration_discovers_repository_instructions() {
        on_codex_runtime(async {
            let home_scratch = Scratch::new("home-docs");
            let home = home_scratch.0.clone();
            let endpoint = ModelEndpoint {
                base_url: "http://127.0.0.1:9/unused".into(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: None,
            };
            let config = new_config(&endpoint, &home, &home, Arg0DispatchPaths::default())
                .await
                .expect("config");
            assert_eq!(config.project_doc_max_bytes, 0);
            assert_eq!(
                config.active_project.trust_level,
                Some(TrustLevel::Untrusted),
                "an unset trust level still lets Codex read the project's docs"
            );
            assert!(
                config.features.enabled(Feature::SkipHostSkillDiscovery),
                "without it Codex searches the checkout for `.agents/skills`"
            );
        });
    }

    /// A turn whose working directory is inside a checkout sees none of the
    /// instruction files lying about it: not the `AGENTS.md` at the project
    /// root Codex walks up to, nor the `AGENTS.override.md` in the turn's own
    /// directory, which is the one Codex would read there. Both are there to
    /// be seen: the turn runs twice, as a step's repair and resume turns do,
    /// and neither request holds either.
    #[test]
    fn a_turn_is_sent_no_instruction_file_of_the_checkout_it_runs_in() {
        on_codex_runtime(async {
            let dir = scratch_once("ambient-docs");
            let (root, nested) = (dir.join("repo"), dir.join("repo/sub"));
            for d in [&root, &nested] {
                std::fs::create_dir_all(d).expect("directory");
            }
            // A project root, so Codex walks up from the turn's directory to
            // it, as it does in a checkout or a worktree.
            std::fs::create_dir(root.join(".git")).expect("the project root's marker");
            std::fs::write(root.join("AGENTS.md"), "AMBIENT-CANARY-ROOT").expect("root doc");
            // In one directory Codex reads the first of these two names, so
            // this is the file its own directory would contribute.
            std::fs::write(nested.join("AGENTS.override.md"), "AMBIENT-CANARY-OVERRIDE")
                .expect("the local override");
            let model = FakeModel::start(vec![Reply::Text("pong".into()); 2])
                .await
                .expect("fake model");
            let home = dir.join("codex");
            std::fs::create_dir_all(&home).expect("codex home");
            let endpoint = ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: None,
            };
            let first = TurnSpec {
                prompt: "ping".into(),
                ..TurnSpec::default()
            };
            let mut turns = 0;
            let (last, report) = converse(
                &CodexPaths::without_helpers(),
                &home,
                &nested,
                &endpoint,
                &StateGuard::NoProjectState,
                &first,
                |answer| {
                    assert_eq!(answer, "pong");
                    turns += 1;
                    (turns < 2).then(|| TurnSpec {
                        prompt: "again".into(),
                        ..TurnSpec::default()
                    })
                },
            )
            .await
            .expect("both turns");
            assert_eq!(last, "pong");
            assert_eq!(report.turns, 2, "the thread ran both turns");
            let requests = model.requests();
            assert_eq!(requests.len(), 2, "requests: {requests:#?}");
            for (i, request) in requests.iter().enumerate() {
                let sent = request.to_string();
                // Each request is there to read: its own prompt is in it.
                assert!(
                    sent.contains(if i == 0 { "ping" } else { "again" }),
                    "turn {i}: {sent}"
                );
                for canary in ["AMBIENT-CANARY-ROOT", "AMBIENT-CANARY-OVERRIDE"] {
                    assert!(
                        !sent.contains(canary),
                        "turn {i} was sent repository instructions: {canary} is in {sent}"
                    );
                }
            }
        });
    }

    /// Nor any skill of that checkout. Pinned Codex loads
    /// `<dir>/.agents/skills/*/SKILL.md` from every directory between the
    /// project root and the thread's working directory, gated by neither the
    /// project's trust level nor the instruction budget, and a `$name` in a
    /// turn's text then sends that skill's `SKILL.md` to the model. The
    /// mention is in the prompt, so the request would show the skill's text
    /// beside it; it shows none.
    #[test]
    fn a_turn_is_sent_no_skill_of_the_checkout_it_runs_in() {
        on_codex_runtime(async {
            let dir = scratch_once("ambient-skills");
            let (root, cwd) = (dir.join("repo"), dir.join("repo/sub"));
            for d in [&root, &cwd] {
                std::fs::create_dir_all(d).expect("directory");
            }
            std::fs::create_dir(root.join(".git")).expect("the project root's marker");
            let skill = root.join(".agents/skills/canary-skill");
            std::fs::create_dir_all(&skill).expect("skill directory");
            std::fs::write(
                skill.join("SKILL.md"),
                "---\nname: canary-skill\ndescription: Say hello, once.\n---\n\n\
                 AMBIENT-SKILL-CANARY say hello and nothing else\n",
            )
            .expect("SKILL.md");
            let model = FakeModel::start(vec![Reply::Text("pong".into())])
                .await
                .expect("fake model");
            let home = dir.join("codex");
            std::fs::create_dir_all(&home).expect("codex home");
            let endpoint = ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: None,
            };
            let (answer, report) = converse(
                &CodexPaths::without_helpers(),
                &home,
                &cwd,
                &endpoint,
                &StateGuard::NoProjectState,
                &TurnSpec {
                    prompt: "run $canary-skill for ping".into(),
                    ..TurnSpec::default()
                },
                |_| None,
            )
            .await
            .expect("one turn");
            assert_eq!(answer, "pong");
            assert_eq!(report.turns, 1, "the thread ran its one turn");
            let requests = model.requests();
            assert_eq!(requests.len(), 1, "requests: {requests:#?}");
            let sent = requests[0].to_string();
            // The request is there to read, and so is the mention in it.
            assert!(sent.contains("$canary-skill"), "{sent}");
            assert!(
                !sent.contains("AMBIENT-SKILL-CANARY"),
                "the turn was sent a repository skill: AMBIENT-SKILL-CANARY is in {sent}"
            );
        });
    }

    /// Nor the Codex home's own instruction file. The home is atomic's
    /// per-project state directory, so it is not a checkout's path, but
    /// pinned Codex reads `AGENTS.override.md` and `AGENTS.md` there as global
    /// instructions for every thread it starts, and an atom's inputs are only
    /// what its file declares. The host declares none: it reads nothing, so a
    /// file planted in the home reaches no model.
    #[test]
    fn a_turn_is_sent_no_instruction_file_of_the_codex_home() {
        on_codex_runtime(async {
            let dir = scratch_once("home-instructions");
            let (home, cwd) = (dir.join("codex"), dir.join("work"));
            std::fs::create_dir_all(&home).expect("codex home");
            std::fs::create_dir_all(&cwd).expect("working directory");
            std::fs::write(
                home.join("AGENTS.override.md"),
                "AMBIENT-HOME-CANARY-OVERRIDE",
            )
            .expect("the override Codex reads first");
            std::fs::write(home.join("AGENTS.md"), "AMBIENT-HOME-CANARY").expect("AGENTS.md");
            let model = FakeModel::start(vec![Reply::Text("pong".into())])
                .await
                .expect("fake model");
            let endpoint = ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: None,
            };
            let (answer, report) = converse(
                &CodexPaths::without_helpers(),
                &home,
                &cwd,
                &endpoint,
                &StateGuard::NoProjectState,
                &TurnSpec {
                    prompt: "ping".into(),
                    ..TurnSpec::default()
                },
                |_| None,
            )
            .await
            .expect("one turn");
            assert_eq!(answer, "pong");
            assert_eq!(report.turns, 1, "the thread ran its one turn");
            let requests = model.requests();
            assert_eq!(requests.len(), 1, "requests: {requests:#?}");
            let sent = requests[0].to_string();
            // The request is there to read: its prompt is in it.
            assert!(sent.contains("ping"), "{sent}");
            for canary in ["AMBIENT-HOME-CANARY-OVERRIDE", "AMBIENT-HOME-CANARY"] {
                assert!(
                    !sent.contains(canary),
                    "the turn was sent the Codex home's instructions: {canary} is in {sent}"
                );
            }
        });
    }

    /// Only Codex's Linux sandbox helper keeps a thread's commands out of
    /// the paths its profile denies, apart from macOS's own sandbox, so
    /// anywhere but macOS a thread with something to deny and no helper is
    /// refused before any model call, and is never run with less denied.
    #[test]
    fn a_thread_with_paths_to_deny_and_no_sandbox_helper_is_refused_except_on_macos() {
        let denied = [PathBuf::from("/scratch/root")];
        let helperless = CodexPaths::without_helpers();
        let why = refuse_denied_without_helper(&helperless, &denied, false)
            .expect_err("no helper to deny with")
            .to_string();
        assert!(
            why.contains("/scratch/root") && why.contains("sandbox helper"),
            "{why}"
        );
        refuse_denied_without_helper(&helperless, &[], false).expect("nothing to deny");
        refuse_denied_without_helper(&helperless, &denied, true).expect("macOS denies itself");
        let with =
            CodexPaths::with_helpers("/bin/atomic".into(), "/bin/codex-linux-sandbox".into());
        refuse_denied_without_helper(&with, &denied, false).expect("a helper denies");

        if cfg!(target_os = "macos") {
            return;
        }
        on_codex_runtime(async move {
            let model = FakeModel::start(vec![Reply::Text("pong".into())])
                .await
                .expect("fake model");
            let (home, cwd) = (scratch("deny-home"), scratch("deny-cwd"));
            let endpoint = ModelEndpoint {
                base_url: model.base_url(),
                model: "synthetic".into(),
                api_key_env: None,
                sign_in: None,
            };
            let spec = TurnSpec {
                prompt: "ping".into(),
                denied: denied.to_vec(),
                ..TurnSpec::default()
            };
            let err = run(
                &CodexPaths::without_helpers(),
                &home,
                &cwd,
                &endpoint,
                &StateGuard::NoProjectState,
                &spec,
            )
            .await
            .expect_err("refused without a helper");
            assert!(err.to_string().contains("sandbox helper"), "{err:#}");
            assert_eq!(model.calls(), 0, "refused before any model call");
        });
    }

    /// A turn's profile, as atomic builds it, denies the scratch root and
    /// so never reads the whole disk: what it reads are its readable roots,
    /// the deepest entry for a path winning. Its own scratch directory, a
    /// writable root inside the denied one, is one of them, and the scratch
    /// root is unreadable. (Codex's macOS sandbox builds its reads from these
    /// roots; that it then reads its own scratch is not verified on macOS.)
    #[test]
    fn a_turns_profile_reads_its_own_scratch_inside_a_denied_root() {
        // Outside the system's temporary directory, as atomic keeps scratch
        // roots.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/atomic-core-tests")
            .join(format!("profile-reads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dir = {
            std::fs::create_dir_all(&dir).expect("dir");
            std::fs::canonicalize(&dir).expect("canonical")
        };
        let (home, cwd, root) = (dir.join("home"), dir.join("cwd"), dir.join("state/scratch"));
        let own = root.join("step/attempt");
        for d in [&home, &cwd, &own] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let profile = deny_also(
            permission_profile(
                crate::typed::Sandbox::WorkspaceWrite,
                &StateGuard::NoProjectState,
                &home,
                Some(&cwd),
                &[own.clone(), cwd.clone()],
            )
            .expect("profile"),
            std::slice::from_ref(&root),
        )
        .expect("denied");
        // With the turn's writable roots as its workspace roots, as
        // `converse_async` sets them, and resolved as Codex's macOS sandbox
        // resolves it.
        let roots: Vec<AbsolutePathBuf> = [&cwd, &own]
            .into_iter()
            .map(|r| AbsolutePathBuf::from_absolute_path(r).expect("absolute"))
            .collect();
        let resolved = profile
            .materialize_project_roots_with_workspace_roots(&roots)
            .file_system_sandbox_policy()
            .resolve_for_enforcement(
                &cwd,
                codex_protocol::permissions::FileSystemPolicyProjection::PreserveMutableRoots,
            )
            .expect("resolved");
        assert!(!resolved.has_full_disk_read_access);
        assert!(
            resolved.readable_roots.iter().any(|r| r.as_path() == own),
            "{:?}, writable {:?}",
            resolved.readable_roots,
            resolved.writable_roots
        );
        assert!(
            resolved
                .unreadable_roots
                .iter()
                .any(|r| r.as_path() == root),
            "{:?}",
            resolved.unreadable_roots
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Codex sends TurnComplete before it marks the thread idle; a follow-up
    /// submitted in that window must wait, not fail as NotIdle.
    #[test]
    fn follow_up_turns_submitted_back_to_back_all_run() {
        const TURNS: usize = 100;
        on_codex_runtime(async {
            let replies = (0..TURNS).map(|i| Reply::Text(format!("r{i}"))).collect();
            let model = FakeModel::start(replies).await.expect("fake model");
            let (home_scratch, cwd_scratch) = (Scratch::new("home3"), Scratch::new("cwd3"));
            let (home, cwd) = (home_scratch.0.clone(), cwd_scratch.0.clone());
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
        let state_scratch = Scratch::new("exec-sandbox");
        let state = state_scratch.0.clone();
        let scratch_dir = state.join("scratch/a1");
        std::fs::create_dir_all(&scratch_dir).expect("scratch");
        // A place there, as a place not there yet in what a command may
        // write would refuse the list.
        std::fs::create_dir_all(state.join("keys")).expect("keys");
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
        // The helper gets its registry's TMPDIR, PATH and the places its list
        // was built with, nothing else: no variable the command declares
        // loads into it.
        let names: Vec<&str> = confined.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["TMPDIR", "PATH", PLACES]);
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
        let state_scratch = Scratch::new("exec-held");
        let state = state_scratch.0.clone();
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
    fn a_commands_proxy_token_reaches_its_helper_on_a_socket_only() {
        use atomic_exec::Confine;
        let state_scratch = Scratch::new("exec-token-socket");
        let state = state_scratch.0.clone();
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
            panic!("the confined command holds exactly one secret variable");
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
            &[(key, "")][..],
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
        let home_scratch = Scratch::new("many-projects");
        let home = home_scratch.0.clone();
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

    /// atomic's own link to itself is made in directories of this user's
    /// that no one else may write, reached never through a link: a link
    /// planted where one of them goes, or one someone else may write, leaves
    /// none, and Codex's own helper is used.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_helpers_link_is_made_only_in_this_users_own_directories() {
        use std::os::unix::fs::PermissionsExt;
        let home = scratch("own-helper");
        let link = own_helper_link(&home).expect("made");
        assert_eq!(
            std::fs::read_link(&link).expect("a link"),
            std::env::current_exe().expect("this test")
        );
        assert_eq!(
            own_helper_link(&home),
            Some(link.clone()),
            "mended, not made twice"
        );
        // Directories of this user's that others may write, as a umask of
        // 002 leaves them, are narrowed and used.
        let tmp = home.join("tmp");
        let helpers = tmp.join(HELPER_DIR);
        for dir in [&home, &tmp, &helpers] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o775)).expect("mode");
        }
        assert_eq!(own_helper_link(&home), Some(link.clone()), "a umask of 002");
        for dir in [&home, &tmp, &helpers] {
            let mode = std::fs::metadata(dir).expect("meta").permissions().mode();
            assert_eq!(mode & 0o022, 0, "{} is writable by others", dir.display());
        }
        std::fs::remove_dir_all(&helpers).expect("removed");
        let elsewhere = scratch("own-helper-elsewhere");
        std::os::unix::fs::symlink(&elsewhere, &helpers).expect("link");
        assert_eq!(
            own_helper_link(&home),
            None,
            "a link where a directory goes"
        );
        assert!(
            std::fs::read_dir(&elsewhere)
                .expect("elsewhere")
                .next()
                .is_none(),
            "something was made where the link leads"
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// A directory others may write is not private, whoever owns it, so one
    /// atomic cannot narrow is never used.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_others_may_write_is_not_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("not-private");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).expect("mode");
        let held: std::os::fd::OwnedFd = std::fs::File::open(&dir).expect("open").into();
        assert!(!private_dir(&held), "group write");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o757)).expect("mode");
        assert!(!private_dir(&held), "other write");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("mode");
        assert!(private_dir(&held));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The helper's directory is left readable only when it is a real
    /// directory: a link planted under its name is denied as what it points
    /// at, as every other entry of the Codex home's `tmp/` is.
    #[cfg(unix)]
    #[test]
    fn a_link_planted_as_the_helpers_directory_is_denied() {
        let dir = std::fs::canonicalize(scratch("helper-dir-link")).expect("real");
        let codex = dir.join("codex");
        std::fs::create_dir_all(codex.join("tmp")).expect("codex home");
        let theirs = dir.join("theirs");
        std::fs::create_dir_all(&theirs).expect("theirs");
        std::os::unix::fs::symlink(&theirs, codex.join("tmp").join(HELPER_DIR)).expect("link");
        let private = PrivateState {
            state_dir: dir.join("state"),
            private: dir.join("private"),
            also: Vec::new(),
        };
        let (denied, _) = denied_paths(&private, &codex, None, &[]).expect("denied");
        assert!(denied.contains(&theirs), "{denied:?}");
        std::fs::remove_file(codex.join("tmp").join(HELPER_DIR)).expect("unlinked");
        std::fs::create_dir_all(codex.join("tmp").join(HELPER_DIR)).expect("real");
        let (denied, _) = denied_paths(&private, &codex, None, &[]).expect("denied");
        assert!(
            !denied.iter().any(|d| d.ends_with(HELPER_DIR)),
            "the real directory was denied: {denied:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        // The Linux sandbox helper by its arguments, as Codex runs atomic
        // itself when it could not link to it under the helper's name.
        #[cfg(target_os = "linux")]
        for by_args in [
            &["/opt/atomic", "--sandbox-policy-cwd", "/x", "--", "true"][..],
            &[
                "/opt/atomic",
                "--inherit-pid-namespace",
                "--sandbox-policy-cwd",
                "/x",
                "--",
                "true",
            ][..],
        ] {
            assert!(is_helper(args(by_args).into_iter()), "{by_args:?}");
            assert!(linux_sandbox_args(args(by_args)), "{by_args:?}");
        }
        #[cfg(target_os = "linux")]
        for not_by_args in [
            &["/opt/atomic", "--inherit-pid-namespace", "run"][..],
            &["/opt/apply_patch", "--sandbox-policy-cwd", "/x"][..],
        ] {
            assert!(!linux_sandbox_args(args(not_by_args)), "{not_by_args:?}");
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
        let dir_scratch = Scratch::new("private");
        let dir = dir_scratch.0.clone();
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
        let state_scratch = Scratch::new("sweep");
        let state = state_scratch.0.clone();
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
            let (home_scratch, cwd_scratch) = (Scratch::new("home2"), Scratch::new("cwd2"));
            let (home, cwd) = (home_scratch.0.clone(), cwd_scratch.0.clone());
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

    /// A trial whose helper turned out to be atomic's own command line, which
    /// refuses the helper's arguments, is told apart from a sandbox that
    /// failed.
    #[test]
    fn a_trial_run_by_atomics_command_line_is_not_the_helper() {
        assert!(not_the_helper(
            "error: unexpected argument '--sandbox-policy-cwd' found\n\n\
             Usage: atomic-x86_64 [OPTIONS] [COMMAND]\n\nFor more information, try '--help'.\n"
        ));
        for by_name in [
            "bwrap: execvp codex-linux-sandbox: No such file or directory",
            "bwrap: execvp /dev/null/codex-linux-sandbox: Not a directory",
        ] {
            assert!(not_the_helper(by_name), "{by_name}");
        }
        for other in [
            "",
            "bwrap: Can't make directory /tmp/x: No space left on device",
            "error: unexpected argument '--other' found",
            "bwrap: execvp /usr/bin/true: No such file or directory",
            "bwrap: execvp /opt/codex-linux-sandbox-x: No such file or directory",
        ] {
            assert!(!not_the_helper(other), "{other}");
        }
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
                    release: crate::usage::Release::default(),
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
            release: crate::usage::Release::default(),
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
        let (home_scratch, cwd_scratch) = (
            Scratch::new(&format!("home-{tag}")),
            Scratch::new(&format!("cwd-{tag}")),
        );
        let (home, cwd) = (home_scratch.0.clone(), cwd_scratch.0.clone());
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
