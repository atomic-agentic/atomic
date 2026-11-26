// SPDX-License-Identifier: Apache-2.0
//! Search only committed local graph items through the SQLite FTS5 index.

use super::{Actor, SqliteStore, StoreError, db};
use atomic_durable::store::graph_search_backfill_batch;
use serde::Serialize;
use sqlx::Row;
use std::sync::Arc;

/// One committed matching item. The excerpt contains plain text, never HTML.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchHit {
    /// Canonical graph item ID, including its punctuation.
    pub id: String,
    /// Current item title.
    pub title: String,
    /// Current item status.
    pub status: String,
    /// A bounded plain-text excerpt near the match.
    pub excerpt: String,
}

fn excerpt(value: String) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(240)
        .collect()
}

impl SqliteStore {
    /// Search committed graph items. An exact ID wins before full-text search;
    /// phrase syntax is quoted so caller punctuation cannot become FTS syntax.
    pub async fn search_items(
        &self,
        query: &str,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let query = query.trim();
        if query.is_empty() || query.len() > 256 || !(1..=100).contains(&limit) {
            return Err(db("search query or result limit is invalid"));
        }
        if status.is_some_and(|s| super::Status::parse(s).is_none()) {
            return Err(db("unknown graph item status"));
        }
        let query = query.to_owned();
        let status = status.map(str::to_owned);
        self.read(move |conn| Box::pin(async move {
            let exact = sqlx::query("SELECT id,title,body,status FROM atomic_graph_items WHERE id=? AND projected_gate_id IS NULL AND (? IS NULL OR status=?)")
                .bind(&query)
                .bind(&status)
                .bind(&status)
                .fetch_optional(&mut *conn)
                .await
                .map_err(db)?;
            if let Some(row) = exact {
                return Ok(vec![SearchHit {
                    id: row.try_get("id").map_err(db)?,
                    title: row.try_get("title").map_err(db)?,
                    status: row.try_get("status").map_err(db)?,
                    excerpt: excerpt(row.try_get("body").map_err(db)?),
                }]);
            }
            if !query.chars().any(char::is_alphanumeric) {
                return Ok(Vec::new());
            }
            let literal = format!("\"{}\"", query.replace('"', "\"\""));
            let rows = sqlx::query(
                "WITH matches AS (\
                   SELECT i.id,i.title,i.status,snippet(atomic_graph_search_fts,-1,'','',' … ',24) AS excerpt, \
                          0 AS source,bm25(atomic_graph_search_fts,10.0,1.0) AS score,0 AS release_epoch \
                   FROM atomic_graph_search_fts JOIN atomic_graph_search_keys k ON k.key=atomic_graph_search_fts.rowid \
                   JOIN atomic_graph_items i ON i.id=k.item_id \
                   WHERE atomic_graph_search_fts MATCH ? AND i.projected_gate_id IS NULL AND (? IS NULL OR i.status=?) \
                   UNION ALL \
                   SELECT i.id,i.title,i.status,snippet(atomic_graph_handoff_search_fts,0,'','',' … ',24) AS excerpt, \
                          1 AS source,bm25(atomic_graph_handoff_search_fts) AS score,k.release_epoch \
                   FROM atomic_graph_handoff_search_fts JOIN atomic_graph_handoff_search_keys k ON k.key=atomic_graph_handoff_search_fts.rowid \
                   JOIN atomic_graph_items i ON i.id=k.item_id \
                   WHERE atomic_graph_handoff_search_fts MATCH ? AND i.projected_gate_id IS NULL AND (? IS NULL OR i.status=?) \
                 ), ranked AS (\
                   SELECT id,title,status,excerpt,source,score, \
                          ROW_NUMBER() OVER (PARTITION BY id ORDER BY source,score,release_epoch DESC) AS pick \
                   FROM matches \
                 ) \
                 SELECT id,title,status,excerpt FROM ranked WHERE pick=1 ORDER BY source,score,id LIMIT ?",
            )
            .bind(&literal)
            .bind(&status)
            .bind(&status)
            .bind(&literal)
            .bind(&status)
            .bind(&status)
            .bind(limit as i64)
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?;
            rows.into_iter().map(|row| Ok(SearchHit {
                id: row.try_get("id").map_err(db)?,
                title: row.try_get("title").map_err(db)?,
                status: row.try_get("status").map_err(db)?,
                excerpt: excerpt(row.try_get("excerpt").map_err(db)?),
            })).collect()
        })).await
    }

    /// Clear and rebuild the SQLite index from canonical graph rows. The CLI
    /// authenticates the operator before reaching this method. Each rebuild
    /// batch commits its durable cursor independently.
    pub async fn rebuild_search_index(&self, actor: Actor) -> Result<(), StoreError> {
        self.actor_for_write(actor, "rebuild the graph search index")?;
        self.run(|conn| Box::pin(async move {
            sqlx::query("DELETE FROM atomic_graph_search_fts")
                .execute(&mut *conn).await.map_err(db)?;
            sqlx::query("DELETE FROM atomic_graph_search_keys")
                .execute(&mut *conn).await.map_err(db)?;
            sqlx::query("DELETE FROM atomic_graph_handoff_search_fts")
                .execute(&mut *conn).await.map_err(db)?;
            sqlx::query("DELETE FROM atomic_graph_handoff_search_keys")
                .execute(&mut *conn).await.map_err(db)?;
            sqlx::query("INSERT INTO atomic_graph_handoff_search_keys(item_id,release_epoch) SELECT h.item_id,h.release_epoch FROM atomic_graph_handoffs h JOIN atomic_graph_items i ON i.id=h.item_id ORDER BY h.item_id,h.release_epoch")
                .execute(&mut *conn).await.map_err(db)?;
            sqlx::query("INSERT INTO atomic_graph_handoff_search_fts(rowid,body) SELECT k.key,COALESCE(json_extract(h.record_json,'$.attempted'),'') || ' ' || COALESCE(json_extract(h.record_json,'$.failed'),'') || ' ' || COALESCE(json_extract(h.record_json,'$.learned'),'') FROM atomic_graph_handoff_search_keys k JOIN atomic_graph_handoffs h ON h.item_id=k.item_id AND h.release_epoch=k.release_epoch")
                .execute(&mut *conn).await.map_err(db)?;
            sqlx::query("UPDATE atomic_graph_search_progress SET last_id='',complete=0 WHERE singleton=1")
                .execute(&mut *conn).await.map_err(db)?;
            Ok(())
        })).await?;
        let connection = Arc::clone(&self.conn);
        let (answer, received) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let result = (|| {
                let mut conn = connection.lock().map_err(db)?;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(db)?;
                runtime.block_on(async {
                    while !graph_search_backfill_batch(&mut conn, 128)
                        .await
                        .map_err(db)?
                    {}
                    Ok::<(), StoreError>(())
                })
            })();
            let _ = answer.send(result);
        });
        received
            .await
            .map_err(|_| db("graph search rebuild worker panicked"))?
    }
}
