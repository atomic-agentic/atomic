// SPDX-License-Identifier: Apache-2.0

//! atomic's own tables in the project database, and their versions.
//!
//! The database file holds durare's system tables and atomic's own. durare
//! versions its tables itself; atomic's are versioned here, in
//! `atomic_store_version`, and migrate forward when a store is opened. A store
//! written by a newer atomic is refused: this binary does not know what the
//! newer migrations changed, so it starts nothing rather than guess.

use sqlx::{Connection, Row, SqliteConnection};
use std::path::Path;
use std::str::FromStr;

/// One forward migration of atomic's tables.
struct Migration {
    version: i64,
    /// Its statements, run in order in one transaction. Each is valid on
    /// SQLite and PostgreSQL alike.
    sql: &'static [&'static str],
}

/// Every migration, in order. Append only: a released migration never changes.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        // Published candidates (see `publication`).
        sql: &["CREATE TABLE IF NOT EXISTS atomic_publications (
              publication_key TEXT PRIMARY KEY,
              digest TEXT NOT NULL,
              output TEXT NOT NULL
          )"],
    },
    Migration {
        version: 2,
        // The gate store (see `gate`): one row per gate, and the journal of
        // decision commands. Times are milliseconds since the Unix epoch.
        sql: &[
            "CREATE TABLE IF NOT EXISTS atomic_gates (
                gate_id TEXT PRIMARY KEY,
                run TEXT NOT NULL,
                workflow TEXT NOT NULL,
                step TEXT NOT NULL,
                title TEXT NOT NULL,
                show TEXT NOT NULL,
                evidence TEXT NOT NULL,
                proposal TEXT NOT NULL,
                approvers TEXT NOT NULL,
                tier TEXT NOT NULL,
                opened_at BIGINT NOT NULL,
                deadline BIGINT,
                on_timeout TEXT NOT NULL,
                state TEXT NOT NULL,
                decided_by TEXT,
                decided_at BIGINT,
                comment TEXT,
                command_id TEXT
            )",
            "CREATE INDEX IF NOT EXISTS atomic_gates_by_state ON atomic_gates (state, opened_at)",
            "CREATE INDEX IF NOT EXISTS atomic_gates_by_run ON atomic_gates (run)",
            "CREATE TABLE IF NOT EXISTS atomic_gate_commands (
                command_id TEXT PRIMARY KEY,
                gate_id TEXT NOT NULL,
                request TEXT NOT NULL,
                result TEXT NOT NULL,
                recorded_at BIGINT NOT NULL
            )",
        ],
    },
    Migration {
        version: 3,
        // The usage ledger (see `usage`): allowances with their caps and what
        // is charged against them, and the journal of call attempts; and the
        // runs parked on a cap. Times are milliseconds since the Unix epoch.
        sql: &[
            "CREATE TABLE IF NOT EXISTS atomic_usage_allowances (
                scope TEXT PRIMARY KEY,
                parent TEXT,
                tokens_cap BIGINT NOT NULL,
                millis_cap BIGINT NOT NULL,
                micro_usd_cap BIGINT,
                tokens_charged BIGINT NOT NULL,
                millis_charged BIGINT NOT NULL,
                micro_usd_charged BIGINT NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS atomic_usage_attempts (
                request_id TEXT NOT NULL,
                attempt BIGINT NOT NULL,
                run TEXT NOT NULL,
                model TEXT NOT NULL,
                scopes TEXT NOT NULL,
                state TEXT NOT NULL,
                reserved_tokens BIGINT NOT NULL,
                reserved_millis BIGINT NOT NULL,
                reserved_micro_usd BIGINT NOT NULL,
                maximum_tokens BIGINT NOT NULL,
                maximum_micro_usd BIGINT NOT NULL,
                input_tokens BIGINT,
                output_tokens BIGINT,
                charged_tokens BIGINT NOT NULL,
                charged_millis BIGINT NOT NULL,
                charged_micro_usd BIGINT NOT NULL,
                admitted_at BIGINT NOT NULL,
                settled_at BIGINT,
                PRIMARY KEY (request_id, attempt)
            )",
            "CREATE INDEX IF NOT EXISTS atomic_usage_attempts_by_run ON atomic_usage_attempts (run)",
            "CREATE TABLE IF NOT EXISTS atomic_parks (
                park_id TEXT PRIMARY KEY,
                run TEXT NOT NULL,
                step TEXT NOT NULL,
                reason TEXT NOT NULL,
                parked_at BIGINT NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS atomic_parks_by_run ON atomic_parks (run)",
        ],
    },
    Migration {
        version: 4,
        // The people who may decide gates (see `gate::credential`): each
        // principal and the verifier of the credential it enrolled.
        sql: &["CREATE TABLE IF NOT EXISTS atomic_operators (
                principal TEXT PRIMARY KEY,
                verifier TEXT NOT NULL,
                enrolled_at BIGINT NOT NULL
            )"],
    },
    Migration {
        version: 5,
        // When each running call last said it was alive (see `usage`), so
        // a call whose process died is charged no time past it.
        sql: &["CREATE TABLE IF NOT EXISTS atomic_usage_heartbeats (
                request_id TEXT NOT NULL,
                attempt BIGINT NOT NULL,
                seen_at BIGINT NOT NULL,
                PRIMARY KEY (request_id, attempt)
            )"],
    },
    Migration {
        version: 6,
        // Each execution of each run (see `history`): when it began.
        sql: &["CREATE TABLE IF NOT EXISTS atomic_executions (
                run TEXT NOT NULL,
                attempt BIGINT NOT NULL,
                started_at BIGINT NOT NULL,
                PRIMARY KEY (run, attempt)
            )"],
    },
    Migration {
        version: 7,
        // Question gates (see `gate`): the question a gate asks, as JSON,
        // and the answer its decision gave; and the feedback log, a dated
        // entry for each answer a question gate asked to remember.
        sql: &[
            "CREATE TABLE IF NOT EXISTS atomic_gate_questions (
                gate_id TEXT PRIMARY KEY,
                question TEXT NOT NULL,
                answer TEXT
            )",
            "CREATE TABLE IF NOT EXISTS atomic_feedback (
                gate_id TEXT PRIMARY KEY,
                workflow TEXT NOT NULL,
                step TEXT NOT NULL,
                ask TEXT NOT NULL,
                answer TEXT NOT NULL,
                decided_by TEXT NOT NULL,
                recorded_at BIGINT NOT NULL
            )",
        ],
    },
    Migration {
        version: 8,
        // Releasing a park (see `release`): the release of each park, the
        // allowance that refused each parked call, and the journal of every
        // person's admin action, by command id.
        sql: &[
            "CREATE TABLE IF NOT EXISTS atomic_park_releases (
                park_id TEXT PRIMARY KEY,
                released_by TEXT NOT NULL,
                released_at BIGINT NOT NULL,
                command_id TEXT NOT NULL
            )",
            // The allowance that refused a parked call, as the refusal
            // recorded it.
            "CREATE TABLE IF NOT EXISTS atomic_park_scopes (
                park_id TEXT PRIMARY KEY,
                scope TEXT NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS atomic_admin_actions (
                command_id TEXT PRIMARY KEY,
                action TEXT NOT NULL,
                target TEXT NOT NULL,
                request TEXT NOT NULL,
                principal TEXT NOT NULL,
                detail TEXT NOT NULL,
                recorded_at BIGINT NOT NULL
            )",
        ],
    },
    Migration {
        version: 9,
        // The project each park is for, on a store several projects share
        // (see `DurableRuntime::project`). A park recorded before has none,
        // and only a runtime for no project sees it.
        sql: &["CREATE TABLE IF NOT EXISTS atomic_park_projects (
                park_id TEXT PRIMARY KEY,
                project TEXT NOT NULL
            )"],
    },
    Migration {
        version: 10,
        // The project each run is for, on a store several projects share,
        // recorded as the run starts: a project shows and resumes only its
        // own runs, and lists and decides only its own runs' gates. A run
        // started before has none, and only a runtime for no project sees
        // it.
        sql: &["CREATE TABLE IF NOT EXISTS atomic_run_projects (
                run_id TEXT PRIMARY KEY,
                project TEXT NOT NULL
            )"],
    },
];

