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

use crate::usage::Ledger;
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
    /// The database failed. The engine's own error travels with it, so a
    /// failure the engine knows how to handle, such as a serialization
    /// conflict it retries, is still handled when it passes back through
    /// workflow code.
    #[error("{message}")]
    Database {
        /// What failed.
        message: String,
        /// The engine's error, as it reported it.
        origin: Origin,
    },
    /// The engine or its store failed.
    #[error("durable engine: {0}")]
    Engine(String),
    /// The run cannot go on until a person acts: a budget cap left no room
    /// for the next model call. An atom's author returns it, and the atom's
    /// turn parks the run instead of recording a failure (see
    /// [`crate::publication::execute_candidate`]).
    #[error("parked: {0}")]
    Parked(String),
    /// Another execution of run `.0` recorded one of its steps first. This
    /// execution's state is behind the run's, so it stops, records nothing,
    /// and takes the outcome the winner records. The workflow context keeps
    /// the loss when the step reports it, so the execution's outcome is the
    /// conflict whatever workflow code makes of it; a conflict that no step
    /// reported is not a lost race, and fails the run.
    #[error("another execution of run {0} recorded its step first")]
    Conflict(String),
}

/// The engine's own error behind a [`DurableError::Database`]: opaque, and
/// ignored by equality, which compares the message.
#[derive(Clone)]
pub struct Origin(Option<Arc<std::sync::Mutex<Option<durare::Error>>>>);

impl Origin {
    fn of(e: durare::Error) -> Self {
        Self(Some(Arc::new(std::sync::Mutex::new(Some(e)))))
    }

    /// The engine's error, once: the first conversion back takes it.
    fn take(&self) -> Option<durare::Error> {
        self.0.as_ref()?.lock().ok()?.take()
    }
}

impl std::fmt::Debug for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Origin(..)")
    }
}

impl PartialEq for Origin {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Origin {}

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

/// What the store records about a run beside its steps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRecord {
    /// Its id.
    pub id: String,
    /// Where it is.
    pub status: RunStatus,
    /// The executor identity that last owned it.
    pub executor: String,
    /// The version it was stamped with, which only an engine of the same
    /// version recovers.
    pub version: String,
    /// The queue it waits on, or was last taken from, if any.
    pub queue: Option<String>,
}

/// Where a run stands in [`Durare::runs_after`]'s order: when it was
/// created, in milliseconds since the epoch, and its id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunKey {
    /// When the run was created.
    pub created_at: i64,
    /// Its id.
    pub id: String,
}

/// A run with what it was given and what it returned or failed with, as
/// [`Durare::runs_after`] reads it.
#[derive(Clone, Debug)]
pub struct RunContents {
    /// The run.
    pub record: RunRecord,
    /// Its input, as stored.
    pub input: Value,
    /// What it returned, once it succeeded.
    pub output: Option<Value>,
    /// Its error, once it failed.
    pub error: Option<String>,
}

/// What every resume queue's name starts with.
pub const RESUME_QUEUES: &str = "atomic.resume.";

impl RunRecord {
    /// The executor that owns the run: the one that last claimed it, or,
    /// while it waits on a resume queue unclaimed, the one the queue is for.
    pub fn owner(&self) -> Option<&str> {
        if !self.executor.is_empty() {
            return Some(&self.executor);
        }
        self.queue
            .as_deref()?
            .strip_prefix(RESUME_QUEUES)?
            .split_once('/')
            .map(|(executor, _)| executor)
    }
}

impl From<durare::WorkflowStatus> for RunRecord {
    fn from(w: durare::WorkflowStatus) -> Self {
        Self {
            id: w.id,
            status: run_status(&w.status, w.error),
            executor: w.executor_id,
            version: w.app_version,
            queue: w.queue_name,
        }
    }
}

