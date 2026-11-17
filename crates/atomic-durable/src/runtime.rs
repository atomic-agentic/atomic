// SPDX-License-Identifier: Apache-2.0

//! The [`DurableRuntime`] trait, atomic's only way to the durable-execution
//! engine, and [`Durare`], its adapter over the durare fork.
//!
//! Workflow code sees a [`WorkflowContext`]: durable steps, transaction steps
//! that commit their writes with their own checkpoint, and the workflow's id.
//! Nothing here exposes the engine's own types, so the engine can be replaced
//! behind the trait without touching a workflow.
//!
//! The rules the engine enforces hold here too: steps never nest, a step's
//! output is recorded once and returned on replay, and a transaction step's
//! writes commit exactly once, together with its checkpoint.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// A failure, as workflow code and its callers see it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DurableError {
    /// The workflow or step failed: an outcome, recorded as its result.
    #[error("{0}")]
    Failed(String),
    /// This execution must stop without recording anything, for example
    /// because it no longer owns its run; recovery runs it again.
    #[error("interrupted: {0}")]
    Interrupted(String),
    /// A step was started inside another step's body. Composition happens in
    /// workflow code only.
    #[error("a step cannot run inside another step: {0}")]
    Nested(String),
    /// The engine or its store failed.
    #[error("durable engine: {0}")]
    Engine(String),
}

/// A result in workflow code.
pub type Result<T> = std::result::Result<T, DurableError>;

/// What a step body is told about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepInfo {
    /// The workflow it belongs to.
    pub workflow_id: String,
    /// Its position in the workflow, stable across replays.
    pub step_id: i32,
}

/// One recorded operation of a workflow, in order.
#[derive(Clone, Debug, PartialEq)]
pub struct StepRecord {
    /// Its position.
    pub step_id: i32,
    /// Its name.
    pub name: String,
    /// What it returned, if it finished.
    pub output: Option<Value>,
    /// Its error, if it failed.
    pub error: Option<String>,
}

/// Which model thread a published step ran on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadRecord {
    /// The run (workflow id).
    pub run: String,
    /// The step that published the result.
    pub step: String,
    /// The candidate, `<run>:<position of its turn>`.
    pub candidate: String,
    /// The model thread its turn ran on.
    pub thread_id: String,
}

/// atomic's index of threads by run and step, written with each publication.
pub(crate) const THREADS_TABLE: &str = "CREATE TABLE IF NOT EXISTS atomic_threads (
    candidate TEXT PRIMARY KEY,
    run TEXT NOT NULL,
    step TEXT NOT NULL,
    thread_id TEXT NOT NULL
)";

/// Where a workflow is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunStatus {
    /// Started and not finished, including an execution that was interrupted
    /// or crashed and awaits recovery.
    Pending,
    /// Finished with an output.
    Succeeded,
    /// Finished with an error.
    Failed(String),
    /// Cancelled, or any other final state the engine reports.
    Other(String),
}

/// A value bound to a `?` placeholder in a transaction step's SQL.
#[derive(Clone, Debug, PartialEq)]
pub enum Param {
    /// SQL `NULL`.
    Null,
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// Text.
    Text(String),
    /// A boolean.
    Bool(bool),
    /// Bytes.
    Bytes(Vec<u8>),
}

impl From<Param> for durare::Param {
    fn from(p: Param) -> Self {
        match p {
            Param::Null => durare::Param::Null,
            Param::Int(v) => durare::Param::Int(v),
            Param::Float(v) => durare::Param::Float(v),
            Param::Text(v) => durare::Param::Text(v),
            Param::Bool(v) => durare::Param::Bool(v),
            Param::Bytes(v) => durare::Param::Bytes(v),
        }
    }
}

fn params(ps: &[Param]) -> Vec<durare::Param> {
    ps.iter().cloned().map(Into::into).collect()
}

/// A row a transaction step's query returned.
pub struct Row(durare::Row);

impl Row {
    /// An integer column.
    pub fn int(&self, column: &str) -> Result<i64> {
        self.0
            .try_get::<i64>(column)
            .or_else(|_| self.0.try_get::<i32>(column).map(i64::from))
            .map_err(engine)
    }

    /// A text column.
    pub fn text(&self, column: &str) -> Result<String> {
        self.0.try_get::<String>(column).map_err(engine)
    }

    /// A text column that may be `NULL`.
    pub fn text_opt(&self, column: &str) -> Result<Option<String>> {
        self.0.try_get::<Option<String>>(column).map_err(engine)
    }
}

/// The open transaction of a transaction step. Its writes commit together
/// with the step's checkpoint, or not at all. SQL uses `?` placeholders on
/// every backend.
pub struct Tx<'t, 'c>(&'t mut durare::Tx<'c>);

