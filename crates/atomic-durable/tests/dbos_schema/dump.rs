// SPDX-License-Identifier: Apache-2.0
//
// Shared by tests/dbos_schema.rs (against atomic's durare fork) and by the
// generator in upstream/ (against upstream durare), so both dump the schema
// the same way. Included with `include!`, not compiled on its own.

/// Register the workflow that dumps the system schema from inside a
/// transaction step, so it reads through the provider's own connection.
pub fn register_dump(engine: &mut durare::DurableEngine) {
    engine.register(
        "schema_dump",
        |ctx: durare::DurableContext, (backend, schema): (String, String)| async move {
            ctx.transaction("dump", move |tx| {
                let (backend, schema) = (backend.clone(), schema.clone());
                Box::pin(async move { dump_in(tx, &backend, &schema).await })
            })
            .await
        },
    );
}

/// The normalized schema, one line per object, sorted.
pub async fn dump(
    engine: &durare::DurableEngine,
    backend: &str,
    schema: &str,
) -> durare::Result<String> {
    let id = format!(
        "schema-dump-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let lines: Vec<String> = engine
        .start::<_, Vec<String>>(
            "schema_dump",
            (backend.to_string(), schema.to_string()),
            durare::WorkflowOptions::with_id(&id),
        )
        .await?
        .result()
        .await?;
    // The dump workflow's own rows are data, not schema; only shapes are kept.
    Ok(lines.join("\n") + "\n")
}

async fn dump_in(tx: &mut durare::Tx<'_>, backend: &str, schema: &str) -> durare::Result<Vec<String>> {
    let s = durare::Param::Text(schema.to_string());
    let queries: Vec<(&str, String, Vec<durare::Param>)> = match backend {
        "sqlite" => vec![(
            "object",
            "SELECT type || ' ' || name || ' ON ' || tbl_name || ': ' || COALESCE(sql, '') AS line \
             FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name"
                .into(),
            vec![],
        )],
        "postgres" => vec![
            (
                "column",
                "SELECT table_name || '.' || column_name || ' #' || ordinal_position || ' ' || data_type \
                 || ' null=' || is_nullable || ' default=' || COALESCE(column_default, '') AS line \
                 FROM information_schema.columns WHERE table_schema = ? \
                 ORDER BY table_name, ordinal_position"
                    .into(),
                vec![s.clone()],
            ),
            (
                "index",
                "SELECT tablename || ' ' || indexname || ': ' || indexdef AS line \
                 FROM pg_indexes WHERE schemaname = ? ORDER BY tablename, indexname"
                    .into(),
                vec![s.clone()],
            ),
            (
                "constraint",
                "SELECT c.conrelid::regclass::text || ' ' || c.conname || ': ' || pg_get_constraintdef(c.oid) AS line \
                 FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace \
                 WHERE n.nspname = ? ORDER BY 1"
                    .into(),
                vec![s.clone()],
            ),
            (
                "function",
                "SELECT p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ') returns ' \
                 || pg_get_function_result(p.oid) || ' md5=' || md5(p.prosrc) AS line \
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = ? ORDER BY 1"
                    .into(),
                vec![s.clone()],
            ),
            (
                "trigger",
                "SELECT c.relname || ' ' || t.tgname || ': ' || pg_get_triggerdef(t.oid) AS line \
                 FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = ? AND NOT t.tgisinternal ORDER BY 1"
                    .into(),
                vec![s.clone()],
            ),
        ],
        other => return Err(durare::Error::app(format!("unknown backend {other}"))),
    };
    let mut out = Vec::new();
    for (kind, sql, params) in queries {
        for row in tx.query_all(&sql, &params).await? {
            let line: String = row.try_get("line")?;
            // Names the schema was created under, so dumps from different
            // schemas compare equal.
            let line = line.replace(&format!("{schema}."), "<schema>.").replace(&format!("\"{schema}\"."), "<schema>.");
            out.push(format!("{kind} {line}"));
        }
    }
    Ok(out)
}
