// SPDX-License-Identifier: Apache-2.0

//! The settings `atomic run`, `runs resume`, `serve`, `gates` and `runs show`
//! use, resolved through atomic's configuration layers
//! ([`atomic_core::config`]), with what atomic detects around it filling
//! what nothing set, and where each came from, for `atomic config --explain`.
//!
//! Model access is ranked: a model endpoint set in a file, a variable or a
//! flag; then the person's ChatGPT sign-in (`atomic model login`, for Codex
//! atoms); then detection. Detection fills only what no file, variable or
//! flag set, and nothing when someone is signed in; it discovers, and it
//! never grants. It reads:
//!
//! - `OPENAI_API_KEY`: when it is set, OpenAI's API is the model endpoint, and
//!   `OPENAI_API_KEY` the variable its key is read from. Its value is never
//!   read here, shown or sent anywhere. `OPENAI_BASE_URL`, as OpenAI's own
//!   SDKs read it, names another endpoint for the key: read from the
//!   person's environment only, since it decides where the key is sent, and
//!   used only when it is `https`, or `http` to a loopback IP address, with
//!   no credential, query or fragment in it; otherwise nothing is detected,
//!   and a command that would send the key refuses to start.
//! - A local Ollama: when no key is set, one request, `GET /api/tags`, to
//!   Ollama's default address on the loopback interface, and nowhere else.
//!   When it answers, it is the endpoint, with the first model it lists.
//!
//! `ATOMIC_DATABASE_URL` selects PostgreSQL; `DATABASE_URL`, which usually
//! belongs to the application, is never read.

use crate::engine::{EngineArgs, RunnerKind};
use atomic_core::budget::{BudgetSettings, CallCaps, Caps, Ceilings};
use atomic_core::codex::ModelEndpoint;
use atomic_core::config::{self, ConfigError, Layers, Source};
use atomic_core::signin::SignIn;
use atomic_schema::yaml;
use atomic_app::model_policy::LanePolicy;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// OpenAI's API, when `OPENAI_API_KEY` is set.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Names another base URL for `OPENAI_API_KEY`'s endpoint, as OpenAI's SDKs
/// read it. Only a variable of the person's sets it: no file does.
pub const OPENAI_BASE_URL_VARIABLE: &str = "OPENAI_BASE_URL";
/// The model atomic uses on OpenAI's API when none is set.
pub const OPENAI_DEFAULT_MODEL: &str = "gpt-5-mini";
/// Where a local Ollama listens by default.
pub const OLLAMA_ADDRESS: &str = "127.0.0.1:11434";
/// Names another loopback address to probe for Ollama instead, for tests and
/// for an Ollama on another local port. A non-loopback address is refused.
pub const OLLAMA_ADDRESS_VARIABLE: &str = "ATOMIC_DETECT_OLLAMA_ADDR";

/// A string setting with no default. A number or a boolean, as a YAML
/// scalar such as `executor_id: 1` reads, is the string it is written as;
/// a variable's value is taken exactly as it was set (see [`resolve`]).
fn scalar<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(Value::Bool(b)) => Ok(Some(b.to_string())),
        Some(_) => Err(serde::de::Error::custom("expected a string")),
    }
}

/// The string settings with no default, by path: a variable naming one is
/// read as the string it holds, never as a number.
const STRINGS: &[&str] = &[
    "model.base_url",
    "model.name",
    "model.api_key_env",
    "database.url",
    "executor_id",
];

/// The model endpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelSettings {
    /// The OpenAI-compatible API's base URL.
    #[serde(deserialize_with = "scalar")]
    pub base_url: Option<String>,
    /// The model there (also `ATOMIC_MODEL`).
    #[serde(deserialize_with = "scalar")]
    pub name: Option<String>,
    /// The environment variable that holds the endpoint's API key. Never
    /// from the project's committed `atomic.yaml`: only from
    /// `atomic.local.yaml`, `ATOMIC_MODEL_API_KEY_ENV` or the flag, so a
    /// cloned project cannot have a person's secret sent where it chooses.
    #[serde(deserialize_with = "scalar")]
    pub api_key_env: Option<String>,
    /// Operator-owned endpoint and item routing policy for graph lanes.
    pub lanes: Option<LanePolicy>,
}

/// atomic's own store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseSettings {
    /// A PostgreSQL URL, or none for the project's SQLite store.
    #[serde(deserialize_with = "scalar")]
    pub url: Option<String>,
    /// The schema atomic's tables live in, on PostgreSQL.
    pub schema: String,
}

impl Default for DatabaseSettings {
    fn default() -> Self {
        Self {
            url: None,
            schema: "atomic".into(),
        }
    }
}

/// How exec atoms' commands run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecSettings {
    /// How many connections a command that declares hosts may hold through
    /// its proxy at once, from 1 to
    /// [`atomic_core::network::MAX_CONNECTIONS_PER_COMMAND`].
    pub connections_per_command: u32,
}

impl Default for ExecSettings {
    fn default() -> Self {
        Self {
            connections_per_command: atomic_core::network::DEFAULT_CONNECTIONS_PER_COMMAND,
        }
    }
}

/// Every setting, as `atomic.yaml` and its layers hold it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The manifest's own keys, which share `atomic.yaml`: carried through,
    /// read by the project loader, and not settings.
    pub atomic: Value,
    /// See [`Settings::atomic`].
    pub name: Value,
    /// See [`Settings::atomic`].
    pub collections: Value,
    /// See [`Settings::atomic`].
    pub ignore: Value,
    /// See [`Settings::atomic`].
    pub generated: Value,
    /// See [`Settings::atomic`].
    pub workspace: Value,
    /// The model endpoint.
    pub model: ModelSettings,
    /// atomic's store.
    pub database: DatabaseSettings,
    /// Where exec atoms' commands run: `local` or `process`.
    pub runner: String,
    /// This process's executor identity; required on PostgreSQL.
    #[serde(deserialize_with = "scalar")]
    pub executor_id: Option<String>,
    /// How many seconds a step waits for the execution backend to have
    /// room for its workspace; none, to wait as long as it takes. Read as
    /// written and checked once resolved, so a refusal can say where the
    /// value came from (see [`Settings::backend_wait`]).
    pub backend_wait: Value,
    /// Budget caps and prices (see [`atomic_core::budget`]).
    pub budget: BudgetSettings,
    /// How exec atoms' commands run.
    pub exec: ExecSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            atomic: Value::Null,
            name: Value::Null,
            collections: Value::Null,
            ignore: Value::Null,
            generated: Value::Null,
            workspace: Value::Null,
            model: ModelSettings::default(),
            database: DatabaseSettings::default(),
            runner: "local".into(),
            executor_id: None,
            backend_wait: Value::Null,
            budget: BudgetSettings::default(),
            exec: ExecSettings::default(),
        }
    }
}

impl Settings {
    /// How many seconds a step waits for the execution backend to have
    /// room for its workspace, at least one, or `None` to wait as long as
    /// it takes. [`resolve`] has refused any other value.
    pub fn backend_wait(&self) -> Option<u64> {
        self.backend_wait.as_u64()
    }
}

/// `per_command`, `exec.connections_per_command` as resolved from `source`,
/// if it is from 1 to [`atomic_core::network::MAX_CONNECTIONS_PER_COMMAND`];
/// otherwise refused, naming where it came from.
fn connections_per_command(per_command: u32, source: Option<&Source>) -> Result<u32, ConfigError> {
    let most = atomic_core::network::MAX_CONNECTIONS_PER_COMMAND;
    if (1..=most).contains(&per_command) {
        return Ok(per_command);
    }
    let from = source.map_or_else(|| "default".into(), describe);
    Err(ConfigError::Invalid(format!(
        "exec.connections_per_command, from {from}, is how many connections a command may hold \
         through its proxy at once: from 1 to {most}"
    )))
}

/// `backend_wait` as resolved, from `source`: a whole number of seconds of at
/// least one, as a number or a string of digits, or, only where nothing set
/// it, none. An empty variable sets nothing; anything else that is set,
/// null and blank text included, is refused, naming where it came from.
fn backend_wait(value: &Value, source: Option<&Source>) -> Result<Value, ConfigError> {
    let unset = matches!(source, None | Some(Source::Default));
    let seconds = match value {
        Value::Null if unset => return Ok(Value::Null),
        Value::Number(n) => n.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    };
    match seconds.filter(|s| *s >= 1) {
        Some(s) => Ok(Value::from(s)),
        None => {
            let from = source.map_or_else(|| "default".into(), describe);
            Err(ConfigError::Invalid(format!(
                "backend_wait, from {from}, is a whole number of seconds, at least 1; to \
                 override a limit set elsewhere, set a larger number"
            )))
        }
    }
}

/// The manifest's keys, which are not settings.
const MANIFEST: &[&str] = &[
    "atomic",
    "name",
    "collections",
    "ignore",
    "generated",
    "workspace",
];