/// The store version this binary writes and understands: its last
/// migration's.
pub const STORE_VERSION: i64 = last_version(MIGRATIONS);

// The migrations are versions 1 to the last, once each and in order, or this
// does not build: a store migrated across a missing version would be marked
// at the newer one without it, and never get it.
const _: () = assert!(
    contiguous(MIGRATIONS),
    "MIGRATIONS must be versions 1 to the last, once each and in order"
);

/// The last of `migrations`' versions, 0 for none.
const fn last_version(migrations: &[Migration]) -> i64 {
    match migrations.last() {
        Some(m) => m.version,
        None => 0,
    }
}

/// Whether `migrations` are exactly versions 1 to the last, in order, once
/// each.
const fn contiguous(migrations: &[Migration]) -> bool {
    let mut i = 0;
    while i < migrations.len() {
        if migrations[i].version != i as i64 + 1 {
            return false;
        }
        i += 1;
    }
    true
}

/// The first version after `found`, up to the last of `migrations`, that
/// they do not provide exactly once. Migrations repeated or out of order
/// are refused outright, whatever the store's version: the first version
/// after `found`. A store at `found` may be migrated only when there is
/// none.
fn first_gap(migrations: &[Migration], found: i64) -> Option<i64> {
    if !migrations.windows(2).all(|w| w[0].version < w[1].version) {
        return Some(found + 1);
    }
    ((found + 1)..=last_version(migrations))
        .find(|v| migrations.iter().filter(|m| m.version == *v).count() != 1)
}

