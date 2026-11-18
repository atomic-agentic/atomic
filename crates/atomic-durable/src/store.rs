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
];

/// The store version this binary writes and understands.
pub const STORE_VERSION: i64 = 2;

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
    let path = database.display().to_string();
    let db = |source| StoreError::Database {
        path: path.clone(),
        source,
    };
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(&format!("sqlite://{path}"))
        .map_err(db)?
        .create_if_missing(true);
    let mut conn = SqliteConnection::connect_with(&options).await.map_err(db)?;
    let result = migrate(&mut conn, &path).await;
    let _ = conn.close().await;
    result
}

/// Open atomic's store in PostgreSQL schema `schema` at `url`: refuse it if a
/// newer atomic wrote it, before anything is created; otherwise create the
/// schema and the version table if needed and migrate forward. Returns the
/// version it is now at.
pub async fn open_postgres(url: &str, schema: &str) -> Result<i64, StoreError> {
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
        if found > STORE_VERSION {
            return Err(StoreError::Newer {
                path: path.clone(),
                found,
                supported: STORE_VERSION,
            });
        }
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
        for m in MIGRATIONS.iter().filter(|m| m.version > found) {
            for sql in m.sql {
                sqlx::query(*sql).execute(&mut *tx).await.map_err(db)?;
            }
        }
        if found < STORE_VERSION {
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO {table} (version) VALUES ($1)"
            )))
            .bind(STORE_VERSION)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        tx.commit().await.map_err(db)?;
        Ok(STORE_VERSION)
    }
    .await;
    let _ = conn.close().await;
    result
}

async fn migrate(conn: &mut SqliteConnection, path: &str) -> Result<i64, StoreError> {
    let db = |source| StoreError::Database {
        path: path.to_owned(),
        source,
    };
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
        if found > STORE_VERSION {
            return Err(StoreError::Newer {
                path: path.to_owned(),
                found,
                supported: STORE_VERSION,
            });
        }
        for m in MIGRATIONS.iter().filter(|m| m.version > found) {
            for sql in m.sql {
                sqlx::query(*sql).execute(&mut *conn).await.map_err(db)?;
            }
        }
        if found < STORE_VERSION {
            sqlx::query("DELETE FROM atomic_store_version")
                .execute(&mut *conn)
                .await
                .map_err(db)?;
            sqlx::query("INSERT INTO atomic_store_version (version) VALUES (?)")
                .bind(STORE_VERSION)
                .execute(&mut *conn)
                .await
                .map_err(db)?;
        }
        Ok(STORE_VERSION)
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
