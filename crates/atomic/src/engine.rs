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
/// The environment variable that names the schema in it.
pub const DATABASE_SCHEMA: &str = "ATOMIC_DATABASE_SCHEMA";

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

/// How a command that executes runs is set up.
#[derive(Clone, Debug, Default, Args)]
pub struct EngineArgs {
    /// Base URL of the OpenAI-compatible Responses API that model atoms use.
    #[arg(long, env = "ATOMIC_MODEL_BASE_URL")]
    pub model_base_url: Option<String>,
    /// Model id at that endpoint.
    #[arg(long, env = "ATOMIC_MODEL")]
    pub model: Option<String>,
    /// Environment variable that holds the endpoint's API key, if it needs one.
    #[arg(long, env = "ATOMIC_MODEL_API_KEY_ENV")]
    pub model_api_key_env: Option<String>,
    /// Where exec atoms' commands run.
    #[arg(long, env = "ATOMIC_RUNNER", value_enum, default_value_t)]
    pub runner: RunnerKind,
    /// This process's executor identity: unique to it and the same across
    /// its restarts. Required on PostgreSQL.
    #[arg(long, env = "ATOMIC_EXECUTOR_ID")]
    pub executor_id: Option<String>,
}

impl EngineArgs {
    fn endpoint(&self, required: bool) -> anyhow::Result<ModelEndpoint> {
        // A configured endpoint wins; with none, a ChatGPT sign-in serves
        // Codex atoms (see atomic_core::signin).
        // An empty base URL is no endpoint.
        let base_url = self.model_base_url.clone().filter(|u| !u.trim().is_empty());
        // Off Linux there is no sign-in to find; any other failure to read
        // one is said, not taken for "signed out".
        let mut unreadable = None;
        let sign_in = match (&base_url, required) {
            (None, true) => match atomic_core::signin::SignIn::configured() {
                Ok(s) => match s.account() {
                    Ok(Some(_)) => Some(s),
                    Ok(None) => None,
                    Err(e) => {
                        unreadable = Some(e.to_string());
                        None
                    }
                },
                Err(atomic_core::signin::SignInError::Unprotected(_)) => None,
                Err(e) => {
                    unreadable = Some(e.to_string());
                    None
                }
            },
            _ => None,
        };
        match (&base_url, &self.model) {
            (Some(base_url), Some(model)) => Ok(ModelEndpoint {
                base_url: base_url.clone(),
                model: model.clone(),
                api_key_env: self.model_api_key_env.clone(),
                sign_in: None,
            }),
            (None, model) if sign_in.is_some() => {
                let sign_in = sign_in.clone();
                let model = model.clone().ok_or_else(|| {
                    Failure::new(
                        "config.model",
                        "you are signed in with ChatGPT, and no model is configured",
                        "set ATOMIC_MODEL (or pass --model) to a model your plan offers for \
                         Codex",
                    )
                })?;
                Ok(ModelEndpoint {
                    base_url: "https://chatgpt.com/backend-api/codex".into(),
                    model,
                    api_key_env: None,
                    sign_in,
                })
            }
            _ if required => Err(Failure::new(
                "config.model",
                match unreadable {
                    Some(why) => format!(
                        "no model is configured, model atoms need one, and the ChatGPT \
                         sign-in could not be read: {why}"
                    ),
                    None => "no model is configured, and model atoms need one".to_owned(),
                },
                "sign in with `atomic model login` (Codex atoms only), or set ATOMIC_MODEL_BASE_URL \
                 and ATOMIC_MODEL (or pass --model-base-url and --model), and \
                 ATOMIC_MODEL_API_KEY_ENV if the endpoint needs a key",
            )
            .into()),
            // Nothing will call a model; an endpoint that cannot be reached
            // makes any call that does fail loudly.
            _ => Ok(ModelEndpoint {
                base_url: "http://127.0.0.1:9/unconfigured".into(),
                model: "unconfigured".into(),
                api_key_env: None,
                sign_in: None,
            }),
        }
    }
}

/// The store atomic is configured to use.
pub fn database(state: &ProjectState) -> Database {
    match std::env::var(DATABASE_URL).ok().filter(|u| !u.is_empty()) {
        Some(url) => Database::Postgres {
            url,
            schema: std::env::var(DATABASE_SCHEMA)
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "atomic".into()),
        },
        None => Database::Sqlite(state.database()),
    }
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
    let database = database(state);
    let executor = atomic_durable::executor_identity(args.executor_id.as_deref())?;
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
    let endpoint = args.endpoint(needs_model)?;
    let signed_in = endpoint.sign_in.is_some();
    let store = LocalStore::open(state.dir().join("artifacts")).context("the artifact store")?;
    let exec = match args.runner {
        RunnerKind::Local => atomic_exec::Backend::default(),
        RunnerKind::Process => atomic_exec::Backend::new(ProcessRunner::atomic(
            std::env::current_exe().context("find atomic's own executable for its runners")?,
        )),
    };
    let runtimes = Arc::new(Runtimes {
        codex: CodexRuntime {
            paths: paths.clone(),
            codex_home: state.codex_home(),
            cwd: state.root().to_owned(),
            endpoint: endpoint.clone(),
            defaults: AtomConfig::default(),
            artifacts: Artifacts::new(store.clone()),
            meter: atomic_core::usage::Metering::none(),
            // Atoms' commands never reach atomic's private state.
            guard: atomic_core::codex::StateGuard::for_project(state)?,
        },
        lite: LiteRuntime::new(endpoint, AtomConfig::default()),
        exec,
        clock: None,
    });
    // A project in a git repository gets worktrees for its worktree steps.
    let worktrees = Worktrees::open(state).ok().map(Arc::new);
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
            // The CLI reads no configuration files yet, so every run is
            // metered under the built-in budget defaults.
            budgets: Arc::default(),
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