/// The settings and where each came from.
#[derive(Debug)]
pub struct Resolved {
    /// The settings.
    pub settings: Settings,
    /// The source of every setting, by dotted path.
    pub sources: BTreeMap<String, Source>,
    /// Remarks for `--explain`, such as a variable that was ignored.
    pub notes: Vec<String>,
    /// The person's own configuration file, read or not.
    pub user: Option<PathBuf>,
    /// Why a key the person named is not sent, when the endpoint is one the
    /// project's files chose: a run that calls a model refuses to start.
    pub withheld_key: Option<String>,
    /// The person's ChatGPT sign-in, when no endpoint is set and someone is
    /// signed in: Codex atoms' model access, ahead of detection.
    pub sign_in: Option<SignIn>,
    /// Why the sign-in could not be read, when it was looked for and could
    /// not be: said, never taken for "signed out".
    pub sign_in_unreadable: Option<String>,
    /// Why the person's `OPENAI_BASE_URL` is not used for `OPENAI_API_KEY`,
    /// when it is not fit to send the key to: no endpoint is detected, and a
    /// run that calls a model refuses to start, while a command that sends
    /// no key, such as `atomic check`, carries on.
    pub refused_endpoint: Option<String>,
}

/// What looking for the person's sign-in found: the sign-in, if someone is
/// signed in, or why it could not be read.
pub type SignInFound = Result<Option<SignIn>, String>;

/// What resolution reads from outside: the environment, a local Ollama, and
/// the person's sign-in. Passed in, so tests choose what is there.
pub struct Surroundings<'a> {
    /// A variable's value, as text, so a value the operating system gives as
    /// bytes that are not text is no value here. That is why the person's
    /// configuration directory, which is a path and not text, does not come
    /// from `env` but from [`Surroundings::user_config_dir`].
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// The variables there are.
    pub env_names: Vec<String>,
    /// The models a local Ollama at an address lists, if one answers.
    pub ollama: &'a dyn Fn(SocketAddr) -> Option<Vec<String>>,
    /// The person's own configuration directory, where their `config.yaml`
    /// lives: [`atomic_core::state::user_config_dir`] in a person's run.
    ///
    /// It is the resolution the denial of that directory to atoms' commands and
    /// the refusal of a state directory inside it are made from — not
    /// `env`, which reads the same `XDG_CONFIG_HOME` and `HOME` as text and so
    /// sees nothing where the operating system gives bytes that are not text.
    /// Read the same way, all three name one directory or none; read two ways,
    /// settings would come from a directory no atom is denied while the denial
    /// covered another.
    pub user_config_dir: &'a dyn Fn() -> Option<PathBuf>,
    /// The person's sign-in, kept apart from the project rooted at the path
    /// when there is one.
    pub sign_in: &'a dyn Fn(Option<&Path>) -> SignInFound,
}

impl Surroundings<'static> {
    /// This process's environment, a real probe and the person's sign-in.
    pub fn here() -> Self {
        Self {
            ollama: &probe_ollama,
            sign_in: &signed_in,
            ..Self::without_probe()
        }
    }

    /// [`Surroundings::here`], but no sign-in is read: for what calls no
    /// model on the person's behalf, such as a replay or a run of exec atoms.
    pub fn here_without_sign_in() -> Self {
        Self {
            ollama: &probe_ollama,
            ..Self::without_probe()
        }
    }

    /// [`Surroundings::here`], but no Ollama answers and no sign-in is read:
    /// for `atomic check` and for finding the store, which read the settings
    /// and contact nothing.
    pub fn without_probe() -> Self {
        Self {
            env: &|name| std::env::var(name).ok(),
            env_names: std::env::vars_os()
                .filter_map(|(k, _)| k.into_string().ok())
                .collect(),
            ollama: &|_| None,
            user_config_dir: &user_config_dir_here,
            sign_in: &|_| Ok(None),
        }
    }
}

/// [`Surroundings`] reads the person's own configuration directory from this
/// process with [`atomic_core::state::user_config_dir`], which reads
/// `XDG_CONFIG_HOME` and `HOME` as the operating system gives them and falls
/// back to the home the password database gives: the same resolution the denial
/// of that directory to atoms' commands and the refusal of a state directory
/// inside it are made from.
///
/// A function, where every other field `without_probe` gives is a closure,
/// because a control's switch is written in a `|`-delimited row (see
/// `scripts/controls-check.sh`), which no closure can be written without.
/// `scripts/controls/settings-user-config-dir.row` switches what this asks of
/// `XDG_CONFIG_HOME` and `HOME` to what [`Surroundings::env`] already gives —
/// the same two variables read as text — which is the one reading that sees
/// nothing where the operating system gives bytes that are not text, and so the
/// one that looks for a person's settings in another directory than the one the
/// denial covers. The row runs
/// `a_setting_in_a_configuration_directory_of_bytes_that_are_not_text_is_sourced_from_it`,
/// which gives a command a configuration directory of just such bytes: that is
/// where the two readings part, and the switch keeps the home the password
/// database gives, since that fallback is what
/// `scripts/controls/state-config-passwd-home.row` and
/// `scripts/controls/state-check-passwd-wired.row` switch, in `state.rs`.
fn user_config_dir_here() -> Option<PathBuf> {
    atomic_core::state::user_config_dir()
}

/// The person's sign-in, if someone is signed in, found without creating
/// anything. Off Linux, where sign-ins are not kept, there is none.
pub fn signed_in(project: Option<&Path>) -> SignInFound {
    match SignIn::located(project) {
        Ok(sign_in) => found(sign_in),
        Err(atomic_core::signin::SignInError::Unprotected(_)) => Ok(None),
        // Refused where it is kept: with no directory there no one ever
        // signed in, and nothing is refused; with one, it may hold a
        // sign-in, and that it cannot be used is said.
        Err(e) => match atomic_core::signin::location().map(|d| std::fs::symlink_metadata(&d)) {
            Some(Err(gone)) if absent(&gone) => Ok(None),
            _ => Err(e.to_string()),
        },
    }
}

/// Whether anyone is signed in at `sign_in`. `atomic model login` creates
/// the sign-in's directory, so with none no one ever signed in here and no
/// store is read. With it, a store that cannot be read, a locked or
/// unreachable keyring among them, is said, never taken for "signed out":
/// that would move model calls to another account without a word.
fn found(sign_in: SignIn) -> SignInFound {
    match std::fs::metadata(sign_in.auth_home()) {
        Ok(meta) if meta.is_dir() => {}
        Err(gone) if absent(&gone) => return Ok(None),
        // Something else there, or a directory that cannot be looked at, as
        // under an ancestor this user cannot enter: not taken for absent.
        Ok(_) => {
            return Err(format!(
                "{} is not a directory",
                sign_in.auth_home().display()
            ));
        }
        Err(e) => return Err(format!("{}: {e}", sign_in.auth_home().display())),
    }
    match sign_in.account() {
        Ok(Some(_)) => Ok(Some(sign_in)),
        Ok(None) => Ok(None),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// The note for a sign-in that is there and cannot be used.
fn unusable(why: &str) -> String {
    format!(
        "the ChatGPT sign-in could not be read: {why}; a run that calls a model is refused \
         until it can be"
    )
}

/// Whether looking at a path failed because nothing is there: no entry, or
/// a component that is a file. Anything else, a permission refused among
/// them, is no answer, and never taken for absent.
fn absent(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// The root of the project configured in `dir`: `dir`, or its parent for an
/// embedded project's `.atomic/`.
fn project_root(dir: &Path) -> &Path {
    if dir.file_name().is_some_and(|n| n == ".atomic") {
        dir.parent().unwrap_or(dir)
    } else {
        dir
    }
}

/// The person's own configuration file, which no project supplies:
/// `$XDG_CONFIG_HOME/atomic/config.yaml`, by default
/// `~/.config/atomic/config.yaml`, and under the home the password database
/// gives when the environment names no absolute one. `config_dir` is where that
/// directory is — [`atomic_core::state::user_config_dir`] in a person's run, the
/// same resolution the denial of that directory to atoms' commands and the
/// refusal of a state directory inside it are made from, so the three cannot
/// name different directories. `None` when it names none.
pub fn user_config(config_dir: &dyn Fn() -> Option<PathBuf>) -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("config.yaml"))
}

/// The directory whose `atomic.yaml` and siblings configure the project at
/// `root`: the root, or its `.atomic/` for an embedded project.
pub fn config_dir(root: &Path) -> PathBuf {
    let embedded = root.join(".atomic");
    if !root.join("atomic.yaml").is_file() && embedded.join("atomic.yaml").is_file() {
        embedded
    } else {
        root.to_owned()
    }
}

/// The flags in `args`, as settings.
fn flags(args: &EngineArgs) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let mut put = |path: &str, v: &Option<String>| {
        if let Some(v) = v {
            out.push((path.to_owned(), Value::String(v.clone())));
        }
    };
    put("model.base_url", &args.model_base_url);
    put("model.name", &args.model);
    put("model.api_key_env", &args.model_api_key_env);
    put("executor_id", &args.executor_id);
    if let Some(secs) = args.backend_wait {
        out.push(("backend_wait".into(), Value::from(secs)));
    }
    if let Some(runner) = args.runner {
        out.push((
            "runner".into(),
            Value::String(
                match runner {
                    RunnerKind::Local => "local",
                    RunnerKind::Process => "process",
                }
                .into(),
            ),
        ));
    }
    out
}

