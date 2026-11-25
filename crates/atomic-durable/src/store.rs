// SPDX-License-Identifier: Apache-2.0

//! atomic's own tables in the project database, and their versions.
//!
//! The database file holds durare's system tables and atomic's own. durare
//! versions its tables itself; atomic's are versioned here, in
//! `atomic_store_version`, and migrate forward when a store is opened. A store
//! written by a newer atomic is refused: this binary does not know what the
//! newer migrations changed, so it starts nothing rather than guess.

use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection};
use std::path::Path;
use std::str::FromStr;

/// One forward migration of atomic's tables.
#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    name: &'static str,
    /// Its statements, run in order in one transaction. Each is valid on
    /// SQLite and PostgreSQL alike.
    sql: &'static [&'static str],
}

/// Every migration, in order. Append only: a released migration never changes.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "publications",
        // Published candidates (see `publication`).
        sql: &["CREATE TABLE IF NOT EXISTS atomic_publications (
              publication_key TEXT PRIMARY KEY,
              digest TEXT NOT NULL,
              output TEXT NOT NULL
          )"],
    },
    Migration {
        version: 2,
        name: "gates",
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
        name: "usage-ledger",
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
        name: "operators",
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
        name: "usage-heartbeats",
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
        name: "executions",
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
        name: "question-gates",
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
        name: "park-releases",
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
        name: "park-projects",
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
        name: "run-projects",
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
    Migration {
        version: 11,
        name: "thread-index",
        // Earlier runtimes made these lazily. A legacy baseline accepts
        // either their absence or their exact old shape; this version makes
        // them part of the store's explicit schema.
        sql: &[
            "CREATE TABLE IF NOT EXISTS atomic_threads (
                candidate TEXT PRIMARY KEY,
                run TEXT NOT NULL,
                step TEXT NOT NULL,
                thread_id TEXT NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS atomic_threads_by_thread ON atomic_threads (thread_id)",
        ],
    },
    Migration {
        version: 12,
        name: "work-graph",
        sql: &[
            "CREATE TABLE IF NOT EXISTS atomic_graph_items (id TEXT PRIMARY KEY, title TEXT NOT NULL, body TEXT NOT NULL, kind TEXT NOT NULL, priority BIGINT NOT NULL, status TEXT NOT NULL, parent TEXT, trust TEXT NOT NULL, created_by TEXT NOT NULL, created_at BIGINT NOT NULL, closed_at BIGINT, terminal_outcome TEXT, due_at BIGINT, imported_json TEXT, claim_epoch BIGINT NOT NULL DEFAULT 0, last_finish_session BIGINT, last_finish_epoch BIGINT, last_finish_claimant TEXT, last_finish_acquisition TEXT)",
            "CREATE TABLE IF NOT EXISTS atomic_graph_edges (source TEXT NOT NULL, kind TEXT NOT NULL, target TEXT NOT NULL, PRIMARY KEY (source, kind, target))",
            "CREATE TABLE IF NOT EXISTS atomic_graph_blocked (source TEXT NOT NULL, target TEXT NOT NULL, PRIMARY KEY (source, target))",
            "CREATE TABLE IF NOT EXISTS atomic_graph_notes (item_id TEXT NOT NULL, ordinal BIGINT NOT NULL, text TEXT NOT NULL, author TEXT NOT NULL, trust TEXT NOT NULL, PRIMARY KEY (item_id, ordinal))",
            "CREATE TABLE IF NOT EXISTS atomic_graph_leases (item_id TEXT PRIMARY KEY, claimant TEXT NOT NULL, session BIGINT NOT NULL, epoch BIGINT NOT NULL, acquisition TEXT NOT NULL, expires_at BIGINT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS atomic_graph_acquisitions (acquisition TEXT PRIMARY KEY, item_id TEXT NOT NULL, claimant TEXT NOT NULL, session BIGINT NOT NULL, epoch BIGINT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS atomic_graph_memories (key TEXT PRIMARY KEY, value TEXT NOT NULL, trust TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS atomic_graph_requests (request_id TEXT PRIMARY KEY, kind TEXT NOT NULL, value TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS atomic_graph_commit_links (item_id TEXT NOT NULL, sha TEXT NOT NULL, indexed BIGINT NOT NULL, on_branch BIGINT NOT NULL, PRIMARY KEY (item_id, sha))",
            "CREATE TABLE IF NOT EXISTS atomic_graph_policy (id BIGINT PRIMARY KEY, requires_commit BIGINT NOT NULL, requires_verified_commit BIGINT NOT NULL)",
            "CREATE INDEX IF NOT EXISTS atomic_graph_edges_by_target ON atomic_graph_edges (target, kind)",
            "CREATE INDEX IF NOT EXISTS atomic_graph_ready_order ON atomic_graph_items (status, priority, id)",
        ],
    },
    Migration {
        version: 13,
        name: "work-graph-human-leases",
        sql: &[
            "CREATE TABLE IF NOT EXISTS atomic_graph_human_leases (item_id TEXT PRIMARY KEY, principal TEXT NOT NULL, claimant TEXT NOT NULL, session BIGINT NOT NULL, epoch BIGINT NOT NULL, acquisition TEXT NOT NULL)",
        ],
    },
    Migration {
        version: 14,
        name: "work-graph-item-close-policy",
        sql: &[
            "CREATE TABLE IF NOT EXISTS atomic_graph_item_policy (item_id TEXT PRIMARY KEY, requires_commit BIGINT NOT NULL, requires_verified_commit BIGINT NOT NULL, authorized_branch TEXT, set_by TEXT NOT NULL, set_at BIGINT NOT NULL)",
        ],
    },
    Migration {
        version: 15,
        name: "work-graph-epic-run-admission",
        sql: &[
            "CREATE TABLE IF NOT EXISTS atomic_graph_run_authorizations (run_id TEXT PRIMARY KEY, epic_id TEXT NOT NULL, principal TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS atomic_graph_epic_runs (epic_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, state TEXT NOT NULL)",
        ],
    },
    Migration {
        version: 16,
        name: "work-graph-verified-commit-proof",
        sql: &[
            "ALTER TABLE atomic_graph_commit_links ADD COLUMN verified_branch TEXT",
            "ALTER TABLE atomic_graph_commit_links ADD COLUMN verified_head TEXT",
            "ALTER TABLE atomic_graph_commit_links ADD COLUMN pinned_base TEXT",
        ],
    },
    Migration {
        version: 17,
        name: "work-graph-goals",
        sql: &[
            "CREATE TABLE IF NOT EXISTS atomic_graph_goal_definitions (item_id TEXT PRIMARY KEY, definition_json TEXT NOT NULL, active BIGINT NOT NULL, revision BIGINT NOT NULL, set_by TEXT NOT NULL, set_at BIGINT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS atomic_graph_goal_wakes (goal_id TEXT NOT NULL, wake_id TEXT NOT NULL, happened_at BIGINT NOT NULL, outcome TEXT NOT NULL, reason TEXT NOT NULL, spent_tokens BIGINT NOT NULL, PRIMARY KEY (goal_id,wake_id))",
            "CREATE TABLE IF NOT EXISTS atomic_graph_goal_measure_samples (goal_id TEXT NOT NULL, wake_id TEXT NOT NULL, sampled_at BIGINT NOT NULL, value_json TEXT NOT NULL, PRIMARY KEY (goal_id,wake_id))",
            "CREATE TABLE IF NOT EXISTS atomic_graph_goal_feedback (goal_id TEXT NOT NULL, ordinal BIGINT NOT NULL, request_id TEXT NOT NULL UNIQUE, kind TEXT NOT NULL, text TEXT NOT NULL, principal TEXT NOT NULL, written_at BIGINT NOT NULL, PRIMARY KEY (goal_id,ordinal))",
            "CREATE TABLE IF NOT EXISTS atomic_graph_goal_ignores (goal_id TEXT NOT NULL, kind TEXT NOT NULL, value TEXT NOT NULL, expires_at BIGINT NOT NULL, reason TEXT NOT NULL, PRIMARY KEY (goal_id,kind,value))",
            "CREATE INDEX IF NOT EXISTS atomic_graph_goal_wakes_by_time ON atomic_graph_goal_wakes (goal_id,happened_at)",
        ],
    },
    Migration {
        version: 18,
        name: "work-graph-item-criteria",
        sql: &[
            "CREATE TABLE IF NOT EXISTS atomic_graph_item_criteria (item_id TEXT PRIMARY KEY, criteria_json TEXT NOT NULL, set_by TEXT NOT NULL, set_at BIGINT NOT NULL)",
        ],
    },
    Migration {
        version: 19,
        name: "delivery-effect-intents",
        sql: &[
            "CREATE TABLE atomic_delivery_effects (provider_key TEXT PRIMARY KEY, plan TEXT NOT NULL, gate_id TEXT NOT NULL, principal TEXT NOT NULL, command_id TEXT NOT NULL, receipt TEXT)",
        ],
    },
    Migration {
        version: 20,
        name: "delivery-effect-stable-key-aliases",
        sql: &[
            "CREATE TABLE atomic_delivery_effect_aliases (stable_key TEXT PRIMARY KEY, provider_key TEXT NOT NULL UNIQUE)",
            "CREATE TABLE atomic_delivery_effect_alias_backfill (id BIGINT PRIMARY KEY, complete BIGINT NOT NULL)",
        ],
    },
    Migration {
        version: 21,
        name: "delivery-queue-observations",
        sql: &[
            "CREATE TABLE atomic_delivery_queue_observations (delivery_run TEXT PRIMARY KEY, plan_digest TEXT NOT NULL, repository TEXT NOT NULL, pull_request_number BIGINT NOT NULL, head TEXT NOT NULL, status TEXT NOT NULL, provider_event_id TEXT, merge_sha TEXT, happened_at BIGINT, observed_at BIGINT NOT NULL, detail TEXT NOT NULL)",
            "CREATE UNIQUE INDEX atomic_delivery_queue_observations_event ON atomic_delivery_queue_observations (repository,provider_event_id)",
            "CREATE INDEX atomic_delivery_queue_observations_period ON atomic_delivery_queue_observations (status,happened_at)",
        ],
    },
];

const HISTORY_TABLE: &str = "CREATE TABLE atomic_store_history (
    version BIGINT PRIMARY KEY,
    name TEXT NOT NULL,
    applied_at BIGINT NOT NULL,
    checksum TEXT
)";
// v0.1 wrote the scalar marker through version 10. Version 11 is the first
// version that must always have a history; it cannot be re-baselined.
const FIRST_HISTORY_VERSION: i64 = 11;