/// Refuse to migrate the store at `path` from `found` across a gap in
/// `migrations`.
fn no_gap(path: &str, found: i64, migrations: &[Migration]) -> Result<(), StoreError> {
    match first_gap(migrations, found) {
        Some(missing) => Err(StoreError::Gap {
            path: path.to_owned(),
            found,
            supported: last_version(migrations),
            missing,
        }),
        None => Ok(()),
    }
}

/// Why the store could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The store was written by a newer atomic.
    #[error(
        "the store at {path} is at version {found}, newer than this atomic understands \
         (version {supported}); use the atomic that wrote it, or a newer one"
    )]
    Newer {
        /// The database file.
        path: String,
        /// Its version.
        found: i64,
        /// The newest version this binary knows.
        supported: i64,
    },
    /// This binary's migrations skip a version between the store's and its
    /// own: a store migrated across the gap would be marked at the newer
    /// version without the skipped one, and never get it.
    #[error(
        "the store at {path} is at version {found}, and this atomic's migrations to version \
         {supported} skip version {missing}; it migrates nothing, so the store keeps every \
         migration it is owed. Use an atomic built with every migration"
    )]
    Gap {
        /// The database file.
        path: String,
        /// Its version.
        found: i64,
        /// The version this binary would migrate it to.
        supported: i64,
        /// The first version no migration provides.
        missing: i64,
    },
    /// The database failed.
    #[error("the store at {path}: {source}")]
    Database {
        /// The database file.
        path: String,
        /// The underlying error.
        source: sqlx::Error,
    },
}

/// Open the SQLite store at `database`, creating it if needed, check its
/// version and migrate it forward. Returns the version it is now at.
pub async fn open_sqlite(database: &Path) -> Result<i64, StoreError> {
    open_sqlite_with(database, MIGRATIONS).await
}

/// [`open_sqlite`], migrating with `migrations`.
async fn open_sqlite_with(database: &Path, migrations: &[Migration]) -> Result<i64, StoreError> {
    let path = database.display().to_string();
    let db = |source| StoreError::Database {
        path: path.clone(),
        source,
    };
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(&format!("sqlite://{path}"))
        .map_err(db)?
        .create_if_missing(true);
    let mut conn = SqliteConnection::connect_with(&options).await.map_err(db)?;
    let result = migrate(&mut conn, &path, migrations).await;
    let _ = conn.close().await;
    result
}

/// Open atomic's store in PostgreSQL schema `schema` at `url`: refuse it if a
/// newer atomic wrote it, before anything is created; otherwise create the
/// schema and the version table if needed and migrate forward. Returns the
/// version it is now at.
pub async fn open_postgres(url: &str, schema: &str) -> Result<i64, StoreError> {
    open_postgres_with(url, schema, MIGRATIONS).await
}

