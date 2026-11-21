// SPDX-License-Identifier: Apache-2.0

//! The control plane that `atomic run`, `atomic runs resume` and
//! `atomic serve` share: the project's execution lock, its durable store,
//! `run_pipeline` registered on it, and the runtimes atoms run on.
//!
//! The store is the project's SQLite database in its state directory, or
//! PostgreSQL when `ATOMIC_DATABASE_URL` names one (in the schema
//! `ATOMIC_DATABASE_SCHEMA`, `atomic` by default). On PostgreSQL every
//! process needs an executor identity of its own (`ATOMIC_EXECUTOR_ID`), so
//! that a restarted process recovers its own unfinished runs and never
//! another live process's.

use crate::failure::Failure;
use crate::settings;
use anyhow::Context;
use atomic_app::pipeline::{LAYOUT_VERSION, Services, register_pipeline};
use atomic_core::AtomConfig;
use atomic_core::artifact::LocalStore;
use atomic_core::codex::{CodexPaths, ModelEndpoint};
use atomic_core::lite::LiteRuntime;
use atomic_core::runtime::{Artifacts, CodexRuntime, Runtimes};
use atomic_core::state::ProjectState;
use atomic_core::worktree::Worktrees;
use atomic_durable::gate::{Deployment, Principal};
use atomic_durable::{Database, DurableRuntime, Durare};
use atomic_exec::runner::ProcessRunner;
use clap::{Args, ValueEnum};
use std::sync::Arc;

/// The environment variable that names atomic's PostgreSQL database.
pub const DATABASE_URL: &str = "ATOMIC_DATABASE_URL";

/// Where exec atoms' commands run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum RunnerKind {
    /// As child processes of this process.
    #[default]
    Local,
    /// In an `atomic runner` process of their own, started for each command,
    /// which sees only the environment it is given.
    Process,
}

/// How a command that executes runs is set up, by flag. Each is also a
/// setting, which a file or an `ATOMIC_*` variable can give (see
/// [`crate::settings`]); a flag wins over both.
#[derive(Clone, Debug, Default, Args)]
pub struct EngineArgs {
    /// Base URL of the OpenAI-compatible API that model atoms use
    /// (`model.base_url`, `ATOMIC_MODEL_BASE_URL`).
    #[arg(long)]
    pub model_base_url: Option<String>,
    /// Model id at that endpoint (`model.name`, `ATOMIC_MODEL`).
    #[arg(long)]
    pub model: Option<String>,
    /// Environment variable that holds the endpoint's API key, if it needs
    /// one (`model.api_key_env`, `ATOMIC_MODEL_API_KEY_ENV`).
    #[arg(long)]
    pub model_api_key_env: Option<String>,
    /// Where exec atoms' commands run (`runner`, `ATOMIC_RUNNER`).
    #[arg(long, value_enum)]
    pub runner: Option<RunnerKind>,
    /// This process's executor identity: unique to it and the same across
    /// its restarts. Required on PostgreSQL (`executor_id`,
    /// `ATOMIC_EXECUTOR_ID`).
    #[arg(long)]
    pub executor_id: Option<String>,
    /// How many seconds a step waits for room, on the execution backend for
    /// its workspace or on the disk for atomic's own writes (its run's build
    /// caches and its worktree's directory), before the run stops,
    /// unfinished and with nothing recorded, for `atomic runs resume`. Each
    /// of those waits has its own clock. A checkout git cannot finish fails
    /// the step instead. Without it, a step waits for as long as it takes
    /// (`backend_wait`, `ATOMIC_BACKEND_WAIT`).
    #[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..))]
    pub backend_wait: Option<u64>,
}

/// The settings for the project at `state`, with `args` on top. The
/// person's sign-in is looked for only when `model_calls` says a model will
/// be called on them: a replay or a run that calls no model never reads it.
pub fn settings(
    state: &ProjectState,
    args: &EngineArgs,
    model_calls: bool,
) -> anyhow::Result<settings::Resolved> {
    let around = if model_calls {
        settings::Surroundings::here()
    } else {
        settings::Surroundings::here_without_sign_in()
    };
    settings::resolve(&settings::config_dir(state.root()), args, &around).map_err(|e| {
        Failure::new(
            "config.invalid",
            format!("the configuration cannot be read: {e}"),
            "fix the setting it names; `atomic config --explain` lists every setting and \
             where it comes from",
        )
        .into()
    })
}