fn run_status(status: &str, error: Option<String>) -> RunStatus {
    match status {
        "PENDING" | "ENQUEUED" => RunStatus::Pending,
        "SUCCESS" => RunStatus::Succeeded,
        "ERROR" => RunStatus::Failed(error.unwrap_or_default()),
        other => RunStatus::Other(other.to_owned()),
    }
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

/// A row a transaction's query returned.
pub struct Row(RowInner);

enum RowInner {
    Engine(durare::Row),
    Sqlite(sqlx::sqlite::SqliteRow),
    Postgres(sqlx::postgres::PgRow),
}

impl Row {
    /// An integer column.
    pub fn int(&self, column: &str) -> Result<i64> {
        self.int_opt(column)?
            .ok_or_else(|| DurableError::Engine(format!("column {column} is NULL")))
    }

    /// An integer column that may be `NULL`.
    pub fn int_opt(&self, column: &str) -> Result<Option<i64>> {
        use sqlx::Row as _;
        match &self.0 {
            RowInner::Engine(r) => r
                .try_get::<Option<i64>>(column)
                .or_else(|_| r.try_get::<Option<i32>>(column).map(|v| v.map(i64::from)))
                .map_err(engine),
            RowInner::Sqlite(r) => r.try_get::<Option<i64>, _>(column).map_err(db),
            RowInner::Postgres(r) => r
                .try_get::<Option<i64>, _>(column)
                .or_else(|_| {
                    r.try_get::<Option<i32>, _>(column)
                        .map(|v| v.map(i64::from))
                })
                .map_err(db),
        }
    }

    /// A text column.
    pub fn text(&self, column: &str) -> Result<String> {
        self.text_opt(column)?
            .ok_or_else(|| DurableError::Engine(format!("column {column} is NULL")))
    }

    /// A text column that may be `NULL`.
    pub fn text_opt(&self, column: &str) -> Result<Option<String>> {
        use sqlx::Row as _;
        match &self.0 {
            RowInner::Engine(r) => r.try_get::<Option<String>>(column).map_err(engine),
            RowInner::Sqlite(r) => r.try_get::<Option<String>, _>(column).map_err(db),
            RowInner::Postgres(r) => r.try_get::<Option<String>, _>(column).map_err(db),
        }
    }
}

/// An open transaction: a transaction step's, whose writes commit together
/// with the step's checkpoint, or a short control transaction outside any
/// workflow (see [`DurableRuntime::control_transaction`]). Either way its writes
/// commit together or not at all. SQL uses `?` placeholders on every backend.
pub struct Tx<'t, 'c>(TxInner<'t, 'c>);

enum TxInner<'t, 'c> {
    Engine(&'t mut durare::Tx<'c>),
    Sqlite(&'t mut sqlx::SqliteConnection),
    Postgres(&'t mut sqlx::PgConnection),
}

/// `sql` with each `?` outside a quoted string turned into `$1`, `$2`, ...
fn to_pg(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let (mut n, mut quoted) = (0, false);
    for c in sql.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                out.push(c);
            }
            '?' if !quoted => {
                n += 1;
                out.push('$');
                out.push_str(&n.to_string());
            }
            _ => out.push(c),
        }
    }
    out
}

type SqliteQuery<'q> = sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>;
type PgQuery<'q> = sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>;

fn bind_sqlite<'q>(mut q: SqliteQuery<'q>, ps: &[Param]) -> SqliteQuery<'q> {
    for p in ps {
        q = match p.clone() {
            Param::Null => q.bind(None::<String>),
            Param::Int(v) => q.bind(v),
            Param::Float(v) => q.bind(v),
            Param::Text(v) => q.bind(v),
            Param::Bool(v) => q.bind(v),
            Param::Bytes(v) => q.bind(v),
        };
    }
    q
}

fn bind_pg<'q>(mut q: PgQuery<'q>, ps: &[Param]) -> PgQuery<'q> {
    for p in ps {
        q = match p.clone() {
            Param::Null => q.bind(None::<String>),
            Param::Int(v) => q.bind(v),
            Param::Float(v) => q.bind(v),
            Param::Text(v) => q.bind(v),
            Param::Bool(v) => q.bind(v),
            Param::Bytes(v) => q.bind(v),
        };
    }
    q
}

impl Tx<'_, '_> {
    /// Run a statement; returns the rows it affected.
    pub async fn execute(&mut self, sql: &str, ps: &[Param]) -> Result<u64> {
        match &mut self.0 {
            TxInner::Engine(tx) => tx.execute(sql, &params(ps)).await.map_err(engine),
            TxInner::Sqlite(c) => {
                let q = bind_sqlite(sqlx::query(sqlx::AssertSqlSafe(sql.to_owned())), ps);
                Ok(q.execute(&mut **c).await.map_err(db)?.rows_affected())
            }
            TxInner::Postgres(c) => {
                let q = bind_pg(sqlx::query(sqlx::AssertSqlSafe(to_pg(sql))), ps);
                Ok(q.execute(&mut **c).await.map_err(db)?.rows_affected())
            }
        }
    }

    /// Run a query that returns at most one row.
    pub async fn query_opt(&mut self, sql: &str, ps: &[Param]) -> Result<Option<Row>> {
        Ok(match &mut self.0 {
            TxInner::Engine(tx) => tx
                .query_opt(sql, &params(ps))
                .await
                .map_err(engine)?
                .map(|r| Row(RowInner::Engine(r))),
            TxInner::Sqlite(c) => {
                let q = bind_sqlite(sqlx::query(sqlx::AssertSqlSafe(sql.to_owned())), ps);
                q.fetch_optional(&mut **c)
                    .await
                    .map_err(db)?
                    .map(|r| Row(RowInner::Sqlite(r)))
            }
            TxInner::Postgres(c) => {
                let q = bind_pg(sqlx::query(sqlx::AssertSqlSafe(to_pg(sql))), ps);
                q.fetch_optional(&mut **c)
                    .await
                    .map_err(db)?
                    .map(|r| Row(RowInner::Postgres(r)))
            }
        })
    }

    /// Run a query that returns exactly one row.
    pub async fn query_one(&mut self, sql: &str, ps: &[Param]) -> Result<Row> {
        self.query_opt(sql, ps)
            .await?
            .ok_or_else(|| DurableError::Engine("query_one: no rows returned".into()))
    }