fn history_required(version: i64) -> bool {
    version >= FIRST_HISTORY_VERSION
}

fn checksum(migration: &Migration) -> String {
    let mut hash = Sha256::new();
    for sql in migration.sql {
        hash.update((sql.len() as u64).to_be_bytes());
        hash.update(sql.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

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

fn applied_at(path: &str) -> Result<i64, StoreError> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| StoreError::Clock {
            path: path.to_owned(),
            reason: e.to_string(),
        })?
        .as_millis();
    i64::try_from(millis).map_err(|e| StoreError::Clock {
        path: path.to_owned(),
        reason: e.to_string(),
    })
}

fn validate_history(
    path: &str,
    found: i64,
    rows: &[(i64, String, Option<String>)],
    migrations: &[Migration],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Err(StoreError::History {
            path: path.to_owned(),
            version: found,
            reason: "history has no recorded version".into(),
        });
    }
    let mut prior = 0;
    for (index, (version, name, digest)) in rows.iter().enumerate() {
        if *version > last_version(migrations) {
            return Err(StoreError::Newer {
                path: path.to_owned(),
                found: *version,
                supported: last_version(migrations),
            });
        }
        let baseline = index == 0
            && *version > 0
            && *version < FIRST_HISTORY_VERSION
            && name == &format!("legacy-baseline-v{version}")
            && digest.is_none();
        if !baseline && *version != prior + 1 {
            return Err(StoreError::History {
                path: path.to_owned(),
                version: prior + 1,
                reason: format!("history skips version {}", prior + 1),
            });
        }
        if !baseline {
            let Some(migration) = migrations.iter().find(|m| m.version == *version) else {
                return Err(StoreError::History {
                    path: path.to_owned(),
                    version: *version,
                    reason: "no embedded migration exists".into(),
                });
            };
            let expected = checksum(migration);
            if name != migration.name || digest.as_deref() != Some(expected.as_str()) {
                return Err(StoreError::History {
                    path: path.to_owned(),
                    version: *version,
                    reason: "name or SQL checksum differs from this binary".into(),
                });
            }
        }
        prior = *version;
    }
    if prior != found {
        return Err(StoreError::History {
            path: path.to_owned(),
            version: found,
            reason: format!("history ends at {prior}, but the legacy marker says {found}"),
        });
    }
    Ok(())
}

async fn sqlite_objects(
    conn: &mut SqliteConnection,
) -> Result<Vec<(String, String, String)>, sqlx::Error> {
    let mut objects = Vec::new();
    for row in
        sqlx::query("SELECT name, type AS kind, sql FROM sqlite_master WHERE sql IS NOT NULL")
            .fetch_all(conn)
            .await?
    {
        let name: String = row.try_get("name")?;
        if name.starts_with("atomic_") {
            let kind: String = row.try_get("kind")?;
            let sql: String = row.try_get("sql")?;
            objects.push((
                name,
                kind,
                sql.split_whitespace().collect::<Vec<_>>().join(" "),
            ));
        }
    }
    objects.sort();
    Ok(objects)
}

async fn verify_legacy_sqlite(
    conn: &mut SqliteConnection,
    path: &str,
    found: i64,
    migrations: &[Migration],
) -> Result<(), StoreError> {
    let fail = |reason: String| StoreError::Shape {
        path: path.to_owned(),
        version: found,
        reason,
    };
    let mut expected = SqliteConnection::connect("sqlite::memory:")
        .await
        .map_err(|e| fail(format!("cannot build a reference schema: {e}")))?;
    sqlx::query("CREATE TABLE IF NOT EXISTS atomic_store_version (version INTEGER NOT NULL)")
        .execute(&mut expected)
        .await
        .map_err(|e| fail(format!("cannot build a reference marker: {e}")))?;
    for migration in migrations.iter().filter(|m| m.version <= found) {
        for sql in migration.sql {
            sqlx::query(*sql)
                .execute(&mut expected)
                .await
                .map_err(|e| {
                    fail(format!(
                        "cannot build reference version {}: {e}",
                        migration.version
                    ))
                })?;
        }
    }
    let mut wanted = sqlite_objects(&mut expected)
        .await
        .map_err(|e| fail(format!("cannot read reference schema: {e}")))?;
    let mut actual = sqlite_objects(conn)
        .await
        .map_err(|e| fail(format!("cannot read existing schema: {e}")))?;
    // In versions before 11 these two objects were made lazily. Check their
    // shape if present, and let version 11 complete the pair.
    if found < 11 && std::ptr::eq(migrations.as_ptr(), MIGRATIONS.as_ptr()) {
        for sql in MIGRATIONS[10].sql {
            sqlx::query(*sql)
                .execute(&mut expected)
                .await
                .map_err(|e| fail(format!("cannot build reference thread index: {e}")))?;
        }
        let optional = sqlite_objects(&mut expected)
            .await
            .map_err(|e| fail(format!("cannot read reference thread index: {e}")))?;
        for object in optional
            .into_iter()
            .filter(|(name, _, _)| name == "atomic_threads" || name == "atomic_threads_by_thread")
        {
            if let Some(index) = actual.iter().position(|item| item.0 == object.0) {
                if actual[index] != object {
                    return Err(fail(format!(
                        "{} differs from its known legacy shape",
                        object.0
                    )));
                }
                actual.remove(index);
            }
        }
        wanted
            .retain(|(name, _, _)| name != "atomic_threads" && name != "atomic_threads_by_thread");
    }
    if actual != wanted {
        let missing = wanted.iter().find(|object| !actual.contains(object));
        let extra = actual.iter().find(|object| !wanted.contains(object));
        return Err(fail(format!(
            "atomic objects differ (missing or changed: {}; extra or changed: {})",
            missing.map_or("none", |x| x.0.as_str()),
            extra.map_or("none", |x| x.0.as_str())
        )));
    }
    Ok(())
}