impl Tx<'_, '_> {
    /// Run a statement; returns the rows it affected.
    pub async fn execute(&mut self, sql: &str, ps: &[Param]) -> Result<u64> {
        self.0.execute(sql, &params(ps)).await.map_err(engine)
    }

    /// Run a query that returns at most one row.
    pub async fn query_opt(&mut self, sql: &str, ps: &[Param]) -> Result<Option<Row>> {
        Ok(self
            .0
            .query_opt(sql, &params(ps))
            .await
            .map_err(engine)?
            .map(Row))
    }

    /// Run a query that returns exactly one row.
    pub async fn query_one(&mut self, sql: &str, ps: &[Param]) -> Result<Row> {
        self.0
            .query_one(sql, &params(ps))
            .await
            .map(Row)
            .map_err(engine)
    }

    /// Run a query and return every row.
    pub async fn query_all(&mut self, sql: &str, ps: &[Param]) -> Result<Vec<Row>> {
        Ok(self
            .0
            .query_all(sql, &params(ps))
            .await
            .map_err(engine)?
            .into_iter()
            .map(Row)
            .collect())
    }
}

/// A boxed future borrowing a transaction for `'t`.
pub type TxFuture<'t, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 't>>;

/// What a running workflow can do.
pub trait WorkflowContext: Clone + Send + Sync + 'static {
    /// The workflow's id.
    fn workflow_id(&self) -> &str;

    /// Run `body` as a durable step: its output is recorded, and on replay
    /// it is returned without running `body` again. The body runs at least
    /// once: a crash after it returns and before its checkpoint commits runs
    /// it again.
    fn step<T, F, Fut>(&self, name: &str, body: F) -> impl Future<Output = Result<T>> + Send
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: FnOnce(StepInfo) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static;

    /// Run `body` in one database transaction that also commits the step's
    /// checkpoint, so its writes happen exactly once. `body` may run more
    /// than once if the transaction must be retried, and should be short: no
    /// model turn ever runs inside it.
    fn transaction<T, F>(&self, name: &str, body: F) -> impl Future<Output = Result<T>> + Send
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: for<'t, 'c> Fn(Tx<'t, 'c>) -> TxFuture<'t, T> + Send + Sync + 'static;
}

/// A durable-execution engine: atomic's only way to one.
pub trait DurableRuntime: Send + Sync + Sized {
    /// The context workflow code receives.
    type Context: WorkflowContext;

    /// Register workflow `name`. Registration happens before
    /// [`launch`](Self::launch), in the same way in every process that may
    /// recover the workflow.
    fn register<I, O, F, Fut>(&mut self, name: &str, workflow: F)
    where
        I: DeserializeOwned + Send + 'static,
        O: Serialize + Send + 'static,
        F: Fn(Self::Context, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O>> + Send + 'static;

    /// Start the engine's background work. Workflows can start after this.
    fn launch(&self) -> impl Future<Output = Result<()>> + Send;

    /// Start workflow `name` with id `id` on `input`. A second start with
    /// the same id does not run it twice.
    fn start<I>(&self, name: &str, id: &str, input: I) -> impl Future<Output = Result<()>> + Send
    where
        I: Serialize + Send;

    /// Wait for workflow `id` to finish and return its output.
    fn result<O>(&self, id: &str) -> impl Future<Output = Result<O>> + Send
    where
        O: DeserializeOwned + Send;

    /// Where workflow `id` is, or `None` if no such workflow exists.
    fn status(&self, id: &str) -> impl Future<Output = Result<Option<RunStatus>>> + Send;

    /// The operations workflow `id` has recorded, in order.
    fn steps(&self, id: &str) -> impl Future<Output = Result<Vec<StepRecord>>> + Send;

    /// The threads a run's published steps ran on, in publication order.
    fn threads(&self, run: &str) -> impl Future<Output = Result<Vec<ThreadRecord>>> + Send;

    /// The run and step a thread belongs to, if a published step ran on it.
    fn thread(&self, thread_id: &str) -> impl Future<Output = Result<Option<ThreadRecord>>> + Send;

    /// Run again every unfinished workflow this process may recover.
    /// Returns how many it started.
    fn recover(&self) -> impl Future<Output = Result<usize>> + Send;

    /// Stop, giving running work up to `timeout` to finish.
    fn shutdown(&self, timeout: Duration) -> impl Future<Output = Result<()>> + Send;
}

// ---------------------------------------------------------------------------
// The durare adapter.

fn db(e: impl std::fmt::Display) -> DurableError {
    DurableError::Engine(format!("atomic's tables: {e}"))
}

fn engine(e: durare::Error) -> DurableError {
    from_durare(e)
}

/// How a durare error reads in atomic's terms.
pub(crate) fn from_durare(e: durare::Error) -> DurableError {
    match e {
        durare::Error::RecoveryRequired(inner) => DurableError::Interrupted(inner.to_string()),
        durare::Error::App { message, .. } => DurableError::Failed(message),
        e @ durare::Error::NestedDurableCall { .. } => DurableError::Nested(e.to_string()),
        e => DurableError::Engine(e.to_string()),
    }
}

/// How an atomic error reads to durare, so it records the right outcome.
pub(crate) fn to_durare(e: DurableError) -> durare::Error {
    match e {
        DurableError::Interrupted(m) => {
            durare::Error::RecoveryRequired(Arc::new(durare::Error::app(m)))
        }
        DurableError::Failed(m) | DurableError::Nested(m) | DurableError::Engine(m) => {
            durare::Error::app(m)
        }
    }
}

/// [`WorkflowContext`] on durare.
#[derive(Clone)]
pub struct DurareContext(durare::DurableContext);

impl WorkflowContext for DurareContext {
    fn workflow_id(&self) -> &str {
        self.0.workflow_id()
    }

