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
    sql: &'static str,
}

/// Every migration, in order. Append only: a released migration never changes.
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    // Published candidates (see `publication`).
    sql: "CREATE TABLE IF NOT EXISTS atomic_publications (
              publication_key TEXT PRIMARY KEY,
              digest TEXT NOT NULL,
              output TEXT NOT NULL
          )",
}];

/// The store version this binary writes and understands.
pub const STORE_VERSION: i64 = 1;

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
            sqlx::query(m.sql).execute(&mut *conn).await.map_err(db)?;
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
        std::env::temp_dir().join(format!(
            "atomic-store-{tag}-{}-{}.db",
            std::process::id(),
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