    /// Run a query and return every row.
    pub async fn query_all(&mut self, sql: &str, ps: &[Param]) -> Result<Vec<Row>> {
        Ok(match &mut self.0 {
            TxInner::Engine(tx) => tx
                .query_all(sql, &params(ps))
                .await
                .map_err(engine)?
                .into_iter()
                .map(|r| Row(RowInner::Engine(r)))
                .collect(),
            TxInner::Sqlite(c) => {
                let q = bind_sqlite(sqlx::query(sqlx::AssertSqlSafe(sql.to_owned())), ps);
                q.fetch_all(&mut **c)
                    .await
                    .map_err(db)?
                    .into_iter()
                    .map(|r| Row(RowInner::Sqlite(r)))
                    .collect()
            }
            TxInner::Postgres(c) => {
                let q = bind_pg(sqlx::query(sqlx::AssertSqlSafe(to_pg(sql))), ps);
                q.fetch_all(&mut **c)
                    .await
                    .map_err(db)?
                    .into_iter()
                    .map(|r| Row(RowInner::Postgres(r)))
                    .collect()
            }
        })
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

    /// Read or update authoritative control records in a short transaction
    /// without recording a workflow checkpoint. Gate authorization must run
    /// again on replay: imported step outputs are not authority to proceed.
    /// Do not use this for ordinary workflow inputs or model execution.
    fn control_transaction<T, F>(&self, body: F) -> impl Future<Output = Result<T>> + Send
    where
        T: Send + 'static,
        F: for<'t, 'c> Fn(Tx<'t, 'c>) -> TxFuture<'t, T> + Send + Sync + 'static;

    /// Wait up to `timeout` for the oldest message sent to this workflow on
    /// `topic` (see [`DurableRuntime::send`]) and take it. The message taken,
    /// or `None` for a timeout, is recorded as this operation's output, so a
    /// replay returns it without taking another. The deadline is durable: it
    /// is fixed the first time the wait finds no message, and a wait resumed
    /// after a crash waits only for what remains of it, however long the
    /// process was down.
    fn recv<T>(
        &self,
        topic: &str,
        timeout: Duration,
    ) -> impl Future<Output = Result<Option<T>>> + Send
    where
        T: DeserializeOwned + Send + 'static;
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

    /// Run again every unfinished workflow this process may recover: those
    /// owned by its executor identity. Returns how many it started.
    fn recover(&self) -> impl Future<Output = Result<usize>> + Send;

    /// Run unfinished workflow `id` again from its checkpoints, whichever
    /// executor last owned it, so [`result`](Self::result) can follow it.
    /// Its previous execution must have stopped. A finished workflow is left
    /// as it is. On [`Durare`], only this executor's engines that take
    /// resumed runs run it.
    fn resume(&self, id: &str) -> impl Future<Output = Result<()>> + Send;

    /// Send `message` to workflow `run` on `topic`, from outside any
    /// workflow, for its [`recv`](WorkflowContext::recv). Delivered at most
    /// once per `key`: a second send with the same key and destination does
    /// nothing, so a caller that is unsure whether a send arrived can send
    /// again. A message whose JSON is `null` reads as a timeout, so send
    /// something else.
    fn send<T>(
        &self,
        run: &str,
        topic: &str,
        message: T,
        key: &str,
    ) -> impl Future<Output = Result<()>> + Send
    where
        T: Serialize + Send;

    /// Run `body` in one short transaction on atomic's tables, outside any
    /// workflow, and commit it: for control records, such as a gate decision,
    /// that a person's command writes. Its writes commit together or not at
    /// all; an error rolls them back. The body runs once and should be short.
    fn control_transaction<T, F>(&self, body: F) -> impl Future<Output = Result<T>> + Send
    where
        T: Send + 'static,
        F: for<'t, 'c> Fn(Tx<'t, 'c>) -> TxFuture<'t, T> + Send + Sync + 'static;

    /// The usage ledger in this runtime's database (see [`crate::usage`]):
    /// budget accounting, apart from step outputs and telemetry.
    fn ledger(&self) -> Ledger;

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
        durare::Error::WorkflowConflict(run) => DurableError::Conflict(run),
        e @ durare::Error::Db(_) => DurableError::Database {
            message: e.to_string(),
            origin: Origin::of(e),
        },
        e => DurableError::Engine(e.to_string()),
    }
}

/// How an atomic error reads to durare, so it records the right outcome.
pub(crate) fn to_durare(e: DurableError) -> durare::Error {
    match e {
        DurableError::Interrupted(m) => {
            durare::Error::RecoveryRequired(Arc::new(durare::Error::app(m)))
        }
        DurableError::Database { message, origin } => {
            origin.take().unwrap_or_else(|| durare::Error::app(message))
        }
        // A lost race reaches the engine through the context's latch (see
        // `Lost`), never through here: a conflict that no step reported was
        // made up, and fails like any other error rather than leave the run
        // waiting for a winner that does not exist.
        DurableError::Conflict(run) => durare::Error::app(format!(
            "workflow code returned a workflow conflict (run {run}) that no step reported"
        )),
        DurableError::Failed(m)
        | DurableError::Nested(m)
        | DurableError::Engine(m)
        | DurableError::Parked(m) => durare::Error::app(m),
    }
}

