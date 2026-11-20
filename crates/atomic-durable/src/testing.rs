// SPDX-License-Identifier: Apache-2.0

//! The PostgreSQL test database, for suites behind the `postgres-tests`
//! feature.
//!
//! A suite that cannot reach its backend fails; it never skips. With the
//! feature on, [`schema`] panics, naming what is missing, when
//! `ATOMIC_TEST_DATABASE_URL` is unset or the database it names does not
//! answer, so a run without the database fails every Postgres test instead of
//! passing it unrun. Nothing else reads the variable.
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
const DATABASE_VAR: &str = "ATOMIC_TEST_DATABASE_URL";

/// How long the database has to accept one connection.
const REACH: Duration = Duration::from_secs(3);

/// The prefix of every schema [`schema`] makes:
/// `atomic_test_<unix seconds>_<tag>_<process>_<n>_<random>`.
pub const PREFIX: &str = "atomic_test_";

/// How old a schema must be before a sweep drops it: older than any test
/// run, so a sweep never drops a schema a running test still uses.
pub const STALE: Duration = Duration::from_secs(3600);

/// The prefixes earlier versions of the tests gave their schemas, which
/// ended with the time they were made: `<prefix><…>_<nanoseconds or
/// seconds>`. A sweep drops these too; no other schema is touched.
const EARLIER: &[&str] = &[
    "atomic_rt_",
    "atomic_gates_",
    "atomic_gates_kill_",
    "atomic_pub_",
    "atomic_usage_",
    "atomic_newer_",
    "command_race_",
    "attempt_",
    "atomic_cli_",
    "atomic_owned_",
    "atomic_queue_",
    "rr_cross_",
    "dbos_upstream_",
];

/// The test database's URL, without checking that it answers. Panics when
/// [`DATABASE_VAR`] is unset or empty.
fn database_url() -> String {
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
async fn database() -> String {
    let url = database_url();
    match connect(&url).await {
        Ok(connection) => {
            let _ = sqlx::Connection::close(connection).await;
            url
        }
        Err(why) => panic!(
            "the PostgreSQL test database {} ({DATABASE_VAR}) cannot be reached: {why}",
            redacted(&url)
        ),
    }
}

/// One connection to `url`, or why there is none within [`REACH`].
async fn connect(url: &str) -> Result<sqlx::PgConnection, String> {
    use sqlx::Connection;
    match tokio::time::timeout(REACH, sqlx::PgConnection::connect(url)).await {
        Ok(Ok(connection)) => Ok(connection),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("no answer in {} s", REACH.as_secs())),
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

/// A schema in the test database for one test, dropped with everything in it
/// when this is dropped. Drop it after the engines using it, as declaring it
/// before them does.
pub struct TestSchema {
    url: String,
    name: String,
}

impl std::fmt::Debug for TestSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestSchema")
            .field("url", &redacted(&self.url))
            .field("name", &self.name)
            .finish()
    }
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
                .block_on(drop_schema(&url, &name, LOCK_WAIT))
        })
        .join();
        match dropped {
            Ok(Ok(())) => {}
            // Not a test failure: a later process's sweep drops it.
            Ok(Err(e)) => eprintln!("could not drop the test schema {}: {e}", self.name),
            Err(_) => eprintln!("could not drop the test schema {}", self.name),
        }
    }
}

/// A fresh schema for a test, named after `tag`: 1 to 16 lowercase ASCII
/// letters, digits and `_`. Panics, naming what is missing, when the database
/// is not configured or does not answer, and on any other tag.
pub async fn schema(tag: &str) -> TestSchema {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static SWEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    assert!(
        (1..=16).contains(&tag.len())
            && tag
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "a test schema's tag is 1 to 16 of a-z, 0-9 and _, not {tag:?}"
    );
    let url = database().await;
    if !SWEPT.swap(true, std::sync::atomic::Ordering::SeqCst) {
        sweep(&url, STALE).await;
    }
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    TestSchema {
        url,
        name: name_for(tag, now(), n),
    }
}

/// A test schema's name. Each process numbers its schemas, and a random
/// token follows, since processes in different containers can have the same
/// process ID at the same second.
fn name_for(tag: &str, at: Duration, n: usize) -> String {
    let mut token = [0u8; 4];
    // Without randomness the number alone must do; uniqueness is then only
    // as good as the process ID.
    let _ = getrandom::fill(&mut token);
    let token: String = token.iter().map(|b| format!("{b:02x}")).collect();
    let name = format!(
        "{PREFIX}{}_{tag}_{}_{n}_{token}",
        at.as_secs(),
        std::process::id()
    );
    // PostgreSQL cuts a longer identifier short, and two cut names could
    // meet; with a 16-byte tag this holds up to the hundred-thousandth
    // schema of a process.
    assert!(
        name.len() <= 63,
        "the test schema name {name} is longer than PostgreSQL's 63 bytes"
    );
    name
}

