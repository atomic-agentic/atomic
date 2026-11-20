// SPDX-License-Identifier: Apache-2.0

//! The fork creates exactly the DBOS system schema upstream durare creates.
//!
//! Other DBOS tools read this schema, and the fork must stay rebasable on
//! upstream, so the port to sqlx 0.9 may not change it. `dbos_schema/*.schema`
//! are dumps of the schema upstream durare creates at the fork's base commit,
//! written by the generator in `dbos_schema/upstream/` (see its manifest).
//! This test creates the schema with the fork and dumps it the same way.

include!("dbos_schema/dump.rs");

use std::sync::Arc;

async fn fork_dump(
    provider: Arc<dyn durare::StateProvider>,
    backend: &str,
    schema: &str,
) -> durare::Result<String> {
    let mut engine = durare::DurableEngine::new(provider).await?;
    register_dump(&mut engine);
    engine.launch().await?;
    let text = dump(&engine, backend, schema).await?;
    engine.shutdown(std::time::Duration::from_secs(5)).await?;
    Ok(text)
}

fn expected(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/dbos_schema")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sqlite_schema_matches_upstream() -> durare::Result<()> {
    let db = std::env::temp_dir().join(format!("atomic-dbos-schema-{}.db", std::process::id()));
    let provider = Arc::new(
        durare::SqliteProvider::connect(&format!("sqlite://{}?mode=rwc", db.display())).await?,
    );
    let got = fork_dump(provider, "sqlite", "main").await;
    let _ = std::fs::remove_file(&db);
    assert_eq!(got?, expected("sqlite.schema"));
    Ok(())
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_postgres_schema_matches_upstream() -> durare::Result<()> {
    let guard = atomic_durable::testing::schema("dbos_fork").await;
    let schema = guard.name().to_owned();
    let provider =
        Arc::new(durare::PostgresProvider::connect_with_schema(guard.url(), &schema).await?);
    let dumped = fork_dump(provider, "postgres", &schema).await;
    assert_eq!(dumped?, expected("postgres.schema"));
    Ok(())
}
