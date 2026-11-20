// SPDX-License-Identifier: Apache-2.0

//! The settings `atomic run`, `runs resume`, `serve`, `gates` and `runs show`
//! use, resolved through atomic's configuration layers
//! ([`atomic_core::config`]), with what atomic detects around it filling
//! what nothing set, and where each came from, for `atomic config --explain`.
//!
//! Detection fills only what no file, variable or flag set, and it discovers;
//! it never grants. It reads:
//!
//! - `OPENAI_API_KEY`: when it is set, OpenAI's API is the model endpoint, and
//!   `OPENAI_API_KEY` the variable its key is read from. Its value is never
//!   read here, shown or sent anywhere.
//! - A local Ollama: when no key is set, one request, `GET /api/tags`, to
//!   Ollama's default address on the loopback interface, and nowhere else.
//!   When it answers, it is the endpoint, with the first model it lists.
//!
//! `ATOMIC_DATABASE_URL` selects PostgreSQL; `DATABASE_URL`, which usually
//! belongs to the application, is never read.

use crate::engine::{EngineArgs, RunnerKind};
use atomic_core::budget::BudgetSettings;
use atomic_core::codex::ModelEndpoint;
use atomic_core::config::{self, ConfigError, Layers, Source};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// OpenAI's API, when `OPENAI_API_KEY` is set.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
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
}

/// What resolution reads from outside: the environment, and a local
/// Ollama. Passed in, so tests choose what is there.
pub struct Surroundings<'a> {
    /// A variable's value.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// The variables there are.
    pub env_names: Vec<String>,
    /// The models a local Ollama at an address lists, if one answers.
    pub ollama: &'a dyn Fn(SocketAddr) -> Option<Vec<String>>,
}

impl Surroundings<'static> {
    /// This process's environment and a real probe.
    pub fn here() -> Self {
        Self {
            ollama: &probe_ollama,
            ..Self::without_probe()
        }
    }

    /// [`Surroundings::here`], but no Ollama answers: for `atomic check`,
    /// which reads the settings and contacts nothing.
    pub fn without_probe() -> Self {
        Self {
            env: &|name| std::env::var(name).ok(),
            env_names: std::env::vars_os()
                .filter_map(|(k, _)| k.into_string().ok())
                .collect(),
            ollama: &|_| None,
        }
    }
}

/// The person's own configuration file, which no project supplies:
/// `$XDG_CONFIG_HOME/atomic/config.yaml`, by default
/// `~/.config/atomic/config.yaml`. `None` when neither variable names an
/// absolute directory.
pub fn user_config(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    atomic_core::state::user_config_dir_in(env).map(|d| d.join("config.yaml"))
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

/// Resolve the settings of the project configured in `dir`, with `args`'
/// flags on top, in `around`.
pub fn resolve(
    dir: &Path,
    args: &EngineArgs,
    around: &Surroundings<'_>,
) -> Result<Resolved, ConfigError> {
    let user = user_config(around.env);
    // A project could supply a "personal" file inside itself, or inside the
    // checkout that holds it, as when the home directory is where the
    // repository is mounted. Only for a project: outside one, such as
    // `atomic config` run from the home directory, nothing supplies it.
    if dir.join("atomic.yaml").is_file()
        && let Some(file) = &user
        && let Ok(file) = std::fs::canonicalize(file)
    {
        let root = if dir.file_name().is_some_and(|n| n == ".atomic") {
            dir.parent().unwrap_or(dir)
        } else {
            dir
        };
        if let Ok(root) = std::fs::canonicalize(root) {
            let checkout = root.ancestors().find(|d| d.join(".git").exists());
            for (what, enclosing) in std::iter::once(("project", root.as_path()))
                .chain(checkout.map(|c| ("checkout holding the project", c)))
            {
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
    }
    let resolved = config::resolve::<Settings>(&Layers {
        dir,
        user: user.as_deref(),
        profile: None,
        env: around.env,
        env_names: around.env_names.clone(),
        flags: flags(args),
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
    let mut notes = Vec::new();
    if let Some(why) = &withheld_key {
        settings.model.api_key_env = None;
        sources.insert(
            "model.api_key_env".into(),
            Source::Detected("not sent to an endpoint the project chose".into()),
        );
        notes.push(why.clone());
    }
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

    // Detection, for what nothing set.
    if unset(&sources, "model.base_url") {
        if set((around.env)("OPENAI_API_KEY")).is_some() {
            let why = "OPENAI_API_KEY is set".to_owned();
            settings.model.base_url = Some(OPENAI_BASE_URL.into());
            sources.insert("model.base_url".into(), Source::Detected(why.clone()));
            if unset(&sources, "model.api_key_env") {
                settings.model.api_key_env = Some("OPENAI_API_KEY".into());
                sources.insert("model.api_key_env".into(), Source::Detected(why.clone()));
            }
            if unset(&sources, "model.name") {
                settings.model.name = Some(OPENAI_DEFAULT_MODEL.into());
                sources.insert(
                    "model.name".into(),
                    Source::Detected(format!("{why}: atomic's default model on OpenAI's API")),
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
    Ok(Resolved {
        settings,
        sources,
        notes,
        user,
        withheld_key,
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

/// Read the settings of the project configured in `dir` as `atomic run`
/// would, with no flags and no Ollama probe, so a setting it would refuse
/// fails `atomic check` instead.
pub fn check(dir: &Path) -> Result<Resolved, ConfigError> {
    resolve(dir, &EngineArgs::default(), &Surroundings::without_probe())
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
    write!(stream, "GET /api/tags HTTP/1.0\r\nHost: {addr}\r\n\r\n").ok()?;
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
    /// The model endpoint, if one is set or detected.
    pub fn endpoint(&self) -> Option<ModelEndpoint> {
        let m = &self.settings.model;
        Some(ModelEndpoint {
            base_url: m.base_url.clone()?,
            model: m.name.clone()?,
            api_key_env: m.api_key_env.clone(),
        })
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
    pub fn announcement(&self) -> Option<String> {
        let m = &self.settings.model;
        let (base_url, name) = (m.base_url.as_deref()?, m.name.as_deref()?);
        let source = self.sources.get("model.base_url")?;
        // Not the person's own configuration: that endpoint they chose.
        let announced = match source {
            Source::Detected(_) => true,
            Source::File { path, .. } => self.user.as_deref() != Some(path.as_path()),
            _ => false,
        };
        announced.then(|| {
            format!(
                "atomic: model {name} at {} ({}); `atomic config --explain` says why",
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
    let root = match crate::target::project_for(project, None, &cwd) {
        Ok(found) => found.root().to_owned(),
        // Outside a project, only the variables and detection apply.
        Err(_) if project.is_none() => cwd,
        Err(e) => return Err(e),
    };
    let dir = config_dir(&root);
    let resolved = resolve(&dir, &args.engine, &Surroundings::here())
        .map_err(|e| anyhow::anyhow!("the configuration cannot be read: {e}"))?;
    let rows = resolved.explain(&|name| std::env::var(name).ok());
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
            "notes": resolved.notes,
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
    for note in &resolved.notes {
        say(format!("note: {note}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
