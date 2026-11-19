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
use atomic_core::state::{ExecutionLock, ProjectState};
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
        match (&self.model_base_url, &self.model) {
            (Some(base_url), Some(model)) => Ok(ModelEndpoint {
                base_url: base_url.clone(),
                model: model.clone(),
                api_key_env: self.model_api_key_env.clone(),
            }),
            _ if required => Err(Failure::new(
                "config.model",
                "no model endpoint is configured, and model atoms need one",
                "set ATOMIC_MODEL_BASE_URL and ATOMIC_MODEL (or pass --model-base-url and \
                 --model), and ATOMIC_MODEL_API_KEY_ENV if the endpoint needs a key",
            )
            .into()),
            // Nothing will call a model; an endpoint that cannot be reached
            // makes any call that does fail loudly.
            _ => Ok(ModelEndpoint {
                base_url: "http://127.0.0.1:9/unconfigured".into(),
                model: "unconfigured".into(),
                api_key_env: None,
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
pub fn deployment() -> Deployment {
    let user = std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "operator".into());
    Deployment::Local {
        operator: Principal::local(&user),
    }
}

/// A control plane for one project, holding its execution lock.
pub struct Engine {
    /// The durable runtime, launched, with `run_pipeline` registered.
    pub rt: Durare,
    /// Where its store is, for messages.
    pub store: String,
    _lock: ExecutionLock,
}

/// Take the project's execution lock for `command`, open its store and
/// launch `run_pipeline` on it.
pub async fn open(
    state: &ProjectState,
    paths: &CodexPaths,
    args: &EngineArgs,
    command: &str,
    needs_model: bool,
) -> anyhow::Result<Engine> {
    state.create()?;
    let lock = state.lock_execution(command)?;
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
    rt.launch().await.context("launch the durable runtime")?;
    Ok(Engine {
        rt,
        store: describe(&database),
        _lock: lock,
    })
}