    fn step<T, F, Fut>(&self, name: &str, body: F) -> impl Future<Output = Result<T>> + Send
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: FnOnce(StepInfo) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let ctx = self.0.clone();
        let name = name.to_owned();
        async move {
            ctx.step(&name, move |s: durare::StepCtx| async move {
                let info = StepInfo {
                    workflow_id: s.workflow_id().to_owned(),
                    step_id: s.step_id(),
                };
                body(info).await.map_err(to_durare)
            })
            .await
            .map_err(from_durare)
        }
    }

    fn transaction<T, F>(&self, name: &str, body: F) -> impl Future<Output = Result<T>> + Send
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: for<'t, 'c> Fn(Tx<'t, 'c>) -> TxFuture<'t, T> + Send + Sync + 'static,
    {
        let ctx = self.0.clone();
        let name = name.to_owned();
        async move {
            ctx.transaction(&name, move |tx| {
                let fut = body(Tx(tx));
                Box::pin(async move { fut.await.map_err(to_durare) })
            })
            .await
            .map_err(from_durare)
        }
    }
}

/// The adapter's own connection for reading atomic's tables outside a
/// workflow: the same database, and on PostgreSQL the same schema.
enum Reads {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

/// [`DurableRuntime`] on the durare fork, over SQLite or PostgreSQL.
pub struct Durare {
    engine: durare::DurableEngine,
    reads: Reads,
    /// Handles of workflows this process started, by id: waiting on one
    /// follows that execution, including a crash, which a lookup by id would
    /// not see.
    started: std::sync::Mutex<std::collections::HashMap<String, durare::WorkflowHandle<Value>>>,
}

impl Durare {
    /// An engine on the SQLite database at `path`, created if missing.
    pub async fn sqlite(path: &Path) -> Result<Self> {
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let provider = durare::SqliteProvider::connect(&url)
            .await
            .map_err(from_durare)?;
        let reads = sqlx::SqlitePool::connect(&url).await.map_err(db)?;
        Self::over(Arc::new(provider), Reads::Sqlite(reads)).await
    }

    /// An engine on PostgreSQL at `url`, in `schema`.
    pub async fn postgres(url: &str, schema: &str) -> Result<Self> {
        let provider = durare::PostgresProvider::connect_with_schema(url, schema)
            .await
            .map_err(from_durare)?;
        let options = <sqlx::postgres::PgConnectOptions as std::str::FromStr>::from_str(url)
            .map_err(db)?
            .options([("search_path", schema)]);
        let reads = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .map_err(db)?;
        Self::over(Arc::new(provider), Reads::Postgres(reads)).await
    }

    async fn over(provider: Arc<dyn durare::StateProvider>, reads: Reads) -> Result<Self> {
        let engine = durare::DurableEngine::new(provider)
            .await
            .map_err(from_durare)?;
        let index =
            "CREATE INDEX IF NOT EXISTS atomic_threads_by_thread ON atomic_threads (thread_id)";
        match &reads {
            Reads::Sqlite(pool) => {
                for sql in [THREADS_TABLE, index] {
                    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                        .execute(pool)
                        .await
                        .map_err(db)?;
                }
            }
            Reads::Postgres(pool) => {
                for sql in [THREADS_TABLE, index] {
                    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                        .execute(pool)
                        .await
                        .map_err(db)?;
                }
            }
        }
        Ok(Self {
            engine,
            reads,
            started: std::sync::Mutex::default(),
        })
    }

