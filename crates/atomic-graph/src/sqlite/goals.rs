// SPDX-License-Identifier: Apache-2.0
//! Goal records live in the graph's transaction and credential boundary.

use super::{SqliteStore, db, millis};
use crate::goal::{
    GoalBrief, GoalChild, GoalDefinition, GoalFeedback, GoalFeedbackKind, GoalIgnore,
    GoalIgnoreKind, GoalOutcome, GoalPeriod, GoalSample, GoalSummary, GoalWakeRecord,
};
use crate::store::StoreError;
use serde_json::Value;
use sqlx::{Row, SqliteConnection};
use std::time::SystemTime;

fn invalid(detail: impl Into<String>) -> StoreError {
    StoreError::InvalidGoal {
        detail: detail.into(),
    }
}

fn goal_id(id: &str) -> Result<(), StoreError> {
    let Some(name) = id.strip_prefix("goal-") else {
        return Err(invalid("goal item ID must start with goal-"));
    };
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid(
            "goal item name must be a single 1..=64 byte component",
        ));
    }
    Ok(())
}

fn bounded(value: &str, field: &str, max: usize) -> Result<(), StoreError> {
    if value.trim().is_empty() || value.len() > max || value.contains('\0') {
        return Err(invalid(format!(
            "{field} must be nonempty and at most {max} bytes"
        )));
    }
    Ok(())
}

async fn require_goal(conn: &mut SqliteConnection, id: &str) -> Result<(), StoreError> {
    let row = sqlx::query(
        "SELECT i.kind,i.status FROM atomic_graph_items i JOIN atomic_graph_goal_definitions d ON d.item_id=i.id WHERE i.id=?",
    )
    .bind(id)
    .fetch_optional(conn)
    .await
    .map_err(db)?;
    let Some(row) = row else {
        return Err(invalid(format!("{id} is not a registered goal")));
    };
    let kind: String = row.try_get("kind").map_err(db)?;
    let status: String = row.try_get("status").map_err(db)?;
    if kind != "goal" || status != "pinned" {
        return Err(invalid(format!("{id} is no longer a pinned goal")));
    }
    Ok(())
}

fn outcome(word: &str) -> Result<GoalOutcome, StoreError> {
    match word {
        "act" => Ok(GoalOutcome::Act),
        "no_action" => Ok(GoalOutcome::NoAction),
        other => Err(db(format!("unknown stored goal outcome {other}"))),
    }
}

fn feedback_kind(word: &str) -> Result<GoalFeedbackKind, StoreError> {
    match word {
        "note" => Ok(GoalFeedbackKind::Note),
        "gate_rejection" => Ok(GoalFeedbackKind::GateRejection),
        "answer" => Ok(GoalFeedbackKind::Answer),
        "steering" => Ok(GoalFeedbackKind::Steering),
        other => Err(db(format!("unknown stored goal feedback kind {other}"))),
    }
}

fn ignore_kind(word: &str) -> Result<GoalIgnoreKind, StoreError> {
    match word {
        "dedupe_key" => Ok(GoalIgnoreKind::DedupeKey),
        "fingerprint" => Ok(GoalIgnoreKind::Fingerprint),
        other => Err(db(format!("unknown stored goal ignore kind {other}"))),
    }
}

