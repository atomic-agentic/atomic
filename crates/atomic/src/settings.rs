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
use std::time::Duration;

/// OpenAI's API, when `OPENAI_API_KEY` is set.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// The model atomic uses on OpenAI's API when none is set.
pub const OPENAI_DEFAULT_MODEL: &str = "gpt-5-mini";
/// Where a local Ollama listens by default.
pub const OLLAMA_ADDRESS: &str = "127.0.0.1:11434";
/// Names another loopback address to probe for Ollama instead, for tests and
/// for an Ollama on another local port. A non-loopback address is refused.
pub const OLLAMA_ADDRESS_VARIABLE: &str = "ATOMIC_DETECT_OLLAMA_ADDR";

/// The model endpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelSettings {
    /// The OpenAI-compatible API's base URL.
    pub base_url: Option<String>,
    /// The model there (also `ATOMIC_MODEL`).
    pub name: Option<String>,
    /// The environment variable that holds the endpoint's API key.
    pub api_key_env: Option<String>,
}

/// atomic's own store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseSettings {
    /// A PostgreSQL URL, or none for the project's SQLite store.
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
    pub executor_id: Option<String>,
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
            budget: BudgetSettings::default(),
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
    /// This process's environment, and a real probe.
    pub fn here() -> Self {
        Self {
            env: &|name| std::env::var(name).ok(),
            env_names: std::env::vars_os()
                .filter_map(|(k, _)| k.into_string().ok())
                .collect(),
            ollama: &probe_ollama,
        }
    }
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
    let resolved = config::resolve::<Settings>(&Layers {
        dir,
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
    let mut notes = Vec::new();
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
                    }
                }
                _ => notes.push(format!(
                    "{OLLAMA_ADDRESS_VARIABLE}={address} is not a loopback address, so no \
                     Ollama was looked for: detection probes this machine only"
                )),
            }
        }
    }
    Ok(Resolved {
        settings,
        sources,
        notes,
    })
}

/// The models a local Ollama at `addr` lists: one `GET /api/tags`, with
/// short timeouts. `None` when nothing answers so.
pub fn probe_ollama(addr: SocketAddr) -> Option<Vec<String>> {
    let timeout = Duration::from_millis(300);
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    write!(
        stream,
        "GET /api/tags HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    // The head, then as much body as it says, at most a megabyte.
    let mut response = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head_end, length) = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        response.extend_from_slice(&chunk[..n]);
        if response.len() > 1 << 20 {
            return None;
        }
        if let Some(at) = response.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&response[..at]).to_lowercase();
            if !head.starts_with("http/1.1 200") && !head.starts_with("http/1.0 200") {
                return None;
            }
            let length = head
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.trim() == "content-length")
                .and_then(|(_, v)| v.trim().parse::<usize>().ok());
            break (at + 4, length);
        }
    };
    while length.is_none_or(|l| response.len() < head_end + l) {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
        }
        if response.len() > 1 << 20 {
            return None;
        }
    }
    let body: Value = serde_json::from_slice(&response[head_end..]).ok()?;
    let models = body.get("models")?.as_array()?;
    Some(
        models
            .iter()
            .filter_map(|m| m.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect(),
    )
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
                if path == "database.url"
                    && let Some(url) = shown.as_str()
                {
                    shown = Value::String(mask_url(url));
                }
                if path == "model.api_key_env"
                    && let Some(name) = shown.as_str()
                {
                    let state = if env(name).is_some_and(|v| !v.is_empty()) {
                        "set"
                    } else {
                        "not set"
                    };
                    shown = Value::String(format!("{name} ({state}; its value is not shown)"));
                }
                (path.clone(), shown, describe(source))
            })
            .collect()
    }
}

/// `url` without the credentials it carries.
fn mask_url(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            format!("{}://***@{}", &url[..scheme], &url[at + 1..])
        }
        _ => url.to_owned(),
    }
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
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "project": root,
                "settings": settings,
                "notes": resolved.notes,
            }))?
        );
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
    for ((path, _, source), value) in rows.iter().zip(&shown) {
        if args.explain {
            println!("{path:<width$}  {value:<value_width$}  {source}");
        } else {
            println!("{path:<width$}  {value}");
        }
    }
    for note in &resolved.notes {
        println!("note: {note}");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
