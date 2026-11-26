// SPDX-License-Identifier: Apache-2.0

//! Apply host-validated model proposals inside the accepted candidate's
//! durable publication transaction. No model-supplied SQL or trust label enters
//! this path.

use atomic_durable::publication::Candidate;
use atomic_durable::{DurableError, Param, Result, Tx};
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::proposal::{AttemptKey, GraphOp, ProposalBuffer, RecordedOp};
use crate::store::Lease;

fn refused(reason: impl std::fmt::Display) -> DurableError {
    DurableError::Interrupted(format!("graph proposal publication refused: {reason}"))
}

fn text(value: &str) -> Param {
    Param::Text(value.to_owned())
}

async fn item_exists(tx: &mut Tx<'_, '_>, id: &str) -> Result<bool> {
    Ok(tx
        .query_opt("SELECT id FROM atomic_graph_items WHERE id=?", &[text(id)])
        .await?
        .is_some())
}

/// Apply a checkpointed turn's graph batch in the caller's transaction. The
/// caller must first fence the live claim, then publish the accepted candidate
/// and close the claimed item in this same transaction. Any error rolls all
/// three parts back. A replay of an already-published candidate never calls
/// this function because the durable transaction checkpoint is returned.
pub async fn apply_candidate_batch(
    tx: &mut Tx<'_, '_>,
    candidate: &Candidate,
    lease: &Lease,
) -> Result<()> {
    let Some(raw) = candidate.proposal.metadata.get("graph_operations") else {
        return Ok(());
    };
    let operations: Vec<RecordedOp> = serde_json::from_value(raw.clone())
        .map_err(|error| refused(format!("decode checkpointed operations: {error}")))?;
    let step = candidate
        .proposal
        .metadata
        .get("graph_attempt_step")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| refused("missing host attempt step"))?;
    let (run, position) = candidate
        .id
        .rsplit_once(':')
        .ok_or_else(|| refused("candidate identity has no turn position"))?;
    let current_position: u32 = position
        .parse()
        .map_err(|_| refused("candidate turn position is invalid"))?;
    let first_position = candidate
        .proposal
        .metadata
        .get("graph_attempt_position")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|position| *position <= current_position)
        .ok_or_else(|| refused("host attempt position is missing or later than its checkpoint"))?;
    let attempt = first_position
        .checked_add(1)
        .ok_or_else(|| refused("host attempt position overflows"))?;
    let key = AttemptKey::new(run, step, attempt).map_err(refused)?;
    let mut checked = ProposalBuffer::new(key);
    for recorded in &operations {
        let receipt = checked.propose(recorded.op.clone()).map_err(refused)?;
        if receipt != recorded.receipt {
            return Err(refused("host-derived proposal receipt changed"));
        }
    }
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(refused)?
            .as_millis(),
    )
    .map_err(refused)?;
    let mut created = BTreeSet::new();
    for recorded in &operations {
        match &recorded.op {
            GraphOp::Create {
                title,
                body,
                kind,
                priority,
                parent,
                due_at,
            } => {
                let id = recorded
                    .receipt
                    .provisional_id
                    .as_deref()
                    .ok_or_else(|| refused("create has no host provisional ID"))?;
                if let Some(parent) = parent
                    && !item_exists(tx, parent).await?
                {
                    return Err(refused(format!("parent {parent} is missing")));
                }
                if item_exists(tx, id).await? {
                    return Err(refused(format!("item {id} already exists")));
                }
                tx.execute(
                    "INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,parent,trust,created_by,created_at,due_at) VALUES (?,?,?,?,?,'open',?,'agent',?,?,?)",
                    &[
                        text(id), text(title), text(body), text(kind),
                        Param::Int(i64::from(*priority)),
                        parent.as_deref().map(text).unwrap_or(Param::Null),
                        text(&lease.claimant), Param::Int(now),
                        due_at.map(Param::Int).unwrap_or(Param::Null),
                    ],
                )
                .await?;
                if let Some(parent) = parent {
                    tx.execute(
                        "INSERT INTO atomic_graph_edges (source,kind,target) VALUES (?,'parent-child',?)",
                        &[text(id), text(parent)],
                    )
                    .await?;
                }
                created.insert(id.to_owned());
            }
            GraphOp::CloseIntent { item_id, reason } => {
                if item_id != &lease.item_id {
                    return Err(refused("close intent requires the current claimed item"));
                }
                let ordinal = tx
                    .query_one(
                        "SELECT COALESCE(MAX(ordinal)+1,0) AS ordinal FROM atomic_graph_notes WHERE item_id=?",
                        &[text(item_id)],
                    )
                    .await?
                    .int("ordinal")?;
                tx.execute(
                    "INSERT INTO atomic_graph_notes (item_id,ordinal,text,author,trust) VALUES (?,?,?,?,'agent')",
                    &[text(item_id), Param::Int(ordinal), text(reason), text(&lease.claimant)],
                )
                .await?;
            }
            GraphOp::DepAdd { from, kind, to } => {
                if from != &lease.item_id && !created.contains(from) {
                    return Err(refused("dependency source is outside this claim's batch"));
                }
                if kind == "parent-child" && !created.contains(from) {
                    return Err(refused("a model cannot reparent the claimed item"));
                }
                if !item_exists(tx, from).await? || !item_exists(tx, to).await? {
                    return Err(refused("dependency endpoint is missing"));
                }
                if matches!(kind.as_str(), "blocks" | "parent-child" | "conditional-blocks" | "waits-for")
                    && tx
                        .query_opt(
                            "WITH RECURSIVE path(id) AS (SELECT ? UNION SELECT e.target FROM atomic_graph_edges e JOIN path p ON e.source=p.id WHERE e.kind IN ('blocks','parent-child','conditional-blocks','waits-for')) SELECT 1 AS found FROM path WHERE id=? LIMIT 1",
                            &[text(to), text(from)],
                        )
                        .await?
                        .is_some()
                {
                    return Err(refused("dependency would create a blocker cycle"));
                }
                if kind == "parent-child" {
                    let parent = tx
                        .query_one(
                            "SELECT parent FROM atomic_graph_items WHERE id=?",
                            &[text(from)],
                        )
                        .await?
                        .text_opt("parent")?;
                    if parent.is_some_and(|parent| parent != *to) {
                        return Err(refused("item already has a different parent"));
                    }
                }
                tx.execute(
                    "INSERT INTO atomic_graph_edges (source,kind,target) VALUES (?,?,?) ON CONFLICT(source,kind,target) DO NOTHING",
                    &[text(from), text(kind), text(to)],
                )
                .await?;
                if kind == "parent-child" {
                    tx.execute(
                        "UPDATE atomic_graph_items SET parent=? WHERE id=?",
                        &[text(to), text(from)],
                    )
                    .await?;
                }
            }
            GraphOp::Remember {
                item_id,
                text: note,
            } => {
                if item_id != &lease.item_id && !created.contains(item_id) {
                    return Err(refused("note target is outside this claim's batch"));
                }
                if !item_exists(tx, item_id).await? {
                    return Err(refused("note target is missing"));
                }
                let ordinal = tx
                    .query_one(
                        "SELECT COALESCE(MAX(ordinal)+1,0) AS ordinal FROM atomic_graph_notes WHERE item_id=?",
                        &[text(item_id)],
                    )
                    .await?
                    .int("ordinal")?;
                tx.execute(
                    "INSERT INTO atomic_graph_notes (item_id,ordinal,text,author,trust) VALUES (?,?,?,?,'agent')",
                    &[text(item_id), Param::Int(ordinal), text(note), text(&lease.claimant)],
                )
                .await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_durable::publication::{Acceptance, Proposal, publish_candidate_in};
    use atomic_durable::{DurableRuntime, Durare};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn candidate(ops: &[GraphOp], position: u32) -> Candidate {
        let mut buffer = ProposalBuffer::new(
            AttemptKey::new("run-one", "author", position + 1).expect("host attempt"),
        );
        for op in ops {
            buffer.propose(op.clone()).expect("valid proposal");
        }
        let proposal = Proposal {
            output: json!({"ok":true}),
            metadata: json!({
                "graph_attempt_step":"author",
                "graph_attempt_position":position,
                "graph_operations":buffer.operations(),
            }),
            writes: Vec::new(),
        };
        Candidate {
            id: format!("run-one:{position}"),
            digest: format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&proposal).expect("candidate JSON"))
            ),
            proposal,
        }
    }

    fn accepted(candidate: &Candidate) -> Acceptance {
        Acceptance {
            candidate: candidate.id.clone(),
            digest: candidate.digest.clone(),
            accepted: true,
            reason: None,
        }
    }

    fn lease() -> Lease {
        Lease {
            item_id: "one".into(),
            claimant: "run-one".into(),
            session: 1,
            epoch: 1,
            acquisition: "claim-one".into(),
        }
    }

    async fn fixture() -> Durare {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "atomic-graph-publication-{}-{}.sqlite",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let rt = Durare::sqlite(&path).await.expect("graph database");
        rt.control_transaction(|mut tx| {
            Box::pin(async move {
                for id in ["one", "other"] {
                    tx.execute(
                        "INSERT INTO atomic_graph_items (id,title,body,kind,priority,status,trust,created_by,created_at) VALUES (?,?, '', 'task',2,'open','human','owner',1)",
                        &[text(id), text(id)],
                    )
                    .await?;
                }
                Ok(())
            })
        })
        .await
        .expect("seed items");
        rt
    }

    async fn counts(rt: &Durare, id: &str) -> (i64, i64, i64) {
        let id = id.to_owned();
        rt.control_transaction(move |mut tx| {
            let id = id.clone();
            Box::pin(async move {
                let items = tx
                    .query_one(
                        "SELECT COUNT(*) AS n FROM atomic_graph_items WHERE id=?",
                        &[text(&id)],
                    )
                    .await?
                    .int("n")?;
                let notes = tx
                    .query_one("SELECT COUNT(*) AS n FROM atomic_graph_notes", &[])
                    .await?
                    .int("n")?;
                let publications = tx
                    .query_one("SELECT COUNT(*) AS n FROM atomic_publications", &[])
                    .await?
                    .int("n")?;
                Ok((items, notes, publications))
            })
        })
        .await
        .expect("inspect graph")
    }

    #[tokio::test]
    async fn rejected_close_and_conflicting_followup_publish_nothing() {
        let rt = fixture().await;
        let rejected_close = candidate(
            &[GraphOp::CloseIntent {
                item_id: "other".into(),
                reason: "no authority".into(),
            }],
            1,
        );
        let close = rejected_close.clone();
        let result = rt
            .control_transaction(move |mut tx| {
                let close = close.clone();
                Box::pin(async move {
                    publish_candidate_in(&mut tx, "publish", &close, &accepted(&close), &[])
                        .await?;
                    apply_candidate_batch(&mut tx, &close, &lease()).await
                })
            })
            .await;
        assert!(result.is_err(), "unauthorized close published");
        assert_eq!(counts(&rt, "other").await, (1, 0, 0));

        let conflicting = candidate(
            &[
                GraphOp::Create {
                    title: "Follow-up".into(),
                    body: "from author".into(),
                    kind: "task".into(),
                    priority: 2,
                    parent: None,
                    due_at: None,
                },
                GraphOp::DepAdd {
                    from: "other".into(),
                    kind: "discovered-from".into(),
                    to: "one".into(),
                },
            ],
            2,
        );
        let created =
            conflicting.proposal.metadata["graph_operations"][0]["receipt"]["provisional_id"]
                .as_str()
                .expect("provisional ID")
                .to_owned();
        let attempt = conflicting.clone();
        let result = rt
            .control_transaction(move |mut tx| {
                let attempt = attempt.clone();
                Box::pin(async move {
                    publish_candidate_in(&mut tx, "publish", &attempt, &accepted(&attempt), &[])
                        .await?;
                    apply_candidate_batch(&mut tx, &attempt, &lease()).await
                })
            })
            .await;
        assert!(result.is_err(), "conflicting follow-up published");
        assert_eq!(counts(&rt, &created).await, (0, 0, 0));
    }

    #[tokio::test]
    async fn candidate_publication_rollback_then_replay_uses_one_batch() {
        let rt = fixture().await;
        let mut candidate = candidate(
            &[
                GraphOp::Create {
                    title: "Follow-up".into(),
                    body: "from author".into(),
                    kind: "task".into(),
                    priority: 2,
                    parent: Some("one".into()),
                    due_at: None,
                },
                GraphOp::Remember {
                    item_id: "one".into(),
                    text: "learned something".into(),
                },
            ],
            3,
        );
        // A released park runs this same author with its first turn's stable
        // tool identity, then checkpoints the candidate at a later position.
        candidate.id = "run-one:4".into();
        let created =
            candidate.proposal.metadata["graph_operations"][0]["receipt"]["provisional_id"]
                .as_str()
                .expect("provisional ID")
                .to_owned();
        let crashed = candidate.clone();
        let result = rt
            .control_transaction(move |mut tx| {
                let crashed = crashed.clone();
                Box::pin(async move {
                    publish_candidate_in(&mut tx, "publish", &crashed, &accepted(&crashed), &[])
                        .await?;
                    apply_candidate_batch(&mut tx, &crashed, &lease()).await?;
                    Err::<(), _>(DurableError::Failed(
                        "crash before checkpoint commit".into(),
                    ))
                })
            })
            .await;
        assert!(result.is_err());
        assert_eq!(counts(&rt, &created).await, (0, 0, 0));
        let replay = candidate.clone();
        rt.control_transaction(move |mut tx| {
            let replay = replay.clone();
            Box::pin(async move {
                publish_candidate_in(&mut tx, "publish", &replay, &accepted(&replay), &[]).await?;
                apply_candidate_batch(&mut tx, &replay, &lease()).await
            })
        })
        .await
        .expect("retry checkpointed candidate");
        assert_eq!(counts(&rt, &created).await, (1, 1, 1));
    }

    #[tokio::test]
    async fn a_model_blocker_cycle_rolls_back_every_created_item() {
        let rt = fixture().await;
        let mut buffer =
            ProposalBuffer::new(AttemptKey::new("run-one", "author", 5).expect("host attempt"));
        let create = |title: &str| GraphOp::Create {
            title: title.into(),
            body: "follow-up".into(),
            kind: "task".into(),
            priority: 2,
            parent: None,
            due_at: None,
        };
        let a = buffer
            .propose(create("Cycle A"))
            .expect("create A")
            .provisional_id
            .expect("A ID");
        let b = buffer
            .propose(create("Cycle B"))
            .expect("create B")
            .provisional_id
            .expect("B ID");
        for (from, to) in [(&a, &b), (&b, &a)] {
            buffer
                .propose(GraphOp::DepAdd {
                    from: from.clone(),
                    kind: "blocks".into(),
                    to: to.clone(),
                })
                .expect("blocker edge");
        }
        let proposal = Proposal {
            output: json!({"ok":true}),
            metadata: json!({"graph_attempt_step":"author",
                "graph_attempt_position":5,"graph_operations":buffer.operations()}),
            writes: Vec::new(),
        };
        let cycle = Candidate {
            id: "run-one:5".into(),
            digest: format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&proposal).expect("candidate JSON"))
            ),
            proposal,
        };
        let attempt = cycle.clone();
        let result = rt
            .control_transaction(move |mut tx| {
                let attempt = attempt.clone();
                Box::pin(async move {
                    publish_candidate_in(&mut tx, "publish", &attempt, &accepted(&attempt), &[])
                        .await?;
                    apply_candidate_batch(&mut tx, &attempt, &lease()).await
                })
            })
            .await;
        assert!(result.is_err(), "model blocker cycle committed");
        assert_eq!(counts(&rt, &a).await, (0, 0, 0));
        assert_eq!(counts(&rt, &b).await, (0, 0, 0));
    }
}