/// How an error a step's or transaction's body returned reads to durare.
/// A conflict belongs to the workflow, which lost a race to record a step:
/// a body that returns one has made it up, or passed on another run's, and
/// it fails the step like any other error rather than reach the engine as a
/// typed conflict that it would record as the step's outcome.
pub(crate) fn body_to_durare(e: DurableError) -> durare::Error {
    match e {
        DurableError::Conflict(run) => durare::Error::app(format!(
            "a step's body returned a workflow conflict (run {run}), which only workflow code \
             may return"
        )),
        e => to_durare(e),
    }
}

/// The failpoints around one durable operation's checkpoint (see
/// [`crate::failpoints`]): `checkpoint:before:<op>` once its body succeeded
/// and before the engine commits its checkpoint, `checkpoint:after:<op>` once
/// the checkpoint is committed. The second is reached only when this process
/// ran the body, not when a replay returned the recorded output.
///
/// Each is reached at most once per operation. The engine may run a
/// transaction's body again within one operation: when the commit of its
/// writes and checkpoint meets a conflict (on SQLite, a snapshot another
/// connection's write made stale), it rolls the attempt back and retries on a
/// fresh transaction. Nothing of the first attempt is kept, so it is not a
/// second run of the operation, and it does not reach the failpoint again; a
/// crash test counting runs would otherwise count it as one. A step's body is
/// called once per operation in any case.
#[derive(Clone)]
struct Checkpoint {
    name: Arc<str>,
    ran: Arc<std::sync::atomic::AtomicBool>,
}

impl Checkpoint {
    /// The pair for operation `name`: one half for the body, one for the
    /// caller.
    fn new(name: &str) -> (Self, Self) {
        let this = Self {
            name: name.into(),
            ran: Arc::default(),
        };
        (this.clone(), this)
    }

    fn before(&self) {
        let again = self.ran.swap(true, std::sync::atomic::Ordering::SeqCst);
        if crate::failpoints::armed() && !again {
            crate::failpoint(&format!("checkpoint:before:{}", self.name));
        }
    }

    fn after(&self) {
        if crate::failpoints::armed() && self.ran.load(std::sync::atomic::Ordering::SeqCst) {
            crate::failpoint(&format!("checkpoint:after:{}", self.name));
        }
    }
}

/// [`WorkflowContext`] on durare.
#[derive(Clone)]
pub struct DurareContext {
    engine: durare::DurableContext,
    reads: Reads,
    /// A workflow conflict any operation of this execution returned: once
    /// the execution has lost a race to record a step, its outcome is the
    /// conflict, whatever its workflow code goes on to return.
    lost: Lost,
}

/// Where an execution keeps the conflict it lost, if it lost one.
#[derive(Clone, Default)]
struct Lost(Arc<std::sync::Mutex<Option<String>>>);

impl Lost {
    /// Keep `out`'s conflict, if it is one, and pass `out` on.
    fn keep<T>(&self, out: Result<T>) -> Result<T> {
        if let Err(DurableError::Conflict(run)) = &out {
            // A poisoned lock still holds the value: a loss is never skipped.
            let mut lost = self.0.lock().unwrap_or_else(|e| e.into_inner());
            lost.get_or_insert_with(|| run.clone());
        }
        out
    }

