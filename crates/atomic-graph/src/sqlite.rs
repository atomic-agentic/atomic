// SPDX-License-Identifier: Apache-2.0
//! SQLite work graph storage. Each operation uses one connection behind a lock;
//! claim and lease writes execute in a transaction on that connection.
use crate::store::*;
use atomic_core::workflow::{Criterion, parse_criteria};
use atomic_core::worktree::{StepCommit, Worktrees};
use atomic_durable::gate::HumanCredential;
use atomic_durable::gate::credential::verify_sqlite_in_write_transaction;
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod goals;
mod search;
pub use search::SearchHit;

type DbFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + 'a>>;
fn db(error: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(error.to_string())
}
fn checked_expiry(now: i64, duration: Duration) -> Result<i64, StoreError> {
    if duration.as_millis() == 0 {
        return Err(StoreError::InvalidDuration);
    }
    let millis = i64::try_from(duration.as_millis()).map_err(|_| StoreError::InvalidDuration)?;
    now.checked_add(millis).ok_or(StoreError::InvalidDuration)
}
fn millis(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis().min(i64::MAX as u128) as i64,
        Err(error) => -(error.duration().as_millis().min(i64::MAX as u128) as i64),
    }
}
fn time(v: i64) -> SystemTime {
    if v >= 0 {
        UNIX_EPOCH + Duration::from_millis(v as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(v.unsigned_abs())
    }
}

/// Revoke only the live child tokens recorded for the graph run bound to an
/// epic. The acquisition ledger distinguishes that run from another caller
/// using the same claimant text. Released, unleased children are instead
/// fenced by the invalidated epic binding checked at re-claim.
async fn revoke_bound_run_children(
    c: &mut SqliteConnection,
    epic: &str,
    run_id: &str,
) -> Result<(), StoreError> {
    let children: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM atomic_graph_items i JOIN atomic_graph_leases l ON l.item_id=i.id JOIN atomic_graph_acquisitions a ON a.item_id=l.item_id AND a.claimant=l.claimant AND a.session=l.session AND a.epoch=l.epoch AND a.acquisition=l.acquisition WHERE i.parent=? AND i.status IN ('open','in_progress') AND l.claimant=?")
        .bind(epic)
        .bind(run_id)
        .fetch_one(&mut *c)
        .await
        .map_err(db)?;
    let revoked = sqlx::query("UPDATE atomic_graph_items SET claim_epoch=claim_epoch+1,status='open' WHERE id IN (SELECT i.id FROM atomic_graph_items i JOIN atomic_graph_leases l ON l.item_id=i.id JOIN atomic_graph_acquisitions a ON a.item_id=l.item_id AND a.claimant=l.claimant AND a.session=l.session AND a.epoch=l.epoch AND a.acquisition=l.acquisition WHERE i.parent=? AND i.status IN ('open','in_progress') AND l.claimant=?) AND claim_epoch<9223372036854775807")
        .bind(epic)
        .bind(run_id)
        .execute(&mut *c)
        .await
        .map_err(db)?;
    if i64::try_from(revoked.rows_affected()).map_err(db)? != children {
        return Err(db("graph child claim epoch exhausted"));
    }
    sqlx::query("DELETE FROM atomic_graph_leases WHERE item_id IN (SELECT l.item_id FROM atomic_graph_leases l JOIN atomic_graph_items i ON i.id=l.item_id JOIN atomic_graph_acquisitions a ON a.item_id=l.item_id AND a.claimant=l.claimant AND a.session=l.session AND a.epoch=l.epoch AND a.acquisition=l.acquisition WHERE i.parent=? AND l.claimant=?)")
        .bind(epic)
        .bind(run_id)
        .execute(&mut *c)
        .await
        .map_err(db)?;
    Ok(())
}
fn trust(actor: &Actor) -> Trust {
    if actor.human {
        Trust::Human
    } else {
        Trust::Agent
    }
}
fn parse_trust(s: &str) -> Trust {
    match s {
        "human" => Trust::Human,
        "approved" => Trust::Approved,
        "external" => Trust::External,
        _ => Trust::Agent,
    }
}
fn parse_edge(s: &str) -> Option<EdgeKind> {
    Some(match s {
        "blocks" => EdgeKind::Blocks,
        "parent-child" => EdgeKind::ParentChild,
        "conditional-blocks" => EdgeKind::ConditionalBlocks,
        "waits-for" => EdgeKind::WaitFor,
        "related" => EdgeKind::Related,
        "discovered-from" => EdgeKind::DiscoveredFrom,
        "relates-to" => EdgeKind::RelatesTo,
        "duplicates" => EdgeKind::Duplicates,
        "supersedes" => EdgeKind::Supersedes,
        "replies-to" => EdgeKind::RepliesTo,
        _ => return None,
    })
}

async fn is_gate_copy(c: &mut SqliteConnection, id: &str) -> Result<bool, StoreError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM atomic_graph_items WHERE id=? AND projected_gate_id IS NOT NULL",
    )
    .bind(id)
    .fetch_one(c)
    .await
    .map_err(db)?;
    Ok(count != 0)
}
fn item(row: &sqlx::sqlite::SqliteRow) -> Result<Item, StoreError> {
    let status: String = row.try_get("status").map_err(db)?;
    let tr: String = row.try_get("trust").map_err(db)?;
    Ok(Item {
        id: row.try_get("id").map_err(db)?,
        title: row.try_get("title").map_err(db)?,
        body: row.try_get("body").map_err(db)?,
        kind: row.try_get("kind").map_err(db)?,
        priority: row.try_get("priority").map_err(db)?,
        status: Status::parse(&status).ok_or_else(|| db("invalid stored status"))?,
        parent: row.try_get("parent").map_err(db)?,
        trust: parse_trust(&tr),
        created_by: row.try_get("created_by").map_err(db)?,
        created_at: time(row.try_get("created_at").map_err(db)?),
        closed_at: row
            .try_get::<Option<i64>, _>("closed_at")
            .map_err(db)?
            .map(time),
        due_at: row
            .try_get::<Option<i64>, _>("due_at")
            .map_err(db)?
            .map(time),
    })
}

/// A persistent graph in one SQLite file. The injected clock makes expiry deterministic.
pub struct SqliteStore {
    conn: Arc<Mutex<SqliteConnection>>,
    now: Arc<dyn Fn() -> SystemTime + Send + Sync>,
    human: Option<HumanCredential>,
    #[cfg(debug_assertions)]
    projection_pause: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// One committed outgoing edge read with a graph item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEdge {
    /// Edge kind as stored.
    pub kind: String,
    /// Target item ID.
    pub to: String,
}

