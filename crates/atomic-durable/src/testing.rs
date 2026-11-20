// SPDX-License-Identifier: Apache-2.0

//! The PostgreSQL test database, for suites behind the `postgres-tests`
//! feature.
//!
//! A suite that cannot reach its backend fails; it never skips. With the
//! feature on, [`database`] panics, naming what is missing, when
//! `ATOMIC_TEST_DATABASE_URL` is unset or the database it names does not
//! answer, so a run without the database fails every Postgres test instead of
//! passing it unrun.
//!
//! The database is shared, by every test process of every checkout using it,
//! and it is small. A test takes a schema of its own from [`schema`], and the
//! [`TestSchema`] it gets drops that schema when it is dropped, whether the
//! test passed or panicked. A process that was killed cannot drop its
//! schemas, so the first [`schema`] in each process also sweeps the schemas
//! test processes left behind more than [`STALE`] ago, judged by the time
//! their names carry.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The variable that names the test database.
pub const DATABASE_VAR: &str = "ATOMIC_TEST_DATABASE_URL";

/// How long the database has to accept one connection.
const REACH: Duration = Duration::from_secs(3);

/// The test database's URL, without checking that it answers. Panics when
/// [`DATABASE_VAR`] is unset or empty.
pub fn database_url() -> String {
    match std::env::var(DATABASE_VAR) {
        Ok(url) if !url.is_empty() => url,
        _ => panic!(
            "the postgres-tests feature needs a PostgreSQL test database, and {DATABASE_VAR} is \
             not set: scripts/dev.sh starts one and sets it, or point {DATABASE_VAR} at your own"
        ),
    }
}

/// The test database's URL, once one connection to it succeeds. Panics,
/// naming the database, when it does not answer within a few seconds.
pub async fn database() -> String {
    use sqlx::Connection;
    let url = database_url();
    let fail = |why: String| -> ! {
        panic!(
            "the PostgreSQL test database {} ({DATABASE_VAR}) cannot be reached: {why}",
            redacted(&url)
        )
    };
    match tokio::time::timeout(REACH, sqlx::PgConnection::connect(&url)).await {
        Ok(Ok(connection)) => {
            let _ = connection.close().await;
            url.clone()
        }
        Ok(Err(e)) => fail(e.to_string()),
        Err(_) => fail(format!("no answer in {} s", REACH.as_secs())),
    }
}

/// `url` without the credentials it carries.
fn redacted(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            format!("{}://***@{}", &url[..scheme], &url[at + 1..])
        }
        _ => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn exists(url: &str, name: &str) -> bool {
        use sqlx::Connection;
        let mut connection = sqlx::PgConnection::connect(url).await.expect("connect");
        let found: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM pg_namespace WHERE nspname = $1")
            .bind(name)
            .fetch_optional(&mut connection)
            .await
            .expect("query");
        found.is_some()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_schema_is_dropped_with_its_guard_on_postgres() {
        let schema = schema("guard").await;
        let (url, name) = (schema.url().to_owned(), schema.name().to_owned());
        let rt = crate::Durare::postgres(&url, &name).await.expect("engine");
        assert!(exists(&url, &name).await, "the engine made {name}");
        drop(rt);
        drop(schema);
        assert!(!exists(&url, &name).await, "{name} was dropped");
    }

    #[test]
    fn a_test_that_panics_drops_its_schema_on_postgres() {
        let made = std::thread::spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(async {
                    let schema = schema("panics").await;
                    let rt = crate::Durare::postgres(schema.url(), schema.name())
                        .await
                        .expect("engine");
                    let made = (schema.url().to_owned(), schema.name().to_owned());
                    drop(rt);
                    std::panic::panic_any(made);
                })
        })
        .join()
        .expect_err("the test panicked");
        let (url, name) = *made.downcast::<(String, String)>().expect("its schema");
        let gone = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async { !exists(&url, &name).await });
        assert!(gone, "{name} was dropped as the panic unwound");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sweep_drops_stale_test_schemas_and_keeps_fresh_ones_on_postgres() {
        use sqlx::Connection;
        let url = database().await;
        let old = now() - STALE - Duration::from_secs(60);
        let names = [
            // Stale: made by this harness, and by an earlier one.
            format!("{PREFIX}{}_sweep_1_0", old.as_secs()),
            format!("atomic_rt_1_0_{}", old.as_nanos()),
            format!("rr_cross_8_{}", old.as_secs()),
            // Fresh, and not a test schema at all.
            format!("{PREFIX}{}_sweep_1_1", now().as_secs()),
            "atomic_sweep_keep".to_owned(),
        ];
        let mut connection = sqlx::PgConnection::connect(&url).await.expect("connect");
        for name in &names {
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "CREATE SCHEMA IF NOT EXISTS \"{name}\""
            )))
            .execute(&mut connection)
            .await
            .expect("create");
        }
        sweep(&url, STALE).await.expect("sweep");
        assert!(!exists(&url, &names[0]).await, "{} is stale", names[0]);
        assert!(!exists(&url, &names[1]).await, "{} is stale", names[1]);
        assert!(!exists(&url, &names[2]).await, "{} is stale", names[2]);
        assert!(exists(&url, &names[3]).await, "{} is fresh", names[3]);
        assert!(exists(&url, &names[4]).await, "{} is no test's", names[4]);
        for name in &names[3..] {
            drop_schema(&url, name).await.expect("clean up");
        }
    }

    #[test]
    fn credentials_are_not_shown() {
        assert_eq!(
            super::redacted("postgres://atomic:secret@db:5432/atomic"),
            "postgres://***@db:5432/atomic"
        );
        assert_eq!(
            super::redacted("postgres://db/atomic"),
            "postgres://db/atomic"
        );
    }
}