/// Where exec atoms' commands run, as the settings say.
pub fn runner_of(resolved: &settings::Resolved) -> anyhow::Result<RunnerKind> {
    match resolved.settings.runner.as_str() {
        "local" => Ok(RunnerKind::Local),
        "process" => Ok(RunnerKind::Process),
        other => Err(Failure::new(
            "config.invalid",
            format!("runner is `{other}`, and atomic knows `local` and `process`"),
            "set runner (ATOMIC_RUNNER, or --runner) to local or process",
        )
        .into()),
    }
}

/// The backend on this machine that `atomic run`, `atomic serve` and the
/// evals run turns and exec atoms' commands on, unconfined and unconnected:
/// scratch in the project's state, outside every directory a sandboxed
/// turn may write.
pub fn local_backend(state: &ProjectState) -> atomic_exec::LocalBackend {
    atomic_exec::LocalBackend::in_dir(state.dir().join("scratch"))
}

/// The backend `atomic debug codex-prompt` and `atomic debug probe` run
/// their turn on: one of its own beside [`local_backend`], since these
/// commands take no execution lock, and connecting empties a backend's
/// shared temporary directory, which a run's turns may be using.
pub fn debug_backend(state: &ProjectState) -> atomic_exec::LocalBackend {
    atomic_exec::LocalBackend::in_dir(state.dir().join("scratch-debug"))
}

/// The backend exec atoms' commands run on with `runner`: scratch in the
/// project's state, confined like turns' commands where that is verified,
/// connected once.
pub fn exec_backend(
    state: &ProjectState,
    paths: &CodexPaths,
    runner: RunnerKind,
) -> anyhow::Result<atomic_exec::Backend> {
    // Scratch directories live in the project's state directory, outside
    // every directory a sandboxed turn may write. Connected once: what the
    // backend states here is what runs plan on, and connecting reclaims
    // what attempts of dead processes left.
    let local = local_backend(state);
    // Exec atoms' commands run under the same sandbox helper as turns'
    // commands, denied atomic's private state. Elsewhere than Linux this is
    // not verified, and they run unconfined (see the availability notes).
    // They may write their scratch directory and their run's own build
    // caches, which each workspace names, and nothing shared between runs.
    #[cfg(target_os = "linux")]
    let local = local.confined(std::sync::Arc::new(
        atomic_core::codex::ExecSandbox::new(
            paths,
            atomic_core::codex::StateGuard::for_project(state)?,
            state.codex_home(),
        )
        .context("confine exec atoms' commands")?,
    ));
    match runner {
        RunnerKind::Local => atomic_exec::Backend::connect(local),
        RunnerKind::Process => atomic_exec::Backend::connect(process_runner(
            local,
            std::env::current_exe().context("find atomic's own executable for its runners")?,
        )),
    }
    .context("connect to the execution backend")
}

/// The runner `--runner process` runs each command in: `atomic runner`,
/// from `atomic`, with only `PATH` from this process's environment, opening
/// workspaces on `local`.
fn process_runner(local: atomic_exec::LocalBackend, atomic: std::path::PathBuf) -> ProcessRunner {
    ProcessRunner {
        workspaces: local,
        ..ProcessRunner::atomic(atomic)
    }
}

#[cfg(test)]
mod process_runner_tests {
    /// The runner `--runner process` starts gets only `PATH` from atomic's
    /// environment, and is `atomic runner`.
    #[test]
    fn the_process_runner_gets_only_path_from_atomics_environment() {
        assert!(std::env::var_os("PATH").is_some(), "this test needs PATH");
        let runner = super::process_runner(
            atomic_exec::LocalBackend::default(),
            std::path::PathBuf::from("/opt/atomic"),
        );
        let names: Vec<&str> = runner.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["PATH"]);
        assert_eq!(runner.args, ["runner"]);
        assert_eq!(runner.program, std::path::PathBuf::from("/opt/atomic"));
    }
}

