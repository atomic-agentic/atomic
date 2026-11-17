// SPDX-License-Identifier: Apache-2.0

//! Write `sqlite.schema` (and, given a Postgres URL, `postgres.schema`) as
//! upstream durare creates them.

include!("../../dump.rs");

use std::sync::Arc;

#[tokio::main]
async fn main() -> durare::Result<()> {
    let mut args = std::env::args().skip(1);
    let out = std::path::PathBuf::from(args.next().expect("output directory"));
    let pg = args.next();

    let db = std::env::temp_dir().join(format!("dbos-schema-upstream-{}.db", std::process::id()));
    let provider = Arc::new(
        durare::SqliteProvider::connect(&format!("sqlite://{}?mode=rwc", db.display())).await?,
    );
    write(&out.join("sqlite.schema"), provider, "sqlite", "main").await?;
    let _ = std::fs::remove_file(&db);

    if let Some(url) = pg {
        let schema = format!("dbos_upstream_{}", std::process::id());
        let provider = Arc::new(durare::PostgresProvider::connect_with_schema(&url, &schema).await?);
        write(&out.join("postgres.schema"), provider, "postgres", &schema).await?;
    }
    Ok(())
}

async fn write(
    path: &std::path::Path,
    provider: Arc<dyn durare::StateProvider>,
    backend: &str,
    schema: &str,
) -> durare::Result<()> {
    let mut engine = durare::DurableEngine::new(provider).await?;
    register_dump(&mut engine);
    engine.launch().await?;
    let text = dump(&engine, backend, schema).await?;
    std::fs::write(path, text).map_err(|e| durare::Error::app(e.to_string()))?;
    engine.shutdown(std::time::Duration::from_secs(5)).await
}
