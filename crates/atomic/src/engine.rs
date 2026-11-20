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
}

/// The settings for the project at `state`, with `args` on top.
pub fn settings(state: &ProjectState, args: &EngineArgs) -> anyhow::Result<settings::Resolved> {
    settings::resolve(
        &settings::config_dir(state.root()),
        args,
        &settings::Surroundings::here(),
    )
    .map_err(|e| {
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
    let local = atomic_exec::LocalBackend::in_dir(state.dir().join("scratch"));
    // Exec atoms' commands run under the same sandbox helper as turns'
    // commands, denied atomic's private state. Elsewhere than Linux this is
    // not verified, and they run unconfined (see the availability notes).
    // They may write their scratch directory and the project's caches.
    #[cfg(target_os = "linux")]
    let local = {
        let caches = atomic_core::worktree::Caches::for_state(state);
        caches.create().context("the project's caches")?;
        local.confined(std::sync::Arc::new(
            atomic_core::codex::ExecSandbox::new(
                paths,
                atomic_core::codex::StateGuard::for_project(state)?,
                state.codex_home(),
            )
            .context("confine exec atoms' commands")?
            .also_writing(vec![caches.dir().to_owned()]),
        ))
    };
    match runner {
        RunnerKind::Local => atomic_exec::Backend::connect(local),
        RunnerKind::Process => atomic_exec::Backend::connect(ProcessRunner {
            workspaces: local,
            ..ProcessRunner::atomic(
                std::env::current_exe().context("find atomic's own executable for its runners")?,
            )
        }),
    }
    .context("connect to the execution backend")
}

/// The endpoint a replay of recorded model calls names: where the calls
/// were recorded for, and the model, with no key, since no call reaches it.
/// A key the project's files may not send is never read either, so a
/// withheld key does not refuse a replay.
pub fn identity(resolved: &settings::Resolved) -> anyhow::Result<ModelEndpoint> {
    match resolved.endpoint() {
        Some(endpoint) => Ok(ModelEndpoint {
            api_key_env: None,
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

/// The model endpoint, or with `required`, a failure saying how to set one.
pub fn endpoint(resolved: &settings::Resolved, required: bool) -> anyhow::Result<ModelEndpoint> {
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
    match resolved.endpoint() {
        Some(endpoint) => Ok(endpoint),
        None if required => Err(Failure::new(
            "config.model",
            "no model endpoint is configured or detected, and model atoms need one",
            "set OPENAI_API_KEY to use OpenAI's API, start Ollama on this machine, or set \
             ATOMIC_MODEL_BASE_URL and ATOMIC_MODEL (or pass --model-base-url and --model), \
             and ATOMIC_MODEL_API_KEY_ENV if the endpoint needs a key; `atomic config \
             --explain` shows what was found",
        )
        .into()),
        // Nothing will call a model; an endpoint that cannot be reached
        // makes any call that does fail loudly.
        None => Ok(ModelEndpoint {
            base_url: "http://127.0.0.1:9/unconfigured".into(),
            model: "unconfigured".into(),
            api_key_env: None,
        }),
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
    let resolved = settings(state, args)?;
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
    let endpoint = endpoint(&resolved, needs_model)?;
    // Where model calls go, when the person running this did not say.
    if let Some(line) = resolved.announcement() {
        atomic_core::log_line!("{line}");
    }
    let store = LocalStore::for_project(state).context("the artifact store")?;
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
            backend: exec.clone(),
            caches: None,
        },
        lite: LiteRuntime {
            backend: exec.clone(),
            ..LiteRuntime::new(endpoint, AtomConfig::default())
        },
        exec,
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