/// The endpoint a replay of recorded model calls names: where the calls
/// were recorded for, and the model, with no key, since no call reaches it.
/// A key the project's files may not send is never read either, so a
/// withheld key does not refuse a replay. `resolved` is resolved without
/// the sign-in (see [`settings`]): lite atoms, which are what replays, are
/// never recorded on one.
pub fn identity(resolved: &settings::Resolved) -> anyhow::Result<ModelEndpoint> {
    match resolved.endpoint() {
        // No token is read either.
        Some(endpoint) => Ok(ModelEndpoint {
            api_key_env: None,
            sign_in: None,
            ..endpoint
        }),
        None => Err(Failure::new(
            "config.model",
            "a replay answers each call by the endpoint and model it was recorded for, and \
             none is configured or detected",
            "set the endpoint and model the recordings were made with (ATOMIC_MODEL_BASE_URL \
             and ATOMIC_MODEL, or --model-base-url and --model); no call reaches them and no \
             key is read",
        )
        .into()),
    }
}

/// Why a personal sign-in is not used with a PostgreSQL store.
pub const SHARED_STORE: &str = "the store is shared (PostgreSQL)";

/// The model endpoint, or with `required`, a failure saying how to set one.
/// A configured endpoint comes first, then the person's ChatGPT sign-in,
/// then what atomic detects (see [`settings::resolve`]). `personal_refused`
/// says why the sign-in, which is the person's own, may not serve here.
pub fn endpoint(
    resolved: &settings::Resolved,
    required: bool,
    personal_refused: Option<&str>,
) -> anyhow::Result<ModelEndpoint> {
    // The person's key goes nowhere the project chose: a run that calls a
    // model has no model access until they choose the endpoint.
    if required && let Some(why) = &resolved.withheld_key {
        return Err(Failure::new(
            "config.model",
            format!("model atoms need a model endpoint you chose, and {why}"),
            "set OPENAI_API_KEY to use OpenAI's API, start Ollama on this machine, or set \
             ATOMIC_MODEL_BASE_URL and ATOMIC_MODEL (or pass --model-base-url and --model), \
             and ATOMIC_MODEL_API_KEY_ENV if the endpoint needs a key; `atomic config \
             --explain` shows what was found",
        )
        .into());
    }
    if resolved.on_sign_in() {
        match (personal_refused, required) {
            (Some(why), true) => {
                // Refused, never passed over for a key or a detected
                // endpoint: the person learns why model calls do not go
                // where they would at their own desk.
                return Err(Failure::new(
                    "config.model",
                    format!(
                        "you are signed in with ChatGPT, and a sign-in is personal: {why}, so \
                         it is not used here, and nothing else is used in its place"
                    ),
                    "configure model.base_url (ATOMIC_MODEL_BASE_URL, or --model-base-url) and \
                     model.name (ATOMIC_MODEL), with model.api_key_env naming the variable that \
                     holds the endpoint's key (OPENAI_API_KEY for OpenAI's API); or run `atomic \
                     model logout`, and atomic detects OPENAI_API_KEY or a local Ollama again",
                )
                .into());
            }
            // Nothing here calls a model, and nothing may on the sign-in.
            (Some(_), false) => return Ok(unconfigured()),
            (None, true) if resolved.settings.model.name.is_none() => {
                return Err(Failure::new(
                    "config.model",
                    "you are signed in with ChatGPT, and no model is configured",
                    "set ATOMIC_MODEL (or pass --model) to a model your plan offers for Codex",
                )
                .into());
            }
            _ => {}
        }
    }
    match resolved.endpoint() {
        Some(endpoint) => Ok(endpoint),
        None if required => Err(Failure::new(
            "config.model",
            match &resolved.sign_in_unreadable {
                Some(why) => format!(
                    "no model endpoint is configured or detected, model atoms need one, and \
                     the ChatGPT sign-in could not be read: {why}"
                ),
                None => {
                    "no model endpoint is configured or detected, and model atoms need one".into()
                }
            },
            "sign in with `atomic model login` (Codex atoms only), set OPENAI_API_KEY to use \
             OpenAI's API, start Ollama on this machine, or set ATOMIC_MODEL_BASE_URL and \
             ATOMIC_MODEL (or pass --model-base-url and --model), and ATOMIC_MODEL_API_KEY_ENV \
             if the endpoint needs a key; `atomic config --explain` shows what was found",
        )
        .into()),
        None => Ok(unconfigured()),
    }
}