/// [`open_postgres`], migrating with `migrations`.
async fn open_postgres_with(
    url: &str,
    schema: &str,
    migrations: &[Migration],
) -> Result<i64, StoreError> {
    let to = last_version(migrations);
    let path = format!("postgres schema {schema}");
    let db = |source| StoreError::Database {
        path: path.clone(),
        source,
    };
    let plain = schema
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && schema
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !plain {
        return Err(db(sqlx::Error::Protocol(format!(
            "invalid schema name {schema:?}: use letters, digits and underscores"
        ))));
    }
    let mut conn = sqlx::PgConnection::connect(url).await.map_err(db)?;
    let result = async {
        let mut tx = conn.begin().await.map_err(db)?;
        // One opener at a time per schema, across processes.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(format!("atomic-store:{schema}"))
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        let table = format!("{schema}.atomic_store_version");
        let exists: bool = sqlx::query("SELECT to_regclass($1) IS NOT NULL AS e")
            .bind(&table)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?
            .try_get("e")
            .map_err(db)?;
        let found: i64 = if exists {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT COALESCE(MAX(version), 0)::BIGINT AS v FROM {table}"
            )))
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?
            .try_get("v")
            .map_err(db)?
        } else {
            0
        };
        if found > to {
            return Err(StoreError::Newer {
                path: path.clone(),
                found,
                supported: to,
            });
        }
        no_gap(&path, found, migrations)?;
        for sql in [
            format!("CREATE SCHEMA IF NOT EXISTS {schema}"),
            format!("CREATE TABLE IF NOT EXISTS {table} (version BIGINT NOT NULL)"),
            format!("SET LOCAL search_path TO {schema}"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        for m in migrations.iter().filter(|m| m.version > found) {
            for sql in m.sql {
                sqlx::query(*sql).execute(&mut *tx).await.map_err(db)?;
            }
        }
        if found < to {
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO {table} (version) VALUES ($1)"
            )))
            .bind(to)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        tx.commit().await.map_err(db)?;
        Ok(to)
    }
    .await;
    let _ = conn.close().await;
    result
}