/// Drop every schema a test process made more than `older_than` ago: one
/// [`schema`] made, or one an earlier version of these tests made under a
/// prefix in [`EARLIER`]. A schema that cannot be dropped is reported and
/// left for the next sweep. Returns how many were dropped.
pub async fn sweep(url: &str, older_than: Duration) -> usize {
    let names: Vec<(String,)> = match connect(url).await {
        Ok(mut connection) => {
            let names = sqlx::query_as("SELECT nspname::text FROM pg_namespace")
                .fetch_all(&mut connection)
                .await;
            let _ = sqlx::Connection::close(connection).await;
            match names {
                Ok(names) => names,
                Err(e) => {
                    eprintln!("could not list the test schemas to sweep: {e}");
                    return 0;
                }
            }
        }
        Err(e) => {
            eprintln!("could not sweep stale test schemas: {e}");
            return 0;
        }
    };
    let cutoff = now().saturating_sub(older_than);
    let stale: Vec<String> = names
        .into_iter()
        .map(|(name,)| name)
        .filter(|name| made_at(name).is_some_and(|at| at < cutoff))
        .collect();
    drop_all(url, &stale, LOCK_WAIT).await
}

/// How long dropping one schema waits for a lock someone else holds on it.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// Drop each of `names`, waiting at most `lock_wait` for each one's locks.
/// One that cannot be dropped is reported and left, and the rest are still
/// dropped. Returns how many were.
async fn drop_all(url: &str, names: &[String], lock_wait: Duration) -> usize {
    let mut dropped = 0;
    for name in names {
        match drop_schema(url, name, lock_wait).await {
            Ok(()) => dropped += 1,
            Err(e) => eprintln!("could not drop the stale test schema {name}: {e}"),
        }
    }
    dropped
}

/// When a test schema called `name` was made, if it is one and its name
/// says: [`PREFIX`] then ten digits of seconds, or a prefix in [`EARLIER`]
/// and, last, nineteen digits of nanoseconds or ten of seconds.
fn made_at(name: &str) -> Option<Duration> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(rest) = name.strip_prefix(PREFIX) {
        let secs = rest.split('_').next()?;
        return (secs.len() == 10 && digits(secs))
            .then(|| secs.parse().ok().map(Duration::from_secs))
            .flatten();
    }
    if !EARLIER.iter().any(|p| name.starts_with(p)) {
        return None;
    }
    let (_, time) = name.rsplit_once('_')?;
    if !digits(time) {
        return None;
    }
    let value: u64 = time.parse().ok()?;
    match time.len() {
        19 => Some(Duration::from_nanos(value)),
        10 => Some(Duration::from_secs(value)),
        _ => None,
    }
}

async fn drop_schema(url: &str, name: &str, lock_wait: Duration) -> Result<(), String> {
    let mut connection = connect(url).await?;
    // A connection an engine left behind may still hold a lock; wait a
    // little, then leave the schema to a sweep.
    let dropped = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SET lock_timeout = '{}ms'; DROP SCHEMA IF EXISTS \"{}\" CASCADE",
        lock_wait.as_millis(),
        name.replace('"', "\"\"")
    )))
    .execute(&mut connection)
    .await
    .map(|_| ())
    .map_err(|e| e.to_string());
    let _ = sqlx::Connection::close(connection).await;
    dropped
}

fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn exists(url: &str, name: &str) -> bool {
        let mut connection = connect(url).await.expect("connect");
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
        let url = database().await;
        // This test's own names, with its process ID and a random token, so
        // that sweeps in other processes and this one do not meet.
        let token = name_for("sweep", now(), 0);
        let me = token.rsplit_once('_').map_or("x", |(_, t)| t).to_owned();
        let pid = std::process::id();
        let old = now() - STALE - Duration::from_secs(60);
        let fresh = now();
        let stale = [
            name_for("sweep", old, 1),
            format!("atomic_rt_{pid}_{me}_0_{}", old.as_nanos()),
            format!("rr_cross_{pid}_{me}_{}", old.as_secs()),
        ];
        let kept = [
            name_for("sweep", fresh, 2),
            // Fresh earlier-format names, in each unit: read in the wrong
            // unit, one of them would look stale.
            format!("atomic_rt_{pid}_{me}_1_{}", fresh.as_nanos()),
            format!("rr_cross_{pid}_{me}_{}", fresh.as_secs()),
            // Not a test's, however old its name looks.
            format!("atomic_sweep_{pid}_{me}_{}", old.as_nanos()),
        ];
        let mut connection = connect(&url).await.expect("connect");
        for name in stale.iter().chain(&kept) {
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "CREATE SCHEMA IF NOT EXISTS \"{name}\""
            )))
            .execute(&mut connection)
            .await
            .expect("create");
        }
        sweep(&url, STALE).await;
        let (mut wrong, mut leftover) = (Vec::new(), Vec::new());
        for name in &stale {
            if exists(&url, name).await {
                wrong.push(format!("{name} is stale and was kept"));
                leftover.push(name);
            }
        }
        for name in &kept {
            if exists(&url, name).await {
                leftover.push(name);
            } else {
                wrong.push(format!("{name} is fresh or no test's and was dropped"));
            }
        }
        for name in leftover {
            drop_schema(&url, name, LOCK_WAIT).await.expect("clean up");
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// A schema the sweep cannot drop, because a lock on it is held, is
    /// reported and left, and the sweep goes on to drop the next one. Fresh
    /// names, which no other process's sweep drops, given to the sweep's
    /// drop loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_sweep_goes_on_past_a_schema_it_cannot_drop_on_postgres() {
        let url = database().await;
        let (held, next) = (
            name_for("sweeplock", now(), 1),
            name_for("sweeplock", now(), 2),
        );
        let mut connection = connect(&url).await.expect("connect");
        for name in [&held, &next] {
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "CREATE SCHEMA \"{name}\"; CREATE TABLE \"{name}\".t (x int)"
            )))
            .execute(&mut connection)
            .await
            .expect("create");
        }
        // Another session holds a lock on a table in one of them.
        let mut holder = connect(&url).await.expect("connect");
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "BEGIN; LOCK TABLE \"{held}\".t IN ACCESS SHARE MODE"
        )))
        .execute(&mut holder)
        .await
        .expect("lock");
        let count = drop_all(
            &url,
            &[held.clone(), next.clone()],
            Duration::from_millis(200),
        )
        .await;
        let (kept, dropped) = (exists(&url, &held).await, !exists(&url, &next).await);
        sqlx::raw_sql("ROLLBACK")
            .execute(&mut holder)
            .await
            .expect("unlock");
        let _ = drop_schema(&url, &held, LOCK_WAIT).await;
        let _ = drop_schema(&url, &next, LOCK_WAIT).await;
        assert!(kept, "{held} was locked, and is left for the next sweep");
        assert!(dropped, "{next} was dropped all the same");
        assert_eq!(count, 1);
    }

    #[test]
    fn a_name_says_when_its_schema_was_made_in_its_own_unit() {
        let at = Duration::from_nanos(1_795_000_000_123_456_789);
        // This harness's names: seconds, ten digits, first.
        assert_eq!(
            made_at(&name_for("tag", at, 3)),
            Some(Duration::from_secs(1_795_000_000))
        );
        // Earlier names: the time last, in nanoseconds or seconds.
        let earlier = |time: &str| made_at(&format!("atomic_rt_1_0_{time}"));
        assert_eq!(earlier("1795000000123456789"), Some(at));
        assert_eq!(
            earlier("1795000000"),
            Some(Duration::from_secs(1_795_000_000))
        );
        // Any other length is no time at all.
        for time in [
            "179500000",
            "17950000001",
            "1795000000123",
            "1795000000123456",
            "17950000001234567890",
        ] {
            assert_eq!(earlier(time), None, "{} digits", time.len());
        }
        // Nor is a name under another prefix, or not ending in digits.
        assert_eq!(made_at("someone_else_1795000000"), None);
        assert_eq!(made_at("atomic_rt_1_0_x795000000"), None);
        assert_eq!(made_at(&format!("{PREFIX}179500000_tag_1_0_ab")), None);
    }

    #[test]
    fn names_are_unique_and_carry_their_tag() {
        let at = now();
        let a = name_for("gates_kill", at, 0);
        let b = name_for("gates_kill", at, 0);
        assert_ne!(a, b, "the same process, second and number still differ");
        assert!(
            a.starts_with(&format!("{PREFIX}{}_gates_kill_", at.as_secs())),
            "{a}"
        );
        assert!(a.len() <= 63, "{a} fits a PostgreSQL identifier");
        // The longest tag, far into a process's schemas, still fits.
        let long = name_for("abcdefghijklmnop", at, 99_999);
        assert!(long.len() <= 63, "{long}");
    }

    #[tokio::test]
    #[should_panic(expected = "a test schema's tag is 1 to 16 of a-z, 0-9 and _")]
    async fn a_tag_outside_its_charset_is_refused() {
        let _ = schema("Has-Dash").await;
    }

    #[test]
    fn credentials_are_not_shown() {
        assert_eq!(
            redacted("postgres://atomic:secret@db:5432/atomic"),
            "postgres://***@db:5432/atomic"
        );
        assert_eq!(redacted("postgres://db/atomic"), "postgres://db/atomic");
        let schema = TestSchema {
            url: "postgres://atomic:secret@db/atomic".into(),
            name: "atomic_test_x".into(),
        };
        let shown = format!("{schema:?}");
        assert!(!shown.contains("secret"), "{shown}");
        // Not dropped: there is no database at that address.
        std::mem::forget(schema);
    }
}