    /// Thread records matching `column = value`, in publication order.
    async fn thread_rows(&self, column: &str, value: &str) -> Result<Vec<ThreadRecord>> {
        let rows: Vec<(String, String, String, String)> = match &self.reads {
            Reads::Sqlite(pool) => {
                let sql = format!(
                    "SELECT run, step, candidate, thread_id FROM atomic_threads WHERE {column} = ?"
                );
                sqlx::query_as(sqlx::AssertSqlSafe(sql))
                    .bind(value)
                    .fetch_all(pool)
                    .await
                    .map_err(db)?
            }
            Reads::Postgres(pool) => {
                let sql = format!(
                    "SELECT run, step, candidate, thread_id FROM atomic_threads WHERE {column} = $1"
                );
                sqlx::query_as(sqlx::AssertSqlSafe(sql))
                    .bind(value)
                    .fetch_all(pool)
                    .await
                    .map_err(db)?
            }
        };
        let mut records: Vec<ThreadRecord> = rows
            .into_iter()
            .map(|(run, step, candidate, thread_id)| ThreadRecord {
                run,
                step,
                candidate,
                thread_id,
            })
            .collect();
        // Publication order is the order of the turns' positions.
        let position = |r: &ThreadRecord| {
            r.candidate
                .rsplit_once(':')
                .and_then(|(_, p)| p.parse::<i64>().ok())
                .unwrap_or(i64::MAX)
        };
        records.sort_by_key(|r| (r.run.clone(), position(r)));
        Ok(records)
    }
}

impl DurableRuntime for Durare {
    type Context = DurareContext;

    fn register<I, O, F, Fut>(&mut self, name: &str, workflow: F)
    where
        I: DeserializeOwned + Send + 'static,
        O: Serialize + Send + 'static,
        F: Fn(Self::Context, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O>> + Send + 'static,
    {
        self.engine
            .register(name, move |ctx: durare::DurableContext, input: I| {
                let run = workflow(DurareContext(ctx), input);
                async move { run.await.map_err(to_durare) }
            });
    }

    async fn launch(&self) -> Result<()> {
        self.engine.launch().await.map_err(from_durare)
    }

    fn start<I>(&self, name: &str, id: &str, input: I) -> impl Future<Output = Result<()>> + Send
    where
        I: Serialize + Send,
    {
        let (name, id) = (name.to_owned(), id.to_owned());
        async move {
            let handle = self
                .engine
                .start::<_, Value>(&name, input, durare::WorkflowOptions::with_id(&id))
                .await
                .map_err(from_durare)?;
            if let Ok(mut started) = self.started.lock() {
                started.insert(id, handle);
            }
            Ok(())
        }
    }

    fn result<O>(&self, id: &str) -> impl Future<Output = Result<O>> + Send
    where
        O: DeserializeOwned + Send,
    {
        let id = id.to_owned();
        async move {
            let local = self.started.lock().ok().and_then(|mut s| s.remove(&id));
            let value = match local {
                Some(handle) => handle.result().await,
                None => match self.engine.retrieve_workflow::<Value>(&id).await {
                    Ok(handle) => handle.result().await,
                    Err(e) => Err(e),
                },
            }
            .map_err(from_durare)?;
            serde_json::from_value(value)
                .map_err(|e| DurableError::Engine(format!("decode the output of {id}: {e}")))
        }
    }

    fn status(&self, id: &str) -> impl Future<Output = Result<Option<RunStatus>>> + Send {
        let id = id.to_owned();
        async move {
            let found = self
                .engine
                .list_workflows(&durare::ListFilter {
                    workflow_ids: vec![id],
                    ..durare::ListFilter::default()
                })
                .await
                .map_err(from_durare)?;
            Ok(found.into_iter().next().map(|w| match w.status.as_str() {
                "PENDING" | "ENQUEUED" => RunStatus::Pending,
                "SUCCESS" => RunStatus::Succeeded,
                "ERROR" => RunStatus::Failed(w.error.unwrap_or_default()),
                other => RunStatus::Other(other.to_owned()),
            }))
        }
    }

    fn steps(&self, id: &str) -> impl Future<Output = Result<Vec<StepRecord>>> + Send {
        let id = id.to_owned();
        async move {
            Ok(self
                .engine
                .get_workflow_steps(&id)
                .await
                .map_err(from_durare)?
                .into_iter()
                .map(|s| StepRecord {
                    step_id: s.step_id,
                    name: s.name,
                    output: s.output,
                    error: s.error,
                })
                .collect())
        }
    }

    async fn threads(&self, run: &str) -> Result<Vec<ThreadRecord>> {
        self.thread_rows("run", run).await
    }

    async fn thread(&self, thread_id: &str) -> Result<Option<ThreadRecord>> {
        Ok(self
            .thread_rows("thread_id", thread_id)
            .await?
            .into_iter()
            .next())
    }

    async fn recover(&self) -> Result<usize> {
        self.engine.recover().await.map_err(from_durare)
    }

    async fn shutdown(&self, timeout: Duration) -> Result<()> {
        self.engine.shutdown(timeout).await.map_err(from_durare)
    }
}

#[cfg(test)]
mod tests;