/// The endpoint when nothing will call a model: one that cannot be reached,
/// so any call that does fails loudly.
fn unconfigured() -> ModelEndpoint {
    ModelEndpoint {
        base_url: "http://127.0.0.1:9/unconfigured".into(),
        model: "unconfigured".into(),
        api_key_env: None,
        sign_in: None,
    }
}

/// The store the resolved settings name.
pub fn store_of(state: &ProjectState, settings: &settings::Settings) -> Database {
    match settings.database.url.clone().filter(|u| !u.is_empty()) {
        Some(url) => Database::Postgres {
            url,
            schema: settings.database.schema.clone(),
        },
        None => Database::Sqlite(state.database()),
    }
}

/// The store atomic is configured to use for the project at `state`.
pub fn database(state: &ProjectState) -> anyhow::Result<Database> {
    // The store needs no model, so no Ollama is looked for.
    let resolved = settings::resolve(
        &settings::config_dir(state.root()),
        &EngineArgs::default(),
        &settings::Surroundings::without_probe(),
    )
    .map_err(|e| anyhow::anyhow!("the configuration cannot be read: {e}"))?;
    Ok(store_of(state, &resolved.settings))
}

/// Where the store is, for a person: never the URL, which holds a password.
pub fn describe(database: &Database) -> String {
    match database {
        Database::Sqlite(path) => path.display().to_string(),
        Database::Postgres { schema, .. } => {
            format!("PostgreSQL ({DATABASE_URL}), schema {schema}")
        }
    }
}

/// One developer on one machine: the local operator decides gates.
///
/// `$USER` only names that operator in gate records. It is not a
/// credential: anyone who can set it can set it to anything, so it must never
/// be what proves a decision came from a person.
pub fn deployment() -> Deployment {
    let user = std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "operator".into());
    Deployment::Local {
        operator: Principal::local(&user),
    }
}

/// A control plane for one project. Its process holds the project's
/// execution lock until it exits.
pub struct Engine {
    /// The durable runtime, launched, with `run_pipeline` registered.
    pub rt: Durare,
    /// Where its store is, for messages.
    pub store: String,
    /// Whether the store is shared with other hosts' executors, which this
    /// process's execution lock does not exclude.
    pub shared: bool,
    /// Whether model atoms run on a ChatGPT sign-in, which lite atoms cannot
    /// use.
    pub signed_in: bool,
}

impl Engine {
    /// Start running: dispatch the resumed runs it takes, so a run resumed
    /// before this is taken once, by this engine.
    pub async fn launch(&self) -> anyhow::Result<()> {
        self.rt.launch().await.context("launch the durable runtime")
    }
}

/// Which resumed runs an engine takes, besides the runs it starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Takes {
    /// None: `atomic run`.
    Own,
    /// This one, from its own queue: `atomic runs resume <run>`.
    Run(String),
    /// Every run waiting on one of its executor's resume queues:
    /// `atomic serve`.
    Waiting,
}