/// The flag each setting is given by, for `--explain`.
fn flag_name(path: &str) -> String {
    match path {
        "model.base_url" => "--model-base-url".into(),
        "model.name" => "--model".into(),
        "model.api_key_env" => "--model-api-key-env".into(),
        other => format!("--{}", other.replace(['.', '_'], "-")),
    }
}

/// The project profile selected by the command or the person's environment.
/// Keep this separate from ordinary settings: the name chooses a file, and
/// cannot itself come from a project file or select another directory. In
/// particular `local` would read `atomic.local.yaml` a second time.
fn selected_profile(
    args: &EngineArgs,
    around: &Surroundings<'_>,
) -> Result<Option<(String, String)>, ConfigError> {
    let (raw, source) = match args.profile.as_deref() {
        Some(raw) => (raw, "--profile"),
        None => match (around.env)("ATOMIC_PROFILE") {
            Some(raw) => return checked_profile(&raw, "ATOMIC_PROFILE"),
            None if around.env_names.iter().any(|name| name == "ATOMIC_PROFILE") => {
                return Err(ConfigError::Value {
                    name: "ATOMIC_PROFILE".into(),
                    message: "must be UTF-8 text; clear it or pass --profile".into(),
                });
            }
            None => return Ok(None),
        },
    };
    checked_profile(raw, source)
}

fn checked_profile(raw: &str, source: &str) -> Result<Option<(String, String)>, ConfigError> {
    if raw.is_empty() {
        return Ok(None);
    }
    let valid = config::valid_profile_name(raw);
    if !valid {
        return Err(ConfigError::Value {
            name: source.into(),
            message: "use a lowercase ASCII name beginning with a letter, with single '-' or \
                      '_' separators; 'local' is reserved for atomic.local.yaml"
                .into(),
        });
    }
    Ok(Some((raw.into(), source.into())))
}

/// Resolve the settings of the project configured in `dir`, with `args`'
/// flags on top, in `around`.
pub fn resolve(
    dir: &Path,
    args: &EngineArgs,
    around: &Surroundings<'_>,
) -> Result<Resolved, ConfigError> {
    let profile = selected_profile(args, around)?;
    let user = user_config(around.user_config_dir);
    // A project could supply a "personal" file inside itself, or inside the
    // checkout that holds it, as when the home directory is where the
    // repository is mounted. Only for a project: outside one, such as
    // `atomic config` run from the home directory, nothing supplies it.
    if dir.join("atomic.yaml").is_file()
        && let Some(file) = &user
        && let Ok(file) = std::fs::canonicalize(file)
        && let Ok(root) = std::fs::canonicalize(project_root(dir))
    {
        // Every checkout around the project counts, not only the nearest:
        // a submodule's superproject, the checkout a worktree sits in, and
        // a linked worktree's main checkout, whose repository it shares.
        let checkouts =
            atomic_core::state::checkouts_holding(&root).map_err(ConfigError::Invalid)?;
        for (what, enclosing) in std::iter::once(("project", root.as_path())).chain(
            checkouts
                .iter()
                .map(|c| ("checkout holding the project", c.as_path())),
        ) {
            if file.starts_with(enclosing) {
                return Err(ConfigError::Invalid(format!(
                    "your own configuration, {}, is inside the {what} at {}, so the project \
                     could have supplied it; point XDG_CONFIG_HOME, or HOME, outside it",
                    file.display(),
                    enclosing.display()
                )));
            }
        }
    }
    let selected_file = profile
        .as_ref()
        .map(|(name, _)| dir.join(format!("atomic.{name}.yaml")));
    let resolved = config::resolve::<Settings>(&Layers {
        dir,
        user: user.as_deref(),
        profile: profile.as_ref().map(|(name, _)| name.as_str()),
        env: around.env,
        env_names: around.env_names.clone(),
        flags: flags(args),
    })
    .map_err(|error| match (error, &profile) {
        (ConfigError::Read { path, source }, Some((_, from)))
            if selected_file.as_deref() == Some(path.as_path())
                && source.kind() == std::io::ErrorKind::NotFound =>
        {
            ConfigError::Invalid(format!(
                "selected profile from {} needs {}; create that file beside atomic.yaml or \
                 clear the profile selection",
                from,
                path.display()
            ))
        }
        (other, _) => other,
    })?;
    let (mut settings, mut sources) = (resolved.config, resolved.sources);
    for (path, source) in sources.iter_mut() {
        if let Source::Flag(_) = source {
            *source = Source::Flag(flag_name(path));
        }
    }
    // A variable naming a string setting holds that string, whatever it
    // looks like: `ATOMIC_EXECUTOR_ID=1` is the identity `1`.
    for path in STRINGS {
        if let Some(Source::Env(name)) = sources.get(*path)
            && let Some(raw) = (around.env)(name)
        {
            let slot = match *path {
                "model.base_url" => &mut settings.model.base_url,
                "model.name" => &mut settings.model.name,
                "model.api_key_env" => &mut settings.model.api_key_env,
                "database.url" => &mut settings.database.url,
                _ => &mut settings.executor_id,
            };
            *slot = Some(raw);
        }
    }
    let withheld_key = refuse_from_project(&settings, &sources, user.as_deref())?;
    settings.backend_wait = backend_wait(&settings.backend_wait, sources.get("backend_wait"))?;
    settings.exec.connections_per_command = connections_per_command(
        settings.exec.connections_per_command,
        sources.get("exec.connections_per_command"),
    )?;
    let mut notes = Vec::new();
    if let Some((name, source)) = &profile {
        notes.push(format!("profile {name} selected by {source}"));
    }
    if let Some(why) = &withheld_key {
        settings.model.api_key_env = None;
        sources.insert(
            "model.api_key_env".into(),
            Source::Detected("not sent to an endpoint the project chose".into()),
        );
        notes.push(why.clone());
    }
    // The person's own caps bind every layer above their file, and their
    // prices hold over a project's: a project can only tighten what they
    // allowed. What bound is said, so a number that is not the highest
    // layer's is never shown without its reason.
    notes.extend(person_budget(&mut settings, user.as_deref(), &mut sources)?);
    let set = |v: Option<String>| v.filter(|v| !v.is_empty());
    let unset = |sources: &BTreeMap<String, Source>, path: &str| {
        matches!(sources.get(path), None | Some(Source::Default))
    };
    // ATOMIC_MODEL names the model, as model.name's variable would.
    if let Some(model) = set((around.env)("ATOMIC_MODEL"))
        && matches!(
            sources.get("model.name"),
            None | Some(Source::Default | Source::File { .. })
        )
    {
        settings.model.name = Some(model);
        sources.insert("model.name".into(), Source::Env("ATOMIC_MODEL".into()));
    }
    if set((around.env)("DATABASE_URL")).is_some() {
        notes.push(
            "DATABASE_URL is set and ignored: it belongs to the application; atomic's own \
             store is named by ATOMIC_DATABASE_URL"
                .into(),
        );
    }

    // An empty base URL is no endpoint, and nor is a null one, as a file's
    // `base_url:` with nothing after it.
    if settings
        .model
        .base_url
        .as_deref()
        .is_none_or(|u| u.trim().is_empty())
        && !unset(&sources, "model.base_url")
    {
        settings.model.base_url = None;
        sources.insert("model.base_url".into(), Source::Default);
    }
    // With no endpoint set, the person's sign-in, kept apart from the
    // project when there is one, comes before detection.
    let (mut sign_in, mut sign_in_unreadable) = (None, None);
    if unset(&sources, "model.base_url") {
        let project = dir.join("atomic.yaml").is_file().then(|| project_root(dir));
        match (around.sign_in)(project) {
            Ok(found) => sign_in = found,
            Err(why) => {
                notes.push(unusable(&why));
                sign_in_unreadable = Some(why);
            }
        }
    }
    if sign_in.is_some() && set((around.env)("OPENAI_API_KEY")).is_some() {
        notes.push(
            "OPENAI_API_KEY is set and not used: the ChatGPT sign-in comes before detection; \
             set model.base_url to use OpenAI's API"
                .into(),
        );
    }
    if sign_in.is_some() && settings.model.api_key_env.is_some() {
        notes.push(
            "model.api_key_env is set and not used: the ChatGPT sign-in sends no key; set \
             model.base_url to the endpoint the key is for"
                .into(),
        );
    }

    // Detection, for what nothing set, when no one is signed in.
    let other_openai = set((around.env)(OPENAI_BASE_URL_VARIABLE));
    let mut other_openai_used = false;
    let mut refused_endpoint = None;
    if unset(&sources, "model.base_url") && sign_in.is_none() {
        if set((around.env)("OPENAI_API_KEY")).is_some()
            && let Err(why) = other_openai
                .as_deref()
                .map_or(Ok(()), |u| openai_base_url(u).map(drop))
        {
            // Not fit for the key: nothing is detected, and only a command
            // that would send it refuses to start.
            other_openai_used = true;
            notes.push(why.clone());
            refused_endpoint = Some(why);
        } else if set((around.env)("OPENAI_API_KEY")).is_some() {
            // The key goes where the person's own OPENAI_BASE_URL says, as
            // OpenAI's SDKs send it, once that is seen to be fit for a key.
            let (base_url, why) = match &other_openai {
                Some(url) => (
                    url.clone(),
                    format!("OPENAI_API_KEY and {OPENAI_BASE_URL_VARIABLE} are set"),
                ),
                None => (
                    OPENAI_BASE_URL.to_owned(),
                    "OPENAI_API_KEY is set".to_owned(),
                ),
            };
            other_openai_used = other_openai.is_some();
            settings.model.base_url = Some(base_url);
            sources.insert("model.base_url".into(), Source::Detected(why.clone()));
            if unset(&sources, "model.api_key_env") {
                settings.model.api_key_env = Some("OPENAI_API_KEY".into());
                sources.insert("model.api_key_env".into(), Source::Detected(why.clone()));
            }
            if unset(&sources, "model.name") {
                settings.model.name = Some(OPENAI_DEFAULT_MODEL.into());
                sources.insert(
                    "model.name".into(),
                    Source::Detected(if other_openai_used {
                        format!("{why}: atomic's default model for an OpenAI API key")
                    } else {
                        format!("{why}: atomic's default model on OpenAI's API")
                    }),
                );
            }
        } else {
            let address =
                set((around.env)(OLLAMA_ADDRESS_VARIABLE)).unwrap_or_else(|| OLLAMA_ADDRESS.into());
            match address.parse::<SocketAddr>() {
                Ok(addr) if addr.ip().is_loopback() => {
                    if let Some(models) = (around.ollama)(addr) {
                        let why = format!("Ollama answered at {addr}");
                        settings.model.base_url = Some(format!("http://{addr}/v1"));
                        sources.insert("model.base_url".into(), Source::Detected(why.clone()));
                        if unset(&sources, "model.name")
                            && let Some(first) = models.first()
                        {
                            settings.model.name = Some(first.clone());
                            sources.insert(
                                "model.name".into(),
                                Source::Detected(format!("{why}: the first model it lists")),
                            );
                        }
                        // A key is for an endpoint someone chose, never
                        // for one atomic found.
                        if settings.model.api_key_env.take().is_some() {
                            sources.insert(
                                "model.api_key_env".into(),
                                Source::Detected(format!("{why}: no key is sent to it")),
                            );
                            notes.push(
                                "model.api_key_env is set and not used: atomic sends no key to \
                                 an Ollama it detected; set model.base_url to the endpoint the \
                                 key is for"
                                    .into(),
                            );
                        }
                    }
                }
                Ok(addr) => notes.push(format!(
                    "{OLLAMA_ADDRESS_VARIABLE}={addr} is not a loopback address, so no Ollama \
                     was looked for: detection probes this machine only"
                )),
                Err(_) => notes.push(format!(
                    "{OLLAMA_ADDRESS_VARIABLE}={address} is not an IP address and port, such as \
                     {OLLAMA_ADDRESS}, so no Ollama was looked for"
                )),
            }
        }
    }
    if other_openai.is_some() && !other_openai_used {
        notes.push(format!(
            "{OPENAI_BASE_URL_VARIABLE} is set and not used: atomic reads it only to send \
             OPENAI_API_KEY there, when the key is set and no model endpoint is set or signed in"
        ));
    }
    Ok(Resolved {
        settings,
        sources,
        notes,
        user,
        withheld_key,
        sign_in,
        sign_in_unreadable,
        refused_endpoint,
    })
}