impl SqliteStore {
    /// Open a graph database. Existing graph rows survive reopening.
    pub fn open(
        path: &Path,
        now: impl Fn() -> SystemTime + Send + Sync + 'static,
    ) -> Result<Self, StoreError> {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(30));
        let database = path.to_path_buf();
        let conn = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(db)?;
            runtime
                .block_on(atomic_durable::store::open_sqlite(&database))
                .map_err(db)?;
            let conn = runtime
                .block_on(SqliteConnection::connect_with(&options))
                .map_err(db)?;
            Ok::<_, StoreError>(conn)
        })
        .join()
        .map_err(|_| db("SQLite connection worker panicked"))??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            now: Arc::new(now),
            human: None,
            #[cfg(debug_assertions)]
            projection_pause: None,
        })
    }
    /// Inspect a graph without creating or upgrading the project database.
    pub async fn open_for_inspection(path: &Path) -> Result<Self, StoreError> {
        let conn = atomic_durable::store::inspect_sqlite(path)
            .await
            .map_err(db)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            now: Arc::new(SystemTime::now),
            human: None,
            #[cfg(debug_assertions)]
            projection_pause: None,
        })
    }

    /// Read one item, its notes, current blockers and all outgoing edges in
    /// one read transaction for a consistent model-facing graph view.
    pub async fn show_with_edges(
        &self,
        id: String,
    ) -> Result<Option<(Show, Vec<StoredEdge>, Option<String>)>, StoreError> {
        self.read(move |c| Box::pin(async move {
            let Some(item) = get_item(c, &id).await? else { return Ok(None) };
            let acceptance_criteria: Option<String> = sqlx::query_scalar(
                "SELECT json_extract(imported_json,'$.acceptance_criteria') FROM atomic_graph_items WHERE id=?"
            ).bind(&id).fetch_one(&mut *c).await.map_err(db)?;
            let rows = sqlx::query("SELECT text,author,trust FROM atomic_graph_notes WHERE item_id=? ORDER BY ordinal")
                .bind(&id).fetch_all(&mut *c).await.map_err(db)?;
            let notes = rows.iter().map(|row| Ok(Note {
                text: row.try_get("text").map_err(db)?,
                author: row.try_get("author").map_err(db)?,
                trust: parse_trust(&row.try_get::<String,_>("trust").map_err(db)?),
            })).collect::<Result<Vec<_>,StoreError>>()?;
            let blocked_by = blocked(c, &id).await?;
            let holder = lease_row(c, &id).await?.map(|(lease, _)| Holder {
                claimant: lease.claimant, epoch: lease.epoch,
            });
            let rows = sqlx::query("SELECT kind,target FROM atomic_graph_edges WHERE source=? ORDER BY kind,target")
                .bind(&id).fetch_all(&mut *c).await.map_err(db)?;
            let edges = rows.iter().map(|row| Ok(StoredEdge {
                kind: row.try_get("kind").map_err(db)?,
                to: row.try_get("target").map_err(db)?,
            })).collect::<Result<Vec<_>,StoreError>>()?;
            Ok(Some((Show { item, notes, blocked_by, holder }, edges, acceptance_criteria)))
        })).await
    }

    /// Open a graph store for a person whose enrolled credential is checked
    /// inside every write transaction. The supplied actor name grants nothing.
    pub fn open_authorized(
        path: &Path,
        now: impl Fn() -> SystemTime + Send + Sync + 'static,
        credential: HumanCredential,
    ) -> Result<Self, StoreError> {
        let mut store = Self::open(path, now)?;
        store.human = Some(credential);
        Ok(store)
    }

    /// Test seam for pausing between projection deletion and re-insertion.
    /// It is absent from release builds and cannot be set through graph data.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn with_projection_pause(mut self, pause: impl Fn() + Send + Sync + 'static) -> Self {
        self.projection_pause = Some(Arc::new(pause));
        self
    }

    /// Resolve a prior keyed create before deriving a new generated ID. Older
    /// releases generated eight-hex-digit IDs; a replay must keep that target
    /// even after new creates use a longer hash. `create_item` still checks the
    /// stored target and payload fingerprint in its write transaction.
    pub async fn created_item_id_for_request(
        &self,
        request_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let request_id = request_id.to_owned();
        self.read(move |conn| {
            Box::pin(async move {
                let row =
                    sqlx::query("SELECT kind,value FROM atomic_graph_requests WHERE request_id=?")
                        .bind(&request_id)
                        .fetch_optional(conn)
                        .await
                        .map_err(db)?;
                let Some(row) = row else { return Ok(None) };
                let kind: String = row.try_get("kind").map_err(db)?;
                if kind != "create" {
                    return Err(StoreError::RequestConflict { request_id });
                }
                let value: String = row.try_get("value").map_err(db)?;
                let id = if let Some(encoded) = value.strip_prefix("create-v2:") {
                    let saved: serde_json::Value = serde_json::from_str(encoded).map_err(db)?;
                    saved
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .filter(|id| !id.is_empty())
                        .ok_or_else(|| db("create request has no item ID"))?
                        .to_owned()
                } else if !value.is_empty() {
                    value
                } else {
                    return Err(db("create request has no item ID"));
                };
                Ok(Some(id))
            })
        })
        .await
    }

    /// Admit one durable graph run as the enrolled operator. Credential
    /// verification, pinned-epic preflight, and authorization insertion share
    /// the same write transaction. A different active or completed run keeps
    /// its exclusive binding; a repeated request for this run is idempotent.
    pub async fn authorize_graph_run(&self, epic: &str, run_id: &str) -> Result<(), StoreError> {
        let principal = self
            .human_principal()
            .ok_or_else(|| StoreError::NeedsHumanCredential {
                action: "authorize a graph epic run".into(),
            })?;
        let epic = epic.to_owned();
        let run_id = run_id.to_owned();
        self.run(move |conn| Box::pin(async move {
            let item = sqlx::query("SELECT kind,status FROM atomic_graph_items WHERE id=?")
                .bind(&epic).fetch_optional(&mut *conn).await.map_err(db)?;
            let Some(item) = item else {
                return Err(StoreError::EpicAdmission { detail: format!("epic {epic} does not exist; create it with atomic graph create --kind epic --pinned") });
            };
            let kind: String = item.try_get("kind").map_err(db)?;
            let status: String = item.try_get("status").map_err(db)?;
            if kind != "epic" || status != "pinned" {
                return Err(StoreError::EpicAdmission { detail: format!("{epic} is {kind}/{status}; graph runs require a pinned epic created with atomic graph create --kind epic --pinned") });
            }
            if let Some(row) = sqlx::query("SELECT run_id,state FROM atomic_graph_epic_runs WHERE epic_id=?")
                .bind(&epic).fetch_optional(&mut *conn).await.map_err(db)? {
                let held: String = row.try_get("run_id").map_err(db)?;
                if held != run_id {
                    return Err(StoreError::EpicAdmission { detail: format!("epic {epic} is already bound to run {held}; resume that run") });
                }
                let state: String = row.try_get("state").map_err(db)?;
                if state != "active" {
                    return Err(StoreError::EpicAdmission { detail: format!("epic {epic}'s run binding is {state}; it cannot be admitted again") });
                }
            }
            if let Some(row) = sqlx::query("SELECT epic_id FROM atomic_graph_run_authorizations WHERE run_id=?")
                .bind(&run_id).fetch_optional(&mut *conn).await.map_err(db)? {
                let bound: String = row.try_get("epic_id").map_err(db)?;
                if bound != epic {
                    return Err(StoreError::EpicAdmission { detail: format!("run {run_id} was already authorized for epic {bound}") });
                }
            } else {
                sqlx::query("INSERT INTO atomic_graph_run_authorizations (run_id,epic_id,principal) VALUES (?,?,?)")
                    .bind(&run_id).bind(&epic).bind(&principal).execute(&mut *conn).await.map_err(db)?;
            }
            Ok(())
        })).await
    }

    /// Release an exclusive graph binding after its durable run has failed.
    /// The enrolled operator, terminal status, lease check, and deletion are
    /// checked in one write transaction. Removing the authorization too keeps
    /// a recovered old bind checkpoint from acquiring the epic again.
    pub async fn reconcile_failed_epic_run(
        &self,
        epic: &str,
        failed_run_id: &str,
    ) -> Result<(), StoreError> {
        self.human_principal()
            .ok_or_else(|| StoreError::NeedsHumanCredential {
                action: "reconcile a failed graph epic run".into(),
            })?;
        let epic = epic.to_owned();
        let failed_run_id = failed_run_id.to_owned();
        self.run(move |conn| Box::pin(async move {
            let item = sqlx::query("SELECT kind,status FROM atomic_graph_items WHERE id=?")
                .bind(&epic).fetch_optional(&mut *conn).await.map_err(db)?;
            let Some(item) = item else {
                return Err(StoreError::EpicAdmission {
                    detail: format!("epic {epic} does not exist"),
                });
            };
            let kind: String = item.try_get("kind").map_err(db)?;
            let epic_status: String = item.try_get("status").map_err(db)?;
            if kind != "epic" || epic_status != "pinned" {
                return Err(StoreError::EpicAdmission {
                    detail: format!("{epic} is {kind}/{epic_status}; reconciliation requires a pinned epic"),
                });
            }
            let workflow = sqlx::query(
                "SELECT status,name FROM workflow_status WHERE workflow_uuid=?",
            )
            .bind(&failed_run_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(db)?;
            let status: Option<String> = workflow.as_ref()
                .map(|row| row.try_get("status")).transpose().map_err(db)?;
            let name: Option<String> = workflow.as_ref()
                .map(|row| row.try_get("name")).transpose().map_err(db)?;
            if status.as_deref() != Some("ERROR") {
                return Err(StoreError::EpicAdmission {
                    detail: format!(
                        "run {failed_run_id} is not terminal failed (status {}); resume or finish it",
                        status.as_deref().unwrap_or("missing")
                    ),
                });
            }
            if name.as_deref() != Some("atomic.graph.pipeline.v1") {
                return Err(StoreError::EpicAdmission {
                    detail: format!("run {failed_run_id} is not an atomic graph workflow"),
                });
            }
            let lease_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM atomic_graph_leases WHERE claimant=?",
            )
            .bind(&failed_run_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(db)?;
            if lease_count != 0 {
                return Err(StoreError::EpicAdmission {
                    detail: format!(
                        "run {failed_run_id} still holds {lease_count} graph item lease(s); release them first"
                    ),
                });
            }
            let held = sqlx::query(
                "SELECT run_id,state FROM atomic_graph_epic_runs WHERE epic_id=?",
            )
            .bind(&epic)
            .fetch_optional(&mut *conn)
            .await
            .map_err(db)?;
            let auth: Option<String> = sqlx::query_scalar(
                "SELECT epic_id FROM atomic_graph_run_authorizations WHERE run_id=?",
            )
            .bind(&failed_run_id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(db)?;
            if let Some(ref authorized_epic) = auth && authorized_epic != &epic {
                return Err(StoreError::EpicAdmission {
                    detail: format!("run {failed_run_id} is authorized for epic {authorized_epic}, not {epic}"),
                });
            }
            if let Some(row) = held {
                let bound_run: String = row.try_get("run_id").map_err(db)?;
                let state: String = row.try_get("state").map_err(db)?;
                if bound_run == failed_run_id {
                    if auth.as_deref() != Some(epic.as_str()) {
                        return Err(StoreError::EpicAdmission {
                            detail: format!("run {failed_run_id} lacks an operator authorization for epic {epic}"),
                        });
                    }
                    if state != "active" {
                        return Err(StoreError::EpicAdmission {
                            detail: format!("epic {epic}'s binding is {state}; only an active failed run can be reconciled"),
                        });
                    }
                    sqlx::query("DELETE FROM atomic_graph_epic_runs WHERE epic_id=? AND run_id=? AND state='active'")
                        .bind(&epic).bind(&failed_run_id).execute(&mut *conn).await.map_err(db)?;
                } else if auth.is_some() {
                    return Err(StoreError::EpicAdmission {
                        detail: format!("epic {epic} is now bound to run {bound_run}; failed run {failed_run_id} still has authorization"),
                    });
                }
            }
            sqlx::query("DELETE FROM atomic_graph_run_authorizations WHERE run_id=? AND epic_id=?")
                .bind(&failed_run_id).bind(&epic).execute(&mut *conn).await.map_err(db)?;
            Ok(())
        })).await
    }

    /// Mint verified commit evidence from Git facts checked by the host. The
    /// generic GraphStore link method intentionally cannot call this path.
    /// Policy and lease are checked again in the short write transaction, so
    /// a branch-policy change or claim takeover during Git inspection refuses.
    pub async fn link_verified_commit(
        &self,
        lease: Lease,
        worktrees: &Worktrees,
        run_id: &str,
        step_commit: &StepCommit,
    ) -> Result<(), StoreError> {
        self.link_verified_commit_from(lease, worktrees, run_id, run_id, step_commit)
            .await
    }

    /// Verify a child lane's branch for a lease held by its parent graph run.
    /// The caller verifies the child's frozen claim; this store rechecks the
    /// durable parent-child link and fences the parent token in its write.
    pub async fn link_verified_child_commit(
        &self,
        lease: Lease,
        worktrees: &Worktrees,
        parent_run: &str,
        child_run: &str,
        step_commit: &StepCommit,
    ) -> Result<(), StoreError> {
        self.link_verified_commit_from(lease, worktrees, parent_run, child_run, step_commit)
            .await
    }

    async fn link_verified_commit_from(
        &self,
        lease: Lease,
        worktrees: &Worktrees,
        claimant_run: &str,
        worktree_run: &str,
        step_commit: &StepCommit,
    ) -> Result<(), StoreError> {
        // This method mints evidence that the model-facing graph API cannot.
        // The credential is verified again by `run` inside the write Tx.
        let principal = self.verified_operator()?;
        if lease.claimant != claimant_run {
            return Err(StoreError::Fenced {
                item_id: lease.item_id,
                epoch: lease.epoch,
            });
        }
        let policy = self.item_close_policy(&lease.item_id).await?;
        let Some(branch) = policy.and_then(|p| {
            p.requires
                .contains(&Evidence::VerifiedCommit)
                .then_some(p.authorized_branch)
                .flatten()
        }) else {
            return Err(StoreError::NeedsEvidence {
                item_id: lease.item_id,
                required: "verified_commit policy with authorized branch".into(),
            });
        };
        if !valid_policy_branch(&branch) {
            return Err(StoreError::InvalidClosePolicy {
                detail: format!("{} has no valid authorized branch", lease.item_id),
            });
        }
        let Some(proof) = worktrees
            .verify_step_commit_on_branch(worktree_run, step_commit, &branch)
            .map_err(db)?
        else {
            return Err(StoreError::NeedsEvidence {
                item_id: lease.item_id,
                required: "host-verified commit on the authorized branch".into(),
            });
        };
        let child_run = (worktree_run != claimant_run).then(|| worktree_run.to_owned());
        let parent_run = claimant_run.to_owned();
        self.run(move |conn| {
            Box::pin(async move {
                authorize_lease(
                    conn,
                    &lease,
                    Some(principal.as_str()),
                    "link a human claim's verified commit",
                )
                .await?;
                check_lease(conn, &lease).await?;
                if let Some(child) = child_run {
                    let linked = sqlx::query(
                        "SELECT parent_workflow_id FROM workflow_status WHERE workflow_uuid=?",
                    )
                    .bind(&child)
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(db)?;
                    let parent: Option<String> = linked
                        .as_ref()
                        .map(|row| row.try_get("parent_workflow_id").map_err(db))
                        .transpose()?
                        .flatten();
                    if parent.as_deref() != Some(parent_run.as_str()) {
                        return Err(StoreError::Fenced {
                            item_id: lease.item_id.clone(),
                            epoch: lease.epoch,
                        });
                    }
                }
                let current = sqlx::query("SELECT requires_verified_commit,authorized_branch FROM atomic_graph_item_policy WHERE item_id=?")
                    .bind(&lease.item_id).fetch_optional(&mut *conn).await.map_err(db)?;
                let still_authorized = match current {
                    Some(row) => {
                        let required: i64 = row.try_get("requires_verified_commit").map_err(db)?;
                        let current_branch: Option<String> = row.try_get("authorized_branch").map_err(db)?;
                        required != 0 && current_branch.as_deref() == Some(proof.branch.as_str())
                    }
                    None => false,
                };
                if !still_authorized {
                    return Err(StoreError::NeedsEvidence {
                        item_id: lease.item_id,
                        required: "current authorized branch".into(),
                    });
                }
                sqlx::query("INSERT INTO atomic_graph_commit_links (item_id,sha,indexed,on_branch,verified_branch,verified_head,pinned_base) VALUES (?,?,1,1,?,?,?) ON CONFLICT(item_id,sha) DO UPDATE SET indexed=1,on_branch=1,verified_branch=excluded.verified_branch,verified_head=excluded.verified_head,pinned_base=excluded.pinned_base")
                    .bind(&lease.item_id)
                    .bind(&proof.commit)
                    .bind(&proof.branch)
                    .bind(&proof.head)
                    .bind(&proof.pinned_base)
                    .execute(&mut *conn).await.map_err(db)?;
                Ok(())
            })
        }).await
    }

    fn actor_for_write(&self, actor: Actor, action: &str) -> Result<Actor, StoreError> {
        if actor.human {
            let credential =
                self.human
                    .as_ref()
                    .ok_or_else(|| StoreError::NeedsHumanCredential {
                        action: action.to_owned(),
                    })?;
            Ok(Actor::human(credential.principal().as_str()))
        } else {
            Ok(actor)
        }
    }
    fn human_principal(&self) -> Option<String> {
        self.human
            .as_ref()
            .map(|credential| credential.principal().as_str().to_owned())
    }
    fn verified_operator(&self) -> Result<String, StoreError> {
        self.human_principal()
            .ok_or_else(|| StoreError::NeedsHumanCredential {
                action: "attest a graph commit".into(),
            })
    }
    async fn run<T: Send + 'static>(
        &self,
        f: impl for<'a> FnOnce(&'a mut SqliteConnection) -> DbFuture<'a, T> + Send + 'static,
    ) -> Result<T, StoreError> {
        self.transaction(true, f).await
    }

    async fn read<T: Send + 'static>(
        &self,
        f: impl for<'a> FnOnce(&'a mut SqliteConnection) -> DbFuture<'a, T> + Send + 'static,
    ) -> Result<T, StoreError> {
        self.transaction(false, f).await
    }

    async fn transaction<T: Send + 'static>(
        &self,
        write: bool,
        f: impl for<'a> FnOnce(&'a mut SqliteConnection) -> DbFuture<'a, T> + Send + 'static,
    ) -> Result<T, StoreError> {
        let connection = Arc::clone(&self.conn);
        let credential = self.human.clone();
        #[cfg(debug_assertions)]
        let projection_pause = self.projection_pause.clone();
        let (answer, received) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let result = (|| {
                let mut conn = connection.lock().map_err(db)?;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(db)?;
                runtime.block_on(async {
                    sqlx::query(if write { "BEGIN IMMEDIATE" } else { "BEGIN" })
                        .execute(&mut *conn)
                        .await
                        .map_err(db)?;
                    if write && let Some(credential) = &credential {
                        match verify_sqlite_in_write_transaction(&mut conn, credential).await {
                            Ok(true) => {}
                            Ok(false) => {
                                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                                return Err(StoreError::NeedsHumanCredential {
                                    action: "mutate graph with current enrollment".to_owned(),
                                });
                            }
                            Err(error) => {
                                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                                return Err(db(error));
                            }
                        }
                    }
                    let result = f(&mut conn).await;
                    match result {
                        Ok(value) => {
                            if write
                                && let Err(error) = refresh_projection(
                                    &mut conn,
                                    #[cfg(debug_assertions)]
                                    projection_pause.as_deref(),
                                )
                                .await
                            {
                                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                                return Err(error);
                            }
                            match sqlx::query("COMMIT").execute(&mut *conn).await {
                                Ok(_) => Ok(value),
                                Err(_) => {
                                    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await; // CONTROL_OFF: graph-commit-recovery
                                    Err(StoreError::OutcomeUnknown {
                                        operation: "commit",
                                    })
                                }
                            }
                        }
                        Err(error) => {
                            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                            Err(error)
                        }
                    }
                })
            })();
            let _ = answer.send(result);
        });
        received
            .await
            .map_err(|_| db("SQLite operation worker panicked"))?
    }
    async fn run_at<T: Send + 'static>(
        &self,
        f: impl for<'a> FnOnce(&'a mut SqliteConnection, i64) -> DbFuture<'a, T> + Send + 'static,
    ) -> Result<T, StoreError> {
        let clock = Arc::clone(&self.now);
        self.run(move |conn| {
            let now = millis(clock());
            f(conn, now)
        })
        .await
    }

    async fn read_at<T: Send + 'static>(
        &self,
        f: impl for<'a> FnOnce(&'a mut SqliteConnection, i64) -> DbFuture<'a, T> + Send + 'static,
    ) -> Result<T, StoreError> {
        let clock = Arc::clone(&self.now);
        self.read(move |conn| {
            let now = millis(clock());
            f(conn, now)
        })
        .await
    }
}