/// Take the project's execution lock for `command`, open its store and
/// register `run_pipeline` on it, taking the runs `takes` says; then
/// [`Engine::launch`] it.
pub async fn open(
    state: &ProjectState,
    paths: &CodexPaths,
    args: &EngineArgs,
    command: &str,
    needs_model: bool,
    takes: Takes,
) -> anyhow::Result<Engine> {
    state.create()?;
    // Held until the process exits: dropping the engine while a command it
    // started still runs must not let another process recover that run.
    state.lock_execution(command)?.hold_until_exit();
    let resolved = settings(state, args, needs_model)?;
    atomic_core::runtime::limit_backend_wait(
        resolved
            .settings
            .backend_wait()
            .map(std::time::Duration::from_secs),
    );
    let database = store_of(state, &resolved.settings);
    let executor = atomic_durable::executor_identity(resolved.settings.executor_id.as_deref())?;
    if matches!(database, Database::Postgres { .. }) && executor.is_none() {
        return Err(Failure::new(
            "config.executor_id",
            format!(
                "{command} on PostgreSQL needs an executor identity of its own, and \
                 ATOMIC_EXECUTOR_ID is not set"
            ),
            "set ATOMIC_EXECUTOR_ID (or pass --executor-id) to a name unique to this process \
             that stays the same across its restarts, such as its host or StatefulSet member \
             name; never a random container or pod name, which would leave its runs \
             unrecovered after a restart",
        )
        .into());
    }
    // A personal sign-in serves runs in a store no other host's executors
    // share, and never `atomic serve`; the store is the one the settings
    // name, whichever file or variable names it. `atomic runs resume` on
    // such a store uses it for whichever run is resumed.
    let personal_refused = if matches!(database, Database::Postgres { .. }) {
        Some(SHARED_STORE)
    } else if takes == Takes::Waiting {
        Some("`atomic serve` runs whatever the store's runs need")
    } else {
        None
    };
    let endpoint = endpoint(&resolved, needs_model, personal_refused)?;
    let signed_in = endpoint.sign_in.is_some();
    // Where model calls go, when the person running this did not say.
    if let Some(line) = resolved.announcement() {
        atomic_core::log_line!("{line}");
    }
    let store = LocalStore::for_project(state).context("the artifact store")?;
    // A run whose workflow code panics is noted where later processes look.
    atomic_app::panics::keep_in(state.private_dir().join("panics"));
    let exec = exec_backend(state, paths, runner_of(&resolved)?)?;
    let runtimes = Arc::new(Runtimes {
        codex: CodexRuntime {
            paths: paths.clone(),
            codex_home: state.codex_home(),
            cwd: state.root().to_owned(),
            endpoint: endpoint.clone(),
            defaults: AtomConfig::default(),
            artifacts: Artifacts::store(store.clone(), file_limits()?),
            // Each step's meter comes from the run's pinned caps, set where
            // the step runs; a step whose atom calls a model and has none is
            // refused there, never run unmetered.
            meter: atomic_core::usage::Metering::none(),
            // Atoms' commands are kept out of atomic's private state.
            guard: atomic_core::codex::StateGuard::for_project(state)?,
            caches: None,
        },
        lite: LiteRuntime::new(endpoint, AtomConfig::default()),
        backend: exec,
        exec_meter: Default::default(),
        clock: None,
    });
    // Never the developer's checkout: a run's worktree steps use the
    // snapshot of the fixture it is bound to (see `atomic_core::fixture`).
    let worktrees: Option<Arc<Worktrees>> = None;
    let mut rt = Durare::open(&database, executor.as_deref(), LAYOUT_VERSION)
        .await
        .with_context(|| format!("open the durable store at {}", describe(&database)))?;
    register_pipeline(
        &mut rt,
        Services {
            runtimes,
            store,
            deployment: deployment(),
            worktrees,
            // Each run's build caches, in a directory of its own here.
            caches: Some(state.dir().join("cache")),
            // Budget caps and prices, through the configuration layers.
            budgets: Arc::new(resolved.settings.budget.clone()),
            // No context settings yet: a step whose atom declares a public
            // context is refused rather than resolved from nothing.
            context: None,
        },
    );
    let only = match &takes {
        Takes::Own => None,
        Takes::Run(run) => Some(Some(run.as_str())),
        Takes::Waiting => Some(None),
    };
    if let Some(only) = only {
        rt.take_resumed_runs(only)
            .await
            .context("take resumed runs on the durable runtime")?;
    }
    Ok(Engine {
        rt,
        store: describe(&database),
        shared: matches!(database, Database::Postgres { .. }),
        signed_in,
    })
}

/// How much an exec atom's command may hand over: the defaults, or
/// `ATOMIC_EXEC_MAX_FILE_BYTES` and `ATOMIC_EXEC_MAX_STEP_BYTES`.
fn file_limits() -> anyhow::Result<atomic_core::artifact::files::Limits> {
    let mut limits = atomic_core::artifact::files::Limits::default();
    for (var, slot) in [
        ("ATOMIC_EXEC_MAX_FILE_BYTES", &mut limits.per_file),
        ("ATOMIC_EXEC_MAX_STEP_BYTES", &mut limits.per_step),
    ] {
        if let Ok(text) = std::env::var(var) {
            *slot = text
                .trim()
                .parse()
                .with_context(|| format!("{var} is {text:?}, not a number of bytes"))?;
        }
    }
    Ok(limits)
}