/// How long the whole probe for a local Ollama may take, from connecting to
/// the last byte of its answer.
const PROBE_DEADLINE: Duration = Duration::from_secs(1);

/// Where `path`'s value came from, when that is a file of the project
/// rather than the person's own configuration: `atomic.yaml` or an
/// `atomic.local.yaml`, tracked by git or not, since either can arrive with
/// a project.
fn project_file<'s>(
    sources: &'s BTreeMap<String, Source>,
    path: &str,
    user: Option<&Path>,
) -> Option<(&'s Path, usize)> {
    match sources.get(path)? {
        Source::File { path, line } if Some(path.as_path()) != user => {
            Some((path.as_path(), *line))
        }
        _ => None,
    }
}

/// The model endpoint `user`, the person's own configuration, sets.
fn users_base_url(user: Option<&Path>) -> Option<String> {
    let user = user?;
    let text = std::fs::read_to_string(user).ok()?;
    let doc = atomic_schema::yaml::load(user, &text).ok()?;
    doc.value
        .pointer("/model/base_url")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Whether `url`, the person's `OPENAI_BASE_URL`, is fit to send
/// `OPENAI_API_KEY` to: a URL, `https`, or `http` only to a loopback IP
/// address written as one, such as a local proxy's, and with no user,
/// password, query or fragment, which a log or a proxy could keep. Why not,
/// when it is not.
fn openai_base_url(url: &str) -> Result<(), String> {
    let refuse = |why: &str| {
        format!(
            "{OPENAI_BASE_URL_VARIABLE} {why}, and it decides where atomic sends \
             OPENAI_API_KEY, so the key is sent nowhere: give an https URL, such as \
             https://host/v1, or http to 127.0.0.1 or [::1], or unset it"
        )
    };
    let parsed = url::Url::parse(url).map_err(|_| refuse("does not read as a URL"))?;
    if has_credential(&parsed) {
        return Err(refuse("carries a user, password, query or fragment"));
    }
    if parsed.fragment().is_some() {
        return Err(refuse("carries a user, password, query or fragment"));
    }
    let loopback = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        "http" => Err(refuse(
            "is http to an address that is not this machine's loopback",
        )),
        _ => Err(refuse("is neither https nor http")),
    }
}

/// Whether `url` carries a credential: a user name or password, which may
/// be a token, or a query, which may hold a key.
fn has_credential(url: &url::Url) -> bool {
    !url.username().is_empty() || url.password().is_some() || url.query().is_some()
}

/// What the project's files may not set, because it would send something of
/// the person who runs the project where the project chooses: the variable
/// whose value is sent as the model key, a key at all to an endpoint the
/// project chose, the store for the run records, or a credential in the
/// endpoint. Those come only from the person's own configuration
/// ([`user_config`]), `ATOMIC_*` variables and flags. A key the person
/// named for an endpoint the project chose is not refused but withheld:
/// the reason is returned, and nothing is sent.
fn refuse_from_project(
    settings: &Settings,
    sources: &BTreeMap<String, Source>,
    user: Option<&Path>,
) -> Result<Option<String>, ConfigError> {
    let mut withheld = None;
    let yours = format!(
        "your own configuration ({})",
        user.map_or_else(
            || "~/.config/atomic/config.yaml".to_owned(),
            |u| u.display().to_string()
        )
    );
    let refuse = |(path, line): (&Path, usize), message: String| ConfigError::Setting {
        path: path.to_owned(),
        line,
        message,
    };
    if let Some(at) = project_file(sources, "model.lanes", user) {
        return Err(refuse(
            at,
            "model.lanes chooses model endpoints and credentials for graph lanes; set it in your own configuration, not a project file".into(),
        ));
    }
    if settings.model.lanes.is_some()
        && !matches!(
            sources.get("model.lanes"),
            Some(Source::File { path, .. }) if Some(path.as_path()) == user
        )
    {
        return Err(ConfigError::Invalid(
            "model.lanes must be in the operator's configuration file so later graph lanes can reload its routes".into(),
        ));
    }
    if let Some(policy) = &settings.model.lanes {
        policy.validate().map_err(ConfigError::Invalid)?;
    }
    if let Some(at) = project_file(sources, "model.api_key_env", user) {
        return Err(refuse(
            at,
            format!(
                "model.api_key_env cannot be set by the project's files: it names the variable \
                 whose value atomic sends to the model endpoint as its key, and that is for \
                 whoever runs the project to choose. Set it in {yours}, \
                 ATOMIC_MODEL_API_KEY_ENV or --model-api-key-env"
            ),
        ));
    }
    if let Some(name) = &settings.model.api_key_env
        && !is_variable_name(name)
    {
        let from = sources
            .get("model.api_key_env")
            .map_or_else(|| "default".into(), describe);
        return Err(ConfigError::Invalid(format!(
            "model.api_key_env, from {from}, is not an environment variable's name; it names \
             the variable that holds the key, never the key itself"
        )));
    }
    if let Some(at) = project_file(sources, "model.base_url", user) {
        let url = settings.model.base_url.as_deref().unwrap_or_default();
        match url::Url::parse(url) {
            Err(_) => {
                return Err(refuse(
                    at,
                    "model.base_url does not read as a URL, such as https://host/v1".into(),
                ));
            }
            Ok(parsed) if has_credential(&parsed) => {
                return Err(refuse(
                    at,
                    format!(
                        "model.base_url carries a credential, a user, password or query, and \
                         the project's files are shared: set it in {yours}, \
                         ATOMIC_MODEL_BASE_URL or --model-base-url"
                    ),
                ));
            }
            Ok(_) => {}
        }
        let same = |other: &str| other.trim_end_matches('/') == url.trim_end_matches('/');
        if settings.model.api_key_env.is_some()
            && let Some(key) = sources.get("model.api_key_env")
            && !users_base_url(user).is_some_and(|u| same(&u))
        {
            withheld = Some(format!(
                "model.api_key_env, from {}, is not used: model.base_url is set by the project \
                 ({}:{}), and atomic sends a key only to an endpoint the person running the \
                 project chose. Set the same model.base_url in {yours}, ATOMIC_MODEL_BASE_URL \
                 or --model-base-url to send it",
                describe(key),
                at.0.display(),
                at.1
            ));
        }
    }
    if let Some(at) = project_file(sources, "database.url", user) {
        return Err(refuse(
            at,
            format!(
                "database.url cannot be set by the project's files: it would move this \
                 machine's run records, and the password it may carry, to whatever store the \
                 project names. Set it in {yours} or ATOMIC_DATABASE_URL"
            ),
        ));
    }
    if !matches!(settings.runner.as_str(), "local" | "process") {
        let from = sources
            .get("runner")
            .map_or_else(|| "default".into(), describe);
        return Err(ConfigError::Invalid(format!(
            "runner, from {from}, must be local or process"
        )));
    }
    Ok(withheld)
}