impl SqliteStore {
    /// Create a pinned goal and its definition in one authenticated transaction.
    /// Repeating the same add is a no-op; changing a definition needs a later
    /// explicit revision action, never a silent overwrite of a running goal.
    pub async fn add_goal(&self, id: &str, definition: GoalDefinition) -> Result<(), StoreError> {
        goal_id(id)?;
        definition.validate().map_err(invalid)?;
        let principal = self
            .human_principal()
            .ok_or_else(|| StoreError::NeedsHumanCredential {
                action: "add a goal".into(),
            })?;
        let id = id.to_owned();
        let encoded = serde_json::to_string(&definition).map_err(db)?;
        self.run_at(move |conn, now| {
            Box::pin(async move {
                let current = sqlx::query("SELECT definition_json FROM atomic_graph_goal_definitions WHERE item_id=?")
                    .bind(&id).fetch_optional(&mut *conn).await.map_err(db)?;
                if let Some(current) = current {
                    require_goal(conn, &id).await?;
                    let existing: String = current.try_get("definition_json").map_err(db)?;
                    if existing == encoded {
                        return Ok(());
                    }
                    return Err(invalid(format!("{id} already has a different definition")));
                }
                if sqlx::query("SELECT id FROM atomic_graph_items WHERE id=?")
                    .bind(&id).fetch_optional(&mut *conn).await.map_err(db)?.is_some() {
                    return Err(invalid(format!("{id} already names another graph item")));
                }
                sqlx::query("INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at,claim_epoch) VALUES (?,? ,?,'goal',2,'pinned',NULL,'human',?,?,0)")
                    .bind(&id).bind(&definition.title).bind(&definition.description)
                    .bind(&principal).bind(now).execute(&mut *conn).await.map_err(db)?;
                sqlx::query("INSERT INTO atomic_graph_goal_definitions (item_id,definition_json,active,revision,set_by,set_at) VALUES (?,?,1,1,?,?)")
                    .bind(&id).bind(encoded).bind(principal).bind(now)
                    .execute(&mut *conn).await.map_err(db)?;
                Ok(())
            })
        }).await
    }

    /// List registered goals without exposing their potentially large briefs.
    pub async fn list_goals(&self) -> Result<Vec<GoalSummary>, StoreError> {
        self.read(|conn| Box::pin(async move {
            let rows = sqlx::query("SELECT i.id,i.title,d.active FROM atomic_graph_items i JOIN atomic_graph_goal_definitions d ON d.item_id=i.id WHERE i.kind='goal' AND i.status='pinned' ORDER BY i.id")
                .fetch_all(&mut *conn).await.map_err(db)?;
            rows.into_iter().map(|row| Ok(GoalSummary {
                id: row.try_get("id").map_err(db)?,
                title: row.try_get("title").map_err(db)?,
                active: row.try_get::<i64,_>("active").map_err(db)? != 0,
            })).collect()
        })).await
    }

    /// Pause or resume future wakes. The graph item stays pinned in either
    /// state, so child tasks remain ordinary ready items.
    pub async fn set_goal_active(&self, id: &str, active: bool) -> Result<(), StoreError> {
        goal_id(id)?;
        self.human_principal()
            .ok_or_else(|| StoreError::NeedsHumanCredential {
                action: "pause or resume a goal".into(),
            })?;
        let id = id.to_owned();
        self.run(move |conn| {
            Box::pin(async move {
                require_goal(conn, &id).await?;
                sqlx::query("UPDATE atomic_graph_goal_definitions SET active=? WHERE item_id=?")
                    .bind(i64::from(active))
                    .bind(&id)
                    .execute(&mut *conn)
                    .await
                    .map_err(db)?;
                Ok(())
            })
        })
        .await
    }