async fn migrate(
    conn: &mut SqliteConnection,
    path: &str,
    migrations: &[Migration],
) -> Result<i64, StoreError> {
    let to = last_version(migrations);
    let db = |source| StoreError::Database {
        path: path.to_owned(),
        source,
    };
    // A store already at this version is opened without the write lock, so
    // a reader such as `atomic runs show` never waits on a writer.
    let current: Option<i64> =
        sqlx::query("SELECT COALESCE(MAX(version), 0) AS v FROM atomic_store_version")
            .fetch_one(&mut *conn)
            .await
            .ok()
            .and_then(|row| row.try_get("v").ok());
    if current == Some(STORE_VERSION) {
        return Ok(STORE_VERSION);
    }
    // One writer at a time: BEGIN IMMEDIATE takes the write lock up front, so
    // two processes opening the store cannot both apply a migration.
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *conn)
        .await
        .map_err(db)?;
    let outcome = async {
        sqlx::query("CREATE TABLE IF NOT EXISTS atomic_store_version (version INTEGER NOT NULL)")
            .execute(&mut *conn)
            .await
            .map_err(db)?;
        let found: i64 =
            sqlx::query("SELECT COALESCE(MAX(version), 0) AS v FROM atomic_store_version")
                .fetch_one(&mut *conn)
                .await
                .map_err(db)?
                .try_get("v")
                .map_err(db)?;
        if found > to {
            return Err(StoreError::Newer {
                path: path.to_owned(),
                found,
                supported: to,
            });
        }
        no_gap(path, found, migrations)?;
        for m in migrations.iter().filter(|m| m.version > found) {
            for sql in m.sql {
                sqlx::query(*sql).execute(&mut *conn).await.map_err(db)?;
            }
        }
        if found < to {
            sqlx::query("DELETE FROM atomic_store_version")
                .execute(&mut *conn)
                .await
                .map_err(db)?;
            sqlx::query("INSERT INTO atomic_store_version (version) VALUES (?)")
                .bind(to)
                .execute(&mut *conn)
                .await
                .map_err(db)?;
        }
        Ok(to)
    }
    .await;
    let end = if outcome.is_ok() {
        "COMMIT"
    } else {
        "ROLLBACK"
    };
    sqlx::query(end).execute(&mut *conn).await.map_err(db)?;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(tag: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "atomic-store-{tag}-{}-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    async fn set_version(path: &Path, v: i64) {
        let mut c = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("connect");
        sqlx::query("UPDATE atomic_store_version SET version = ?")
            .bind(v)
            .execute(&mut c)
            .await
            .expect("update");
    }

    /// The migrations are versions 1 to the store version, each once, in
    /// order (the build checks this too).
    #[test]
    fn the_migrations_are_every_version_once_in_order() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert!(
            contiguous(MIGRATIONS) && STORE_VERSION == MIGRATIONS.len() as i64,
            "migrations {versions:?} are not exactly 1..={STORE_VERSION}"
        );
    }

    const fn m(version: i64, sql: &'static [&'static str]) -> Migration {
        Migration { version, sql }
    }

    #[test]
    fn the_first_gap_is_found_wherever_it_is() {
        let whole = [m(1, &[]), m(2, &[]), m(3, &[])];
        assert_eq!(first_gap(&whole, 0), None);
        assert_eq!(first_gap(&whole, 2), None);
        let gapped = [m(1, &[]), m(2, &[]), m(4, &[])];
        // A fresh store, or one below the gap, would skip version 3.
        assert_eq!(first_gap(&gapped, 0), Some(3));
        assert_eq!(first_gap(&gapped, 2), Some(3));
        // A store already past it has nothing to skip.
        assert_eq!(first_gap(&gapped, 3), None);
        // Repeated or out of order, no store below the last is migrated.
        let twice = [m(1, &[]), m(2, &[]), m(2, &[]), m(3, &[])];
        assert_eq!(first_gap(&twice, 1), Some(2));
        let swapped = [m(1, &[]), m(3, &[]), m(2, &[])];
        assert_eq!(first_gap(&swapped, 0), Some(1));
        // Even for a store at the last version, whose range is empty.
        assert_eq!(first_gap(&swapped, 2), Some(3));
        assert_eq!(first_gap(&twice, 3), Some(4));
        assert!(contiguous(&whole));
        assert!(!contiguous(&gapped) && !contiguous(&twice) && !contiguous(&swapped));
    }

    /// Two migrations with version 2 missing between them.
    const GAPPED: &[Migration] = &[
        m(1, &["CREATE TABLE gap_one (x INTEGER)"]),
        m(3, &["CREATE TABLE gap_three (x INTEGER)"]),
    ];

    async fn tables(path: &Path) -> Vec<String> {
        let mut c = SqliteConnection::connect(&format!("sqlite://{}", path.display()))
            .await
            .expect("connect");
        sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .fetch_all(&mut c)
            .await
            .expect("tables")
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect()
    }

    #[tokio::test]
    async fn a_store_is_never_migrated_across_a_gap() {
        // Fresh: refused, and nothing is created.
        let p = db("gap-fresh");
        match open_sqlite_with(&p, GAPPED).await {
            Err(StoreError::Gap {
                found: 0,
                missing: 2,
                supported: 3,
                ..
            }) => {}
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(tables(&p).await.is_empty(), "{:?}", tables(&p).await);
        let _ = std::fs::remove_file(&p);
        // At version 1: refused, and left at version 1.
        let p = db("gap-one");
        assert_eq!(open_sqlite_with(&p, &GAPPED[..1]).await.expect("to 1"), 1);
        assert!(matches!(
            open_sqlite_with(&p, GAPPED).await,
            Err(StoreError::Gap {
                found: 1,
                missing: 2,
                ..
            })
        ));
        assert_eq!(tables(&p).await, ["atomic_store_version", "gap_one"]);
        assert_eq!(
            open_sqlite_with(&p, &GAPPED[..1]).await.expect("still 1"),
            1
        );
        let _ = std::fs::remove_file(&p);
    }

    #[cfg(feature = "postgres-tests")]
    #[tokio::test]
    async fn a_postgres_store_is_never_migrated_across_a_gap() {
        let schema = crate::testing::schema("gap").await;
        let (url, name) = (schema.url(), schema.name());
        match open_postgres_with(url, name, GAPPED).await {
            Err(StoreError::Gap {
                found: 0,
                missing: 2,
                ..
            }) => {}
            other => panic!("expected a refusal, got {other:?}"),
        }
        let mut c = sqlx::PgConnection::connect(url).await.expect("connect");
        // Not even the schema was made.
        let schemas: i64 = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*)::BIGINT AS n FROM information_schema.schemata \
             WHERE schema_name = '{name}'"
        )))
        .fetch_one(&mut c)
        .await
        .expect("count")
        .get("n");
        assert_eq!(schemas, 0, "the schema was created");
        let count = |table: &'static str| {
            format!(
                "SELECT COUNT(*)::BIGINT AS n FROM information_schema.tables \
                 WHERE table_schema = '{name}' AND table_name = '{table}'"
            )
        };
        for table in ["atomic_store_version", "gap_one"] {
            let n: i64 = sqlx::query(sqlx::AssertSqlSafe(count(table)))
                .fetch_one(&mut c)
                .await
                .expect("count")
                .get("n");
            assert_eq!(n, 0, "{table} was created");
        }
        assert_eq!(
            open_postgres_with(url, name, &GAPPED[..1])
                .await
                .expect("to 1"),
            1
        );
        assert!(matches!(
            open_postgres_with(url, name, GAPPED).await,
            Err(StoreError::Gap {
                found: 1,
                missing: 2,
                ..
            })
        ));
        let n: i64 = sqlx::query(sqlx::AssertSqlSafe(count("gap_three")))
            .fetch_one(&mut c)
            .await
            .expect("count")
            .get("n");
        assert_eq!(n, 0, "the migration past the gap ran");
        let _ = c.close().await;
    }

    #[tokio::test]
    async fn a_new_store_migrates_to_the_current_version_and_reopens() {
        let p = db("new");
        assert_eq!(open_sqlite(&p).await.expect("open"), STORE_VERSION);
        assert_eq!(open_sqlite(&p).await.expect("reopen"), STORE_VERSION);
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn an_older_store_migrates_forward() {
        let p = db("older");
        open_sqlite(&p).await.expect("open");
        set_version(&p, 0).await;
        assert_eq!(open_sqlite(&p).await.expect("migrate"), STORE_VERSION);
        let _ = std::fs::remove_file(&p);
    }

    /// Question gates' tables (migration 7) reach a store at 5, which gets
    /// the executions table (6) on the way, and one at 6.
    #[tokio::test]
    async fn question_gates_migrate_onto_a_store_at_5_or_6() {
        for from in [5, 6] {
            let p = db(&format!("from-{from}"));
            open_sqlite(&p).await.expect("open");
            {
                use sqlx::Connection;
                let mut c = sqlx::SqliteConnection::connect(&format!("sqlite://{}", p.display()))
                    .await
                    .expect("connect");
                for table in ["atomic_gate_questions", "atomic_feedback"] {
                    sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE {table}")))
                        .execute(&mut c)
                        .await
                        .expect("drop");
                }
            }
            set_version(&p, from).await;
            assert_eq!(open_sqlite(&p).await.expect("migrate"), STORE_VERSION);
            {
                use sqlx::Connection;
                let mut c = sqlx::SqliteConnection::connect(&format!("sqlite://{}", p.display()))
                    .await
                    .expect("connect");
                for table in ["atomic_gate_questions", "atomic_feedback"] {
                    sqlx::query(sqlx::AssertSqlSafe(format!("SELECT * FROM {table}")))
                        .fetch_all(&mut c)
                        .await
                        .unwrap_or_else(|e| panic!("{table} after migrating from {from}: {e}"));
                }
            }
            let _ = std::fs::remove_file(&p);
        }
    }

    #[tokio::test]
    async fn a_store_newer_than_this_binary_is_refused() {
        let p = db("newer");
        open_sqlite(&p).await.expect("open");
        set_version(&p, STORE_VERSION + 1).await;
        match open_sqlite(&p).await {
            Err(StoreError::Newer {
                found, supported, ..
            }) => {
                assert_eq!((found, supported), (STORE_VERSION + 1, STORE_VERSION));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let _ = std::fs::remove_file(&p);
    }
}