/// The `budget` groups the person's own configuration can cap, and the only
/// ones it can.
const GROUPS: [&str; 3] = ["atom", "workflow", "call"];

/// Where in a parsed document each dotted setting path sits, so a setting
/// taken from it can be named with the line it is on. A mapping is recorded
/// too, so a path the loader has no line for falls back to the section above
/// it instead of to no line at all.
fn lines_of(doc: &yaml::Document) -> BTreeMap<String, usize> {
    fn walk(
        doc: &yaml::Document,
        value: &Value,
        pointer: &str,
        dotted: &str,
        out: &mut BTreeMap<String, usize>,
    ) {
        out.insert(
            dotted.to_owned(),
            doc.position(pointer).map_or(1, |p| p.line),
        );
        if let Value::Object(map) = value {
            for (key, child) in map {
                // The loader records positions by JSON Pointer, whose tokens
                // escape `~` and `/`.
                let token = key.replace('~', "~0").replace('/', "~1");
                let child_dotted = if dotted.is_empty() {
                    key.clone()
                } else {
                    format!("{dotted}.{key}")
                };
                walk(
                    doc,
                    child,
                    &format!("{pointer}/{token}"),
                    &child_dotted,
                    out,
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(doc, &doc.value, "", "", &mut out);
    out
}

/// The line `dotted` is on, or, when nothing records one for it, the line of
/// the deepest part of it that has one.
fn line_of(at: &BTreeMap<String, usize>, dotted: &str) -> usize {
    let mut path = dotted;
    loop {
        if let Some(line) = at.get(path) {
            return *line;
        }
        match path.rfind('.') {
            Some(below) => path = &path[..below],
            None => return 1,
        }
    }
}

/// The value at `dotted` in `v`, or `None` when nothing is there.
fn at<'v>(v: &'v Value, dotted: &str) -> Option<&'v Value> {
    dotted.split('.').try_fold(v, |v, key| v.get(key))
}

/// The value at `dotted` in `v`, as a slot to write through, or `None` when
/// nothing is there.
fn slot<'v>(v: &'v mut Value, dotted: &str) -> Option<&'v mut Value> {
    let mut cur = v;
    for key in dotted.split('.') {
        cur = cur.as_object_mut()?.get_mut(key)?;
    }
    Some(cur)
}

/// The value at `dotted` in `v`, as a slot to write through, made where
/// it is missing: each step on the way that is not an object, as a layer's
/// `null` leaves one, becomes an empty object.
fn slot_made<'v>(v: &'v mut Value, dotted: &str) -> &'v mut Value {
    let mut cur = v;
    for key in dotted.split('.') {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        cur = cur
            .as_object_mut()
            .expect("an object, made so above")
            .entry(key.to_owned())
            .or_insert(Value::Null);
    }
    cur
}

/// The person's own configuration's `budget` section, held over the merged
/// settings, and the remarks naming what it bound. With it, their
/// [`Ceilings`], which travel with the settings so that a run holds an atom's
/// or workflow's own `budget` block to them too (see
/// [`atomic_core::budget::resolve`]).
///
/// [`config::resolve`] read this file already, and refused every key of it
/// that is not a setting; the section is read again here as a whole for one
/// reason: a ceiling needs to know what each layer set, not only what the
/// last of them set.
fn person_budget(
    settings: &mut Settings,
    user: Option<&Path>,
    sources: &mut BTreeMap<String, Source>,
) -> Result<Vec<String>, ConfigError> {
    let mut budget = serde_json::to_value(&settings.budget)
        .map_err(|e| ConfigError::Invalid(format!("the budget settings cannot be held: {e}")))?;
    let mut ceilings = Ceilings::default();
    let mut priced_by_project = std::collections::BTreeSet::new();
    let mut notes = Vec::new();
    if let Some(user) = user {
        match std::fs::read_to_string(user) {
            Ok(text) => {
                let doc = yaml::load(user, &text)?;
                // A `budget` that is not a mapping was refused with its file
                // and line while the layers merged, so nothing caps from it.
                if let Some(Value::Object(written)) = doc.value.pointer("/budget") {
                    let lines = lines_of(&doc);
                    own_budget_read(user, written, &lines)?;
                    let (held, said) = hold_budget(user, written, &lines, &mut budget, sources);
                    nothing_free_under(user, written, &budget, &held)?;
                    priced_by_project = project_priced(written, &budget, &held);
                    ceilings = held;
                    notes = said;
                    if !priced_by_project.is_empty() {
                        notes.push(format!(
                            "budget.pricing: your own dollar cap ({}) counts calls to {} at \
                             prices only a layer above yours sets (the project's files or \
                             ATOMIC_BUDGET_PRICING); price them in your own configuration to \
                             hold them to yours",
                            user.display(),
                            priced_by_project
                                .iter()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                }
            }
            // No file of their own: nothing caps, as [`config::resolve`] read
            // nothing from it either.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ConfigError::Read {
                    path: user.to_owned(),
                    source,
                });
            }
        }
    }
    settings.budget = serde_json::from_value(budget).map_err(|e| {
        ConfigError::Invalid(format!(
            "the budget settings cannot be read back: {}",
            config::without_values(&e.to_string())
        ))
    })?;
    settings.budget.ceilings = ceilings;
    settings.budget.priced_by_project = priced_by_project;
    Ok(notes)
}

/// Refuse what the person's own `budget` section writes that its setting
/// cannot take, by its file and line: a cap that is neither a number nor
/// `null` (a count that is not a whole number), and a price that is not a
/// price. It is checked on its own, as a value a project's layer replaces is
/// never read otherwise, and a cap of theirs that could not be read would
/// otherwise be dropped without a word.
fn own_budget_read(
    user: &Path,
    written: &Map<String, Value>,
    at_line: &BTreeMap<String, usize>,
) -> Result<(), ConfigError> {
    let refuse = |dotted: &str, what: &str| {
        Err(ConfigError::Setting {
            path: user.to_owned(),
            line: line_of(at_line, dotted),
            message: format!("{dotted} {what}"),
        })
    };
    let whole = |v: &Value| v.is_null() || v.as_u64().is_some();
    let number = |v: &Value| v.is_null() || v.as_f64().is_some();
    for group in GROUPS {
        let Some(section) = written.get(group) else {
            continue;
        };
        let Some(fields) = section.as_object() else {
            return refuse(&format!("budget.{group}"), "is a section, not a value");
        };
        for (field, value) in fields {
            let dotted = format!("budget.{group}.{field}");
            if ["tokens", "input_tokens", "output_tokens"].contains(&field.as_str()) {
                if !whole(value) {
                    return refuse(&dotted, "is not a whole number");
                }
            } else if !number(value) {
                return refuse(&dotted, "is not a number");
            }
        }
    }
    let Some(pricing) = written.get("pricing") else {
        return Ok(());
    };
    let Some(models) = pricing.as_object() else {
        return refuse("budget.pricing", "is a section, not a value");
    };
    for (model, price) in models {
        let dotted = format!("budget.pricing.{model}");
        let Some(fields) = price.as_object() else {
            return refuse(
                &dotted,
                "is not a price: write its input_usd_per_mtok and output_usd_per_mtok",
            );
        };
        for field in ["input_usd_per_mtok", "output_usd_per_mtok"] {
            match fields.get(field) {
                Some(v) if v.is_number() => {}
                Some(_) => return refuse(&format!("{dotted}.{field}"), "is not a number"),
                None => {
                    return refuse(
                        &format!("{dotted}.{field}"),
                        "is missing: a price needs both input_usd_per_mtok and \
                         output_usd_per_mtok",
                    );
                }
            }
        }
        for field in ["max_input_tokens", "max_output_tokens"] {
            if fields.get(field).is_some_and(|v| !whole(v)) {
                return refuse(&format!("{dotted}.{field}"), "is not a whole number");
            }
        }
    }
    Ok(())
}