    /// Append human feedback once for a stable request key. A conflicting
    /// reuse refuses before it can attach one person's words to another goal.
    pub async fn add_goal_feedback(
        &self,
        id: &str,
        request_id: &str,
        kind: GoalFeedbackKind,
        text: &str,
    ) -> Result<(), StoreError> {
        goal_id(id)?;
        bounded(request_id, "feedback request ID", 128)?;
        bounded(text, "feedback text", 8_192)?;
        let principal = self
            .human_principal()
            .ok_or_else(|| StoreError::NeedsHumanCredential {
                action: "write goal feedback".into(),
            })?;
        let (id, request_id, text) = (id.to_owned(), request_id.to_owned(), text.to_owned());
        self.run_at(move |conn, now| Box::pin(async move {
            require_goal(conn, &id).await?;
            if let Some(row) = sqlx::query("SELECT goal_id,kind,text,principal FROM atomic_graph_goal_feedback WHERE request_id=?")
                .bind(&request_id).fetch_optional(&mut *conn).await.map_err(db)? {
                let same = row.try_get::<String,_>("goal_id").map_err(db)? == id
                    && row.try_get::<String,_>("kind").map_err(db)? == kind.as_str()
                    && row.try_get::<String,_>("text").map_err(db)? == text
                    && row.try_get::<String,_>("principal").map_err(db)? == principal;
                return if same { Ok(()) } else { Err(invalid("feedback request ID was reused for different content")) };
            }
            let last: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(ordinal),-1) FROM atomic_graph_goal_feedback WHERE goal_id=?")
                .bind(&id).fetch_one(&mut *conn).await.map_err(db)?;
            let ordinal = last.checked_add(1).ok_or_else(|| invalid("feedback ordinal overflow"))?;
            sqlx::query("INSERT INTO atomic_graph_goal_feedback (goal_id,ordinal,request_id,kind,text,principal,written_at) VALUES (?,?,?,?,?,?,?)")
                .bind(&id).bind(ordinal).bind(request_id).bind(kind.as_str()).bind(text)
                .bind(principal).bind(now).execute(&mut *conn).await.map_err(db)?;
            Ok(())
        })).await
    }

    /// Record one measure sample and the action decision for a wake in the
    /// same transaction. Replaying the wake ID returns the first result only
    /// when every caller-supplied field matches it.
    pub async fn record_goal_wake(
        &self,
        id: &str,
        wake_id: &str,
        measure: Value,
        outcome: GoalOutcome,
        reason: &str,
        spent_tokens: u64,
    ) -> Result<(), StoreError> {
        goal_id(id)?;
        bounded(wake_id, "wake ID", 128)?;
        bounded(reason, "wake reason", 4_096)?;
        let spent = i64::try_from(spent_tokens)
            .map_err(|_| invalid("wake token spend exceeds stored range"))?;
        let sample = serde_json::to_string(&measure).map_err(db)?;
        if sample.len() > 16_384 {
            return Err(invalid("measure sample exceeds 16384 bytes"));
        }
        let (id, wake_id, reason) = (id.to_owned(), wake_id.to_owned(), reason.to_owned());
        self.run_at(move |conn, now| Box::pin(async move {
            require_goal(conn, &id).await?;
            if let Some(row) = sqlx::query("SELECT w.outcome,w.reason,w.spent_tokens,s.value_json FROM atomic_graph_goal_wakes w JOIN atomic_graph_goal_measure_samples s ON s.goal_id=w.goal_id AND s.wake_id=w.wake_id WHERE w.goal_id=? AND w.wake_id=?")
                .bind(&id).bind(&wake_id).fetch_optional(&mut *conn).await.map_err(db)? {
                let same = row.try_get::<String,_>("outcome").map_err(db)? == outcome.as_str()
                    && row.try_get::<String,_>("reason").map_err(db)? == reason
                    && row.try_get::<i64,_>("spent_tokens").map_err(db)? == spent
                    && row.try_get::<String,_>("value_json").map_err(db)? == sample;
                return if same { Ok(()) } else { Err(invalid("wake ID was reused for a different measure or decision")) };
            }
            let active: i64 = sqlx::query_scalar("SELECT active FROM atomic_graph_goal_definitions WHERE item_id=?")
                .bind(&id).fetch_one(&mut *conn).await.map_err(db)?;
            if active == 0 { return Err(invalid(format!("{id} is paused"))); }
            sqlx::query("INSERT INTO atomic_graph_goal_wakes (goal_id,wake_id,happened_at,outcome,reason,spent_tokens) VALUES (?,?,?,?,?,?)")
                .bind(&id).bind(&wake_id).bind(now).bind(outcome.as_str()).bind(reason).bind(spent)
                .execute(&mut *conn).await.map_err(db)?;
            sqlx::query("INSERT INTO atomic_graph_goal_measure_samples (goal_id,wake_id,sampled_at,value_json) VALUES (?,?,?,?)")
                .bind(&id).bind(wake_id).bind(now).bind(sample).execute(&mut *conn).await.map_err(db)?;
            Ok(())
        })).await
    }

    /// Ignore one trigger key or candidate fingerprint until its expiry.
    pub async fn ignore_goal_value(
        &self,
        id: &str,
        kind: GoalIgnoreKind,
        value: &str,
        expires_at: SystemTime,
        reason: &str,
    ) -> Result<(), StoreError> {
        goal_id(id)?;
        bounded(value, "ignore value", 256)?;
        bounded(reason, "ignore reason", 2_048)?;
        let expiry = millis(expires_at);
        let (id, value, reason) = (id.to_owned(), value.to_owned(), reason.to_owned());
        self.run_at(move |conn, now| Box::pin(async move {
            require_goal(conn, &id).await?;
            if expiry <= now { return Err(invalid("ignore expiry must be in the future")); }
            let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_goal_ignores WHERE goal_id=? AND expires_at>? AND NOT (kind=? AND value=?)")
                .bind(&id).bind(now).bind(kind.as_str()).bind(&value).fetch_one(&mut *conn).await.map_err(db)?;
            if active >= 100 { return Err(invalid("goal already has 100 active ignore entries")); }
            sqlx::query("INSERT INTO atomic_graph_goal_ignores (goal_id,kind,value,expires_at,reason) VALUES (?,?,?,?,?) ON CONFLICT(goal_id,kind,value) DO UPDATE SET expires_at=excluded.expires_at,reason=excluded.reason")
                .bind(&id).bind(kind.as_str()).bind(value).bind(expiry).bind(reason)
                .execute(&mut *conn).await.map_err(db)?;
            Ok(())
        })).await
    }

    /// Read the bounded, provenance-bearing context the next goal run sees.
    /// This is a host seam for M16/M47; importing external text never turns
    /// it into a command or an executable criterion.
    pub async fn goal_brief(&self, id: &str) -> Result<GoalBrief, StoreError> {
        goal_id(id)?;
        let id = id.to_owned();
        self.read_at(move |conn, now| Box::pin(async move {
            require_goal(conn, &id).await?;
            let row = sqlx::query("SELECT definition_json,active,revision FROM atomic_graph_goal_definitions WHERE item_id=?")
                .bind(&id).fetch_one(&mut *conn).await.map_err(db)?;
            let encoded: String = row.try_get("definition_json").map_err(db)?;
            let definition: GoalDefinition = serde_json::from_str(&encoded).map_err(db)?;
            definition.validate().map_err(db)?;
            let active = row.try_get::<i64,_>("active").map_err(db)? != 0;
            let revision = u64::try_from(row.try_get::<i64,_>("revision").map_err(db)?).map_err(db)?;
            let measure = sqlx::query("SELECT wake_id,sampled_at,value_json FROM atomic_graph_goal_measure_samples WHERE goal_id=? ORDER BY sampled_at DESC,wake_id DESC LIMIT 1")
                .bind(&id).fetch_optional(&mut *conn).await.map_err(db)?
                .map(|row| -> Result<GoalSample,StoreError> {
                    let encoded: String = row.try_get("value_json").map_err(db)?;
                    Ok(GoalSample {
                        wake_id: row.try_get("wake_id").map_err(db)?,
                        sampled_at: row.try_get("sampled_at").map_err(db)?,
                        value: serde_json::from_str(&encoded).map_err(db)?,
                    })
                }).transpose()?;
            let feedback = sqlx::query("SELECT kind,text,principal,written_at FROM atomic_graph_goal_feedback WHERE goal_id=? ORDER BY ordinal DESC LIMIT 20")
                .bind(&id).fetch_all(&mut *conn).await.map_err(db)?
                .into_iter().map(|row| -> Result<GoalFeedback,StoreError> {
                    let kind: String = row.try_get("kind").map_err(db)?;
                    Ok(GoalFeedback {
                        kind: feedback_kind(&kind)?,
                        text: row.try_get("text").map_err(db)?,
                        principal: row.try_get("principal").map_err(db)?,
                        written_at: row.try_get("written_at").map_err(db)?,
                    })
                }).collect::<Result<Vec<_>,_>>()?;
            let ignores = sqlx::query("SELECT kind,value,expires_at,reason FROM atomic_graph_goal_ignores WHERE goal_id=? AND expires_at>? ORDER BY expires_at,value")
                .bind(&id).bind(now).fetch_all(&mut *conn).await.map_err(db)?
                .into_iter().map(|row| -> Result<GoalIgnore,StoreError> {
                    let kind: String = row.try_get("kind").map_err(db)?;
                    Ok(GoalIgnore {
                        kind: ignore_kind(&kind)?,
                        value: row.try_get("value").map_err(db)?,
                        expires_at: row.try_get("expires_at").map_err(db)?,
                        reason: row.try_get("reason").map_err(db)?,
                    })
                }).collect::<Result<Vec<_>,_>>()?;
            let wakes = sqlx::query("SELECT wake_id,happened_at,outcome,reason,spent_tokens FROM atomic_graph_goal_wakes WHERE goal_id=? ORDER BY happened_at DESC,wake_id DESC LIMIT 10")
                .bind(&id).fetch_all(&mut *conn).await.map_err(db)?
                .into_iter().map(|row| -> Result<GoalWakeRecord,StoreError> {
                    let word: String = row.try_get("outcome").map_err(db)?;
                    Ok(GoalWakeRecord {
                        wake_id: row.try_get("wake_id").map_err(db)?,
                        happened_at: row.try_get("happened_at").map_err(db)?,
                        outcome: outcome(&word)?,
                        reason: row.try_get("reason").map_err(db)?,
                        spent_tokens: u64::try_from(row.try_get::<i64,_>("spent_tokens").map_err(db)?).map_err(db)?,
                    })
                }).collect::<Result<Vec<_>,_>>()?;
            let period_ms: i64 = match definition.budget.period {
                GoalPeriod::Day => 86_400_000,
                GoalPeriod::Week => 604_800_000,
            };
            let start = now.checked_sub(now.rem_euclid(period_ms)).ok_or_else(|| invalid("goal period clock underflow"))?;
            let spent: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(spent_tokens),0) FROM atomic_graph_goal_wakes WHERE goal_id=? AND happened_at>=? AND happened_at<=?")
                .bind(&id).bind(start).bind(now).fetch_one(&mut *conn).await.map_err(db)?;
            let spent = u64::try_from(spent).map_err(db)?;
            let used_runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_goal_wakes WHERE goal_id=? AND happened_at>=? AND happened_at<=?")
                .bind(&id).bind(start).bind(now).fetch_one(&mut *conn).await.map_err(db)?;
            let used_runs = u64::try_from(used_runs).map_err(db)?;
            let total_children: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_items WHERE parent=?")
                .bind(&id).fetch_one(&mut *conn).await.map_err(db)?;
            let total_children = u64::try_from(total_children).map_err(db)?;
            let mut open_children = Vec::new();
            let mut closed_children = Vec::new();
            for row in sqlx::query("SELECT id,status FROM atomic_graph_items WHERE parent=? ORDER BY CASE WHEN status='closed' THEN 1 ELSE 0 END,id LIMIT 200")
                .bind(&id).fetch_all(&mut *conn).await.map_err(db)? {
                let child = GoalChild {
                    id: row.try_get("id").map_err(db)?,
                    status: row.try_get("status").map_err(db)?,
                };
                if child.status == "closed" { closed_children.push(child); }
                else { open_children.push(child); }
            }
            let tokens_remaining = definition.budget.tokens.saturating_sub(spent);
            let runs_remaining = definition.budget.runs.saturating_sub(used_runs);
            Ok(GoalBrief {
                id,
                definition,
                active,
                revision,
                measure,
                feedback,
                ignores,
                wakes,
                open_children,
                closed_children,
                total_children,
                children_truncated: total_children > 200,
                tokens_remaining,
                runs_remaining,
            })
        })).await
    }
}