    /// The conflict this execution lost, if it lost one.
    fn conflict(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl WorkflowContext for DurareContext {
    fn workflow_id(&self) -> &str {
        self.engine.workflow_id()
    }

    fn step<T, F, Fut>(&self, name: &str, body: F) -> impl Future<Output = Result<T>> + Send
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: FnOnce(StepInfo) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        // Called now, not when first polled: durare claims the step's
        // position here, so positions follow construction order whatever
        // order the futures are awaited in.
        let (checkpoint, ran) = Checkpoint::new(name);
        let pending = self
            .engine
            .step(name, move |s: durare::StepCtx| async move {
                let info = StepInfo {
                    workflow_id: s.workflow_id().to_owned(),
                    step_id: s.step_id(),
                };
                let out = body(info).await;
                if out.is_ok() {
                    ran.before();
                }
                out.map_err(body_to_durare)
            });
        let lost = self.lost.clone();
        async move {
            let out = lost.keep(pending.await.map_err(from_durare));
            if out.is_ok() {
                checkpoint.after();
            }
            out
        }
    }

    fn transaction<T, F>(&self, name: &str, body: F) -> impl Future<Output = Result<T>> + Send
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: for<'t, 'c> Fn(Tx<'t, 'c>) -> TxFuture<'t, T> + Send + Sync + 'static,
    {
        // As for a step, the position is claimed now.
        let (checkpoint, ran) = Checkpoint::new(name);
        let pending = self.engine.transaction(name, move |tx| {
            let fut = body(Tx(TxInner::Engine(tx)));
            let ran = ran.clone();
            Box::pin(async move {
                let out = fut.await;
                if out.is_ok() {
                    // Inside the transaction: its writes and its checkpoint
                    // commit together after this, or not at all.
                    ran.before();
                }
                out.map_err(body_to_durare)
            })
        });
        let lost = self.lost.clone();
        async move {
            let out = lost.keep(pending.await.map_err(from_durare));
            if out.is_ok() {
                checkpoint.after();
            }
            out
        }
    }

    async fn control_transaction<T, F>(&self, body: F) -> Result<T>
    where
        T: Send + 'static,
        F: for<'t, 'c> Fn(Tx<'t, 'c>) -> TxFuture<'t, T> + Send + Sync + 'static,
    {
        self.reads.control_transaction(body).await
    }

    fn recv<T>(
        &self,
        topic: &str,
        timeout: Duration,
    ) -> impl Future<Output = Result<Option<T>>> + Send
    where
        T: DeserializeOwned + Send + 'static,
    {
        // As for a step, the position is claimed now.
        let pending = self.engine.recv::<T>(topic, timeout);
        let topic = topic.to_owned();
        let lost = self.lost.clone();
        async move {
            let out = lost.keep(pending.await.map_err(from_durare));
            // The message taken, or the timeout, is recorded. A replay that
            // returns the recorded one reaches this too.
            if out.is_ok() && crate::failpoints::armed() {
                crate::failpoint(&format!("recv:after:{topic}"));
            }
            out
        }
    }
}

/// Connections for atomic's control records, separate from step checkpoints:
/// the same database, and on PostgreSQL the same schema. A workflow uses them
/// only for authorization that must be checked again on replay.
#[derive(Clone)]
pub(crate) enum Reads {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

/// Where atomic's durable store is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Database {
    /// A SQLite file, created if missing.
    Sqlite(std::path::PathBuf),
    /// A PostgreSQL database, in one schema of its own.
    Postgres {
        /// The connection URL.
        url: String,
        /// The schema atomic's tables live in.
        schema: String,
    },
}

/// The environment variable the engine would otherwise read its executor
/// identity from, ahead of anything atomic configures.
pub const ENGINE_IDENTITY_VARIABLE: &str = "DBOS__VMID";

/// The environment variable the engine would otherwise read the version it
/// stamps on runs from, ahead of anything atomic configures.
pub const ENGINE_VERSION_VARIABLE: &str = "DBOS__APPVERSION";

/// The version stamped on runs by an engine whose caller names none, as the
/// plain [`Durare::sqlite`] and [`Durare::postgres`] constructors do. The
/// engine recovers and dequeues only runs of its own version, so a version
/// that followed the executable would strand every unfinished run at each
/// rebuild; atomic always names one.
pub const DEFAULT_VERSION: &str = "atomic-durable.1";

/// Why an executor identity was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// The engine's own variable is set, which would override atomic's
    /// setting without atomic knowing.
    #[error(
        "{variable} is set to {value:?}; atomic takes its executor identity from its own \
         configuration, never from the engine's variable"
    )]
    EngineVariable {
        /// The variable.
        variable: &'static str,
        /// Its value.
        value: String,
    },
    /// The identity is empty, too long or has characters outside
    /// letters, digits, `.`, `_`, `:` and `-`.
    #[error("executor identity {0:?} is not 1 to 128 letters, digits, `.`, `_`, `:` or `-`")]
    Invalid(String),
    /// The engine's version variable is set, which would stamp runs with a
    /// version atomic does not choose, and strand them for any atomic that
    /// does.
    #[error(
        "{variable} is set to {value:?}; atomic stamps runs with the version of its own durable \
         step layout, never with the engine's variable"
    )]
    VersionVariable {
        /// The variable.
        variable: &'static str,
        /// Its value.
        value: String,
    },
}

/// Refuse the engine's own identity and version variables, which would
/// override atomic's settings without atomic knowing.
fn engine_variables() -> std::result::Result<(), IdentityError> {
    let set = |name| std::env::var_os(name).filter(|v| !v.is_empty());
    if let Some(value) = set(ENGINE_IDENTITY_VARIABLE) {
        return Err(IdentityError::EngineVariable {
            variable: ENGINE_IDENTITY_VARIABLE,
            value: value.to_string_lossy().into_owned(),
        });
    }
    if let Some(value) = set(ENGINE_VERSION_VARIABLE) {
        return Err(IdentityError::VersionVariable {
            variable: ENGINE_VERSION_VARIABLE,
            value: value.to_string_lossy().into_owned(),
        });
    }
    Ok(())
}