/// Under any dollar cap of the person's in `ceilings`, the models of the
/// merged `budget`'s price table that their own `written` section does not
/// price: a project prices them alone.
fn project_priced(
    written: &Map<String, Value>,
    budget: &Value,
    ceilings: &Ceilings,
) -> std::collections::BTreeSet<String> {
    if ceilings.atom.cost_usd.is_none() && ceilings.workflow.cost_usd.is_none() {
        return Default::default();
    }
    let theirs = written.get("pricing").and_then(Value::as_object);
    at(budget, "pricing")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|table| table.keys())
        .filter(|model| !theirs.is_some_and(|t| t.contains_key(*model)))
        .cloned()
        .collect()
}

/// Refuse a project's zero price for a model under the person's own dollar
/// cap: with any dollar cap in `ceilings`, a model in the merged `budget`
/// that the person's `written` section does not price, priced at nothing
/// for both input and output, would make every call to it free and the cap
/// meaningless. A nonzero price for such a model is the project's to set.
fn nothing_free_under(
    user: &Path,
    written: &Map<String, Value>,
    budget: &Value,
    ceilings: &Ceilings,
) -> Result<(), ConfigError> {
    if ceilings.atom.cost_usd.is_none() && ceilings.workflow.cost_usd.is_none() {
        return Ok(());
    }
    let theirs = |model: &str| {
        written
            .get("pricing")
            .and_then(Value::as_object)
            .is_some_and(|pricing| pricing.contains_key(model))
    };
    let Some(table) = at(budget, "pricing").and_then(Value::as_object) else {
        return Ok(());
    };
    for (model, price) in table {
        let zero = |field: &str| price.get(field).and_then(Value::as_f64) == Some(0.0);
        if !theirs(model) && zero("input_usd_per_mtok") && zero("output_usd_per_mtok") {
            return Err(ConfigError::Invalid(format!(
                "budget.pricing.{model}: a price of nothing for {model} would leave your own \
                 dollar cap ({}) nothing to count; price {model} in your own configuration",
                user.display()
            )));
        }
    }
    Ok(())
}

/// The `written` `budget` section of the person's own configuration file
/// `user`, held over the merged `budget` section, which is changed in place:
///
/// - every cap they wrote caps the merged one, so the effective limit is the
///   smaller of the two and a higher layer can only bring a cap down to
///   theirs, never above it, nor remove it with `null`. A measure they leave
///   unset caps nothing;
/// - every price they wrote holds for the model it names, so a project's
///   `budget.pricing` cannot make a model they priced look cheaper than they
///   priced it, and cannot give any model a per-call maximum above their own
///   `budget.call` cap;
/// - what binds is recorded in `sources` as theirs, at the line `at_line`
///   gives, so `atomic config --explain` names the file the effective number
///   came from, and is said in a remark, so a number that is not the highest
///   layer's is never shown without its reason.
///
/// Nothing here fails: `config::resolve` refused every key that is not a
/// setting, and [`own_budget_read`] every value of theirs a setting cannot
/// take, each with its file and line.
fn hold_budget(
    user: &Path,
    written: &Map<String, Value>,
    at_line: &BTreeMap<String, usize>,
    budget: &mut Value,
    sources: &mut BTreeMap<String, Source>,
) -> (Ceilings, Vec<String>) {
    let yours = user.display().to_string();
    let theirs = |dotted: &str| Source::File {
        path: user.to_owned(),
        line: line_of(at_line, dotted),
    };
    // `budget` is the merged `budget` section on its own, so a lookup drops
    // the section's own name from the setting path.
    fn inside(dotted: &str) -> &str {
        dotted.strip_prefix("budget.").unwrap_or(dotted)
    }
    let mut notes = Vec::new();
    // 1. Their caps, held over the merged ones, and gathered as the ceilings
    //    that travel with the settings to hold a run's own budget block.
    //
    // Every key of a group is one of its settings (`config::resolve` refused
    // any other), and `own_budget_read` checked this section on its own
    // before this ran: what is here is a number for the limit it names, a
    // whole one for a count, or `null`, which is how every layer says it sets
    // no cap. What is in range from there, and what is too large to account
    // for, `BudgetSettings::validate` says by its setting.
    let mut ceilings = Ceilings::default();
    for group in GROUPS {
        let Some(wrote) = written.get(group).and_then(Value::as_object) else {
            continue;
        };
        let mut bound = Vec::new();
        for (field, cap) in wrote {
            let dotted = format!("budget.{group}.{field}");
            let Some(want) = cap.as_f64() else {
                continue;
            };
            // Their cap gives way only to a smaller one, which leaves a
            // project's smaller cap tightening what they allow. A layer
            // above that writes a larger cap, or none at all (`null`, or a
            // whole group set to `null`), leaves their number standing.
            let have = at(budget, inside(&dotted)).and_then(Value::as_f64);
            if let Some(have) = have
                && want >= have
            {
                continue;
            }
            bound.push(format!("{dotted} at {}", show(cap)));
            *slot_made(budget, inside(&dotted)) = cap.clone();
            sources.insert(dotted.clone(), theirs(&dotted));
        }
        if !bound.is_empty() {
            notes.push(format!(
                "budget.{group}: your own configuration ({yours}) caps {}, which no project or \
                 variable can raise",
                bound.join(", ")
            ));
        }
        let count = |what: &str| wrote.get(what).and_then(Value::as_u64);
        let number = |what: &str| wrote.get(what).and_then(Value::as_f64);
        match group {
            "atom" | "workflow" => {
                let caps = Caps {
                    tokens: count("tokens"),
                    minutes: number("minutes"),
                    cost_usd: number("cost_usd"),
                };
                if group == "atom" {
                    ceilings.atom = caps;
                } else {
                    ceilings.workflow = caps;
                }
            }
            _ => {
                ceilings.call = CallCaps {
                    input_tokens: count("input_tokens"),
                    output_tokens: count("output_tokens"),
                    minutes: number("minutes"),
                };
            }
        }
    }
    // 2. Their prices, over a project's, and their call cap over a project's
    //    per-call maxima. A price of the wrong shape was refused with its file
    //    and line while the layers merged, as any other setting is.
    let mut their_prices: BTreeMap<&str, &Map<String, Value>> = BTreeMap::new();
    if let Some(Value::Object(pricing)) = written.get("pricing") {
        for (model, price) in pricing {
            if let Value::Object(price) = price {
                their_prices.insert(model, price);
            }
        }
    }
    let mut bound = Vec::new();
    let mut clamped = Vec::new();
    if let Some(Value::Object(table)) = slot(budget, inside("budget.pricing")) {
        // A model they priced and a project named too: their fields, and the
        // project's only where they wrote none. A model they never named is
        // held to their call cap below, so it needs no fields of its own.
        let none = Map::new();
        for (model, entry) in table.iter_mut() {
            let Value::Object(entry) = entry else {
                continue;
            };
            let their = their_prices.get(model.as_str()).copied().unwrap_or(&none);
            let mut held = Vec::new();
            let mut capped = Vec::new();
            for field in [
                "input_usd_per_mtok",
                "output_usd_per_mtok",
                "max_input_tokens",
                "max_output_tokens",
            ] {
                let dotted = format!("budget.pricing.{model}.{field}");
                // A field they wrote is theirs, at their line. A maximum held
                // down is cited from the cap that holds it, so the number is
                // never shown against a file that says a larger one.
                let mut held_by: Option<&str> = None;
                let keep = match their.get(field) {
                    // Their price for a model holds: a project cannot make a
                    // model they priced look cheaper, nor give a call to it a
                    // larger bound than they gave it.
                    Some(written) => {
                        if same(Some(written), entry.get(field)) {
                            None
                        } else {
                            Some(written.clone())
                        }
                    }
                    // A maximum a project gave a model cannot pass their cap
                    // on the most one call may use, whether or not they priced
                    // the model at all.
                    None => {
                        let (cap, cap_of) = match field {
                            "max_input_tokens" => {
                                (ceilings.call.input_tokens, "budget.call.input_tokens")
                            }
                            "max_output_tokens" => {
                                (ceilings.call.output_tokens, "budget.call.output_tokens")
                            }
                            // A price is no cap on what one call may use.
                            _ => (None, ""),
                        };
                        match (cap, entry.get(field).and_then(Value::as_f64)) {
                            (Some(cap), Some(have)) if (cap as f64) < have => {
                                held_by = Some(cap_of);
                                Some(Value::from(cap))
                            }
                            _ => None,
                        }
                    }
                };
                if let Some(keep) = keep {
                    entry.insert(field.to_owned(), keep);
                    match held_by {
                        Some(cap_of) => {
                            capped.push(field.to_string());
                            sources.insert(dotted.clone(), theirs(cap_of));
                        }
                        None => {
                            held.push(field.to_string());
                            sources.insert(dotted.clone(), theirs(&dotted));
                        }
                    }
                }
            }
            if !held.is_empty() {
                bound.push(format!("{model}: {}", held.join(", ")));
            }
            if !capped.is_empty() {
                clamped.push(format!("{model}: {}", capped.join(", ")));
            }
        }
        // A model only they priced: a layer that writes `budget.pricing`
        // replaces the whole table below it, so their price for it would be
        // lost without this, and a project could leave it out to the same end.
        for (model, price) in &their_prices {
            if table.contains_key(*model) {
                continue;
            }
            let mut fields = Vec::new();
            for field in price.keys() {
                let dotted = format!("budget.pricing.{model}.{field}");
                sources.insert(dotted.clone(), theirs(&dotted));
                fields.push(field.as_str());
            }
            table.insert((*model).to_owned(), Value::Object((*price).clone()));
            bound.push(format!("{model}: {}", fields.join(", ")));
        }
    }
    if !bound.is_empty() {
        notes.push(format!(
            "budget.pricing: priced by your own configuration ({yours}), which a project's \
             budget.pricing cannot change: {}",
            bound.join("; ")
        ));
    }
    if !clamped.is_empty() {
        notes.push(format!(
            "budget.pricing: your own configuration ({yours}) caps the most one call may use, \
             which a project's budget.pricing cannot raise: {}",
            clamped.join("; ")
        ));
    }
    (ceilings, notes)
}