/// The prefix of every schema [`schema`] makes: `atomic_test_<unix
/// seconds>_<tag>_<process>_<n>`.
pub const PREFIX: &str = "atomic_test_";

/// How old a schema must be before a sweep drops it: older than any test
/// run, so a sweep never drops a schema a running test still uses.
pub const STALE: Duration = Duration::from_secs(3600);

/// A schema in the test database for one test, dropped with everything in it
/// when this is dropped. Drop it after the engines using it, as declaring it
/// before them does.
#[derive(Debug)]
pub struct TestSchema {
    url: String,
    name: String,
}

impl TestSchema {
    /// The test database's URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The schema's name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for TestSchema {
    fn drop(&mut self) {
        // Drop runs inside a test's runtime, or while a panic unwinds out of
        // it, so the schema is dropped on a thread with a runtime of its own.
        let (url, name) = (self.url.clone(), self.name.clone());
        let dropped = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?
                .block_on(drop_schema(&url, &name))
        })
        .join();
        match dropped {
            Ok(Ok(())) => {}
            // Not a test failure: the next process's sweep drops it.
            Ok(Err(e)) => eprintln!("could not drop the test schema {}: {e}", self.name),
            Err(_) => eprintln!("could not drop the test schema {}", self.name),
        }
    }
}

/// A fresh schema for a test, named after `tag` (lowercase letters, digits
/// and `_`). Panics as [`database`] does when the database is missing.
pub async fn schema(tag: &str) -> TestSchema {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static SWEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let url = database().await;
    if !SWEPT.swap(true, std::sync::atomic::Ordering::SeqCst)
        && let Err(e) = sweep(&url, STALE).await
    {
        eprintln!("could not sweep stale test schemas: {e}");
    }
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!(
        "{PREFIX}{}_{tag}_{}_{n}",
        now().as_secs(),
        std::process::id()
    );
    TestSchema { url, name }
}

/// Drop every schema a test process made more than `older_than` ago: one
/// [`schema`] made, or one an earlier version of these tests named with the
/// time it was made at, in nanoseconds or seconds, last. Returns how many it
/// dropped.
pub async fn sweep(url: &str, older_than: Duration) -> Result<usize, String> {
    use sqlx::Connection;
    let mut connection = sqlx::PgConnection::connect(url)
        .await
        .map_err(|e| e.to_string())?;
    let names: Vec<(String,)> = sqlx::query_as("SELECT nspname::text FROM pg_namespace")
        .fetch_all(&mut connection)
        .await
        .map_err(|e| e.to_string())?;
    let _ = connection.close().await;
    let cutoff = now().saturating_sub(older_than);
    let mut dropped = 0;
    for (name,) in names {
        if made_at(&name).is_some_and(|at| at < cutoff) {
            drop_schema(url, &name).await?;
            dropped += 1;
        }
    }
    Ok(dropped)
}

/// When a test schema called `name` was made, if its name says.
fn made_at(name: &str) -> Option<Duration> {
    if let Some(rest) = name.strip_prefix(PREFIX) {
        let secs = rest.split('_').next()?;
        return (secs.len() == 10).then(|| secs.parse().ok().map(Duration::from_secs))?;
    }
    // Earlier tests ended their schema names with the time they were made,
    // in nanoseconds (`<tag>_<process>_<n>_<nanos>`) or seconds.
    let (_, time) = name.rsplit_once('_')?;
    let value: u64 = time.parse().ok()?;
    match time.len() {
        19 => Some(Duration::from_nanos(value)),
        10 => Some(Duration::from_secs(value)),
        _ => None,
    }
}

async fn drop_schema(url: &str, name: &str) -> Result<(), String> {
    use sqlx::Connection;
    let mut connection = sqlx::PgConnection::connect(url)
        .await
        .map_err(|e| e.to_string())?;
    // A connection an engine left behind may still hold a lock; wait a
    // little, then leave the schema to a sweep.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SET lock_timeout = '10s'; DROP SCHEMA IF EXISTS \"{}\" CASCADE",
        name.replace('"', "\"\"")
    )))
    .execute(&mut connection)
    .await
    .map_err(|e| e.to_string())?;
    let _ = connection.close().await;
    Ok(())
}

fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}