/// Make a stable 96-bit item ID from source bytes. Older short IDs remain
/// replayable through the stored request target.
pub fn hash_id(namespace: &str, source: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(namespace.as_bytes());
    h.update([0]);
    h.update(source);
    format!("at-{:x}", h.finalize())[..27].to_owned()
}

async fn get_item(conn: &mut SqliteConnection, id: &str) -> Result<Option<Item>, StoreError> {
    sqlx::query("SELECT * FROM atomic_graph_items WHERE id=?")
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(db)?
        .as_ref()
        .map(item)
        .transpose()
}
async fn request_seen(
    conn: &mut SqliteConnection,
    request_id: &str,
    kind: &str,
    target: &str,
) -> Result<bool, StoreError> {
    let row = sqlx::query("SELECT kind,value FROM atomic_graph_requests WHERE request_id=?")
        .bind(request_id)
        .fetch_optional(conn)
        .await
        .map_err(db)?;
    let Some(row) = row else { return Ok(false) };
    let old_kind: String = row.try_get("kind").map_err(db)?;
    let old_target: String = row.try_get("value").map_err(db)?;
    if old_kind != kind || old_target != target {
        return Err(StoreError::RequestConflict {
            request_id: request_id.to_owned(),
        });
    }
    Ok(true)
}
fn create_request_fingerprint(actor: &Actor, draft: &Draft) -> Result<String, StoreError> {
    let payload = serde_json::json!({
        "actor": &actor.principal,
        "human": actor.human,
        "id": &draft.id,
        "title": &draft.title,
        "body": &draft.body,
        "kind": &draft.kind,
        "priority": draft.priority,
        "status": draft.status.as_str(),
        "parent": &draft.parent,
        "due_at_ms": draft.due_at.map(millis),
        "notes": &draft.notes,
    });
    let bytes = serde_json::to_vec(&payload).map_err(db)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
async fn create_request_seen(
    conn: &mut SqliteConnection,
    request_id: &str,
    target: &str,
    fingerprint: &str,
) -> Result<bool, StoreError> {
    let row = sqlx::query("SELECT kind,value FROM atomic_graph_requests WHERE request_id=?")
        .bind(request_id)
        .fetch_optional(conn)
        .await
        .map_err(db)?;
    let Some(row) = row else { return Ok(false) };
    let kind: String = row.try_get("kind").map_err(db)?;
    let value: String = row.try_get("value").map_err(db)?;
    // Stores written before payload fingerprints kept only the item ID. They
    // remain replayable, although their original draft cannot be recovered.
    let same = if value == target {
        true
    } else if let Some(encoded) = value.strip_prefix("create-v2:") {
        let saved: serde_json::Value = serde_json::from_str(encoded).map_err(db)?;
        saved.get("id").and_then(serde_json::Value::as_str) == Some(target)
            && saved.get("sha256").and_then(serde_json::Value::as_str) == Some(fingerprint)
    } else {
        false
    };
    if kind != "create" || !same {
        return Err(StoreError::RequestConflict {
            request_id: request_id.to_owned(),
        });
    }
    Ok(true)
}
async fn refresh_projection(
    conn: &mut SqliteConnection,
    #[cfg(debug_assertions)] pause: Option<&(dyn Fn() + Send + Sync)>,
) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM atomic_graph_blocked")
        .execute(&mut *conn)
        .await
        .map_err(db)?;
    #[cfg(debug_assertions)]
    if let Some(pause) = pause {
        pause();
    }
    // Imported parent-child relations need the original beads representation:
    // `parent_issue` alone never blocks, while an explicit dependency inherits
    // the parent's active blockers. Keep those imported relations out of the
    // baseline, regardless of the parent's status. Native relations retain
    // their existing closed/pinned behavior.
    sqlx::query(r#"
        INSERT OR IGNORE INTO atomic_graph_blocked (source,target)
        SELECT e.source,e.target
        FROM atomic_graph_edges e
        JOIN atomic_graph_items b ON b.id=e.target
        JOIN atomic_graph_items child ON child.id=e.source
        WHERE e.kind IN ('blocks','parent-child','conditional-blocks','waits-for')
          AND ((e.kind='conditional-blocks'
                AND (b.status!='closed' OR COALESCE(b.terminal_outcome,'unknown')!='failure'))
               OR (e.kind!='conditional-blocks' AND b.status!='closed'))
          AND NOT (e.kind='parent-child' AND (b.status='pinned' OR
            (b.imported_json IS NOT NULL AND child.imported_json IS NOT NULL
             AND NOT COALESCE(
               json_extract(child.imported_json,'$.metadata.atomic_graph.schema_version')=1
               AND json_extract(child.imported_json,'$.metadata.atomic_graph.native_parent')=e.target,
               0))))
    "#)
    .execute(&mut *conn)
    .await
    .map_err(db)?;
    // Follow each active blocker through explicitly imported parent-child
    // edges. Beads leaves the blocker itself ready when an ancestor depends on
    // it, so that item can clear the dependency; intermediate ancestors stay
    // held. The target index finds children at each depth; UNION bounds cycles.
    sqlx::query(
        r#"
        WITH RECURSIVE held(id, blocker) AS (
            SELECT b.source, b.target
            FROM atomic_graph_blocked b
            JOIN atomic_graph_items i ON i.id=b.source
            WHERE i.imported_json IS NOT NULL
            UNION
            SELECT e.source, h.blocker
            FROM held h
            JOIN atomic_graph_edges e ON e.target=h.id AND e.kind='parent-child'
            JOIN atomic_graph_items child ON child.id=e.source
            WHERE child.imported_json IS NOT NULL
              AND NOT COALESCE(
                    json_extract(child.imported_json,'$.metadata.atomic_graph.schema_version')=1
                    AND json_extract(child.imported_json,'$.metadata.atomic_graph.native_parent')=e.target,
                    0)
              AND EXISTS (SELECT 1 FROM json_each(child.imported_json,'$.dependencies') d
                          WHERE json_extract(d.value,'$.type')='parent-child'
                            AND json_extract(d.value,'$.depends_on_id')=e.target)
              AND e.source != h.blocker
        )
        INSERT OR IGNORE INTO atomic_graph_blocked (source,target)
        SELECT e.source,e.target
        FROM held h
        JOIN atomic_graph_edges e ON e.target=h.id AND e.kind='parent-child'
        JOIN atomic_graph_items child ON child.id=e.source
        WHERE child.imported_json IS NOT NULL
          AND NOT COALESCE(
                json_extract(child.imported_json,'$.metadata.atomic_graph.schema_version')=1
                AND json_extract(child.imported_json,'$.metadata.atomic_graph.native_parent')=e.target,
                0)
          AND EXISTS (SELECT 1 FROM json_each(child.imported_json,'$.dependencies') d
                      WHERE json_extract(d.value,'$.type')='parent-child'
                        AND json_extract(d.value,'$.depends_on_id')=e.target)
          AND e.source != h.blocker
    "#,
    )
    .execute(conn)
    .await
    .map_err(db)?;
    Ok(())
}
async fn blocked(conn: &mut SqliteConnection, id: &str) -> Result<Vec<String>, StoreError> {
    let rows =
        sqlx::query("SELECT target FROM atomic_graph_blocked WHERE source=? ORDER BY target")
            .bind(id)
            .fetch_all(conn)
            .await
            .map_err(db)?;
    rows.iter()
        .map(|r| r.try_get("target").map_err(db))
        .collect()
}
// `atomic_graph_blocked` already holds every unresolved direct blocker. Any
// longer path starts with one of these rows, so source membership needs only
// this indexed lookup; expanding all reachable pairs adds no information.
const READY_ITEMS_SQL: &str = "SELECT i.* FROM atomic_graph_items i LEFT JOIN atomic_graph_leases l ON l.item_id=i.id WHERE (i.status='open' OR (i.status='in_progress' AND l.expires_at<=?) OR (?=1 AND i.status='in_progress' AND l.item_id IS NULL)) AND i.kind NOT IN ('goal','message') AND i.projected_gate_id IS NULL AND (i.due_at IS NULL OR i.due_at<=?) AND NOT EXISTS (SELECT 1 FROM atomic_graph_blocked b WHERE b.source=i.id) ORDER BY i.priority, i.id LIMIT ?";
const CLAIM_HAS_BLOCKER_SQL: &str =
    "SELECT EXISTS (SELECT 1 FROM atomic_graph_blocked WHERE source=?)";
async fn ready_items(
    conn: &mut SqliteConnection,
    now: i64,
    limit: usize,
    include_unheld_in_progress: bool,
) -> Result<Vec<Item>, StoreError> {
    let rows = sqlx::query(READY_ITEMS_SQL)
        .bind(now)
        .bind(i64::from(include_unheld_in_progress))
        .bind(now)
        .bind(limit as i64)
        .fetch_all(conn)
        .await
        .map_err(db)?;
    rows.iter().map(item).collect()
}
async fn lease_row(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<Option<(Lease, i64)>, StoreError> {
    let row = sqlx::query("SELECT * FROM atomic_graph_leases WHERE item_id=?")
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(db)?;
    row.map(|r| {
        Ok((
            Lease {
                item_id: id.to_owned(),
                claimant: r.try_get("claimant").map_err(db)?,
                session: r.try_get::<i64, _>("session").map_err(db)? as u64,
                epoch: r.try_get("epoch").map_err(db)?,
                acquisition: r.try_get("acquisition").map_err(db)?,
            },
            r.try_get("expires_at").map_err(db)?,
        ))
    })
    .transpose()
}
async fn check_lease(conn: &mut SqliteConnection, lease: &Lease) -> Result<(), StoreError> {
    let status = get_item(conn, &lease.item_id)
        .await?
        .ok_or_else(|| StoreError::MissingItem {
            item_id: lease.item_id.clone(),
        })?
        .status;
    let current = lease_row(conn, &lease.item_id).await?;
    let Some((held, _)) = current else {
        return Err(StoreError::Fenced {
            item_id: lease.item_id.clone(),
            epoch: 0,
        });
    };
    if held != *lease || status != Status::InProgress {
        return Err(StoreError::Fenced {
            item_id: lease.item_id.clone(),
            epoch: held.epoch,
        });
    }
    Ok(())
}
async fn human_lease_principal(
    conn: &mut SqliteConnection,
    lease: &Lease,
) -> Result<Option<String>, StoreError> {
    let row = sqlx::query("SELECT principal,claimant,session,epoch,acquisition FROM atomic_graph_human_leases WHERE item_id=?")
        .bind(&lease.item_id)
        .fetch_optional(conn)
        .await
        .map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    let claimant: String = row.try_get("claimant").map_err(db)?;
    let session: i64 = row.try_get("session").map_err(db)?;
    let epoch: i64 = row.try_get("epoch").map_err(db)?;
    let acquisition: String = row.try_get("acquisition").map_err(db)?;
    if claimant == lease.claimant
        && session == lease.session as i64
        && epoch == lease.epoch
        && acquisition == lease.acquisition
    {
        Ok(Some(row.try_get("principal").map_err(db)?))
    } else {
        Ok(None)
    }
}

async fn authorize_lease(
    conn: &mut SqliteConnection,
    lease: &Lease,
    verified_principal: Option<&str>,
    action: &str,
) -> Result<(), StoreError> {
    if let Some(principal) = human_lease_principal(conn, lease).await?
        && Some(principal.as_str()) != verified_principal
    {
        return Err(StoreError::NeedsHumanCredential {
            action: action.to_owned(),
        });
    }
    Ok(())
}

async fn record_claim_provenance(
    conn: &mut SqliteConnection,
    lease: &Lease,
    verified_principal: Option<&str>,
) -> Result<(), StoreError> {
    authorize_lease(conn, lease, verified_principal, "use a human claim").await?;
    if let Some(principal) = verified_principal {
        sqlx::query("INSERT OR REPLACE INTO atomic_graph_human_leases VALUES (?,?,?,?,?,?)")
            .bind(&lease.item_id)
            .bind(principal)
            .bind(&lease.claimant)
            .bind(lease.session as i64)
            .bind(lease.epoch)
            .bind(&lease.acquisition)
            .execute(conn)
            .await
            .map_err(db)?;
    } else {
        sqlx::query("DELETE FROM atomic_graph_human_leases WHERE item_id=?")
            .bind(&lease.item_id)
            .execute(conn)
            .await
            .map_err(db)?;
    }
    Ok(())
}
/// Branch names in a verified-commit policy are intentionally narrower than
/// Git's full ref grammar. The host still resolves and verifies the ref before
/// it may attest to evidence; this check only prevents an ambiguous stored
/// policy from being accepted at the graph boundary.
fn valid_policy_branch(branch: &str) -> bool {
    let Some(name) = branch.strip_prefix("refs/heads/") else {
        return false;
    };
    !name.is_empty()
        && branch.len() <= 255
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && !part.ends_with('.')
                && !part.ends_with(".lock")
                && !part.contains("..")
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

async fn enforce_close_policy(
    conn: &mut SqliteConnection,
    item_id: &str,
) -> Result<(), StoreError> {
    let item_policy = sqlx::query(
        "SELECT requires_commit,requires_verified_commit,authorized_branch FROM atomic_graph_item_policy WHERE item_id=?",
    )
    .bind(item_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db)?;
    let mut authorized_branch = None;
    let policy = match item_policy {
        Some(policy) => {
            if policy
                .try_get::<i64, _>("requires_verified_commit")
                .map_err(db)?
                != 0
            {
                let branch: Option<String> = policy.try_get("authorized_branch").map_err(db)?;
                if !branch.as_deref().is_some_and(valid_policy_branch) {
                    return Err(StoreError::InvalidClosePolicy {
                        detail: format!("{item_id} has no valid authorized branch"),
                    });
                }
                authorized_branch = branch;
            }
            Some(policy)
        }
        None => {
            let legacy = sqlx::query(
                "SELECT requires_commit,requires_verified_commit FROM atomic_graph_policy WHERE id=1",
            )
            .fetch_optional(&mut *conn)
            .await
            .map_err(db)?;
            if let Some(row) = &legacy
                && row
                    .try_get::<i64, _>("requires_verified_commit")
                    .map_err(db)?
                    != 0
            {
                return Err(StoreError::InvalidClosePolicy {
                    detail: "legacy verified_commit policy has no authorized branch".into(),
                });
            }
            legacy
        }
    };
    let Some(policy) = policy else { return Ok(()) };
    let requires_commit: i64 = policy.try_get("requires_commit").map_err(db)?;
    let requires_verified: i64 = policy.try_get("requires_verified_commit").map_err(db)?;
    let links =
        sqlx::query("SELECT indexed,on_branch,verified_branch,verified_head,pinned_base FROM atomic_graph_commit_links WHERE item_id=?")
            .bind(item_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(db)?;
    if requires_commit != 0 && links.is_empty() {
        return Err(StoreError::NeedsEvidence {
            item_id: item_id.to_owned(),
            required: "commit".into(),
        });
    }
    if requires_verified != 0 {
        let mut verified = false;
        for link in links {
            let indexed: i64 = link.try_get("indexed").map_err(db)?;
            let on_branch: i64 = link.try_get("on_branch").map_err(db)?;
            let branch: Option<String> = link.try_get("verified_branch").map_err(db)?;
            let head: Option<String> = link.try_get("verified_head").map_err(db)?;
            let base: Option<String> = link.try_get("pinned_base").map_err(db)?;
            let attested = branch.as_deref() == authorized_branch.as_deref()
                && head.is_some()
                && base.is_some();
            verified |= indexed != 0 && on_branch != 0 && attested;
        }
        if !verified {
            return Err(StoreError::NeedsEvidence {
                item_id: item_id.to_owned(),
                required: "verified_commit".into(),
            });
        }
    }
    Ok(())
}
// The trait deliberately spells out a Send future for every backend operation.
#[allow(clippy::manual_async_fn)]
impl GraphStore for SqliteStore {
    fn create_item(
        &self,
        actor: Actor,
        request_id: String,
        draft: Draft,
    ) -> impl Future<Output = Result<Item, StoreError>> + Send {
        async move {
            if draft.status == Status::Closed {
                return Err(StoreError::CannotCreateClosed { item_id: draft.id });
            }
            if draft.kind == "gate" || draft.id.starts_with("gate:") {
                return Err(StoreError::InvalidStatusTransition {
                    detail: "gate items are read-only copies of the gate store".into(),
                });
            }
            let actor = self.actor_for_write(actor, "create a human item")?;
            let fingerprint = create_request_fingerprint(&actor, &draft)?;
            self.run_at(move |c, now|Box::pin(async move {
   if create_request_seen(c,&request_id,&draft.id,&fingerprint).await? { return get_item(c,&draft.id).await?.ok_or_else(||db("create request refers to missing item")); }
   if let Some(parent)=&draft.parent {
       if get_item(c,parent).await?.is_none() { return Err(StoreError::MissingItem { item_id: parent.clone() }); }
       if is_gate_copy(c,parent).await? { return Err(StoreError::InvalidEdge { detail: "a gate copy cannot be a parent".into() }); }
   }
   let status=draft.status.as_str();let closed=if draft.status==Status::Closed {Some(now)}else{None};let tr=trust(&actor);
   sqlx::query("INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at,closed_at,due_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)").bind(&draft.id).bind(&draft.title).bind(&draft.body).bind(&draft.kind).bind(draft.priority).bind(status).bind(&draft.parent).bind(tr.as_str()).bind(&actor.principal).bind(now).bind(closed).bind(draft.due_at.map(millis)).execute(&mut *c).await.map_err(db)?;
   if let Some(parent)=&draft.parent {sqlx::query("INSERT OR IGNORE INTO atomic_graph_edges VALUES (?,'parent-child',?)").bind(&draft.id).bind(parent).execute(&mut *c).await.map_err(db)?;}
   for (n,text) in draft.notes.iter().enumerate(){sqlx::query("INSERT INTO atomic_graph_notes VALUES (?,?,?,?,?)").bind(&draft.id).bind(n as i64).bind(text).bind(&actor.principal).bind(tr.as_str()).execute(&mut *c).await.map_err(db)?;}
   let request_value = format!("create-v2:{}", serde_json::json!({"id":&draft.id,"sha256":fingerprint}));
   sqlx::query("INSERT INTO atomic_graph_requests VALUES (?,'create',?)").bind(request_id).bind(request_value).execute(&mut *c).await.map_err(db)?;
   get_item(c,&draft.id).await?.ok_or_else(||db("inserted item missing"))
  })).await
        }
    }
    fn note(
        &self,
        actor: Actor,
        lease: Lease,
        request_id: String,
        text: String,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async move {
            let actor = self.actor_for_write(actor, "write a human note")?;
            let principal = self.human_principal();
            self.run(move |c| {
                Box::pin(async move {
                    authorize_lease(c, &lease, principal.as_deref(), "note on a human claim")
                        .await?;
                    check_lease(c, &lease).await?;
                    if request_seen(c, &request_id, "note", &lease.item_id).await? {
                        return Ok(());
                    }
                    let n: i64 = sqlx::query_scalar(
                        "SELECT COALESCE(MAX(ordinal)+1,0) FROM atomic_graph_notes WHERE item_id=?",
                    )
                    .bind(&lease.item_id)
                    .fetch_one(&mut *c)
                    .await
                    .map_err(db)?;
                    sqlx::query("INSERT INTO atomic_graph_notes VALUES (?,?,?,?,?)")
                        .bind(&lease.item_id)
                        .bind(n)
                        .bind(text)
                        .bind(&actor.principal)
                        .bind(trust(&actor).as_str())
                        .execute(&mut *c)
                        .await
                        .map_err(db)?;
                    sqlx::query("INSERT INTO atomic_graph_requests VALUES (?,'note',?)")
                        .bind(request_id)
                        .bind(&lease.item_id)
                        .execute(&mut *c)
                        .await
                        .map_err(db)?;
                    Ok(())
                })
            })
            .await
        }
    }
    fn add_edge(
        &self,
        actor: Actor,
        from: String,
        kind: EdgeKind,
        to: String,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async move {
            self.actor_for_write(actor, "link items as a person")?;
            self.run(move |c| {
                Box::pin(async move {
                    if kind == EdgeKind::ParentChild {
                        let existing = get_item(c, &from).await?.and_then(|item| item.parent);
                        if let Some(parent) = existing
                            && parent != to
                        {
                            return Err(StoreError::InvalidEdge {
                                detail: format!("{from} already has parent {parent}"),
                            });
                        }
                    }
                    for endpoint in [&from, &to] {
                        let item = get_item(c, endpoint).await?;
                        if is_gate_copy(c, endpoint).await? {
                            return Err(StoreError::InvalidEdge {
                                detail: "gate items are read-only copies of the gate store".into(),
                            });
                        }
                        if item.is_none() {
                            return Err(StoreError::MissingItem {
                                item_id: endpoint.clone(),
                            });
                        }
                    }
                    sqlx::query("INSERT OR IGNORE INTO atomic_graph_edges VALUES (?,?,?)")
                        .bind(&from)
                        .bind(kind.as_str())
                        .bind(&to)
                        .execute(&mut *c)
                        .await
                        .map_err(db)?;
                    if kind == EdgeKind::ParentChild {
                        sqlx::query("UPDATE atomic_graph_items SET parent=? WHERE id=?")
                            .bind(&to)
                            .bind(&from)
                            .execute(c)
                            .await
                            .map_err(db)?;
                    }
                    Ok(())
                })
            })
            .await
        }
    }
    fn set_status(
        &self,
        actor: Actor,
        id: String,
        status: Status,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async move {
            let actor = self.actor_for_write(actor, "set status")?;
            // CONTROL_OFF: graph-agent-status-human-only
            let agent_status_action = !actor.human;
            if agent_status_action {
                return Err(StoreError::NeedsHumanCredential {
                    action: "set status".to_owned(),
                });
            }
            self.run_at(move |c, now| {
                Box::pin(async move {
                    let item = get_item(c, &id)
                        .await?
                        .ok_or_else(|| StoreError::MissingItem {
                            item_id: id.clone(),
                        })?;
                    if is_gate_copy(c, &id).await? {
                        return Err(StoreError::InvalidStatusTransition {
                            detail: "gate items are read-only copies of the gate store".into(),
                        });
                    }
                    // CONTROL_OFF: graph-admin-pin-epic-only
                    // Import leaves Beads epics open; only an operator may pin
                    // an open epic for a graph run. A terminal or non-epic
                    // item cannot be made runnable by a status edit.
                    if status == Status::Pinned
                        && (item.kind != "epic"
                            || !matches!(item.status, Status::Open | Status::Pinned))
                    {
                        return Err(StoreError::InvalidStatusTransition {
                            detail: format!(
                                "only an open epic can be pinned ({} is a {} in status {})",
                                id,
                                item.kind,
                                item.status.as_str()
                            ),
                        });
                    }
                    // Replaying a no-op operator command must not revoke an
                    // otherwise unchanged item. In-progress is different:
                    // an explicit admin status action revokes its live lease.
                    if item.status == status && status != Status::InProgress {
                        return Ok(());
                    }
                    if status == Status::Closed {
                        enforce_close_policy(c, &id).await?;
                    }
                    if item.kind == "epic"
                        && item.status == Status::Pinned
                        && status != Status::Pinned
                    {
                        let run_id: Option<String> = sqlx::query_scalar("SELECT run_id FROM atomic_graph_epic_runs WHERE epic_id=? AND state='active'")
                            .bind(&id)
                            .fetch_optional(&mut *c)
                            .await
                            .map_err(db)?;
                        if let Some(run_id) = run_id {
                            sqlx::query("UPDATE atomic_graph_epic_runs SET state='invalidated' WHERE epic_id=? AND state='active'")
                                .bind(&id)
                                .execute(&mut *c)
                                .await
                                .map_err(db)?;
                            // CONTROL_OFF: graph-admin-epic-child-revoke
                            revoke_bound_run_children(c, &id, &run_id).await?;
                        }
                    }
                    // CONTROL_OFF: graph-admin-status-epoch
                    // A human status change revokes a claim even if the item is
                    // later reopened. In particular, close -> reopen must not
                    // restore the release epoch of a pending review gate.
                    let changed = sqlx::query("UPDATE atomic_graph_items SET status=?,closed_at=?,terminal_outcome=NULL,last_finish_session=NULL,last_finish_epoch=NULL,last_finish_claimant=NULL,last_finish_acquisition=NULL,claim_epoch=claim_epoch+1 WHERE id=? AND claim_epoch<9223372036854775807")
                        .bind(status.as_str())
                        .bind(if status == Status::Closed { Some(now) } else { None })
                        .bind(&id)
                        .execute(&mut *c)
                        .await
                        .map_err(db)?;
                    if changed.rows_affected() != 1 {
                        return Err(db("graph claim epoch exhausted"));
                    }
                    sqlx::query("DELETE FROM atomic_graph_leases WHERE item_id=?")
                        .bind(id)
                        .execute(c)
                        .await
                        .map_err(db)?;
                    Ok(())
                })
            })
            .await
        }
    }
    fn show(
        &self,
        _actor: Actor,
        id: String,
    ) -> impl Future<Output = Result<Option<Show>, StoreError>> + Send {
        async move {
            self.show_with_edges(id)
                .await
                .map(|view| view.map(|(show, _, _)| show))
        }
    }
    fn all_items(
        &self,
        _actor: Actor,
    ) -> impl Future<Output = Result<Vec<Item>, StoreError>> + Send {
        async move {
            self.read(move |c| {
                Box::pin(async move {
                    let rows = sqlx::query("SELECT * FROM atomic_graph_items ORDER BY id")
                        .fetch_all(c)
                        .await
                        .map_err(db)?;
                    rows.iter().map(item).collect()
                })
            })
            .await
        }
    }
    fn ready(
        &self,
        _actor: Actor,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<Item>, StoreError>> + Send {
        async move {
            self.read_at(move |c, now| {
                Box::pin(async move { ready_items(c, now, limit, false).await })
            })
            .await
        }
    }
    fn claim(
        &self,
        actor: Actor,
        claimant: String,
        id: String,
        lease_for: Duration,
    ) -> impl Future<Output = Result<Option<Lease>, StoreError>> + Send {
        async move {
            let actor = self.actor_for_write(actor, "claim as a person")?;
            let principal = if actor.human {
                self.human_principal()
            } else {
                None
            };
            let claimant = if actor.human {
                actor.principal
            } else {
                claimant
            };
            self.run_at(move |c, now| {
                Box::pin(async move {
                    let claimed = claim_id(c, &claimant, &id, now, lease_for, false).await?;
                    if let Some(lease) = &claimed {
                        record_claim_provenance(c, lease, principal.as_deref()).await?;
                    }
                    Ok(claimed)
                })
            })
            .await
        }
    }
    fn claim_next(
        &self,
        actor: Actor,
        claimant: String,
        lease_for: Duration,
    ) -> impl Future<Output = Result<Option<Lease>, StoreError>> + Send {
        async move {
            let actor = self.actor_for_write(actor, "claim as a person")?;
            let principal = if actor.human {
                self.human_principal()
            } else {
                None
            };
            let claimant = if actor.human {
                actor.principal
            } else {
                claimant
            };
            self.run_at(move |c, now| {
                Box::pin(async move {
                    checked_expiry(now, lease_for)?;
                    for i in ready_items(c, now, 10000, true).await? {
                        if let Some(l) = claim_id(c, &claimant, &i.id, now, lease_for, true).await?
                        {
                            record_claim_provenance(c, &l, principal.as_deref()).await?;
                            return Ok(Some(l));
                        }
                    }
                    Ok(None)
                })
            })
            .await
        }
    }
    fn heartbeat(
        &self,
        lease: Lease,
        lease_for: Duration,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async move {
            let principal = self.human_principal();
            self.run_at(move |c, now| {
                Box::pin(async move {
                    authorize_lease(c, &lease, principal.as_deref(), "heartbeat a human claim")
                        .await?;
                    check_lease(c, &lease).await?;
                    sqlx::query("UPDATE atomic_graph_leases SET expires_at=? WHERE item_id=?")
                        .bind(checked_expiry(now, lease_for)?)
                        .bind(&lease.item_id)
                        .execute(c)
                        .await
                        .map_err(db)?;
                    Ok(())
                })
            })
            .await
        }
    }
    fn finish(
        &self,
        lease: Lease,
        done: Finish,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async move {
            let principal = self.human_principal();
            self.run_at(move |c, now| Box::pin(async move {
            authorize_lease(c, &lease, principal.as_deref(), "finish a human claim").await?;
            let row = sqlx::query("SELECT status,claim_epoch,terminal_outcome,last_finish_session,last_finish_epoch,last_finish_claimant,last_finish_acquisition FROM atomic_graph_items WHERE id=?")
                .bind(&lease.item_id).fetch_optional(&mut *c).await.map_err(db)?;
            let Some(row) = row else { return Err(StoreError::MissingItem { item_id: lease.item_id }); };
            let status: String = row.try_get("status").map_err(db)?;
            let epoch: i64 = row.try_get("claim_epoch").map_err(db)?;
            if status == "closed" {
                let session: Option<i64> = row.try_get("last_finish_session").map_err(db)?;
                let finished_epoch: Option<i64> = row.try_get("last_finish_epoch").map_err(db)?;
                let finished_claimant: Option<String> = row.try_get("last_finish_claimant").map_err(db)?;
                let finished_acquisition: Option<String> = row.try_get("last_finish_acquisition").map_err(db)?;
                if done != Finish::Release && row.try_get::<Option<String>,_>("terminal_outcome").map_err(db)?.as_deref() == Some(if done == Finish::Close { "success" } else { "failure" }) && session == Some(lease.session as i64) && finished_epoch == Some(lease.epoch) && finished_claimant.as_deref() == Some(lease.claimant.as_str()) && finished_acquisition.as_deref() == Some(lease.acquisition.as_str()) { return Ok(()); }
                return Err(StoreError::Fenced { item_id: lease.item_id, epoch });
            }
            check_lease(c, &lease).await?;
            if done == Finish::Close {
                enforce_close_policy(c, &lease.item_id).await?;
            }
            sqlx::query("UPDATE atomic_graph_items SET status=?,closed_at=?,terminal_outcome=?,last_finish_session=?,last_finish_epoch=?,last_finish_claimant=?,last_finish_acquisition=? WHERE id=?")
                .bind(if done == Finish::Release { "open" } else { "closed" })
                .bind(if done == Finish::Release { None } else { Some(now) })
                .bind(match done { Finish::Close => Some("success"), Finish::Fail => Some("failure"), Finish::Release => None })
                .bind(if done == Finish::Release { None } else { Some(lease.session as i64) })
                .bind(if done == Finish::Release { None } else { Some(lease.epoch) })
                .bind(if done == Finish::Release { None } else { Some(lease.claimant.as_str()) })
                .bind(if done == Finish::Release { None } else { Some(lease.acquisition.as_str()) })
                .bind(&lease.item_id).execute(&mut *c).await.map_err(db)?;
            sqlx::query("DELETE FROM atomic_graph_leases WHERE item_id=?").bind(&lease.item_id).execute(c).await.map_err(db)?;
            Ok(())
        })).await
        }
    }
    fn sweep(&self, _actor: Actor) -> impl Future<Output = Result<Vec<String>, StoreError>> + Send {
        async move {
            self.run_at(move |c, now| Box::pin(async move {
            let rows = sqlx::query("SELECT item_id FROM atomic_graph_leases WHERE expires_at<=? ORDER BY item_id")
                .bind(now).fetch_all(&mut *c).await.map_err(db)?;
            let ids = rows.iter().map(|r| r.try_get("item_id").map_err(db)).collect::<Result<Vec<String>,_>>()?;
            for id in &ids {
                sqlx::query("UPDATE atomic_graph_items SET status='open',claim_epoch=claim_epoch+1 WHERE id=?")
                    .bind(id).execute(&mut *c).await.map_err(db)?;
                sqlx::query("DELETE FROM atomic_graph_leases WHERE item_id=?").bind(id).execute(&mut *c).await.map_err(db)?;
            }
            Ok(ids)
        })).await
        }
    }
    fn remember(
        &self,
        actor: Actor,
        request_id: String,
        key: String,
        value: String,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async move {
            let actor = self.actor_for_write(actor, "write a human memory")?;
            self.run(move |c| {
                Box::pin(async move {
                    let exists = request_seen(c, &request_id, "memory", &key).await?;
                    if !exists {
                        if !actor.human {
                            let existing =
                                sqlx::query("SELECT trust FROM atomic_graph_memories WHERE key=?")
                                    .bind(&key)
                                    .fetch_optional(&mut *c)
                                    .await
                                    .map_err(db)?;
                            if let Some(row) = existing {
                                let old: String = row.try_get("trust").map_err(db)?;
                                // CONTROL_OFF: graph-agent-memory-trust
                                let trusted_memory = old == "human" || old == "approved";
                                if trusted_memory {
                                    return Err(StoreError::NeedsHumanCredential {
                                        action: "replace trusted memory".to_owned(),
                                    });
                                }
                            }
                        }
                        sqlx::query("INSERT OR REPLACE INTO atomic_graph_memories VALUES (?,?,?)")
                            .bind(&key)
                            .bind(value)
                            .bind(trust(&actor).as_str())
                            .execute(&mut *c)
                            .await
                            .map_err(db)?;
                        sqlx::query("INSERT INTO atomic_graph_requests VALUES (?,'memory',?)")
                            .bind(request_id)
                            .bind(key)
                            .execute(c)
                            .await
                            .map_err(db)?;
                    }
                    Ok(())
                })
            })
            .await
        }
    }
    fn prompt_memories(
        &self,
        _actor: Actor,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<Memory>, StoreError>> + Send {
        async move {
            self.read(move |c|Box::pin(async move{let rows=sqlx::query("SELECT * FROM atomic_graph_memories WHERE trust IN ('human','approved') ORDER BY CASE trust WHEN 'human' THEN 0 ELSE 1 END,key LIMIT ?").bind(limit as i64).fetch_all(c).await.map_err(db)?;rows.iter().map(|r|Ok(Memory{key:r.try_get("key").map_err(db)?,value:r.try_get("value").map_err(db)?,trust:parse_trust(&r.try_get::<String,_>("trust").map_err(db)?)})).collect()})).await
        }
    }
    fn set_close_policy(
        &self,
        actor: Actor,
        requires: Vec<Evidence>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async move {
            let actor = self.actor_for_write(actor, "set close policy")?;
            if !actor.human {
                return Err(StoreError::NeedsHumanCredential {
                    action: "set close policy".to_owned(),
                });
            }
            if requires.contains(&Evidence::VerifiedCommit) {
                return Err(StoreError::InvalidClosePolicy {
                    detail: "the legacy tenant default cannot require verified_commit without an authorized branch; use a named item policy".into(),
                });
            }
            self.run(move |c| {
                Box::pin(async move {
                    sqlx::query("INSERT OR REPLACE INTO atomic_graph_policy VALUES (1,?,?)")
                        .bind(i64::from(requires.contains(&Evidence::Commit)))
                        .bind(i64::from(requires.contains(&Evidence::VerifiedCommit)))
                        .execute(c)
                        .await
                        .map_err(db)?;
                    Ok(())
                })
            })
            .await
        }
    }
    fn set_item_close_policy(
        &self,
        actor: Actor,
        item_id: String,
        requires: Vec<Evidence>,
        authorized_branch: Option<String>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async move {
            let actor = self.actor_for_write(actor, "set item close policy")?;
            if !actor.human {
                return Err(StoreError::NeedsHumanCredential {
                    action: "set item close policy".to_owned(),
                });
            }
            self.run_at(move |c, now| {
                Box::pin(async move {
                    if get_item(c, &item_id).await?.is_none() {
                        return Err(StoreError::MissingItem { item_id });
                    }
                    if is_gate_copy(c, &item_id).await? {
                        return Err(StoreError::InvalidClosePolicy {
                            detail: "gate copies are read-only".into(),
                        });
                    }
                    let verified = requires.contains(&Evidence::VerifiedCommit);
                    if verified != authorized_branch.is_some()
                        || authorized_branch
                            .as_deref()
                            .is_some_and(|branch| !valid_policy_branch(branch))
                    {
                        return Err(StoreError::InvalidClosePolicy {
                            detail: "verified_commit requires one valid full refs/heads/* branch; other policies name none".into(),
                        });
                    }
                    if requires.is_empty() {
                        sqlx::query("DELETE FROM atomic_graph_item_policy WHERE item_id=?")
                            .bind(item_id)
                            .execute(c)
                            .await
                            .map_err(db)?;
                        return Ok(());
                    }
                    sqlx::query("INSERT INTO atomic_graph_item_policy (item_id,requires_commit,requires_verified_commit,authorized_branch,set_by,set_at) VALUES (?,?,?,?,?,?) ON CONFLICT (item_id) DO UPDATE SET requires_commit=excluded.requires_commit,requires_verified_commit=excluded.requires_verified_commit,authorized_branch=excluded.authorized_branch,set_by=excluded.set_by,set_at=excluded.set_at")
                        .bind(item_id)
                        .bind(i64::from(requires.contains(&Evidence::Commit)))
                        .bind(i64::from(verified))
                        .bind(authorized_branch)
                        .bind(actor.principal)
                        .bind(now)
                        .execute(c)
                        .await
                        .map_err(db)?;
                    Ok(())
                })
            })
            .await
        }
    }
    fn item_close_policy(
        &self,
        item_id: &str,
    ) -> impl Future<Output = Result<Option<ItemClosePolicy>, StoreError>> + Send {
        let item_id = item_id.to_owned();
        async move {
            self.read(move |c| {
                Box::pin(async move {
                    if get_item(c, &item_id).await?.is_none() {
                        return Err(StoreError::MissingItem { item_id });
                    }
                    let item = sqlx::query("SELECT requires_commit,requires_verified_commit,authorized_branch,set_by FROM atomic_graph_item_policy WHERE item_id=?")
                        .bind(&item_id)
                        .fetch_optional(&mut *c)
                        .await
                        .map_err(db)?;
                    let (row, source) = match item {
                        Some(row) => (row, ClosePolicySource::Item),
                        None => {
                            let Some(row) = sqlx::query("SELECT requires_commit,requires_verified_commit FROM atomic_graph_policy WHERE id=1")
                                .fetch_optional(&mut *c)
                                .await
                                .map_err(db)? else { return Ok(None) };
                            (row, ClosePolicySource::LegacyGlobal)
                        }
                    };
                    let mut requires = Vec::new();
                    if row.try_get::<i64, _>("requires_commit").map_err(db)? != 0 {
                        requires.push(Evidence::Commit);
                    }
                    if row.try_get::<i64, _>("requires_verified_commit").map_err(db)? != 0 {
                        requires.push(Evidence::VerifiedCommit);
                    }
                    let (authorized_branch, set_by) = match source {
                        ClosePolicySource::Item => (
                            row.try_get("authorized_branch").map_err(db)?,
                            Some(row.try_get("set_by").map_err(db)?),
                        ),
                        ClosePolicySource::LegacyGlobal => (None, None),
                    };
                    Ok(Some(ItemClosePolicy {
                        requires,
                        authorized_branch,
                        source,
                        set_by,
                    }))
                })
            })
            .await
        }
    }
    fn set_item_criteria(
        &self,
        actor: Actor,
        item_id: String,
        criteria: Vec<Criterion>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async move {
            let actor = self.actor_for_write(actor, "set item criteria")?;
            let missing_human = !actor.human; // graph-item-criteria-human
            if missing_human {
                // CONTROL_OFF: graph-item-criteria-human
                return Err(StoreError::NeedsHumanCredential {
                    action: "set item criteria".to_owned(),
                });
            }
            let value = serde_json::to_value(&criteria).map_err(db)?;
            let validated = parse_criteria(&value) /* graph-item-criteria-schema */
                .map_err(|detail| StoreError::InvalidCriteria { detail })?;
            let encoded = serde_json::to_string(&validated).map_err(db)?;
            self.run_at(move |c, now| {
                Box::pin(async move {
                    if get_item(c, &item_id).await?.is_none() {
                        return Err(StoreError::MissingItem { item_id });
                    }
                    if is_gate_copy(c, &item_id).await? {
                        return Err(StoreError::InvalidCriteria {
                            detail: "gate copies are read-only".into(),
                        });
                    }
                    if validated.is_empty() {
                        sqlx::query("DELETE FROM atomic_graph_item_criteria WHERE item_id=?")
                            .bind(item_id)
                            .execute(c)
                            .await
                            .map_err(db)?;
                    } else {
                        sqlx::query("INSERT INTO atomic_graph_item_criteria (item_id,criteria_json,set_by,set_at) VALUES (?,?,?,?) ON CONFLICT(item_id) DO UPDATE SET criteria_json=excluded.criteria_json,set_by=excluded.set_by,set_at=excluded.set_at")
                            .bind(item_id)
                            .bind(encoded)
                            .bind(actor.principal)
                            .bind(now)
                            .execute(c)
                            .await
                            .map_err(db)?;
                    }
                    Ok(())
                })
            })
            .await
        }
    }
    fn item_criteria(
        &self,
        item_id: &str,
    ) -> impl Future<Output = Result<Option<ItemCriteria>, StoreError>> + Send {
        let item_id = item_id.to_owned();
        async move {
            self.read(move |c| {
                Box::pin(async move {
                    if get_item(c, &item_id).await?.is_none() {
                        return Err(StoreError::MissingItem { item_id });
                    }
                    let row = sqlx::query("SELECT criteria_json,set_by FROM atomic_graph_item_criteria WHERE item_id=?")
                        .bind(item_id)
                        .fetch_optional(&mut *c)
                        .await
                        .map_err(db)?;
                    let Some(row) = row else { return Ok(None) };
                    let raw: String = row.try_get("criteria_json").map_err(db)?;
                    let value: serde_json::Value = serde_json::from_str(&raw).map_err(db)?;
                    let criteria = parse_criteria(&value).map_err(db)?;
                    Ok(Some(ItemCriteria {
                        criteria,
                        set_by: row.try_get("set_by").map_err(db)?,
                    }))
                })
            })
            .await
        }
    }
    fn link_commit(
        &self,
        lease: Lease,
        sha: String,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        async move {
            let principal = self.human_principal();
            self.run(move |c| {
                Box::pin(async move {
                    authorize_lease(
                        c,
                        &lease,
                        principal.as_deref(),
                        "link a human claim's commit",
                    )
                    .await?;
                    check_lease(c, &lease).await?;
                    sqlx::query(
                        "INSERT INTO atomic_graph_commit_links (item_id,sha,indexed,on_branch,verified_branch,verified_head,pinned_base) VALUES (?,?,0,0,NULL,NULL,NULL) ON CONFLICT(item_id,sha) DO NOTHING",
                    )
                    .bind(&lease.item_id)
                    .bind(sha)
                    .execute(c)
                    .await
                    .map_err(db)?;
                    Ok(())
                })
            })
            .await
        }
    }
    fn import(
        &self,
        _actor: Actor,
        jsonl: String,
        dry_run: bool,
    ) -> impl Future<Output = Result<Report, StoreError>> + Send {
        async move {
            if !dry_run && self.human.is_none() {
                return Err(StoreError::NeedsHumanCredential {
                    action: "import graph".to_owned(),
                });
            }
            if dry_run {
                self.read_at(move |c, now| {
                    Box::pin(async move { import_jsonl(c, &jsonl, true, now).await })
                })
                .await
            } else {
                self.run_at(move |c, now| {
                    Box::pin(async move { import_jsonl(c, &jsonl, false, now).await })
                })
                .await
            }
        }
    }
    fn export(&self, _actor: Actor) -> impl Future<Output = Result<String, StoreError>> + Send {
        async move {
            self.read(move |c| Box::pin(async move { export_jsonl(c).await }))
                .await
        }
    }
}
async fn claim_id(
    c: &mut SqliteConnection,
    claimant: &str,
    id: &str,
    now: i64,
    lease_for: Duration,
    sweep_expired: bool,
) -> Result<Option<Lease>, StoreError> {
    let expires_at = checked_expiry(now, lease_for)?;
    let Some(i) = get_item(c, id).await? else {
        return Ok(None);
    };
    let current = lease_row(c, id).await?;
    if let Some((l, expiry)) = &current {
        if l.claimant == claimant && i.status == Status::InProgress {
            sqlx::query("UPDATE atomic_graph_leases SET expires_at=? WHERE item_id=?")
                .bind(expires_at)
                .bind(id)
                .execute(&mut *c)
                .await
                .map_err(db)?;
            return Ok(Some(l.clone()));
        }
        if *expiry > now {
            return Ok(None);
        }
    }
    if i.status != Status::Open && i.status != Status::InProgress {
        return Ok(None);
    }
    if i.kind == "goal"
        || i.kind == "message"
        || is_gate_copy(c, id).await?
        || i.due_at.is_some_and(|due| millis(due) > now)
        || sqlx::query_scalar::<_, i64>(CLAIM_HAS_BLOCKER_SQL)
            .bind(id)
            .fetch_one(&mut *c)
            .await
            .map_err(db)?
            != 0
    {
        return Ok(None);
    }
    let previous: i64 = sqlx::query_scalar("SELECT claim_epoch FROM atomic_graph_items WHERE id=?")
        .bind(id)
        .fetch_one(&mut *c)
        .await
        .map_err(db)?;
    let epoch = previous
        + if current.is_some() && sweep_expired {
            2
        } else {
            1
        };
    let session = epoch as u64;
    let acquisition = format!("{id}:{epoch}:{claimant}");
    sqlx::query("INSERT OR REPLACE INTO atomic_graph_leases VALUES (?,?,?,?,?,?)")
        .bind(id)
        .bind(claimant)
        .bind(session as i64)
        .bind(epoch)
        .bind(&acquisition)
        .bind(expires_at)
        .execute(&mut *c)
        .await
        .map_err(db)?;
    sqlx::query("UPDATE atomic_graph_items SET status='in_progress', claim_epoch=? WHERE id=?")
        .bind(epoch)
        .bind(id)
        .execute(c)
        .await
        .map_err(db)?;
    Ok(Some(Lease {
        item_id: id.to_owned(),
        claimant: claimant.to_owned(),
        session,
        epoch,
        acquisition,
    }))
}
fn import_date(value: &serde_json::Value, key: &str) -> Result<Option<i64>, StoreError> {
    let Some(raw) = value.get(key) else {
        return Ok(None);
    };
    let Some(text) = raw.as_str() else {
        return Err(StoreError::ImportRefused(vec![format!(
            "{key} must be an RFC3339 string"
        )]));
    };
    let parsed = chrono::DateTime::parse_from_rfc3339(text)
        .map_err(|_| StoreError::ImportRefused(vec![format!("invalid {key}: {text}")]))?;
    Ok(Some(parsed.timestamp_millis()))
}
fn date_string(value: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(value)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}
fn native_pinned_marker(value: &serde_json::Value) -> bool {
    let marker = &value["metadata"]["atomic_graph"];
    marker["schema_version"] == 1 && marker["native_status"] == "pinned"
}
fn scrub_import_text(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) if text.contains("ghp_") => *text = "[redacted]".to_owned(),
        serde_json::Value::Array(values) => values.iter_mut().for_each(scrub_import_text),
        serde_json::Value::Object(fields) => fields.values_mut().for_each(scrub_import_text),
        _ => {}
    }
}
async fn import_jsonl(
    c: &mut SqliteConnection,
    jsonl: &str,
    dry_run: bool,
    now: i64,
) -> Result<Report, StoreError> {
    let mut lines: Vec<(serde_json::Value, String)> = Vec::new();
    let mut memories: Vec<(String, String)> = Vec::new();
    let mut errors = Vec::new();
    for (n, raw) in jsonl.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(mut v) => {
                // Keep the historical short ghp_ marker as well as the shared
                // redactor's provider, field-name and structured-value rules.
                scrub_import_text(&mut v);
                atomic_core::redact::value(&mut v);
                if let Some(record_type) = v.get("_type") {
                    if record_type != "memory" && record_type != "issue" {
                        errors.push(format!("line {} has unsupported _type", n + 1));
                        continue;
                    }
                    if record_type == "memory" {
                        let (Some(key), Some(value)) = (json_str(&v, "key"), json_str(&v, "value"))
                        else {
                            errors
                                .push(format!("line {} memory needs string key and value", n + 1));
                            continue;
                        };
                        memories.push((key.to_owned(), value.to_owned()));
                        continue;
                    }
                }
                if json_str(&v, "id").is_none() || json_str(&v, "title").is_none() {
                    errors.push(format!("line {} needs id and title", n + 1));
                }
                if json_str(&v, "id").is_some_and(|id| id.starts_with("gate:"))
                    || json_str(&v, "issue_type") == Some("gate")
                {
                    errors.push(format!("line {} uses the reserved gate projection", n + 1));
                }
                let status = json_str(&v, "status").unwrap_or("");
                if status == "pinned" || Status::parse(status).is_none() {
                    errors.push(format!("line {} has unsupported status {status}", n + 1));
                }
                if let Some(outcome) = v.pointer("/metadata/atomic_graph/terminal_outcome")
                    && (v.pointer("/metadata/atomic_graph/schema_version")
                        != Some(&serde_json::json!(1))
                        || status != "closed"
                        || !matches!(outcome.as_str(), Some("success" | "failure")))
                {
                    errors.push(format!(
                        "line {} has invalid native terminal outcome",
                        n + 1
                    ));
                }
                if let Some(pinned) = v.get("pinned") {
                    match pinned.as_bool() {
                        Some(true) if !native_pinned_marker(&v) => {
                            errors.push(format!("line {} has unsupported pinned flag", n + 1))
                        }
                        Some(true) if status != "open" => errors.push(format!(
                            "line {} native pinned record must have open status",
                            n + 1
                        )),
                        Some(false) if native_pinned_marker(&v) => errors.push(format!(
                            "line {} native pinned marker requires pinned:true",
                            n + 1
                        )),
                        None => errors.push(format!("line {} pinned must be a boolean", n + 1)),
                        _ => {}
                    }
                } else if native_pinned_marker(&v) {
                    errors.push(format!(
                        "line {} native pinned record needs pinned:true",
                        n + 1
                    ));
                }
                if let Some(marker) = v.pointer("/metadata/atomic_graph/native_parent") {
                    let parent = marker.as_str();
                    let explicit = v
                        .get("dependencies")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|dependencies| {
                            dependencies.iter().any(|dependency| {
                                json_str(dependency, "type") == Some("parent-child")
                                    && json_str(dependency, "depends_on_id") == parent
                            })
                        });
                    if v.pointer("/metadata/atomic_graph/schema_version")
                        != Some(&serde_json::json!(1))
                        || parent.is_none()
                        || json_str(&v, "parent_issue") != parent
                        || !explicit
                    {
                        errors.push(format!("line {} has invalid native parent marker", n + 1));
                    }
                }
                for key in [
                    "description",
                    "issue_type",
                    "parent_issue",
                    "notes",
                    "external_ref",
                    "acceptance_criteria",
                ] {
                    if v.get(key).is_some_and(|field| !field.is_string()) {
                        errors.push(format!("line {} {key} must be a string", n + 1));
                    }
                }
                if v.get("priority")
                    .is_some_and(|field| field.as_i64().is_none_or(|p| i32::try_from(p).is_err()))
                {
                    errors.push(format!("line {} priority must be a 32-bit integer", n + 1));
                }
                if v.get("labels").is_some_and(|field| {
                    field
                        .as_array()
                        .is_none_or(|labels| labels.iter().any(|label| !label.is_string()))
                }) {
                    errors.push(format!("line {} labels must be strings", n + 1));
                }
                if v.get("metadata").is_some_and(|field| !field.is_object()) {
                    errors.push(format!("line {} metadata must be an object", n + 1));
                }
                if v.get("dependencies").is_some_and(|field| !field.is_array()) {
                    errors.push(format!("line {} dependencies must be an array", n + 1));
                }
                if v.get("comments").is_some_and(|field| !field.is_array()) {
                    errors.push(format!("line {} comments must be an array", n + 1));
                }
                if let Some(comments) = v.get("comments").and_then(serde_json::Value::as_array) {
                    for comment in comments {
                        if json_str(comment, "author").is_none()
                            || json_str(comment, "text").is_none()
                        {
                            errors.push(format!(
                                "line {} comment needs string author and text",
                                n + 1
                            ));
                        }
                    }
                }
                for key in ["created_at", "closed_at", "due_at"] {
                    if let Err(StoreError::ImportRefused(problems)) = import_date(&v, key) {
                        errors.extend(
                            problems
                                .into_iter()
                                .map(|problem| format!("line {}: {problem}", n + 1)),
                        );
                    }
                }
                let normalized = serde_json::to_string(&v).map_err(db)?;
                lines.push((v, normalized))
            }
            Err(e) => errors.push(format!("line {}: {e}", n + 1)),
        }
    }
    let ids: std::collections::BTreeSet<String> = lines
        .iter()
        .filter_map(|(v, _)| json_str(v, "id").map(str::to_owned))
        .collect();
    if ids.len() != lines.len() {
        errors.push("duplicate id in import".to_owned())
    }
    for (v, _) in &lines {
        let id = json_str(v, "id").unwrap_or("<missing>");
        let mut parents = std::collections::BTreeSet::new();
        if let Some(parent) = json_str(v, "parent_issue") {
            parents.insert(parent);
            if is_gate_copy(c, parent).await? {
                errors.push(format!("{id} parent {parent} is a read-only gate copy"));
            }
            if !ids.contains(parent) && get_item(c, parent).await?.is_none() {
                errors.push(format!("{id} parent {parent} is missing"))
            }
        }
        if let Some(deps) = v.get("dependencies").and_then(serde_json::Value::as_array) {
            for d in deps {
                let target = json_str(d, "depends_on_id").unwrap_or("");
                let kind = json_str(d, "type").unwrap_or("");
                if json_str(d, "issue_id") != Some(id) || target.is_empty() {
                    errors.push(format!(
                        "{id} dependency needs matching issue_id and depends_on_id"
                    ));
                }
                if parse_edge(kind).is_none() {
                    errors.push(format!("{id} dependency has unsupported type {kind}"))
                }
                if kind == "parent-child" {
                    parents.insert(target);
                }
                if is_gate_copy(c, target).await? {
                    errors.push(format!("{id} dependency {target} is a read-only gate copy"));
                }
                if !ids.contains(target) && get_item(c, target).await?.is_none() {
                    errors.push(format!("{id} dependency {target} is missing"))
                }
            }
        }
        if parents.len() > 1 {
            errors.push(format!("{id} names more than one parent"));
        }
    }
    if !errors.is_empty() {
        return Err(StoreError::ImportRefused(errors));
    }
    let mut report = Report {
        items: lines.len(),
        ..Report::default()
    };
    for (v, raw) in lines {
        let id = json_str(&v, "id").unwrap_or_default().to_owned();
        let old = sqlx::query("SELECT imported_json FROM atomic_graph_items WHERE id=?")
            .bind(&id)
            .fetch_optional(&mut *c)
            .await
            .map_err(db)?;
        if let Some(row) = old {
            let imported: Option<String> = row.try_get("imported_json").map_err(db)?;
            match imported {
                None => report.left_as_is.push((id, LeftAlone::MadeInAtomic)),
                Some(previous) if previous == raw => report.unchanged.push(id),
                Some(_) => report
                    .left_as_is
                    .push((id, LeftAlone::ImportedBeforeFromDifferentText)),
            }
            continue;
        }
        let status = if v.get("pinned").and_then(serde_json::Value::as_bool) == Some(true)
            && native_pinned_marker(&v)
        {
            "pinned"
        } else {
            json_str(&v, "status").unwrap_or("open")
        };
        let created = import_date(&v, "created_at")?.unwrap_or(now);
        let closed = if status == "closed" {
            Some(import_date(&v, "closed_at")?.unwrap_or(created))
        } else {
            None
        };
        let parent = json_str(&v, "parent_issue").or_else(|| {
            v.get("dependencies")
                .and_then(serde_json::Value::as_array)
                .and_then(|deps| {
                    deps.iter()
                        .find(|dep| json_str(dep, "type") == Some("parent-child"))
                })
                .and_then(|dep| json_str(dep, "depends_on_id"))
        });
        let due = import_date(&v, "due_at")?;
        if !dry_run {
            let terminal_outcome = v
                .pointer("/metadata/atomic_graph/terminal_outcome")
                .and_then(serde_json::Value::as_str);
            sqlx::query("INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at,closed_at,due_at,imported_json,terminal_outcome) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
                .bind(&id)
                .bind(json_str(&v, "title").unwrap_or(""))
                .bind(json_str(&v, "description").unwrap_or(""))
                .bind(json_str(&v, "issue_type").unwrap_or("task"))
                .bind(v.get("priority").and_then(serde_json::Value::as_i64).unwrap_or(2))
                .bind(status)
                .bind(parent)
                .bind("external")
                .bind("bd")
                .bind(created)
                .bind(closed)
                .bind(due)
                .bind(&raw)
                .bind(terminal_outcome)
                .execute(&mut *c)
                .await.map_err(db)?;
        }
        report.added.push(id.clone());
        report.external += 1;
        if status == "in_progress" {
            report.in_progress += 1
        }
        if let Some(p) = parent {
            if !dry_run {
                sqlx::query("INSERT OR IGNORE INTO atomic_graph_edges VALUES (?,'parent-child',?)")
                    .bind(&id)
                    .bind(p)
                    .execute(&mut *c)
                    .await
                    .map_err(db)?;
            }
            report.parents_set += 1
        }
        if let Some(deps) = v.get("dependencies").and_then(serde_json::Value::as_array) {
            let mut has_relation = false;
            for dep in deps {
                let target = json_str(dep, "depends_on_id").unwrap_or("");
                let kind = json_str(dep, "type").unwrap_or("");
                if !dry_run {
                    sqlx::query("INSERT OR IGNORE INTO atomic_graph_edges VALUES (?,?,?)")
                        .bind(&id)
                        .bind(kind)
                        .bind(target)
                        .execute(&mut *c)
                        .await
                        .map_err(db)?;
                }
                if kind == "blocks" {
                    report.dependencies_added += 1
                } else if kind != "parent-child" {
                    has_relation = true
                }
            }
            if has_relation {
                *report.kept.entry("relations".to_owned()).or_default() += 1
            }
        }
        let mut notes: Vec<(String, String)> = Vec::new();
        if let Some(text) = json_str(&v, "notes")
            && !text.trim().is_empty()
        {
            notes.push(("bd".to_owned(), text.to_owned()))
        }
        if let Some(comments) = v.get("comments").and_then(serde_json::Value::as_array) {
            for comment in comments {
                if let Some(text) = json_str(comment, "text") {
                    notes.push((
                        format!("bd:{}", json_str(comment, "author").unwrap_or("unknown")),
                        text.to_owned(),
                    ))
                }
            }
        }
        for (n, (author, text)) in notes.into_iter().enumerate() {
            if !dry_run {
                sqlx::query("INSERT INTO atomic_graph_notes VALUES (?,?,?,?, 'external')")
                    .bind(&id)
                    .bind(n as i64)
                    .bind(text)
                    .bind(author)
                    .execute(&mut *c)
                    .await
                    .map_err(db)?;
            }
            report.notes_added += 1
        }
        for key in ["labels", "external_ref", "acceptance_criteria"] {
            if v.get(key).is_some() {
                *report.kept.entry(key.to_owned()).or_default() += 1
            }
        }
    }
    let mut preview_keys = std::collections::BTreeSet::new();
    for (key, value) in memories {
        if dry_run {
            if preview_keys.insert(key.clone())
                && sqlx::query("SELECT 1 FROM atomic_graph_memories WHERE key=?")
                    .bind(&key)
                    .fetch_optional(&mut *c)
                    .await
                    .map_err(db)?
                    .is_none()
            {
                *report.kept.entry("memories".to_owned()).or_default() += 1;
            }
            continue;
        }
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO atomic_graph_memories (key,value,trust) VALUES (?,?,'external')",
        )
        .bind(&key)
        .bind(value)
        .execute(&mut *c)
        .await
        .map_err(db)?;
        if inserted.rows_affected() != 0 {
            *report.kept.entry("memories".to_owned()).or_default() += 1;
        }
    }
    if dry_run {
        return Err(StoreError::ImportDryRun(Box::new(report)));
    }
    Ok(report)
}
async fn export_jsonl(c: &mut SqliteConnection) -> Result<String, StoreError> {
    let rows =
        sqlx::query("SELECT * FROM atomic_graph_items WHERE projected_gate_id IS NULL ORDER BY id")
            .fetch_all(&mut *c)
            .await
            .map_err(db)?;
    let mut out = String::new();
    for row in rows {
        let i = item(&row)?;
        let original: Option<String> = row.try_get("imported_json").map_err(db)?;
        let imported = original.is_some();
        let mut value = match original {
            Some(raw) => serde_json::from_str::<serde_json::Value>(&raw).map_err(db)?,
            None => serde_json::json!({}),
        };
        value["id"] = i.id.clone().into();
        value["title"] = i.title.clone().into();
        value["description"] = i.body.clone().into();
        value["issue_type"] = i.kind.clone().into();
        value["priority"] = i.priority.into();
        if i.status == Status::Pinned {
            value["status"] = "open".into();
            value["pinned"] = true.into();
            if value.get("metadata").is_none() {
                value["metadata"] = serde_json::json!({});
            }
            value["metadata"]["atomic_graph"] =
                serde_json::json!({"schema_version":1,"native_status":"pinned"});
        } else {
            value["status"] = i.status.as_str().into();
            if native_pinned_marker(&value) {
                if let Some(obj) = value.as_object_mut() {
                    obj.remove("pinned");
                }
                if let Some(meta) = value
                    .get_mut("metadata")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    meta.remove("atomic_graph");
                }
            }
        }
        let outcome: Option<String> = row.try_get("terminal_outcome").map_err(db)?;
        if let Some(outcome) = outcome {
            if !value["metadata"]["atomic_graph"].is_object() {
                value["metadata"]["atomic_graph"] = serde_json::json!({});
            }
            value["metadata"]["atomic_graph"]["schema_version"] = 1.into();
            value["metadata"]["atomic_graph"]["terminal_outcome"] = outcome.into();
        } else if let Some(marker) = value
            .pointer_mut("/metadata/atomic_graph")
            .and_then(serde_json::Value::as_object_mut)
        {
            marker.remove("terminal_outcome");
        }
        if !imported || value.get("created_at").is_none() {
            value["created_at"] = date_string(i.created_at).into();
        }
        if let Some(closed) = i.closed_at {
            if import_date(&value, "closed_at").ok().flatten() != Some(millis(closed)) {
                value["closed_at"] = date_string(closed).into();
            }
        } else if let Some(obj) = value.as_object_mut() {
            obj.remove("closed_at");
        }
        if let Some(parent) = &i.parent {
            value["parent_issue"] = parent.clone().into();
            if !imported {
                if !value["metadata"]["atomic_graph"].is_object() {
                    value["metadata"]["atomic_graph"] = serde_json::json!({});
                }
                value["metadata"]["atomic_graph"]["schema_version"] = 1.into();
                value["metadata"]["atomic_graph"]["native_parent"] = parent.clone().into();
            }
        }
        if let Some(due) = i.due_at
            && (!imported || value.get("due_at").is_none())
        {
            value["due_at"] = date_string(due).into();
        }
        let deps = sqlx::query(
            "SELECT kind,target FROM atomic_graph_edges WHERE source=? ORDER BY kind,target",
        )
        .bind(&i.id)
        .fetch_all(&mut *c)
        .await
        .map_err(db)?;
        let mut old_deps = value
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        for edge in deps {
            let kind: String = edge.try_get("kind").map_err(db)?;
            let target: String = edge.try_get("target").map_err(db)?;
            if imported
                && kind == "parent-child"
                && i.parent.as_deref() == Some(&target)
                && !old_deps.iter().any(|old| {
                    json_str(old, "type") == Some("parent-child")
                        && json_str(old, "depends_on_id") == Some(target.as_str())
                })
            {
                continue;
            }
            if !old_deps.iter().any(|old| {
                json_str(old, "type") == Some(kind.as_str())
                    && json_str(old, "depends_on_id") == Some(target.as_str())
            }) {
                old_deps
                    .push(serde_json::json!({"issue_id":i.id,"depends_on_id":target,"type":kind}));
            }
        }
        if !old_deps.is_empty() {
            value["dependencies"] = old_deps.into();
        }
        let notes = sqlx::query(
            "SELECT text,author FROM atomic_graph_notes WHERE item_id=? ORDER BY ordinal",
        )
        .bind(&i.id)
        .fetch_all(&mut *c)
        .await
        .map_err(db)?;
        let original_note = usize::from(
            imported && json_str(&value, "notes").is_some_and(|text| !text.trim().is_empty()),
        );
        let mut comments = value
            .get("comments")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let already = original_note + comments.len();
        for note in notes.iter().skip(already) {
            comments.push(serde_json::json!({
                "author":note.try_get::<String,_>("author").map_err(db)?,
                "text":note.try_get::<String,_>("text").map_err(db)?
            }));
        }
        if !comments.is_empty() {
            value["comments"] = comments.into();
        }
        out.push_str(&serde_json::to_string(&value).map_err(db)?);
        out.push('\n');
    }
    let memories = sqlx::query("SELECT key,value FROM atomic_graph_memories ORDER BY key")
        .fetch_all(&mut *c)
        .await
        .map_err(db)?;
    for memory in memories {
        let key: String = memory.try_get("key").map_err(db)?;
        let value: String = memory.try_get("value").map_err(db)?;
        out.push_str(
            &serde_json::to_string(&serde_json::json!({"_type":"memory","key":key,"value":value}))
                .map_err(db)?,
        );
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn search_ranks_titles_and_tracks_edits_deletes_rebuild_and_exact_ids() {
        let path =
            std::env::temp_dir().join(format!("atomic-graph-search-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = SqliteStore::open(&path, SystemTime::now).expect("graph store");
        store.run(|conn| Box::pin(async move {
            for (id, title, body, status) in [
                ("at-search.1", "violet sample", "plain body", "open"),
                ("at-search.2", "unrelated", "violet sample in the body", "closed"),
                ("at-search.3", "other", "no match", "open"),
            ] {
                sqlx::query("INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at) VALUES (?,?,?,'task',2,?,NULL,'human','test',1)")
                    .bind(id).bind(title).bind(body).bind(status)
                    .execute(&mut *conn).await.map_err(db)?;
            }
            sqlx::query("INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at,projected_gate_id) VALUES ('gate:search','violet sample gate','violet sample private gate','gate',0,'blocked',NULL,'agent','gate-store',1,'run:search')")
                .execute(&mut *conn).await.map_err(db)?;
            Ok(())
        })).await.expect("seed items");
        let hits = store
            .search_items("violet sample", None, 10)
            .await
            .expect("search");
        assert_eq!(
            hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
            ["at-search.1", "at-search.2"]
        );
        assert!(!hits[1].excerpt.contains('<'), "excerpt is plain text");
        assert_eq!(
            store
                .search_items("at-search.2", None, 10)
                .await
                .expect("exact id")[0]
                .id,
            "at-search.2"
        );
        assert_eq!(
            store
                .search_items("at-search.2", Some("open"), 10)
                .await
                .expect("filtered id")
                .len(),
            0
        );
        assert!(
            store
                .search_items("gate:search", None, 10)
                .await
                .expect("private gate ID")
                .is_empty()
        );
        assert_eq!(
            store
                .search_items("violet sample", Some("closed"), 10)
                .await
                .expect("filtered")
                .len(),
            1
        );

        store.run(|conn| Box::pin(async move {
            sqlx::query("UPDATE atomic_graph_items SET title='moved',body='changed' WHERE id='at-search.1'")
                .execute(&mut *conn).await.map_err(db)?;
            sqlx::query("DELETE FROM atomic_graph_items WHERE id='at-search.2'")
                .execute(&mut *conn).await.map_err(db)?;
            Ok(())
        })).await.expect("edit and delete");
        assert!(
            store
                .search_items("violet sample", None, 10)
                .await
                .expect("stale search")
                .is_empty()
        );
        assert_eq!(
            store
                .search_items("changed", None, 10)
                .await
                .expect("fresh search")[0]
                .id,
            "at-search.1"
        );
        store
            .rebuild_search_index(Actor::agent("test"))
            .await
            .expect("rebuild");
        assert_eq!(
            store
                .search_items("changed", None, 10)
                .await
                .expect("rebuilt search")
                .len(),
            1
        );
        assert!(
            store
                .search_items("violet sample", None, 10)
                .await
                .expect("rebuilt stale search")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn gate_copies_are_visible_but_never_work_or_exported_items() {
        let path = std::env::temp_dir().join(format!(
            "atomic-graph-gate-copy-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let store = SqliteStore::open(&path, SystemTime::now).expect("graph store");
        store
            .run(|conn| {
                Box::pin(async move {
                    sqlx::query("INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at,projected_gate_id) VALUES ('gate:run:ship','Ship','open human gate','gate',0,'blocked',NULL,'agent','gate-store',1,'run:ship')")
                        .execute(conn)
                        .await
                        .map_err(db)?;
                    Ok(())
                })
            })
            .await
            .expect("project gate");

        let actor = Actor::agent("test");
        let shown = store
            .show(actor.clone(), "gate:run:ship".into())
            .await
            .expect("show")
            .expect("gate copy");
        assert_eq!(shown.item.kind, "gate");
        assert_eq!(shown.item.status, Status::Blocked);
        assert!(
            store
                .ready(actor.clone(), 10)
                .await
                .expect("ready")
                .is_empty()
        );
        assert!(
            store
                .claim(
                    actor.clone(),
                    "test".into(),
                    "gate:run:ship".into(),
                    Duration::from_secs(30)
                )
                .await
                .expect("claim")
                .is_none()
        );
        assert_eq!(store.export(actor.clone()).await.expect("export"), "");

        for mut draft in [
            Draft::new("gate:fake", "fake"),
            Draft::new("at-fake", "fake"),
        ] {
            if draft.id == "at-fake" {
                draft.kind = "gate".into();
            }
            assert!(matches!(
                store
                    .create_item(actor.clone(), draft.id.clone(), draft)
                    .await,
                Err(StoreError::InvalidStatusTransition { .. })
            ));
        }
    }

    #[test]
    fn readiness_and_claim_use_the_blocker_source_index_without_a_recursive_closure() {
        let path = std::env::temp_dir().join(format!(
            "atomic-graph-blocker-plan-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let store = SqliteStore::open(&path, SystemTime::now).expect("graph store");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let mut conn = store.conn.lock().expect("graph connection");
        let ready_sql = format!("EXPLAIN QUERY PLAN {READY_ITEMS_SQL}");
        let claim_sql = format!("EXPLAIN QUERY PLAN {CLAIM_HAS_BLOCKER_SQL}");
        let plans = runtime.block_on(async {
            // The EXPLAIN prefix and queried SQL are both fixed source text.
            let ready = sqlx::query(sqlx::AssertSqlSafe(ready_sql))
                .bind(0_i64)
                .bind(0_i64)
                .bind(0_i64)
                .bind(10_i64)
                .fetch_all(&mut *conn)
                .await
                .expect("ready query plan");
            let claim = sqlx::query(sqlx::AssertSqlSafe(claim_sql))
                .bind("at-item")
                .fetch_all(&mut *conn)
                .await
                .expect("claim blocker query plan");
            [ready, claim].map(|rows| {
                rows.iter()
                    .map(|row| row.try_get::<String, _>("detail").expect("plan detail"))
                    .collect::<Vec<_>>()
            })
        });
        for (plan, table) in plans.iter().zip(["b", "atomic_graph_blocked"]) {
            assert!(
                plan.iter()
                    .any(|detail| detail.contains(&format!("SEARCH {table} USING"))
                        && detail.contains("(source=?)")),
                "blocker source lookup did not use its index: {plan:?}"
            );
            assert!(
                !plan
                    .iter()
                    .any(|detail| detail.contains("CO-ROUTINE") || detail.contains("MATERIALIZE")),
                "blocker lookup materialized a recursive result: {plan:?}"
            );
        }
        drop(conn);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sqlite_commit_failure_rolls_back_and_next_operation_runs() {
        let path = std::env::temp_dir().join(format!(
            "atomic-graph-commit-failure-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let store = SqliteStore::open(&path, SystemTime::now).expect("test operation");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test operation");
        {
            let mut conn = store.conn.lock().expect("test operation");
            runtime.block_on(async {
                sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await.expect("test operation");
                sqlx::query("CREATE TABLE graph_commit_parent (id INTEGER PRIMARY KEY)")
                    .execute(&mut *conn).await.expect("test operation");
                sqlx::query("CREATE TABLE graph_commit_child (parent_id INTEGER REFERENCES graph_commit_parent(id) DEFERRABLE INITIALLY DEFERRED)")
                    .execute(&mut *conn).await.expect("test operation");
            });
        }
        let result = runtime.block_on(store.run(move |conn| {
            Box::pin(async move {
                sqlx::query("INSERT INTO graph_commit_child VALUES (99)")
                    .execute(conn)
                    .await
                    .map_err(db)?;
                Ok(())
            })
        }));
        assert!(matches!(
            result,
            Err(StoreError::OutcomeUnknown {
                operation: "commit"
            })
        ));
        let count: i64 = runtime
            .block_on(store.run(move |conn| {
                Box::pin(async move {
                    sqlx::query_scalar("SELECT COUNT(*) FROM graph_commit_child")
                        .fetch_one(conn)
                        .await
                        .map_err(db)
                })
            }))
            .expect("test operation");
        assert_eq!(count, 0, "the failed transaction left a child row behind");
        let created = runtime
            .block_on(store.create_item(
                Actor::agent("recovery"),
                "after-failure".into(),
                Draft::new("at-recovered", "Recovered"),
            ))
            .expect("test operation");
        assert_eq!(created.id, "at-recovered");
        drop(store);
        let _ = std::fs::remove_file(path);
    }
}