async fn pg_objects(
    conn: &mut sqlx::PgConnection,
    schema: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let mut objects = Vec::new();
    let tables = sqlx::query(
        "SELECT c.relname AS name, \
         CASE WHEN n.oid = pg_catalog.pg_my_temp_schema() AND c.relpersistence = 't' \
              THEN 'p' ELSE c.relpersistence END::TEXT AS persistence, \
         c.relrowsecurity AS row_security, c.relforcerowsecurity AS force_row_security \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind = 'r' AND c.relname LIKE 'atomic\\_%' ESCAPE '\\' \
         ORDER BY c.relname",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await?;
    for table in tables {
        let name: String = table.try_get("name")?;
        let persistence: String = table.try_get("persistence")?;
        let row_security: bool = table.try_get("row_security")?;
        let force_row_security: bool = table.try_get("force_row_security")?;
        let columns = sqlx::query(
            "SELECT a.attname AS name, pg_catalog.format_type(a.atttypid, a.atttypmod) AS ty, \
             a.attnotnull AS required, COALESCE(pg_catalog.pg_get_expr(d.adbin, d.adrelid), '') AS default_value, \
             a.attidentity::TEXT AS identity_kind, a.attgenerated::TEXT AS generated_kind, \
             a.attcollation::regcollation::TEXT AS collation \
             FROM pg_catalog.pg_attribute a \
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum \
             WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
        )
        .bind(schema)
        .bind(&name)
        .fetch_all(&mut *conn)
        .await?;
        let mut fields = Vec::new();
        for column in columns {
            fields.push(format!(
                "{}:{}:{}:{}:{}:{}:{}",
                column.try_get::<String, _>("name")?,
                column.try_get::<String, _>("ty")?,
                column.try_get::<bool, _>("required")?,
                column.try_get::<String, _>("default_value")?,
                column.try_get::<String, _>("identity_kind")?,
                column.try_get::<String, _>("generated_kind")?,
                column.try_get::<String, _>("collation")?
            ));
        }
        objects.push(format!(
            "table:{name}:{persistence}:{row_security}:{force_row_security}:{}",
            fields.join("|")
        ));
    }
    let other_relations = sqlx::query(
        "SELECT c.relname AS name, c.relkind::TEXT AS kind FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname LIKE 'atomic\\_%' ESCAPE '\\' \
         AND c.relkind NOT IN ('r', 'i', 'I') ORDER BY c.relname",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await?;
    for relation in other_relations {
        objects.push(format!(
            "relation:{}:{}",
            relation.try_get::<String, _>("name")?,
            relation.try_get::<String, _>("kind")?
        ));
    }
    let indexes = sqlx::query(
        "SELECT ic.relname AS name, tc.relname AS table_name, i.indisunique AS unique_key, \
         i.indisprimary AS primary_key, i.indkey::TEXT AS keys, \
         COALESCE(pg_catalog.pg_get_expr(i.indpred, i.indrelid), '') AS predicate, \
         am.amname AS method, i.indisvalid AS valid, i.indisready AS ready, \
         i.indislive AS live, i.indnullsnotdistinct AS nulls_not_distinct, \
         COALESCE((SELECT string_agg(pg_catalog.pg_get_indexdef(i.indexrelid, key_no, true), \
                          '|' ORDER BY key_no) \
                   FROM generate_series(1, i.indnkeyatts) AS key_no), '') AS key_definitions \
         FROM pg_catalog.pg_index i \
         JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
         JOIN pg_catalog.pg_class tc ON tc.oid = i.indrelid \
         JOIN pg_catalog.pg_am am ON am.oid = ic.relam \
         JOIN pg_catalog.pg_namespace n ON n.oid = tc.relnamespace \
         WHERE n.nspname = $1 \
         AND (tc.relname LIKE 'atomic\\_%' ESCAPE '\\' \
              OR ic.relname LIKE 'atomic\\_%' ESCAPE '\\') \
         ORDER BY ic.relname",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await?;
    for index in indexes {
        objects.push(format!(
            "index:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            index.try_get::<String, _>("name")?,
            index.try_get::<String, _>("table_name")?,
            index.try_get::<bool, _>("unique_key")?,
            index.try_get::<bool, _>("primary_key")?,
            index.try_get::<String, _>("keys")?,
            index.try_get::<String, _>("predicate")?,
            index.try_get::<String, _>("method")?,
            index.try_get::<bool, _>("valid")?,
            index.try_get::<bool, _>("ready")?,
            index.try_get::<bool, _>("live")?,
            index.try_get::<bool, _>("nulls_not_distinct")?,
            index.try_get::<String, _>("key_definitions")?
        ));
    }
    let constraints = sqlx::query(
        "SELECT co.conname AS name, co.contype::TEXT AS kind, \
         pg_catalog.pg_get_constraintdef(co.oid, true) AS definition, \
         co.convalidated AS validated \
         FROM pg_catalog.pg_constraint co \
         JOIN pg_catalog.pg_class c ON c.oid = co.conrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname LIKE 'atomic\\_%' ESCAPE '\\' \
         ORDER BY co.conname",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await?;
    for constraint in constraints {
        objects.push(format!(
            "constraint:{}:{}:{}:{}",
            constraint.try_get::<String, _>("name")?,
            constraint.try_get::<String, _>("kind")?,
            constraint.try_get::<String, _>("definition")?,
            constraint.try_get::<bool, _>("validated")?
        ));
    }
    let triggers = sqlx::query(
        "SELECT t.tgname AS name FROM pg_catalog.pg_trigger t \
         JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname LIKE 'atomic\\_%' ESCAPE '\\' \
         AND NOT t.tgisinternal ORDER BY t.tgname",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await?;
    for trigger in triggers {
        objects.push(format!("trigger:{}", trigger.try_get::<String, _>("name")?));
    }
    let rules = sqlx::query(
        "SELECT r.rulename AS name FROM pg_catalog.pg_rewrite r \
         JOIN pg_catalog.pg_class c ON c.oid = r.ev_class \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname LIKE 'atomic\\_%' ESCAPE '\\' \
         AND r.rulename <> '_RETURN' ORDER BY r.rulename",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await?;
    for rule in rules {
        objects.push(format!("rule:{}", rule.try_get::<String, _>("name")?));
    }
    objects.sort();
    Ok(objects)
}

async fn verify_legacy_pg(
    conn: &mut sqlx::PgConnection,
    path: &str,
    schema: &str,
    found: i64,
    migrations: &[Migration],
) -> Result<(), StoreError> {
    let fail = |reason: String| StoreError::Shape {
        path: path.to_owned(),
        version: found,
        reason,
    };
    // pg_temp is per connection and requires no database CREATE privilege.
    // Roll back the reference objects before applying real migrations: even
    // with the project schema in search_path, PostgreSQL resolves unqualified
    // table names in pg_temp first.
    sqlx::query("SAVEPOINT atomic_legacy_reference")
        .execute(&mut *conn)
        .await
        .map_err(|e| fail(format!("cannot start reference savepoint: {e}")))?;
    sqlx::query("SET LOCAL search_path TO pg_temp")
        .execute(&mut *conn)
        .await
        .map_err(|e| fail(format!("cannot select reference schema: {e}")))?;
    sqlx::query("CREATE TABLE atomic_store_version (version BIGINT NOT NULL)")
        .execute(&mut *conn)
        .await
        .map_err(|e| fail(format!("cannot build reference marker: {e}")))?;
    for migration in migrations.iter().filter(|m| m.version <= found) {
        for sql in migration.sql {
            sqlx::query(*sql).execute(&mut *conn).await.map_err(|e| {
                fail(format!(
                    "cannot build reference version {}: {e}",
                    migration.version
                ))
            })?;
        }
    }
    let reference: String = sqlx::query("SELECT pg_my_temp_schema()::regnamespace::TEXT AS name")
        .fetch_one(&mut *conn)
        .await
        .and_then(|row| row.try_get("name"))
        .map_err(|e| fail(format!("cannot identify reference schema: {e}")))?;
    let mut wanted = pg_objects(conn, &reference)
        .await
        .map_err(|e| fail(format!("cannot read reference schema: {e}")))?;
    let mut actual = pg_objects(conn, schema)
        .await
        .map_err(|e| fail(format!("cannot read existing schema: {e}")))?;
    if found < 11 && std::ptr::eq(migrations.as_ptr(), MIGRATIONS.as_ptr()) {
        for sql in MIGRATIONS[10].sql {
            sqlx::query(*sql)
                .execute(&mut *conn)
                .await
                .map_err(|e| fail(format!("cannot build reference thread index: {e}")))?;
        }
        let optional = pg_objects(conn, &reference)
            .await
            .map_err(|e| fail(format!("cannot read reference thread index: {e}")))?;
        for item in optional.into_iter().filter(|item| {
            item.starts_with("table:atomic_threads:")
                || item.starts_with("index:atomic_threads_")
                || item.starts_with("constraint:atomic_threads_")
        }) {
            if let Some(index) = actual
                .iter()
                .position(|candidate| candidate.split(':').take(2).eq(item.split(':').take(2)))
            {
                if actual[index] != item {
                    return Err(fail(format!(
                        "{} differs from its known legacy shape",
                        item
                    )));
                }
                actual.remove(index);
            }
        }
        wanted.retain(|item| {
            !item.starts_with("table:atomic_threads:")
                && !item.starts_with("index:atomic_threads_")
                && !item.starts_with("constraint:atomic_threads_")
        });
    }
    if actual != wanted {
        return Err(fail(format!(
            "atomic objects differ (missing or changed: {}; extra or changed: {})",
            wanted
                .iter()
                .find(|item| !actual.contains(item))
                .map_or("none", String::as_str),
            actual
                .iter()
                .find(|item| !wanted.contains(item))
                .map_or("none", String::as_str)
        )));
    }
    sqlx::query("ROLLBACK TO SAVEPOINT atomic_legacy_reference")
        .execute(&mut *conn)
        .await
        .map_err(|e| fail(format!("cannot remove reference objects: {e}")))?;
    sqlx::query("RELEASE SAVEPOINT atomic_legacy_reference")
        .execute(&mut *conn)
        .await
        .map_err(|e| fail(format!("cannot release reference savepoint: {e}")))?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL search_path TO {schema}"
    )))
    .execute(&mut *conn)
    .await
    .map_err(|e| fail(format!("cannot restore store search path: {e}")))?;
    Ok(())
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
    /// The store has an unrecognized schema or migration history.
    #[error(
        "the store at {path} has an unrecognized atomic schema at version {version}: {reason}; \
         it was not changed. Restore a known backup or apply a reviewed forward repair for this state"
    )]
    Shape {
        /// The database file or schema.
        path: String,
        /// Version whose state cannot be trusted.
        version: i64,
        /// What did not match.
        reason: String,
    },
    /// A recorded migration does not match this binary's immutable SQL.
    #[error(
        "the store at {path} has changed migration {version} ({reason}); it was not changed. \
         Restore the matching atomic binary or apply a reviewed forward repair"
    )]
    History {
        /// The database file or schema.
        path: String,
        /// The first offending version.
        version: i64,
        /// What did not match.
        reason: String,
    },
    /// The migration record cannot be timestamped safely.
    #[error(
        "the store at {path} was not changed because a migration timestamp is unavailable: \
         {reason}; correct the system clock and retry"
    )]
    Clock {
        /// The database file or schema.
        path: String,
        /// The clock failure.
        reason: String,
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

/// Open a current store read-only for inspection. A missing database has
/// an empty in-memory schema. For older databases, SQLite takes a consistent
/// snapshot (including committed WAL pages) and only that snapshot migrates.
pub async fn inspect_sqlite(database: &Path) -> Result<SqliteConnection, StoreError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let path = database.display().to_string();
    let db = |source| StoreError::Database {
        path: path.clone(),
        source,
    };
    let mut conn = match std::fs::symlink_metadata(database) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            SqliteConnection::connect("sqlite::memory:")
                .await
                .map_err(db)?
        }
        Err(error) => return Err(db(sqlx::Error::Io(error))),
        Ok(_) => {
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(database)
                .read_only(true);
            let mut source = SqliteConnection::connect_with(&options).await.map_err(db)?;
            let (_, version) = sqlite_marker(&mut source, &path).await?;
            if version >= STORE_VERSION {
                // Reuse the normal newer-version and migration-history checks.
                migrate(&mut source, &path, MIGRATIONS).await?;
                return Ok(source);
            }
            // Named memory databases are process-local. The open destination
            // holds this one alive until the caller finishes its inspection.
            let name = format!("/atomic-inspect-{}", NEXT.fetch_add(1, Ordering::Relaxed));
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&name)
                .vfs("memdb")
                .create_if_missing(true);
            let copy = SqliteConnection::connect_with(&options).await.map_err(db)?;
            sqlx::query("VACUUM INTO ?")
                .bind(format!("file:{name}?vfs=memdb"))
                .execute(&mut source)
                .await
                .map_err(db)?;
            copy
        }
    };
    migrate(&mut conn, &path, MIGRATIONS).await?;
    Ok(conn)
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
    // Unquoted PostgreSQL identifiers fold to lowercase. Catalog probes use
    // literal names, so compare against the same physical schema name that
    // the subsequent unquoted DDL will address.
    let physical_schema = schema.to_ascii_lowercase();
    let schema = physical_schema.as_str();
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
        let (present, found) = pg_marker(&mut tx, &path, schema).await?;
        if found > to {
            return Err(StoreError::Newer {
                path: path.clone(),
                found,
                supported: to,
            });
        }
        no_gap(&path, found, migrations)?;
        let history = pg_history(&mut tx, &path, schema).await?;
        if let Some(rows) = &history {
            validate_history(&path, found, rows, migrations)?;
        } else if present && history_required(found) {
            return Err(StoreError::History {
                path: path.clone(),
                version: found,
                reason: "a version with migration history has no history table".into(),
            });
        } else if present {
            verify_legacy_pg(&mut tx, &path, schema, found, migrations).await?;
        } else if !pg_objects(&mut tx, schema).await.map_err(db)?.is_empty() {
            return Err(StoreError::Shape {
                path: path.clone(),
                version: 0,
                reason: "atomic objects exist without a version marker".into(),
            });
        }
        if present && found == to && history.is_some() {
            tx.commit().await.map_err(db)?;
            return Ok(to);
        }
        let schema_exists: bool = sqlx::query(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = $1) AS present",
        )
        .bind(schema)
        .fetch_one(&mut *tx)
        .await
        .and_then(|row| row.try_get("present"))
        .map_err(db)?;
        if !schema_exists {
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        for sql in [
            format!("CREATE TABLE IF NOT EXISTS {table} (version BIGINT NOT NULL)"),
            format!("SET LOCAL search_path TO {schema}"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        if history.is_none() {
            sqlx::query(HISTORY_TABLE)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            if found > 0 {
                sqlx::query(
                    "INSERT INTO atomic_store_history (version, name, applied_at, checksum) \
                     VALUES ($1, $2, $3, NULL)",
                )
                .bind(found)
                .bind(format!("legacy-baseline-v{found}"))
                .bind(applied_at(&path)?)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            }
        }
        for m in migrations.iter().filter(|m| m.version > found) {
            for sql in m.sql {
                sqlx::query(*sql).execute(&mut *tx).await.map_err(db)?;
            }
            sqlx::query(
                "INSERT INTO atomic_store_history (version, name, applied_at, checksum) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(m.version)
            .bind(m.name)
            .bind(applied_at(&path)?)
            .bind(checksum(m))
            .execute(&mut *tx)
            .await
            .map_err(db)?;
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

async fn pg_marker(
    conn: &mut sqlx::PgConnection,
    path: &str,
    schema: &str,
) -> Result<(bool, i64), StoreError> {
    let table = format!("{schema}.atomic_store_version");
    let exists: bool = sqlx::query("SELECT to_regclass($1) IS NOT NULL AS e")
        .bind(&table)
        .fetch_one(&mut *conn)
        .await
        .and_then(|row| row.try_get("e"))
        .map_err(|source| StoreError::Database {
            path: path.to_owned(),
            source,
        })?;
    if !exists {
        return Ok((false, 0));
    }
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT version FROM {table}")))
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| StoreError::Shape {
            path: path.to_owned(),
            version: 0,
            reason: format!("cannot read version marker: {e}"),
        })?;
    if rows.len() != 1 {
        return Err(StoreError::Shape {
            path: path.to_owned(),
            version: 0,
            reason: format!("version marker has {} rows, expected one", rows.len()),
        });
    }
    let version: i64 = rows[0].try_get("version").map_err(|e| StoreError::Shape {
        path: path.to_owned(),
        version: 0,
        reason: format!("invalid version marker: {e}"),
    })?;
    if version < 0 {
        return Err(StoreError::Shape {
            path: path.to_owned(),
            version,
            reason: "negative version marker".into(),
        });
    }
    Ok((true, version))
}

async fn pg_history(
    conn: &mut sqlx::PgConnection,
    path: &str,
    schema: &str,
) -> Result<Option<Vec<(i64, String, Option<String>)>>, StoreError> {
    let table = format!("{schema}.atomic_store_history");
    let exists: bool = sqlx::query("SELECT to_regclass($1) IS NOT NULL AS e")
        .bind(&table)
        .fetch_one(&mut *conn)
        .await
        .and_then(|row| row.try_get("e"))
        .map_err(|source| StoreError::Database {
            path: path.to_owned(),
            source,
        })?;
    if !exists {
        return Ok(None);
    }
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT version, name, checksum FROM {table} ORDER BY version"
    )))
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| StoreError::History {
        path: path.to_owned(),
        version: 0,
        reason: format!("cannot read history: {e}"),
    })?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("version").map_err(|e| StoreError::History {
                    path: path.to_owned(),
                    version: 0,
                    reason: format!("invalid history version: {e}"),
                })?,
                row.try_get("name").map_err(|e| StoreError::History {
                    path: path.to_owned(),
                    version: 0,
                    reason: format!("invalid history name: {e}"),
                })?,
                row.try_get("checksum").map_err(|e| StoreError::History {
                    path: path.to_owned(),
                    version: 0,
                    reason: format!("invalid history checksum: {e}"),
                })?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
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
    let (present, current) = sqlite_marker(conn, path).await?;
    if current > to {
        return Err(StoreError::Newer {
            path: path.to_owned(),
            found: current,
            supported: to,
        });
    }
    let history = sqlite_history(conn, path).await?;
    if let Some(rows) = &history {
        validate_history(path, current, rows, migrations)?;
    }
    // A store already at this version is opened without the write lock, so
    // a reader such as `atomic runs show` never waits on a writer.
    if present && current == to && history.is_some() {
        return Ok(to);
    }
    // One writer at a time: BEGIN IMMEDIATE takes the write lock up front, so
    // two processes opening the store cannot both apply a migration.
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *conn)
        .await
        .map_err(db)?;
    let outcome = async {
        let (present, found) = sqlite_marker(conn, path).await?;
        if found > to {
            return Err(StoreError::Newer {
                path: path.to_owned(),
                found,
                supported: to,
            });
        }
        no_gap(path, found, migrations)?;
        let history = sqlite_history(conn, path).await?;
        if let Some(rows) = &history {
            validate_history(path, found, rows, migrations)?;
        } else if present && history_required(found) {
            return Err(StoreError::History {
                path: path.to_owned(),
                version: found,
                reason: "a version with migration history has no history table".into(),
            });
        } else if present {
            verify_legacy_sqlite(conn, path, found, migrations).await?;
        } else if !sqlite_objects(conn).await.map_err(db)?.is_empty() {
            return Err(StoreError::Shape {
                path: path.to_owned(),
                version: 0,
                reason: "atomic objects exist without a version marker".into(),
            });
        }
        if !present {
            sqlx::query("CREATE TABLE atomic_store_version (version INTEGER NOT NULL)")
                .execute(&mut *conn)
                .await
                .map_err(db)?;
        }
        if history.is_none() {
            sqlx::query(HISTORY_TABLE)
                .execute(&mut *conn)
                .await
                .map_err(db)?;
            if found > 0 {
                sqlx::query(
                    "INSERT INTO atomic_store_history (version, name, applied_at, checksum) \
                     VALUES (?, ?, ?, NULL)",
                )
                .bind(found)
                .bind(format!("legacy-baseline-v{found}"))
                .bind(applied_at(path)?)
                .execute(&mut *conn)
                .await
                .map_err(db)?;
            }
        }
        for m in migrations.iter().filter(|m| m.version > found) {
            for sql in m.sql {
                sqlx::query(*sql).execute(&mut *conn).await.map_err(db)?;
            }
            sqlx::query(
                "INSERT INTO atomic_store_history (version, name, applied_at, checksum) \
                 VALUES (?, ?, ?, ?)",
            )
            .bind(m.version)
            .bind(m.name)
            .bind(applied_at(path)?)
            .bind(checksum(m))
            .execute(&mut *conn)
            .await
            .map_err(db)?;
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

async fn sqlite_marker(conn: &mut SqliteConnection, path: &str) -> Result<(bool, i64), StoreError> {
    let exists: bool = sqlx::query("SELECT COUNT(*) AS n FROM sqlite_master WHERE type='table' AND name='atomic_store_version'")
        .fetch_one(&mut *conn).await
        .and_then(|row| row.try_get::<i64, _>("n"))
        .map(|count| count == 1)
        .map_err(|source| StoreError::Database { path: path.to_owned(), source })?;
    if !exists {
        return Ok((false, 0));
    }
    let rows = sqlx::query("SELECT version FROM atomic_store_version")
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| StoreError::Shape {
            path: path.to_owned(),
            version: 0,
            reason: format!("cannot read version marker: {e}"),
        })?;
    if rows.len() != 1 {
        return Err(StoreError::Shape {
            path: path.to_owned(),
            version: 0,
            reason: format!("version marker has {} rows, expected one", rows.len()),
        });
    }
    let version: i64 = rows[0].try_get("version").map_err(|e| StoreError::Shape {
        path: path.to_owned(),
        version: 0,
        reason: format!("invalid version marker: {e}"),
    })?;
    if version < 0 {
        return Err(StoreError::Shape {
            path: path.to_owned(),
            version,
            reason: "negative version marker".into(),
        });
    }
    Ok((true, version))
}

async fn sqlite_history(
    conn: &mut SqliteConnection,
    path: &str,
) -> Result<Option<Vec<(i64, String, Option<String>)>>, StoreError> {
    let exists: bool = sqlx::query("SELECT COUNT(*) AS n FROM sqlite_master WHERE type='table' AND name='atomic_store_history'")
        .fetch_one(&mut *conn).await
        .and_then(|row| row.try_get::<i64, _>("n"))
        .map(|count| count == 1)
        .map_err(|source| StoreError::Database { path: path.to_owned(), source })?;
    if !exists {
        return Ok(None);
    }
    let rows =
        sqlx::query("SELECT version, name, checksum FROM atomic_store_history ORDER BY version")
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| StoreError::History {
                path: path.to_owned(),
                version: 0,
                reason: format!("cannot read history: {e}"),
            })?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("version").map_err(|e| StoreError::History {
                    path: path.to_owned(),
                    version: 0,
                    reason: format!("invalid history version: {e}"),
                })?,
                row.try_get("name").map_err(|e| StoreError::History {
                    path: path.to_owned(),
                    version: 0,
                    reason: format!("invalid history name: {e}"),
                })?,
                row.try_get("checksum").map_err(|e| StoreError::History {
                    path: path.to_owned(),
                    version: 0,
                    reason: format!("invalid history checksum: {e}"),
                })?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
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

    async fn legacy_sqlite(path: &Path, version: i64, threads: bool) {
        let options =
            sqlx::sqlite::SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
                .expect("options")
                .create_if_missing(true);
        let mut conn = SqliteConnection::connect_with(&options)
            .await
            .expect("connect");
        sqlx::query("CREATE TABLE IF NOT EXISTS atomic_store_version (version INTEGER NOT NULL)")
            .execute(&mut conn)
            .await
            .expect("marker");
        for migration in MIGRATIONS.iter().filter(|m| m.version <= version) {
            for sql in migration.sql {
                sqlx::query(*sql)
                    .execute(&mut conn)
                    .await
                    .expect("legacy DDL");
            }
        }
        if threads && version < 11 {
            for sql in MIGRATIONS[10].sql {
                sqlx::query(*sql)
                    .execute(&mut conn)
                    .await
                    .expect("lazy thread DDL");
            }
        }
        sqlx::query("INSERT INTO atomic_store_version (version) VALUES (?)")
            .bind(version)
            .execute(&mut conn)
            .await
            .expect("marker value");
    }

    #[cfg(feature = "postgres-tests")]
    async fn legacy_postgres(url: &str, schema: &str, version: i64, threads: bool) {
        let mut conn = sqlx::PgConnection::connect(url).await.expect("connect");
        let mut tx = conn.begin().await.expect("transaction");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&mut *tx)
            .await
            .expect("schema");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SET LOCAL search_path TO {schema}"
        )))
        .execute(&mut *tx)
        .await
        .expect("search path");
        sqlx::query("CREATE TABLE atomic_store_version (version BIGINT NOT NULL)")
            .execute(&mut *tx)
            .await
            .expect("marker");
        for migration in MIGRATIONS.iter().filter(|m| m.version <= version) {
            for sql in migration.sql {
                sqlx::query(*sql)
                    .execute(&mut *tx)
                    .await
                    .expect("legacy DDL");
            }
        }
        if threads && version < 11 {
            for sql in MIGRATIONS[10].sql {
                sqlx::query(*sql)
                    .execute(&mut *tx)
                    .await
                    .expect("lazy thread DDL");
            }
        }
        sqlx::query("INSERT INTO atomic_store_version (version) VALUES ($1)")
            .bind(version)
            .execute(&mut *tx)
            .await
            .expect("marker value");
        tx.commit().await.expect("commit legacy store");
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
        Migration {
            version,
            name: "test",
            sql,
        }
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
        assert_eq!(
            tables(&p).await,
            ["atomic_store_history", "atomic_store_version", "gap_one"]
        );
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
    async fn inspection_reads_committed_wal_pages_and_migrates_only_its_copy() {
        let path = db("inspection");
        let legacy = &MIGRATIONS[..MIGRATIONS.len() - 1];
        let version = open_sqlite_with(&path, legacy).await.expect("old store");
        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
        let mut writer = SqliteConnection::connect_with(&options)
            .await
            .expect("writer");
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&mut writer)
            .await
            .expect("enable WAL");
        sqlx::query(
            "INSERT INTO atomic_graph_memories VALUES ('inspection','committed','external')",
        )
        .execute(&mut writer)
        .await
        .expect("commit memory");
        let mut copy = inspect_sqlite(&path).await.expect("inspect old store");
        assert_eq!(
            sqlite_marker(&mut copy, "copy")
                .await
                .expect("copy version"),
            (true, STORE_VERSION)
        );
        let text: String =
            sqlx::query_scalar("SELECT value FROM atomic_graph_memories WHERE key='inspection'")
                .fetch_one(&mut copy)
                .await
                .expect("committed WAL row");
        assert_eq!(text, "committed");
        assert_eq!(
            sqlite_marker(&mut writer, "source")
                .await
                .expect("source version"),
            (true, version)
        );
        writer.close().await.expect("close writer");
        std::fs::remove_file(path).expect("remove fixture");
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
        legacy_sqlite(&p, 10, false).await;
        assert_eq!(open_sqlite(&p).await.expect("migrate"), STORE_VERSION);
        let mut conn = SqliteConnection::connect(&format!("sqlite://{}", p.display()))
            .await
            .expect("connect");
        let rows = sqlx::query(
            "SELECT version, name, checksum FROM atomic_store_history ORDER BY version",
        )
        .fetch_all(&mut conn)
        .await
        .expect("history");
        assert_eq!(rows.len(), 12, "one baseline and eleven new migrations");
        assert_eq!(rows[0].get::<i64, _>("version"), 10);
        assert_eq!(rows[0].get::<String, _>("name"), "legacy-baseline-v10");
        assert_eq!(rows[0].get::<Option<String>, _>("checksum"), None);
        assert_eq!(rows[1].get::<i64, _>("version"), 11);
        assert_eq!(rows[1].get::<String, _>("name"), "thread-index");
        assert_eq!(
            rows[1].get::<Option<String>, _>("checksum"),
            Some(checksum(&MIGRATIONS[10]))
        );
        assert_eq!(rows[2].get::<i64, _>("version"), 12);
        assert_eq!(rows[2].get::<String, _>("name"), "work-graph");
        assert_eq!(
            rows[2].get::<Option<String>, _>("checksum"),
            Some(checksum(&MIGRATIONS[11]))
        );
        assert_eq!(rows[3].get::<i64, _>("version"), 13);
        assert_eq!(rows[3].get::<String, _>("name"), "work-graph-human-leases");
        assert_eq!(
            rows[3].get::<Option<String>, _>("checksum"),
            Some(checksum(&MIGRATIONS[12]))
        );
        assert_eq!(rows[4].get::<i64, _>("version"), 14);
        assert_eq!(
            rows[4].get::<String, _>("name"),
            "work-graph-item-close-policy"
        );
        assert_eq!(
            rows[4].get::<Option<String>, _>("checksum"),
            Some(checksum(&MIGRATIONS[13]))
        );
        assert_eq!(rows[5].get::<i64, _>("version"), 15);
        assert_eq!(
            rows[5].get::<String, _>("name"),
            "work-graph-epic-run-admission"
        );
        assert_eq!(
            rows[5].get::<Option<String>, _>("checksum"),
            Some(checksum(&MIGRATIONS[14]))
        );
        assert_eq!(rows[6].get::<i64, _>("version"), 16);
        assert_eq!(
            rows[6].get::<String, _>("name"),
            "work-graph-verified-commit-proof"
        );
        assert_eq!(
            rows[6].get::<Option<String>, _>("checksum"),
            Some(checksum(&MIGRATIONS[15]))
        );
        assert_eq!(rows[7].get::<i64, _>("version"), 17);
        assert_eq!(rows[7].get::<String, _>("name"), "work-graph-goals");
        assert_eq!(
            rows[7].get::<Option<String>, _>("checksum"),
            Some(checksum(&MIGRATIONS[16]))
        );
        assert_eq!(rows[8].get::<i64, _>("version"), 18);
        assert_eq!(rows[8].get::<String, _>("name"), "work-graph-item-criteria");
        assert_eq!(
            rows[8].get::<Option<String>, _>("checksum"),
            Some(checksum(&MIGRATIONS[17]))
        );
        assert_eq!(rows[9].get::<i64, _>("version"), 19);
        assert_eq!(rows[9].get::<String, _>("name"), "delivery-effect-intents");
        assert_eq!(
            rows[9].get::<Option<String>, _>("checksum"),
            Some(checksum(&MIGRATIONS[18]))
        );
        assert_eq!(rows[10].get::<i64, _>("version"), 20);
        assert_eq!(
            rows[10].get::<String, _>("name"),
            "delivery-effect-stable-key-aliases"
        );
        assert_eq!(
            rows[10].get::<Option<String>, _>("checksum"),
            Some(checksum(&MIGRATIONS[19]))
        );
        assert_eq!(rows[11].get::<i64, _>("version"), STORE_VERSION);
        assert_eq!(
            rows[11].get::<String, _>("name"),
            "delivery-queue-observations"
        );
        assert_eq!(
            rows[11].get::<Option<String>, _>("checksum"),
            Some(checksum(&MIGRATIONS[20]))
        );
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn graph_item_policy_migration_retains_the_legacy_global_default() {
        let p = db("item-policy-legacy");
        assert_eq!(
            open_sqlite_with(&p, &MIGRATIONS[..13])
                .await
                .expect("open version 13"),
            13
        );
        let mut conn = SqliteConnection::connect(&format!("sqlite://{}", p.display()))
            .await
            .expect("connect version 13");
        sqlx::query("INSERT INTO atomic_graph_policy VALUES (1,1,0)")
            .execute(&mut conn)
            .await
            .expect("legacy global policy");
        assert_eq!(open_sqlite(&p).await.expect("migrate to 14"), STORE_VERSION);
        let legacy: i64 =
            sqlx::query_scalar("SELECT requires_commit FROM atomic_graph_policy WHERE id=1")
                .fetch_one(&mut conn)
                .await
                .expect("legacy policy remains");
        assert_eq!(legacy, 1);
        let item_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_item_policy")
            .fetch_one(&mut conn)
            .await
            .expect("new item policy table exists");
        assert_eq!(
            item_rows, 0,
            "migration did not silently override legacy policy"
        );
        let _ = conn.close().await;
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn goal_tables_migrate_from_existing_graph_store_without_rewriting_items() {
        let p = db("goal-v17-upgrade");
        assert_eq!(
            open_sqlite_with(&p, &MIGRATIONS[..16])
                .await
                .expect("open version 16"),
            16
        );
        let mut conn = SqliteConnection::connect(&format!("sqlite://{}", p.display()))
            .await
            .expect("connect version 16");
        sqlx::query("INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at,claim_epoch) VALUES ('existing','Existing','before goal migration','task',2,'open',NULL,'human','owner',1,0)")
            .execute(&mut conn).await.expect("existing graph item");
        assert_eq!(open_sqlite(&p).await.expect("migrate to 17"), STORE_VERSION);
        let title: String =
            sqlx::query_scalar("SELECT title FROM atomic_graph_items WHERE id='existing'")
                .fetch_one(&mut conn)
                .await
                .expect("existing item survived");
        assert_eq!(title, "Existing");
        let tables: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name LIKE 'atomic_graph_goal_%'")
            .fetch_one(&mut conn).await.expect("goal tables");
        assert_eq!(tables, 5);
        let _ = conn.close().await;
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn item_criteria_migration_adds_a_separate_empty_policy_table() {
        let p = db("item-criteria-v18-upgrade");
        assert_eq!(
            open_sqlite_with(&p, &MIGRATIONS[..17])
                .await
                .expect("open version 17"),
            17
        );
        let mut conn = SqliteConnection::connect(&format!("sqlite://{}", p.display()))
            .await
            .expect("connect version 17");
        sqlx::query("INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at,claim_epoch) VALUES ('old-item','Old','imported prose','task',2,'open',NULL,'external','import',1,0)")
            .execute(&mut conn)
            .await
            .expect("existing graph item");
        assert_eq!(open_sqlite(&p).await.expect("migrate to 18"), STORE_VERSION);
        let body: String =
            sqlx::query_scalar("SELECT body FROM atomic_graph_items WHERE id='old-item'")
                .fetch_one(&mut conn)
                .await
                .expect("existing imported prose survived");
        assert_eq!(body, "imported prose");
        let criteria_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_item_criteria")
                .fetch_one(&mut conn)
                .await
                .expect("criteria table");
        assert_eq!(
            criteria_rows, 0,
            "migration promoted imported prose to executable criteria"
        );
        let _ = conn.close().await;
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn verified_commit_migration_keeps_old_flags_without_minting_a_branch_proof() {
        let p = db("verified-commit-legacy");
        assert_eq!(
            open_sqlite_with(&p, &MIGRATIONS[..15])
                .await
                .expect("open version 15"),
            15
        );
        let mut conn = SqliteConnection::connect(&format!("sqlite://{}", p.display()))
            .await
            .expect("connect version 15");
        sqlx::query("INSERT INTO atomic_graph_commit_links (item_id,sha,indexed,on_branch) VALUES ('old','aabb',1,1)")
            .execute(&mut conn)
            .await
            .expect("legacy flags");
        assert_eq!(open_sqlite(&p).await.expect("migrate to 16"), STORE_VERSION);
        let row = sqlx::query("SELECT indexed,on_branch,verified_branch,verified_head,pinned_base FROM atomic_graph_commit_links WHERE item_id='old'")
            .fetch_one(&mut conn)
            .await
            .expect("migrated link");
        assert_eq!(row.get::<i64, _>("indexed"), 1);
        assert_eq!(row.get::<i64, _>("on_branch"), 1);
        assert_eq!(row.get::<Option<String>, _>("verified_branch"), None);
        assert_eq!(row.get::<Option<String>, _>("verified_head"), None);
        assert_eq!(row.get::<Option<String>, _>("pinned_base"), None);
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn a_legacy_lazy_thread_index_is_accepted() {
        let p = db("legacy-lazy-threads");
        legacy_sqlite(&p, 10, true).await;
        assert_eq!(open_sqlite(&p).await.expect("migrate"), STORE_VERSION);
        assert_eq!(open_sqlite(&p).await.expect("reopen"), STORE_VERSION);
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn sqlite_fresh_and_both_legacy_thread_shapes_reach_the_same_schema() {
        let fresh = db("shape-fresh");
        open_sqlite(&fresh).await.expect("fresh");
        let mut fresh_conn = SqliteConnection::connect(&format!("sqlite://{}", fresh.display()))
            .await
            .expect("connect fresh");
        let mut expected = sqlite_objects(&mut fresh_conn).await.expect("fresh schema");
        // The marker's legacy spelling includes IF NOT EXISTS, while the
        // current creator omits it. Its one-column shape is checked by the
        // marker reader; application objects have the same definitions.
        expected.retain(|(name, _, _)| name != "atomic_store_version");
        for lazy_threads in [false, true] {
            let legacy = db(if lazy_threads {
                "shape-lazy"
            } else {
                "shape-plain"
            });
            legacy_sqlite(&legacy, 10, lazy_threads).await;
            open_sqlite(&legacy).await.expect("upgrade");
            let mut conn = SqliteConnection::connect(&format!("sqlite://{}", legacy.display()))
                .await
                .expect("connect legacy");
            let mut actual = sqlite_objects(&mut conn).await.expect("legacy schema");
            actual.retain(|(name, _, _)| name != "atomic_store_version");
            assert_eq!(actual, expected);
            let _ = std::fs::remove_file(&legacy);
        }
        let _ = std::fs::remove_file(&fresh);
    }

    /// Question gates' tables (migration 7) reach a store at 5, which gets
    /// the executions table (6) on the way, and one at 6.
    #[tokio::test]
    async fn question_gates_migrate_onto_a_store_at_5_or_6() {
        for from in [5, 6] {
            let p = db(&format!("from-{from}"));
            legacy_sqlite(&p, from, false).await;
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

    #[tokio::test]
    async fn fresh_history_has_one_verified_row_per_migration() {
        let p = db("fresh-history");
        open_sqlite(&p).await.expect("open");
        let mut conn = SqliteConnection::connect(&format!("sqlite://{}", p.display()))
            .await
            .expect("connect");
        let rows = sqlx::query(
            "SELECT version, name, applied_at, checksum FROM atomic_store_history ORDER BY version",
        )
        .fetch_all(&mut conn)
        .await
        .expect("history");
        assert_eq!(rows.len(), MIGRATIONS.len());
        for (row, migration) in rows.iter().zip(MIGRATIONS) {
            assert_eq!(row.get::<i64, _>("version"), migration.version);
            assert_eq!(row.get::<String, _>("name"), migration.name);
            assert!(row.get::<i64, _>("applied_at") > 0);
            assert_eq!(
                row.get::<Option<String>, _>("checksum"),
                Some(checksum(migration))
            );
        }
        assert_eq!(open_sqlite(&p).await.expect("reopen"), STORE_VERSION);
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn legacy_shape_refuses_a_missing_table_without_writes() {
        let p = db("bad-legacy");
        legacy_sqlite(&p, 10, true).await;
        let mut conn = SqliteConnection::connect(&format!("sqlite://{}", p.display()))
            .await
            .expect("connect");
        sqlx::query("DROP TABLE atomic_run_projects")
            .execute(&mut conn)
            .await
            .expect("damage legacy shape");
        assert!(matches!(
            open_sqlite(&p).await,
            Err(StoreError::Shape { version: 10, .. })
        ));
        assert!(!tables(&p).await.contains(&"atomic_store_history".into()));
        assert_eq!(
            sqlx::query("SELECT version FROM atomic_store_version")
                .fetch_one(&mut conn)
                .await
                .expect("marker")
                .get::<i64, _>("version"),
            10
        );
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn history_checksum_and_hole_refuse_before_any_change() {
        for (tag, statement, version) in [
            (
                "checksum",
                "UPDATE atomic_store_history SET checksum='different' WHERE version=5",
                5,
            ),
            (
                "hole",
                "DELETE FROM atomic_store_history WHERE version=5",
                5,
            ),
        ] {
            let p = db(tag);
            open_sqlite(&p).await.expect("open");
            let mut conn = SqliteConnection::connect(&format!("sqlite://{}", p.display()))
                .await
                .expect("connect");
            sqlx::query(statement)
                .execute(&mut conn)
                .await
                .expect("damage history");
            assert!(matches!(
                open_sqlite(&p).await,
                Err(StoreError::History { version: bad, .. }) if bad == version
            ));
            assert_eq!(
                sqlx::query("SELECT version FROM atomic_store_version")
                    .fetch_one(&mut conn)
                    .await
                    .expect("marker")
                    .get::<i64, _>("version"),
                STORE_VERSION
            );
            let _ = std::fs::remove_file(&p);
        }
    }

    #[tokio::test]
    async fn a_history_era_version_cannot_be_rebaselined() {
        let p = db("no-rebaseline");
        open_sqlite(&p).await.expect("fresh");
        let mut conn = SqliteConnection::connect(&format!("sqlite://{}", p.display()))
            .await
            .expect("connect");
        sqlx::query("DROP TABLE atomic_store_history")
            .execute(&mut conn)
            .await
            .expect("remove history");
        assert!(matches!(
            open_sqlite(&p).await,
            Err(StoreError::History {
                version: STORE_VERSION,
                ..
            })
        ));
        assert!(!tables(&p).await.contains(&"atomic_store_history".into()));
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn a_failed_migration_rolls_back_schema_history_and_marker() {
        let p = db("rollback");
        open_sqlite(&p).await.expect("open");
        let mut bad = MIGRATIONS.to_vec();
        bad.push(m(
            STORE_VERSION + 1,
            &[
                "CREATE TABLE atomic_rollback_probe (value INTEGER)",
                "INVALID SQL",
            ],
        ));
        assert!(matches!(
            open_sqlite_with(&p, &bad).await,
            Err(StoreError::Database { .. })
        ));
        assert!(!tables(&p).await.contains(&"atomic_rollback_probe".into()));
        let mut conn = SqliteConnection::connect(&format!("sqlite://{}", p.display()))
            .await
            .expect("connect");
        let marker: i64 = sqlx::query("SELECT version FROM atomic_store_version")
            .fetch_one(&mut conn)
            .await
            .expect("marker")
            .get("version");
        let history: i64 = sqlx::query("SELECT MAX(version) AS version FROM atomic_store_history")
            .fetch_one(&mut conn)
            .await
            .expect("history")
            .get("version");
        assert_eq!((marker, history), (STORE_VERSION, STORE_VERSION));
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn a_current_sqlite_store_opens_while_a_writer_holds_its_lock() {
        let p = db("read-only-open");
        open_sqlite(&p).await.expect("open");
        let mut writer = SqliteConnection::connect(&format!("sqlite://{}", p.display()))
            .await
            .expect("writer");
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut writer)
            .await
            .expect("writer lock");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), open_sqlite(&p))
                .await
                .expect("current opener does not wait on writer")
                .expect("open"),
            STORE_VERSION
        );
        sqlx::query("ROLLBACK")
            .execute(&mut writer)
            .await
            .expect("release writer");
        let _ = std::fs::remove_file(&p);
    }

    #[cfg(feature = "postgres-tests")]
    #[tokio::test]
    async fn postgres_fresh_and_legacy_stores_have_matching_atomic_tables() {
        let fresh = crate::testing::schema("hist_fresh").await;
        assert_eq!(
            open_postgres(fresh.url(), fresh.name())
                .await
                .expect("fresh"),
            STORE_VERSION
        );
        let mut conn = sqlx::PgConnection::connect(fresh.url())
            .await
            .expect("connect");
        let fresh_objects = pg_objects(&mut conn, fresh.name())
            .await
            .expect("fresh schema");
        for (tag, lazy_threads) in [("hist_legacy", true), ("hist_plain", false)] {
            let legacy = crate::testing::schema(tag).await;
            legacy_postgres(legacy.url(), legacy.name(), 10, lazy_threads).await;
            assert_eq!(
                open_postgres(legacy.url(), legacy.name())
                    .await
                    .expect("upgrade"),
                STORE_VERSION
            );
            let legacy_objects = pg_objects(&mut conn, legacy.name())
                .await
                .expect("upgraded schema");
            assert_eq!(fresh_objects, legacy_objects);
            let baseline = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT version, name, checksum FROM {}.atomic_store_history ORDER BY version",
                legacy.name()
            )))
            .fetch_all(&mut conn)
            .await
            .expect("legacy history");
            assert_eq!(baseline.len(), 11);
            assert_eq!(baseline[0].get::<i64, _>("version"), 10);
            assert_eq!(baseline[0].get::<String, _>("name"), "legacy-baseline-v10");
            assert_eq!(baseline[0].get::<Option<String>, _>("checksum"), None);
            assert_eq!(baseline[1].get::<i64, _>("version"), 11);
            assert_eq!(
                baseline[1].get::<Option<String>, _>("checksum"),
                Some(checksum(&MIGRATIONS[10]))
            );
            assert_eq!(baseline[2].get::<i64, _>("version"), 12);
            assert_eq!(
                baseline[2].get::<Option<String>, _>("checksum"),
                Some(checksum(&MIGRATIONS[11]))
            );
            assert_eq!(baseline[3].get::<i64, _>("version"), 13);
            assert_eq!(
                baseline[3].get::<Option<String>, _>("checksum"),
                Some(checksum(&MIGRATIONS[12]))
            );
            assert_eq!(baseline[4].get::<i64, _>("version"), 14);
            assert_eq!(
                baseline[4].get::<Option<String>, _>("checksum"),
                Some(checksum(&MIGRATIONS[13]))
            );
            assert_eq!(baseline[5].get::<i64, _>("version"), 15);
            assert_eq!(
                baseline[5].get::<Option<String>, _>("checksum"),
                Some(checksum(&MIGRATIONS[14]))
            );
            assert_eq!(baseline[6].get::<i64, _>("version"), 16);
            assert_eq!(
                baseline[6].get::<Option<String>, _>("checksum"),
                Some(checksum(&MIGRATIONS[15]))
            );
            assert_eq!(baseline[7].get::<i64, _>("version"), 17);
            assert_eq!(
                baseline[7].get::<Option<String>, _>("checksum"),
                Some(checksum(&MIGRATIONS[16]))
            );
            assert_eq!(baseline[8].get::<i64, _>("version"), 18);
            assert_eq!(
                baseline[8].get::<Option<String>, _>("checksum"),
                Some(checksum(&MIGRATIONS[17]))
            );
            assert_eq!(baseline[9].get::<i64, _>("version"), 19);
            assert_eq!(
                baseline[9].get::<Option<String>, _>("checksum"),
                Some(checksum(&MIGRATIONS[18]))
            );
            assert_eq!(baseline[10].get::<i64, _>("version"), STORE_VERSION);
            assert_eq!(
                baseline[10].get::<Option<String>, _>("checksum"),
                Some(checksum(&MIGRATIONS[19]))
            );
        }
    }

    #[cfg(feature = "postgres-tests")]
    #[tokio::test]
    async fn postgres_graph_item_policy_migration_retains_legacy_global_default() {
        let schema = crate::testing::schema("hist_item_pol").await;
        assert_eq!(
            open_postgres_with(schema.url(), schema.name(), &MIGRATIONS[..13])
                .await
                .expect("open version 13"),
            13
        );
        let mut conn = sqlx::PgConnection::connect(schema.url())
            .await
            .expect("connect");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {}.atomic_graph_policy VALUES (1,1,0)",
            schema.name()
        )))
        .execute(&mut conn)
        .await
        .expect("legacy global policy");
        assert_eq!(
            open_postgres(schema.url(), schema.name())
                .await
                .expect("migrate to item policy"),
            STORE_VERSION
        );
        let legacy: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT requires_commit FROM {}.atomic_graph_policy WHERE id=1",
            schema.name()
        )))
        .fetch_one(&mut conn)
        .await
        .expect("legacy policy remains");
        assert_eq!(legacy, 1);
        let item_rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {}.atomic_graph_item_policy",
            schema.name()
        )))
        .fetch_one(&mut conn)
        .await
        .expect("new table exists");
        assert_eq!(item_rows, 0);
    }

    #[cfg(feature = "postgres-tests")]
    #[tokio::test]
    async fn postgres_mixed_case_schema_name_opens_fresh_and_legacy_stores() {
        let fresh = crate::testing::schema("hist_case_fresh").await;
        let mixed_fresh = fresh.name().to_ascii_uppercase();
        assert_eq!(
            open_postgres(fresh.url(), &mixed_fresh)
                .await
                .expect("create through mixed-case name"),
            STORE_VERSION
        );
        assert_eq!(
            open_postgres(fresh.url(), fresh.name())
                .await
                .expect("reopen through physical name"),
            STORE_VERSION
        );

        let legacy = crate::testing::schema("hist_case_old").await;
        legacy_postgres(legacy.url(), legacy.name(), 10, false).await;
        let mixed_legacy = legacy.name().to_ascii_uppercase();
        assert_eq!(
            open_postgres(legacy.url(), &mixed_legacy)
                .await
                .expect("upgrade through mixed-case name"),
            STORE_VERSION
        );
        let mut conn = sqlx::PgConnection::connect(legacy.url())
            .await
            .expect("connect");
        assert_eq!(
            pg_marker(&mut conn, "test", legacy.name())
                .await
                .expect("marker"),
            (true, STORE_VERSION)
        );
        assert_eq!(
            pg_history(&mut conn, "test", legacy.name())
                .await
                .expect("history")
                .expect("recorded history")
                .len(),
            11
        );
    }

    #[cfg(feature = "postgres-tests")]
    #[tokio::test]
    async fn postgres_legacy_verification_needs_no_database_create_privilege() {
        let schema = crate::testing::schema("hist_limited").await;
        legacy_postgres(schema.url(), schema.name(), 10, false).await;
        let mut conn = sqlx::PgConnection::connect(schema.url())
            .await
            .expect("connect");
        let database: String = sqlx::query("SELECT current_database() AS name")
            .fetch_one(&mut conn)
            .await
            .expect("database")
            .get("name");
        let role = format!("role_{}", schema.name());
        let mut tx = conn.begin().await.expect("transaction");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} NOLOGIN")))
            .execute(&mut *tx)
            .await
            .expect("limited role");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT USAGE, CREATE ON SCHEMA {} TO {role}",
            schema.name()
        )))
        .execute(&mut *tx)
        .await
        .expect("schema rights");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT TEMPORARY ON DATABASE \"{}\" TO {role}",
            database.replace('"', "\"\"")
        )))
        .execute(&mut *tx)
        .await
        .expect("temporary rights");
        sqlx::query(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {role}")))
            .execute(&mut *tx)
            .await
            .expect("become limited role");
        let can_create_database_schema: bool = sqlx::query(
            "SELECT has_database_privilege(current_user, current_database(), 'CREATE') AS allowed",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("database rights")
        .get("allowed");
        assert!(
            !can_create_database_schema,
            "the role lacks database CREATE"
        );
        verify_legacy_pg(&mut tx, "limited-role test", schema.name(), 10, MIGRATIONS)
            .await
            .expect("verify using only temporary reference objects");
        tx.rollback()
            .await
            .expect("remove temporary role and reference objects");
    }

    #[cfg(feature = "postgres-tests")]
    #[tokio::test]
    async fn postgres_public_upgrade_needs_no_database_create_privilege() {
        let schema = crate::testing::schema("hist_role_open").await;
        legacy_postgres(schema.url(), schema.name(), 10, false).await;
        let mut admin = sqlx::PgConnection::connect(schema.url())
            .await
            .expect("connect as test admin");
        let role = format!("role_{}", schema.name());
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE ROLE {role} LOGIN PASSWORD 'migration_test'"
        )))
        .execute(&mut admin)
        .await
        .expect("limited login");
        for grant in [
            format!("GRANT USAGE, CREATE ON SCHEMA {} TO {role}", schema.name()),
            format!(
                "GRANT ALL ON ALL TABLES IN SCHEMA {} TO {role}",
                schema.name()
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(grant))
                .execute(&mut admin)
                .await
                .expect("existing schema and table rights");
        }
        let (scheme, remainder) = schema.url().split_once("://").expect("database URL");
        let (_, host_and_database) = remainder.split_once('@').expect("database URL userinfo");
        let limited_url = format!("{scheme}://{role}:migration_test@{host_and_database}");
        let mut limited = sqlx::PgConnection::connect(&limited_url)
            .await
            .expect("connect as limited role");
        let rights = sqlx::query(
            "SELECT has_database_privilege(current_user, current_database(), 'CREATE') AS create_schema, \
             has_database_privilege(current_user, current_database(), 'TEMP') AS use_temp",
        )
        .fetch_one(&mut limited)
        .await
        .expect("check limited rights");
        assert!(!rights.get::<bool, _>("create_schema"));
        assert!(rights.get::<bool, _>("use_temp"));
        limited.close().await.expect("close limited connection");
        let opened = open_postgres(&limited_url, schema.name()).await;
        let owner: String = sqlx::query("SELECT current_user AS name")
            .fetch_one(&mut admin)
            .await
            .expect("admin user")
            .get("name");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "REASSIGN OWNED BY {role} TO {owner}"
        )))
        .execute(&mut admin)
        .await
        .expect("transfer created objects for cleanup");
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP OWNED BY {role}")))
            .execute(&mut admin)
            .await
            .expect("revoke test rights");
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP ROLE {role}")))
            .execute(&mut admin)
            .await
            .expect("drop limited role");
        assert_eq!(
            opened.expect("upgrade with no database CREATE"),
            STORE_VERSION
        );
    }

    #[cfg(feature = "postgres-tests")]
    #[tokio::test]
    async fn postgres_legacy_upgrade_without_temp_privilege_refuses_without_writes() {
        let schema = crate::testing::schema("hist_no_temp").await;
        legacy_postgres(schema.url(), schema.name(), 10, false).await;
        let mut conn = sqlx::PgConnection::connect(schema.url())
            .await
            .expect("connect");
        let database: String = sqlx::query("SELECT current_database() AS name")
            .fetch_one(&mut conn)
            .await
            .expect("database")
            .get("name");
        let role = format!("role_{}", schema.name());
        let mut tx = conn.begin().await.expect("transaction");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "REVOKE TEMPORARY ON DATABASE \"{}\" FROM PUBLIC",
            database.replace('"', "\"\"")
        )))
        .execute(&mut *tx)
        .await
        .expect("remove default temporary privilege in this transaction");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} NOLOGIN")))
            .execute(&mut *tx)
            .await
            .expect("limited role");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT USAGE, CREATE ON SCHEMA {} TO {role}",
            schema.name()
        )))
        .execute(&mut *tx)
        .await
        .expect("schema rights");
        sqlx::query(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {role}")))
            .execute(&mut *tx)
            .await
            .expect("become limited role");
        let can_use_temp: bool = sqlx::query(
            "SELECT has_database_privilege(current_user, current_database(), 'TEMP') AS allowed",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("temporary privilege")
        .get("allowed");
        assert!(!can_use_temp);
        assert!(matches!(
            verify_legacy_pg(&mut tx, "no-temp test", schema.name(), 10, MIGRATIONS).await,
            Err(StoreError::Shape { version: 10, .. })
        ));
        tx.rollback().await.expect("undo privilege change");
        assert_eq!(
            pg_marker(&mut conn, "test", schema.name())
                .await
                .expect("marker"),
            (true, 10)
        );
        assert!(
            pg_history(&mut conn, "test", schema.name())
                .await
                .expect("history")
                .is_none()
        );
    }

    #[cfg(feature = "postgres-tests")]
    #[tokio::test]
    async fn postgres_bad_legacy_shape_and_changed_history_refuse_without_writes() {
        let legacy = crate::testing::schema("hist_bad_legacy").await;
        legacy_postgres(legacy.url(), legacy.name(), 10, false).await;
        let mut conn = sqlx::PgConnection::connect(legacy.url())
            .await
            .expect("connect");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TABLE {}.atomic_run_projects",
            legacy.name()
        )))
        .execute(&mut conn)
        .await
        .expect("damage legacy shape");
        assert!(matches!(
            open_postgres(legacy.url(), legacy.name()).await,
            Err(StoreError::Shape { version: 10, .. })
        ));
        let history = pg_history(&mut conn, "test", legacy.name())
            .await
            .expect("read history");
        assert!(history.is_none(), "refusal wrote no baseline");

        let checked = crate::testing::schema("hist_bad_check").await;
        legacy_postgres(checked.url(), checked.name(), 10, false).await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE {}.atomic_publications ADD CONSTRAINT reject_publication CHECK (false)",
            checked.name()
        )))
        .execute(&mut conn)
        .await
        .expect("add rejecting constraint");
        assert!(matches!(
            open_postgres(checked.url(), checked.name()).await,
            Err(StoreError::Shape { version: 10, .. })
        ));
        assert!(
            pg_history(&mut conn, "test", checked.name())
                .await
                .expect("check history")
                .is_none(),
            "the CHECK(false) legacy store was not baselined"
        );

        let indexed = crate::testing::schema("hist_bad_index").await;
        legacy_postgres(indexed.url(), indexed.name(), 10, false).await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE UNIQUE INDEX custom_digest ON {}.atomic_publications (digest)",
            indexed.name()
        )))
        .execute(&mut conn)
        .await
        .expect("add custom named unique index");
        assert!(matches!(
            open_postgres(indexed.url(), indexed.name()).await,
            Err(StoreError::Shape { version: 10, .. })
        ));
        assert!(
            pg_history(&mut conn, "test", indexed.name())
                .await
                .expect("index history")
                .is_none(),
            "the custom-index legacy store was not baselined"
        );
        assert_eq!(
            pg_marker(&mut conn, "test", indexed.name())
                .await
                .expect("index marker"),
            (true, 10),
            "refusal did not advance the scalar marker"
        );

        let colliding = crate::testing::schema("hist_name_index").await;
        legacy_postgres(colliding.url(), colliding.name(), 10, false).await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE {}.unrelated (thread_id TEXT)",
            colliding.name()
        )))
        .execute(&mut conn)
        .await
        .expect("unrelated table");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE INDEX atomic_threads_by_thread ON {}.unrelated (thread_id)",
            colliding.name()
        )))
        .execute(&mut conn)
        .await
        .expect("colliding Atomic index name");
        assert!(matches!(
            open_postgres(colliding.url(), colliding.name()).await,
            Err(StoreError::Shape { version: 10, .. })
        ));
        assert!(
            pg_history(&mut conn, "test", colliding.name())
                .await
                .expect("collision history")
                .is_none(),
            "the colliding index was not baselined"
        );
        assert_eq!(
            pg_marker(&mut conn, "test", colliding.name())
                .await
                .expect("collision marker"),
            (true, 10)
        );

        for (tag, ddl) in [
            (
                "hist_bad_view",
                "CREATE VIEW {schema}.atomic_surprise AS SELECT 1 AS value",
            ),
            ("hist_bad_seq", "CREATE SEQUENCE {schema}.atomic_surprise"),
        ] {
            let unexpected = crate::testing::schema(tag).await;
            legacy_postgres(unexpected.url(), unexpected.name(), 10, false).await;
            sqlx::query(sqlx::AssertSqlSafe(
                ddl.replace("{schema}", unexpected.name()),
            ))
            .execute(&mut conn)
            .await
            .expect("add unexpected Atomic relation");
            assert!(matches!(
                open_postgres(unexpected.url(), unexpected.name()).await,
                Err(StoreError::Shape { version: 10, .. })
            ));
            assert!(
                pg_history(&mut conn, "test", unexpected.name())
                    .await
                    .expect("unexpected relation history")
                    .is_none(),
                "the unexpected relation was not baselined"
            );
            assert_eq!(
                pg_marker(&mut conn, "test", unexpected.name())
                    .await
                    .expect("unexpected relation marker"),
                (true, 10)
            );
        }

        for (tag, ddl) in [
            (
                "hist_unlogged",
                "ALTER TABLE {schema}.atomic_publications SET UNLOGGED",
            ),
            (
                "hist_bad_rls",
                "ALTER TABLE {schema}.atomic_publications ENABLE ROW LEVEL SECURITY",
            ),
        ] {
            let altered = crate::testing::schema(tag).await;
            legacy_postgres(altered.url(), altered.name(), 10, false).await;
            sqlx::query(sqlx::AssertSqlSafe(ddl.replace("{schema}", altered.name())))
                .execute(&mut conn)
                .await
                .expect("alter Atomic table durability or access");
            assert!(matches!(
                open_postgres(altered.url(), altered.name()).await,
                Err(StoreError::Shape { version: 10, .. })
            ));
            assert!(
                pg_history(&mut conn, "test", altered.name())
                    .await
                    .expect("altered table history")
                    .is_none(),
                "the altered table was not baselined"
            );
            assert_eq!(
                pg_marker(&mut conn, "test", altered.name())
                    .await
                    .expect("altered table marker"),
                (true, 10)
            );
        }

        let ruled = crate::testing::schema("hist_bad_rule").await;
        legacy_postgres(ruled.url(), ruled.name(), 10, false).await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE RULE suppress_publication AS ON INSERT TO {}.atomic_publications \
             DO INSTEAD NOTHING",
            ruled.name()
        )))
        .execute(&mut conn)
        .await
        .expect("add rewrite rule");
        assert!(matches!(
            open_postgres(ruled.url(), ruled.name()).await,
            Err(StoreError::Shape { version: 10, .. })
        ));
        assert!(
            pg_history(&mut conn, "test", ruled.name())
                .await
                .expect("rule history")
                .is_none(),
            "the rule-bearing legacy store was not baselined"
        );
        assert_eq!(
            pg_marker(&mut conn, "test", ruled.name())
                .await
                .expect("rule marker"),
            (true, 10)
        );

        let triggered = crate::testing::schema("hist_bad_trig").await;
        legacy_postgres(triggered.url(), triggered.name(), 10, false).await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE FUNCTION {}.reject_publication() RETURNS trigger LANGUAGE plpgsql AS \
             'BEGIN RAISE EXCEPTION ''publication blocked''; END'",
            triggered.name()
        )))
        .execute(&mut conn)
        .await
        .expect("trigger function");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER reject_publication BEFORE INSERT ON {}.atomic_publications \
             FOR EACH ROW EXECUTE FUNCTION {}.reject_publication()",
            triggered.name(),
            triggered.name()
        )))
        .execute(&mut conn)
        .await
        .expect("add rejecting trigger");
        assert!(matches!(
            open_postgres(triggered.url(), triggered.name()).await,
            Err(StoreError::Shape { version: 10, .. })
        ));
        assert!(
            pg_history(&mut conn, "test", triggered.name())
                .await
                .expect("trigger history")
                .is_none(),
            "the trigger-bearing legacy store was not baselined"
        );

        let fresh = crate::testing::schema("hist_bad_sum").await;
        open_postgres(fresh.url(), fresh.name())
            .await
            .expect("fresh");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {}.atomic_store_history SET checksum='different' WHERE version=5",
            fresh.name()
        )))
        .execute(&mut conn)
        .await
        .expect("damage checksum");
        assert!(matches!(
            open_postgres(fresh.url(), fresh.name()).await,
            Err(StoreError::History { version: 5, .. })
        ));

        let hole = crate::testing::schema("hist_bad_hole").await;
        open_postgres(hole.url(), hole.name()).await.expect("fresh");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {}.atomic_store_history WHERE version=5",
            hole.name()
        )))
        .execute(&mut conn)
        .await
        .expect("remove history row");
        assert!(matches!(
            open_postgres(hole.url(), hole.name()).await,
            Err(StoreError::History { version: 5, .. })
        ));

        let absent = crate::testing::schema("hist_no_table").await;
        open_postgres(absent.url(), absent.name())
            .await
            .expect("fresh");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TABLE {}.atomic_store_history",
            absent.name()
        )))
        .execute(&mut conn)
        .await
        .expect("remove history table");
        assert!(matches!(
            open_postgres(absent.url(), absent.name()).await,
            Err(StoreError::History {
                version: STORE_VERSION,
                ..
            })
        ));
    }

    #[cfg(feature = "postgres-tests")]
    #[tokio::test]
    async fn postgres_failed_migration_rolls_back_schema_history_and_marker() {
        let schema = crate::testing::schema("hist_rollback").await;
        open_postgres(schema.url(), schema.name())
            .await
            .expect("fresh");
        let mut bad = MIGRATIONS.to_vec();
        bad.push(m(
            STORE_VERSION + 1,
            &[
                "CREATE TABLE atomic_rollback_probe (value INTEGER)",
                "INVALID SQL",
            ],
        ));
        assert!(matches!(
            open_postgres_with(schema.url(), schema.name(), &bad).await,
            Err(StoreError::Database { .. })
        ));
        let mut conn = sqlx::PgConnection::connect(schema.url())
            .await
            .expect("connect");
        assert!(
            pg_objects(&mut conn, schema.name())
                .await
                .expect("objects")
                .iter()
                .all(|item| !item.contains("atomic_rollback_probe")),
            "failed DDL left no table"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT (SELECT version FROM {}.atomic_store_version) AS marker, \
             (SELECT MAX(version) FROM {}.atomic_store_history) AS history",
            schema.name(),
            schema.name()
        )))
        .fetch_one(&mut conn)
        .await
        .expect("versions");
        assert_eq!(row.get::<i64, _>("marker"), STORE_VERSION);
        assert_eq!(row.get::<i64, _>("history"), STORE_VERSION);
    }
}