/// `v` as `atomic config` shows it, for a remark that names a cap.
fn show(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Whether a price written in two files is the same: `2` and `2.0` are the
/// same price, and holding one over the other would say so for nothing.
fn same(written: Option<&Value>, merged: Option<&Value>) -> bool {
    match (written, merged) {
        (Some(a), Some(b)) => {
            a == b || matches!((a.as_f64(), b.as_f64()), (Some(x), Some(y)) if x == y)
        }
        (None, None) => true,
        _ => false,
    }
}

/// Read the settings of the project configured in `dir` as `atomic run`
/// would, with no model flag and no Ollama probe, so a setting it would
/// refuse fails `atomic check` instead. A selected profile is checked from
/// the same resolved project's manifest directory.
pub fn check(dir: &Path, profile: Option<&str>) -> Result<Resolved, ConfigError> {
    resolve(
        dir,
        &EngineArgs {
            profile: profile.map(str::to_owned),
            ..EngineArgs::default()
        },
        &Surroundings::without_probe(),
    )
}

/// The models a local Ollama at `addr` lists: one `GET /api/tags`, all
/// within [`PROBE_DEADLINE`]. `None` when nothing answers so in time.
pub fn probe_ollama(addr: SocketAddr) -> Option<Vec<String>> {
    probe_within(addr, PROBE_DEADLINE)
}

/// [`probe_ollama`], with its deadline `within` from now.
fn probe_within(addr: SocketAddr, within: Duration) -> Option<Vec<String>> {
    let deadline = Instant::now() + within;
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
    };
    let mut stream = TcpStream::connect_timeout(&addr, left()?).ok()?;
    stream.set_write_timeout(left()).ok()?;
    // HTTP/1.0, so the answer comes whole and the connection closes after
    // it; a chunked answer is decoded all the same.
    let request = format!("GET /api/tags HTTP/1.0\r\nHost: {addr}\r\n\r\n");
    write_request(&mut stream, request.as_bytes()).ok()?;
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    let body = loop {
        match body_of(&response, false) {
            Body::Done(body) => break body,
            Body::Bad => return None,
            Body::More => {}
        }
        // No read waits past the deadline, however slowly bytes trickle.
        stream.set_read_timeout(Some(left()?)).ok()?;
        match stream.read(&mut chunk) {
            Ok(0) => match body_of(&response, true) {
                Body::Done(body) => break body,
                _ => return None,
            },
            Ok(n) => response.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
        if response.len() > 1 << 20 {
            return None;
        }
    };
    let body: Value = serde_json::from_slice(&body).ok()?;
    let models = body.get("models")?.as_array()?;
    Some(
        models
            .iter()
            .filter_map(|m| m.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect(),
    )
}

/// The probe's `head`, put on `w` in exactly one write. Sent in one write, so
/// a server that answers what its first read gets, and then closes with the
/// rest unread, which resets the connection, has the whole request. That one
/// call is the control, so it is said here rather than written at the call
/// site: `settings::tests` counts the writes `write_request` makes and fails
/// when the request goes out in pieces.
fn write_request<W: Write>(w: &mut W, head: &[u8]) -> std::io::Result<()> {
    w.write_all(head)
}

/// How much of an HTTP answer has arrived.
enum Body {
    /// Not all of it: read on.
    More,
    /// All of a successful answer's body.
    Done(Vec<u8>),
    /// Not a successful answer, or not HTTP.
    Bad,
}

/// The body of `response` so far, `closed` when the connection has ended.
fn body_of(response: &[u8], closed: bool) -> Body {
    let more = if closed { Body::Bad } else { Body::More };
    let Some(at) = response.windows(4).position(|w| w == b"\r\n\r\n") else {
        return more;
    };
    let head = String::from_utf8_lossy(&response[..at]).to_ascii_lowercase();
    let mut lines = head.lines();
    let status = lines.next().unwrap_or_default();
    if !(status.starts_with("http/1.1 200") || status.starts_with("http/1.0 200")) {
        return Body::Bad;
    }
    let headers: Vec<(&str, &str)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect();
    let header = |name: &str| headers.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
    let rest = &response[at + 4..];
    if header("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        return match dechunk(rest) {
            Some(Some(body)) => Body::Done(body),
            Some(None) => more,
            None => Body::Bad,
        };
    }
    match header("content-length").map(str::parse::<usize>) {
        Some(Ok(length)) if rest.len() >= length => Body::Done(rest[..length].to_vec()),
        Some(Ok(_)) => more,
        Some(Err(_)) => Body::Bad,
        // No length: the body is whatever came before the connection closed.
        None if closed => Body::Done(rest.to_vec()),
        None => Body::More,
    }
}

/// A chunked body, decoded: `Some(None)` while its last chunk has not
/// arrived, `None` when it is not chunked as HTTP says.
fn dechunk(mut rest: &[u8]) -> Option<Option<Vec<u8>>> {
    let mut body = Vec::new();
    loop {
        let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") else {
            return Some(None);
        };
        let line = std::str::from_utf8(&rest[..eol]).ok()?;
        let size = usize::from_str_radix(line.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[eol + 2..];
        if size == 0 {
            // Trailers, if any, are not needed.
            return Some(Some(body));
        }
        let end = size.checked_add(2)?;
        if rest.len() < end {
            return Some(None);
        }
        if &rest[size..end] != b"\r\n" {
            return None;
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[end..];
    }
}

impl Resolved {
    /// The model endpoint, if one is set, signed in to or detected, and a
    /// model is named.
    pub fn endpoint(&self) -> Option<ModelEndpoint> {
        let m = &self.settings.model;
        match (&m.base_url, &self.sign_in) {
            (Some(base_url), _) => Some(ModelEndpoint {
                base_url: base_url.clone(),
                model: m.name.clone()?,
                api_key_env: m.api_key_env.clone(),
                sign_in: None,
            }),
            // No key: the sign-in's tokens are Codex's to send.
            (None, Some(sign_in)) => Some(ModelEndpoint {
                base_url: atomic_core::signin::CODEX_BACKEND.into(),
                model: m.name.clone()?,
                api_key_env: None,
                sign_in: Some(sign_in.clone()),
            }),
            (None, None) => None,
        }
    }

    /// Whether model atoms would run on the sign-in: someone is signed in
    /// and no endpoint is set.
    pub fn on_sign_in(&self) -> bool {
        self.sign_in.is_some() && self.settings.model.base_url.is_none()
    }

    /// Whether someone chose the model endpoint: a flag, a variable or a
    /// configuration file set `model.base_url`. Such an endpoint comes before
    /// the sign-in, which is then not looked for. One detection found, from
    /// `OPENAI_API_KEY` or a local Ollama, comes only after it.
    pub fn endpoint_chosen(&self) -> bool {
        let source = self.sources.get("model.base_url");
        let detected = matches!(source, Some(Source::Detected(_)));
        self.settings.model.base_url.is_some() && !detected
    }

    /// Every setting for `atomic config --explain`: its path, its value as
    /// shown, with secrets masked, and where it came from.
    pub fn explain(&self, env: &dyn Fn(&str) -> Option<String>) -> Vec<(String, Value, String)> {
        let value = serde_json::to_value(&self.settings).unwrap_or(Value::Null);
        self.sources
            .iter()
            .filter(|(path, _)| !MANIFEST.contains(&path.split('.').next().unwrap_or("")))
            .map(|(path, source)| {
                let mut shown = path
                    .split('.')
                    .try_fold(&value, |v, k| v.get(k))
                    .cloned()
                    .unwrap_or(Value::Null);
                if (path == "database.url" || path == "model.base_url")
                    && let Some(url) = shown.as_str()
                {
                    shown = Value::String(mask_url(url));
                }
                if path == "model.api_key_env"
                    && let Some(name) = shown.as_str()
                {
                    shown = Value::String(if is_variable_name(name) {
                        let state = if env(name).is_some_and(|v| !v.is_empty()) {
                            "set"
                        } else {
                            "not set"
                        };
                        format!("{name} ({state}; its value is not shown)")
                    } else {
                        "(set, and not a variable's name; not shown)".into()
                    });
                }
                (path.clone(), shown, describe(source))
            })
            .collect()
    }

    /// The line `atomic run`, `runs resume` and `serve` print when the model
    /// endpoint was detected or set by the project's files rather than by
    /// whoever runs it, so where model calls go is never a surprise.
    /// On the sign-in, it names the model going to ChatGPT's Codex backend.
    pub fn announcement(&self) -> Option<String> {
        let m = &self.settings.model;
        if self.on_sign_in() {
            let name = m.name.as_deref()?;
            return Some(format!(
                "atomic: model {name} on your ChatGPT sign-in, at {}; `atomic config --explain` \
                 says why",
                mask_url(atomic_core::signin::CODEX_BACKEND)
            ));
        }
        let (base_url, name) = (m.base_url.as_deref()?, m.name.as_deref()?);
        let source = self.sources.get("model.base_url")?;
        // Not the person's own configuration: that endpoint they chose.
        let announced = match source {
            Source::Detected(_) => true,
            Source::File { path, .. } => self.user.as_deref() != Some(path.as_path()),
            _ => false,
        };
        // A key, and where it goes, is said too.
        let key = m.api_key_env.as_deref().map_or_else(String::new, |key| {
            let host = url::Url::parse(base_url)
                .ok()
                .and_then(|u| {
                    u.host_str().map(|h| match u.port() {
                        Some(port) => format!("{h}:{port}"),
                        None => h.to_owned(),
                    })
                })
                .unwrap_or_else(|| mask_url(base_url));
            format!("; {key} is sent to {host}")
        });
        announced.then(|| {
            format!(
                "atomic: model {name} at {} ({}){key}; `atomic config --explain` says why",
                mask_url(base_url),
                describe(source)
            )
        })
    }
}

/// Whether `name` could be an environment variable's name, and so is safe
/// to show: what is not may be the secret itself, put in the wrong place.
fn is_variable_name(name: &str) -> bool {
    name.len() <= 128
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// What `atomic config` shows of a URL that cannot be shown safely.
const NOT_SHOWN: &str = "(set; not shown)";

/// `raw` rebuilt from its scheme, host, port and path only: never its user,
/// password or query, which may carry credentials. A value that does not
/// parse as a URL with a host is not shown at all.
fn mask_url(raw: &str) -> String {
    let Ok(url) = url::Url::parse(raw) else {
        return NOT_SHOWN.into();
    };
    let Some(host) = url.host_str() else {
        return NOT_SHOWN.into();
    };
    let mut out = format!("{}://", url.scheme());
    if !url.username().is_empty() || url.password().is_some() {
        out.push_str("***@");
    }
    out.push_str(host);
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    // A path is shown as far as it names things, such as `/v1` or a
    // database; a segment that could be a key, long or of other
    // characters, is not.
    if !url.path().is_empty() {
        let Some(segments) = url.path_segments() else {
            return NOT_SHOWN.into();
        };
        for segment in segments {
            let plain = segment.len() <= 16
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
            out.push('/');
            out.push_str(if plain { segment } else { "***" });
        }
    }
    if url.query().is_some() {
        out.push_str("?***");
    }
    out
}

/// Where a setting came from, for a person.
pub fn describe(source: &Source) -> String {
    match source {
        Source::Default => "default".into(),
        Source::File { path, line } => format!("{}:{line}", path.display()),
        Source::Env(name) => format!("environment variable {name}"),
        Source::Flag(name) => format!("flag {name}"),
        Source::Detected(why) => format!("detected: {why}"),
    }
}

/// `atomic config`.
#[derive(Debug, clap::Args)]
pub struct ConfigArgs {
    /// Say where each setting came from, and why.
    #[arg(long)]
    pub explain: bool,
    #[command(flatten)]
    pub engine: EngineArgs,
}

/// `atomic config`: every setting the project resolves to, here and now,
/// and with `--explain` where each came from. Detection runs as it would
/// for `atomic run`; nothing else does.
pub fn config(args: &ConfigArgs, project: Option<&Path>, json: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let (root, in_project) = match crate::target::project_for(project, None, &cwd) {
        Ok(found) => (found.root().to_owned(), true),
        // Outside a project, only the variables and detection apply.
        Err(_) if project.is_none() => (cwd, false),
        Err(e) => return Err(e),
    };
    let dir = config_dir(&root);
    let around = Surroundings::here();
    if !in_project && selected_profile(&args.engine, &around)?.is_some() {
        anyhow::bail!(
            "a configuration profile needs a project; run from that project or pass --project"
        );
    }
    let resolved = resolve(&dir, &args.engine, &around)
        .map_err(|e| anyhow::anyhow!("the configuration cannot be read: {e}"))?;
    let mut rows = resolved.explain(&|name| std::env::var(name).ok());
    let mut notes = resolved.notes.clone();
    // The sign-in, if anyone is signed in: how and where it is kept, never a
    // token or an email, and whether model atoms use it here. One that
    // cannot be looked for is said, as resolution says it.
    let signed_in = match SignIn::located(in_project.then_some(root.as_path())) {
        Ok(sign_in) => sign_in.explain(),
        Err(atomic_core::signin::SignInError::Unprotected(_)) => None,
        // Said as resolution says it, when there is a sign-in's directory
        // there to say it of.
        Err(e) => {
            let there = match atomic_core::signin::location().map(|d| std::fs::symlink_metadata(&d))
            {
                Some(Err(gone)) => !absent(&gone),
                _ => true,
            };
            // With an endpoint configured, the sign-in is not looked for and
            // a run uses the endpoint.
            let configured = resolved.sign_in_unreadable.is_none() && resolved.endpoint_chosen();
            let note = if configured {
                format!(
                    "the ChatGPT sign-in could not be read: {e}; not used: a model endpoint is \
                     configured, which comes first"
                )
            } else {
                unusable(&e.to_string())
            };
            if there && !notes.contains(&note) {
                notes.push(note);
            }
            None
        }
    };
    if let Some(explained) = signed_in {
        let shared = resolved
            .settings
            .database
            .url
            .as_deref()
            .is_some_and(|u| !u.is_empty());
        let source = if resolved.settings.model.base_url.is_some() {
            format!(
                "{}; not used: a model endpoint is configured, which comes first",
                explained.source
            )
        } else if shared {
            format!(
                "{}; not used: the store is shared (PostgreSQL), and a sign-in is personal",
                explained.source
            )
        } else {
            explained.source
        };
        rows.push((
            "model.sign_in".into(),
            Value::String(explained.value),
            source,
        ));
    }
    if json {
        let settings: Vec<Value> = rows
            .iter()
            .map(|(path, value, source)| {
                serde_json::json!({"setting": path, "value": value, "source": source})
            })
            .collect();
        // Masked already, and redacted besides, as everything atomic prints.
        let mut doc = serde_json::json!({
            "project": root,
            "settings": settings,
            "notes": notes,
        });
        atomic_core::redact::value(&mut doc);
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(());
    }
    let width = rows.iter().map(|(p, _, _)| p.len()).max().unwrap_or(0);
    let shown: Vec<String> = rows
        .iter()
        .map(|(_, v, _)| match v {
            Value::Null => "(not set)".to_owned(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    let value_width = shown.iter().map(String::len).max().unwrap_or(0);
    let say = |line: String| println!("{}", atomic_core::redact::text(&line));
    for ((path, _, source), value) in rows.iter().zip(&shown) {
        if args.explain {
            say(format!("{path:<width$}  {value:<value_width$}  {source}"));
        } else {
            say(format!("{path:<width$}  {value}"));
        }
    }
    for note in &notes {
        say(format!("note: {note}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