/// Check the executor identity a process is configured with, and that
/// nothing outside atomic overrides it or the version runs are stamped with.
/// `None` means the store's default owner, `local`, which is safe only while
/// one process uses the store.
pub fn executor_identity(
    configured: Option<&str>,
) -> std::result::Result<Option<String>, IdentityError> {
    engine_variables()?;
    let Some(id) = configured else {
        return Ok(None);
    };
    let fine = (1..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'));
    if fine {
        Ok(Some(id.to_owned()))
    } else {
        Err(IdentityError::Invalid(id.to_owned()))
    }
}

/// Whether a short transaction on atomic's tables commits.
pub(crate) enum End<T> {
    /// Commit its writes and return this.
    Commit(T),
    /// Roll its writes back and return this.
    Rollback(T),
}

impl Reads {
    /// Run `body` in one short transaction on atomic's tables that commits
    /// unless the body fails.
    pub(crate) async fn control_transaction<T, F>(&self, body: F) -> Result<T>
    where
        T: Send + 'static,
        F: for<'t, 'c> Fn(Tx<'t, 'c>) -> TxFuture<'t, T> + Send + Sync + 'static,
    {
        let body = Arc::new(body);
        let out = self
            .transaction(move |tx| {
                let body = body.clone();
                Box::pin(async move { body(tx).await.map(End::Commit) })
            })
            .await?;
        match out {
            End::Commit(v) | End::Rollback(v) => Ok(v),
        }
    }

    /// Run `body` in one short transaction on atomic's tables, committing or
    /// rolling back as it says; an error rolls back. On SQLite the write lock
    /// is taken up front, so a read followed by a write in the body cannot
    /// lose a race to another writer between the two.
    pub(crate) async fn transaction<T, F>(&self, body: F) -> Result<End<T>>
    where
        T: Send + 'static,
        F: for<'t, 'c> Fn(Tx<'t, 'c>) -> TxFuture<'t, End<T>> + Send + Sync + 'static,
    {
        match self {
            Reads::Sqlite(pool) => {
                let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.map_err(db)?;
                let out = body(Tx(TxInner::Sqlite(&mut tx))).await?;
                match &out {
                    End::Commit(_) => tx.commit().await.map_err(db)?,
                    End::Rollback(_) => tx.rollback().await.map_err(db)?,
                }
                Ok(out)
            }
            Reads::Postgres(pool) => {
                let mut tx = pool.begin().await.map_err(db)?;
                let out = body(Tx(TxInner::Postgres(&mut tx))).await?;
                match &out {
                    End::Commit(_) => tx.commit().await.map_err(db)?,
                    End::Rollback(_) => tx.rollback().await.map_err(db)?,
                }
                Ok(out)
            }
        }
    }
}

/// [`DurableRuntime`] on the durare fork, over SQLite or PostgreSQL.
pub struct Durare {
    pub(crate) engine: durare::DurableEngine,
    reads: Reads,
    /// Handles of workflows this process started, by id: waiting on one
    /// follows that execution, including a crash, which a lookup by id would
    /// not see.
    started: std::sync::Mutex<std::collections::HashMap<String, durare::WorkflowHandle<Value>>>,
    /// The resume queues this engine takes runs from (see
    /// [`Durare::take_resumed_runs`]).
    resume_queues: std::collections::BTreeSet<String>,
}

impl Durare {
    /// An engine on the SQLite database at `path`, created if missing. The
    /// store is opened through atomic's version check first, so a store
    /// written by a newer atomic is refused before anything touches it.
    pub async fn sqlite(path: &Path) -> Result<Self> {
        crate::store::open_sqlite(path)
            .await
            .map_err(|e| DurableError::Engine(e.to_string()))?;
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let provider = durare::SqliteProvider::connect(&url)
            .await
            .map_err(from_durare)?;
        let reads = sqlx::SqlitePool::connect(&url).await.map_err(db)?;
        Self::over(
            Arc::new(provider),
            Reads::Sqlite(reads),
            None,
            DEFAULT_VERSION,
        )
        .await
    }

    /// An engine on `database` that owns its runs as `executor`, checked by
    /// [`executor_identity`]: each process sharing a store needs an identity
    /// of its own that stays the same across its restarts, so it recovers
    /// its own unfinished runs and no one else's. `None` is the default
    /// owner, `local`.
    ///
    /// Runs it starts are stamped with `version`, and it recovers and
    /// dequeues only runs of that version. Name the layout of the durable
    /// steps the registered workflows record, changed exactly when a run
    /// started under the old layout could not replay under the new one, so
    /// any build of the same layout continues the same runs.
    pub async fn open(database: &Database, executor: Option<&str>, version: &str) -> Result<Self> {
        let executor =
            executor_identity(executor).map_err(|e| DurableError::Engine(e.to_string()))?;
        match database {
            Database::Sqlite(path) => {
                crate::store::open_sqlite(path)
                    .await
                    .map_err(|e| DurableError::Engine(e.to_string()))?;
                let url = format!("sqlite://{}?mode=rwc", path.display());
                let provider = durare::SqliteProvider::connect(&url)
                    .await
                    .map_err(from_durare)?;
                let reads = sqlx::SqlitePool::connect(&url).await.map_err(db)?;
                Self::over(Arc::new(provider), Reads::Sqlite(reads), executor, version).await
            }
            Database::Postgres { url, schema } => {
                Self::postgres_as(url, schema, executor, version).await
            }
        }
    }

    /// The identity this engine owns its runs as.
    pub fn executor_id(&self) -> &str {
        self.engine.executor_id()
    }

    /// The queue a resumed run `run` of this executor goes on:
    /// `atomic.resume.<executor>/<run>`, one queue per run. An executor
    /// identity has no `/`, so the queue names the executor that owns a run
    /// while it waits there.
    pub fn resume_queue(&self, run: &str) -> String {
        format!("{RESUME_QUEUES}{}/{run}", self.engine.executor_id())
    }

    /// Take resumed runs of this executor: run `only`, or with `None`,
    /// every run already waiting on one of this executor's resume queues.
    /// [`DurableRuntime::resume`] puts a run on its queue, which only an
    /// engine that takes that run dispatches; a run it does not take, it
    /// leaves alone, and resume refuses it. Call it before
    /// [`launch`](DurableRuntime::launch).
    pub async fn take_resumed_runs(&mut self, only: Option<&str>) -> Result<()> {
        let queues: Vec<String> = match only {
            Some(run) => vec![self.resume_queue(run)],
            None => {
                let mine = format!("{RESUME_QUEUES}{}/", self.engine.executor_id());
                self.unfinished()
                    .await?
                    .into_iter()
                    .filter_map(|r| r.queue.filter(|q| q.starts_with(&mine)))
                    .collect()
            }
        };
        for name in &queues {
            // Prompt while its run waits, and backing off to a poll every
            // few seconds once it has been taken: the engine cannot stop
            // one queue's dispatcher, and the run may come back to it.
            let mut queue = durare::WorkflowQueue::new(name)
                .max_polling_interval(std::time::Duration::from_secs(5));
            queue.base_polling_interval = std::time::Duration::from_millis(100);
            self.engine.register_queue(queue);
        }
        self.engine
            .listen_queues(queues.clone())
            .map_err(from_durare)?;
        self.resume_queues = queues.into_iter().collect();
        Ok(())
    }

    /// The version this engine stamps on runs, and the only one it recovers.
    pub fn version(&self) -> &str {
        self.engine.app_version()
    }

    /// Run `id`'s status, owner and version, or `None` if there is no such run.
    pub async fn run_record(&self, id: &str) -> Result<Option<RunRecord>> {
        let found = self
            .engine
            .list_workflows(&durare::ListFilter {
                workflow_ids: vec![id.to_owned()],
                ..durare::ListFilter::default()
            })
            .await
            .map_err(from_durare)?;
        Ok(found.into_iter().next().map(RunRecord::from))
    }

    /// The input run `id` was started with, as stored, or `None` if there
    /// is no such run.
    pub async fn run_input(&self, id: &str) -> Result<Option<Value>> {
        let found = self
            .engine
            .list_workflows(&durare::ListFilter {
                workflow_ids: vec![id.to_owned()],
                ..durare::ListFilter::default()
            })
            .await
            .map_err(from_durare)?;
        Ok(found.into_iter().next().map(|w| w.input))
    }

    /// Every run in the store in engine state `state`, such as
    /// `MAX_RECOVERY_ATTEMPTS_EXCEEDED`.
    pub async fn runs_in(&self, state: &str) -> Result<Vec<RunRecord>> {
        let found = self
            .engine
            .list_workflows(&durare::ListFilter {
                status: vec![state.to_owned()],
                ..durare::ListFilter::default()
            })
            .await
            .map_err(from_durare)?;
        Ok(found.into_iter().map(RunRecord::from).collect())
    }

    /// A page of the store's runs, finished or not, whoever owns them: at
    /// most `limit` of them, in order of creation and then of id, after the
    /// run `after` names, each with what it was given and what it returned
    /// or failed with. Paging by that key misses no run however many share a
    /// creation time, and a run removed between pages shifts nothing.
    pub async fn runs_after(
        &self,
        after: Option<&RunKey>,
        limit: i64,
    ) -> Result<Vec<(RunKey, RunContents)>> {
        let after = after.cloned();
        let keys: Vec<RunKey> = self
            .control_transaction(move |mut tx| {
                let after = after.clone();
                Box::pin(async move {
                    let rows = match &after {
                        None => {
                            tx.query_all(
                                "SELECT workflow_uuid AS id, created_at FROM workflow_status \
                                 ORDER BY created_at, workflow_uuid LIMIT ?",
                                &[Param::Int(limit)],
                            )
                            .await?
                        }
                        Some(key) => {
                            tx.query_all(
                                "SELECT workflow_uuid AS id, created_at FROM workflow_status \
                                 WHERE created_at > ? OR (created_at = ? AND workflow_uuid > ?) \
                                 ORDER BY created_at, workflow_uuid LIMIT ?",
                                &[
                                    Param::Int(key.created_at),
                                    Param::Int(key.created_at),
                                    Param::Text(key.id.clone()),
                                    Param::Int(limit),
                                ],
                            )
                            .await?
                        }
                    };
                    rows.iter()
                        .map(|r| {
                            Ok(RunKey {
                                id: r.text("id")?,
                                created_at: r.int("created_at")?,
                            })
                        })
                        .collect()
                })
            })
            .await?;
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut found: std::collections::BTreeMap<String, durare::WorkflowStatus> = self
            .engine
            .list_workflows(&durare::ListFilter {
                workflow_ids: keys.iter().map(|k| k.id.clone()).collect(),
                ..durare::ListFilter::default()
            })
            .await
            .map_err(from_durare)?
            .into_iter()
            .map(|w| (w.id.clone(), w))
            .collect();
        // A run removed since its key was read is passed over.
        Ok(keys
            .into_iter()
            .filter_map(|key| {
                let w = found.remove(&key.id)?;
                Some((
                    key,
                    RunContents {
                        input: w.input.clone(),
                        output: w.output.clone(),
                        error: w.error.clone(),
                        record: RunRecord::from(w),
                    },
                ))
            })
            .collect())
    }

    /// Every unfinished run in the store, whoever owns it and whatever its
    /// version.
    pub async fn unfinished(&self) -> Result<Vec<RunRecord>> {
        let found = self
            .engine
            .list_workflows(&durare::ListFilter {
                status: vec!["PENDING".into(), "ENQUEUED".into()],
                ..durare::ListFilter::default()
            })
            .await
            .map_err(from_durare)?;
        Ok(found.into_iter().map(RunRecord::from).collect())
    }

    /// An engine on PostgreSQL at `url`, in `schema`. As on SQLite, the
    /// store's version is checked first, and a schema written by a newer
    /// atomic is refused before anything is created in it.
    pub async fn postgres(url: &str, schema: &str) -> Result<Self> {
        Self::postgres_as(url, schema, None, DEFAULT_VERSION).await
    }

    async fn postgres_as(
        url: &str,
        schema: &str,
        executor: Option<String>,
        version: &str,
    ) -> Result<Self> {
        crate::store::open_postgres(url, schema)
            .await
            .map_err(|e| DurableError::Engine(e.to_string()))?;
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
        Self::over(
            Arc::new(provider),
            Reads::Postgres(reads),
            executor,
            version,
        )
        .await
    }

    async fn over(
        provider: Arc<dyn durare::StateProvider>,
        reads: Reads,
        executor: Option<String>,
        version: &str,
    ) -> Result<Self> {
        // The engine would let its variables override both settings.
        engine_variables().map_err(|e| DurableError::Engine(e.to_string()))?;
        let config = durare::EngineConfig {
            executor_id: executor,
            app_version: Some(version.to_owned()),
            ..durare::EngineConfig::default()
        };
        let mut engine = durare::DurableEngine::with_config(provider, config)
            .await
            .map_err(from_durare)?;
        // No queue but the engine's internal one, which atomic puts nothing
        // on: an engine runs what it starts and recovers, and resumed runs
        // only once it takes them (see `take_resumed_runs`).
        engine
            .listen_queues(Vec::<String>::new())
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
            resume_queues: Default::default(),
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
        let reads = self.reads.clone();
        self.engine
            .register(name, move |ctx: durare::DurableContext, input: I| {
                let lost = Lost::default();
                let run = workflow(
                    DurareContext {
                        engine: ctx,
                        reads: reads.clone(),
                        lost: lost.clone(),
                    },
                    input,
                );
                // A lost race is the execution's outcome even if its code
                // made something else of it, so the engine waits for the
                // winner's instead of recording this one.
                async move {
                    let out = run.await;
                    match lost.conflict() {
                        Some(run) => Err(durare::Error::WorkflowConflict(run)),
                        None => out.map_err(to_durare),
                    }
                }
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
            Ok(found
                .into_iter()
                .next()
                .map(|w| run_status(&w.status, w.error)))
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
        let own = [self.engine.executor_id().to_owned()];
        Ok(self
            .engine
            .recover_pending_for(&own)
            .await
            .map_err(from_durare)?
            .len())
    }

    fn resume(&self, id: &str) -> impl Future<Output = Result<()>> + Send {
        let id = id.to_owned();
        async move {
            let queue = self.resume_queue(&id);
            if !self.resume_queues.contains(&queue) {
                return Err(DurableError::Engine(format!(
                    "this engine does not take resumed run {id}; call take_resumed_runs for it"
                )));
            }
            let handle = self
                .engine
                .resume_workflow_on::<Value>(&id, &queue)
                .await
                .map_err(from_durare)?;
            if let Ok(mut started) = self.started.lock() {
                started.insert(id, handle);
            }
            Ok(())
        }
    }

    fn send<T>(
        &self,
        run: &str,
        topic: &str,
        message: T,
        key: &str,
    ) -> impl Future<Output = Result<()>> + Send
    where
        T: Serialize + Send,
    {
        let (run, topic, key) = (run.to_owned(), topic.to_owned(), key.to_owned());
        async move {
            self.engine
                .send_with_idempotency_key(&run, message, &topic, &key)
                .await
                .map_err(from_durare)
        }
    }

    async fn control_transaction<T, F>(&self, body: F) -> Result<T>
    where
        T: Send + 'static,
        F: for<'t, 'c> Fn(Tx<'t, 'c>) -> TxFuture<'t, T> + Send + Sync + 'static,
    {
        self.reads.control_transaction(body).await
    }

    fn ledger(&self) -> Ledger {
        Ledger::new(self.reads.clone())
    }

    async fn shutdown(&self, timeout: Duration) -> Result<()> {
        self.engine.shutdown(timeout).await.map_err(from_durare)
    }
}

#[cfg(test)]
mod tests;
