// SPDX-License-Identifier: Apache-2.0

//! The PostgreSQL test database, for suites behind the `postgres-tests`
//! feature.
//!
//! A suite that cannot reach its backend fails; it never skips. With the
//! feature on, [`database`] panics, naming what is missing, when
//! `ATOMIC_TEST_DATABASE_URL` is unset or the database it names does not
//! answer, so a run without the database fails every Postgres test instead of
//! passing it unrun.

use std::time::Duration;

/// The variable that names the test database.
pub const DATABASE_VAR: &str = "ATOMIC_TEST_DATABASE_URL";

/// How long the database has to accept a connection.
const REACH: Duration = Duration::from_secs(10);

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

/// The test database's URL, once a connection to it succeeds. Panics, naming
/// the database, when it does not answer.
pub async fn database() -> String {
    let url = database_url();
    let reached = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(REACH)
        .connect(&url)
        .await;
    match reached {
        Ok(pool) => {
            pool.close().await;
            url
        }
        Err(e) => panic!(
            "the PostgreSQL test database {} ({DATABASE_VAR}) cannot be reached: {e}",
            redacted(&url)
        ),
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
