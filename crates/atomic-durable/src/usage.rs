// SPDX-License-Identifier: Apache-2.0

//! The durable model-usage ledger: budget accounting, kept apart from step
//! outputs and telemetry.
//!
//! An **allowance** is a cap on cumulative usage for one scope: a whole run,
//! or one atom invocation within it. Allowances form a chain: an atom's
//! allowance sits under its run's, and a child run's under its parent's. Each
//! allowance holds its caps and what is charged against it.
//!
//! Every model call goes through the ledger twice:
//!
//! 1. **Admission**, before the call is dispatched: one short transaction
//!    creates any allowance in the chain that does not exist yet, numbers a
//!    new attempt at the call's stable request id, and reserves the call's
//!    whole bound against every allowance in the chain, root first, each
//!    reservation a conditional update that fails when the cap has no room.
//!    Either every allowance takes the reservation or none does. The
//!    conditional update is the lock: on PostgreSQL a concurrent admission
//!    against the same allowance waits for this one to commit and then sees
//!    its charge, and on SQLite every write transaction runs alone. Two
//!    parallel children can therefore never both take the last of their
//!    parent's allowance.
//! 2. **Settlement**, after the call, once per attempt: the reservation is
//!    replaced by what the provider reported. A second settlement of the same
//!    attempt changes nothing.
//!
//! An attempt that is never settled, because the process died after the
//! provider was called or the provider reported nothing, stays charged at
//! its bound. Recovery never releases it: a re-run of the step is a new
//! attempt at the same request with its own reservation, and when the
//! allowance runs out admission refuses, and the run parks. Only a call
//! known never to have been dispatched is released.
//!
//! Caps are fixed when an allowance is first created. Admitting again with
//! larger caps, as a resumed run with an edited budget would, changes
//! nothing, so resuming alone never raises a cap.
//!
//! Charges are integers: tokens, milliseconds of active execution and
//! micro-dollars (millionths of a US dollar).

use crate::failpoint;
use crate::runtime::{DurableError, End, Param, Reads, Result, Row, Tx, TxFuture};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// The caps of one allowance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    /// Input and output tokens.
    pub tokens: i64,
    /// Milliseconds of active execution.
    pub millis: i64,
    /// Micro-dollars, or `None` for no monetary cap.
    pub micro_usd: Option<i64>,
}

/// An allowance in an admission's chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// Its id, unique in the ledger, such as `run:<run id>`.
    pub id: String,
    /// The caps it is created with, if it does not exist yet.
    pub caps: Caps,
}

impl Scope {
    /// The allowance of run `run`.
    pub fn run(run: &str, caps: Caps) -> Self {
        Self {
            id: format!("run:{run}"),
            caps,
        }
    }

    /// The allowance of the atom invocation at position `step` in run `run`.
    pub fn atom(run: &str, step: i32, caps: Caps) -> Self {
        Self {
            id: format!("atom:{run}:{step}"),
            caps,
        }
    }
}

/// An amount charged against an allowance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Charge {
    /// Input and output tokens.
    pub tokens: i64,
    /// Milliseconds of active execution.
    pub millis: i64,
    /// Micro-dollars.
    pub micro_usd: i64,
}

/// A call to admit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Its stable id: the same for every attempt at the same call, such as
    /// `<run>:<step position>:<call index>`.
    pub id: String,
    /// The run it belongs to.
    pub run: String,
    /// The model it goes to.
    pub model: String,
    /// The allowances it is charged against, root first.
    pub chain: Vec<Scope>,
    /// What to reserve: the call's whole bound.
    pub reserve: Charge,
    /// The provider's documented maximum for the call. Usage beyond the
    /// reservation can reach at most `max(0, maximum - reserve)`, which is
    /// zero when the whole maximum is reserved.
    pub maximum: Charge,
}

/// What admission decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Reserved: dispatch the call, then settle this attempt.
    Admitted {
        /// The request.
        request: String,
        /// The attempt, from 1.
        attempt: i64,
    },
    /// Refused: an allowance has no room for the reservation. Nothing was
    /// written.
    Refused {
        /// The allowance with no room.
        scope: String,
        /// Why, for a person.
        reason: String,
    },
}

/// How a call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settlement {
    /// The provider reported usage: it replaces the reservation.
    Reported {
        /// Input tokens.
        input_tokens: i64,
        /// Output tokens.
        output_tokens: i64,
        /// Active milliseconds the call took.
        millis: i64,
        /// What it cost, in micro-dollars.
        micro_usd: i64,
    },
    /// Nothing is known: the reservation stays charged at its bound until a
    /// usage query reconciles it.
    Unknown,
    /// The call was never dispatched: the reservation is released.
    NotDispatched,
}

/// What a settlement did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    /// This settlement replaced the reservation.
    Now,
    /// The attempt was settled or released already; nothing changed.
    Already,
    /// Nothing was known, so the attempt stays charged at its bound.
    Uncertain,
}

/// An allowance as the ledger has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Allowance {
    /// Its id.
    pub scope: String,
    /// The allowance it sits under.
    pub parent: Option<String>,
    /// Its caps, fixed when it was created.
    pub caps: Caps,
    /// What is charged against it: settled usage plus every reservation not
    /// yet settled, its own and its descendants'.
    pub charged: Charge,
}

/// One attempt at a request, as the ledger journals it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// The request.
    pub request: String,
    /// Which attempt, from 1.
    pub attempt: i64,
    /// The run.
    pub run: String,
    /// The model.
    pub model: String,
    /// `reserved` until settled, then `settled`, or `released` if it was
    /// never dispatched.
    pub state: String,
    /// What admission reserved.
    pub reserved: Charge,
    /// The provider's documented maximum.
    pub maximum: Charge,
    /// The reported usage, once settled.
    pub usage: Option<(i64, i64)>,
    /// What is charged for it now: its reservation until settled, then what
    /// it actually used.
    pub charged: Charge,
}

impl Attempt {
    /// How far its reported usage went past its reservation, in tokens.
    pub fn overshoot(&self) -> i64 {
        (self.charged.tokens - self.reserved.tokens).max(0)
    }

    /// The most its usage may go past its reservation by declaration:
    /// `max(0, maximum - reserved)` tokens.
    pub fn excess_bound(&self) -> i64 {
        (self.maximum.tokens - self.reserved.tokens).max(0)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn corrupt(what: impl std::fmt::Display) -> DurableError {
    DurableError::Engine(format!("the usage ledger: {what}"))
}

/// The usage ledger in a runtime's database.
#[derive(Clone)]
pub struct Ledger {
    db: Reads,
}

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ledger(..)")
    }
}

const ATTEMPT_COLUMNS: &str = "request_id, attempt, run, model, state, reserved_tokens, \
    reserved_millis, reserved_micro_usd, maximum_tokens, maximum_micro_usd, input_tokens, \
    output_tokens, charged_tokens, charged_millis, charged_micro_usd";

fn attempt_from_row(row: &Row) -> Result<Attempt> {
    let usage = match (row.int_opt("input_tokens")?, row.int_opt("output_tokens")?) {
        (Some(i), Some(o)) => Some((i, o)),
        _ => None,
    };
    Ok(Attempt {
        request: row.text("request_id")?,
        attempt: row.int("attempt")?,
        run: row.text("run")?,
        model: row.text("model")?,
        state: row.text("state")?,
        reserved: Charge {
            tokens: row.int("reserved_tokens")?,
            millis: row.int("reserved_millis")?,
            micro_usd: row.int("reserved_micro_usd")?,
        },
        maximum: Charge {
            tokens: row.int("maximum_tokens")?,
            millis: row.int("reserved_millis")?,
            micro_usd: row.int("maximum_micro_usd")?,
        },
        usage,
        charged: Charge {
            tokens: row.int("charged_tokens")?,
            millis: row.int("charged_millis")?,
            micro_usd: row.int("charged_micro_usd")?,
        },
    })
}

fn allowance_from_row(row: &Row) -> Result<Allowance> {
    Ok(Allowance {
        scope: row.text("scope")?,
        parent: row.text_opt("parent")?,
        caps: Caps {
            tokens: row.int("tokens_cap")?,
            millis: row.int("millis_cap")?,
            micro_usd: row.int_opt("micro_usd_cap")?,
        },
        charged: Charge {
            tokens: row.int("tokens_charged")?,
            millis: row.int("millis_charged")?,
            micro_usd: row.int("micro_usd_charged")?,
        },
    })
}

async fn read_allowance(tx: &mut Tx<'_, '_>, scope: &str) -> Result<Option<Allowance>> {
    match tx
        .query_opt(
            "SELECT scope, parent, tokens_cap, millis_cap, micro_usd_cap, tokens_charged, \
             millis_charged, micro_usd_charged FROM atomic_usage_allowances WHERE scope = ?",
            &[Param::Text(scope.to_owned())],
        )
        .await?
    {
        Some(row) => Ok(Some(allowance_from_row(&row)?)),
        None => Ok(None),
    }
}

/// Add `delta` to what `scopes` are charged, root first, so concurrent
/// changes to the same chain lock in the same order.
async fn add_charge(tx: &mut Tx<'_, '_>, scopes: &[String], delta: Charge) -> Result<()> {
    for scope in scopes {
        tx.execute(
            "UPDATE atomic_usage_allowances SET tokens_charged = tokens_charged + ?, \
             millis_charged = millis_charged + ?, micro_usd_charged = micro_usd_charged + ? \
             WHERE scope = ?",
            &[
                Param::Int(delta.tokens),
                Param::Int(delta.millis),
                Param::Int(delta.micro_usd),
                Param::Text(scope.clone()),
            ],
        )
        .await?;
    }
    Ok(())
}

async fn admit_in(tx: &mut Tx<'_, '_>, request: &Request) -> Result<End<Admission>> {
    let refuse = |scope: &str, reason: String| {
        Ok(End::Rollback(Admission::Refused {
            scope: scope.to_owned(),
            reason,
        }))
    };
    if request.chain.is_empty() {
        return Err(corrupt(format!(
            "request {} names no allowance to charge",
            request.id
        )));
    }
    let r = request.reserve;
    if r.tokens < 0 || r.millis < 0 || r.micro_usd < 0 {
        return Err(corrupt(format!(
            "request {} reserves less than nothing",
            request.id
        )));
    }
    // Create what is missing, root first, with the caps given now. An
    // allowance that exists keeps the caps it was created with.
    let mut parent: Option<&str> = None;
    for scope in &request.chain {
        // A NULL binds as text on PostgreSQL, hence the casts.
        tx.execute(
            "INSERT INTO atomic_usage_allowances (scope, parent, tokens_cap, millis_cap, \
             micro_usd_cap, tokens_charged, millis_charged, micro_usd_charged) \
             VALUES (?, CAST(? AS TEXT), ?, ?, CAST(? AS BIGINT), 0, 0, 0) \
             ON CONFLICT (scope) DO NOTHING",
            &[
                Param::Text(scope.id.clone()),
                parent.map_or(Param::Null, |p| Param::Text(p.to_owned())),
                Param::Int(scope.caps.tokens),
                Param::Int(scope.caps.millis),
                scope.caps.micro_usd.map_or(Param::Null, Param::Int),
            ],
        )
        .await?;
        parent = Some(&scope.id);
    }
    // Reserve against each, root first. The conditional update both checks
    // the room and takes it, under the row's lock.
    for scope in &request.chain {
        let rows = tx
            .execute(
                "UPDATE atomic_usage_allowances SET tokens_charged = tokens_charged + ?, \
                 millis_charged = millis_charged + ?, micro_usd_charged = micro_usd_charged + ? \
                 WHERE scope = ? AND tokens_charged + ? <= tokens_cap \
                 AND millis_charged + ? <= millis_cap \
                 AND (micro_usd_cap IS NULL OR micro_usd_charged + ? <= micro_usd_cap)",
                &[
                    Param::Int(r.tokens),
                    Param::Int(r.millis),
                    Param::Int(r.micro_usd),
                    Param::Text(scope.id.clone()),
                    Param::Int(r.tokens),
                    Param::Int(r.millis),
                    Param::Int(r.micro_usd),
                ],
            )
            .await?;
        if rows != 1 {
            let Some(a) = read_allowance(tx, &scope.id).await? else {
                return Err(corrupt(format!("allowance {} vanished", scope.id)));
            };
            let mut short = Vec::new();
            if a.charged.tokens + r.tokens > a.caps.tokens {
                short.push(format!(
                    "{} of {} tokens charged, and the call reserves {}",
                    a.charged.tokens, a.caps.tokens, r.tokens
                ));
            }
            if a.charged.millis + r.millis > a.caps.millis {
                short.push(format!(
                    "{} of {} ms of active time charged, and the call reserves {}",
                    a.charged.millis, a.caps.millis, r.millis
                ));
            }
            if let Some(cap) = a.caps.micro_usd
                && a.charged.micro_usd + r.micro_usd > cap
            {
                short.push(format!(
                    "{} of {} micro-dollars charged, and the call reserves {}",
                    a.charged.micro_usd, cap, r.micro_usd
                ));
            }
            return refuse(
                &scope.id,
                format!(
                    "the allowance {} has no room for request {}: {}",
                    scope.id,
                    request.id,
                    short.join("; ")
                ),
            );
        }
    }
    let attempt = tx
        .query_one(
            "SELECT COALESCE(MAX(attempt), 0) AS n FROM atomic_usage_attempts WHERE request_id = ?",
            &[Param::Text(request.id.clone())],
        )
        .await?
        .int("n")?
        + 1;
    let scopes: Vec<&str> = request.chain.iter().map(|s| s.id.as_str()).collect();
    tx.execute(
        "INSERT INTO atomic_usage_attempts (request_id, attempt, run, model, scopes, state, \
         reserved_tokens, reserved_millis, reserved_micro_usd, maximum_tokens, maximum_micro_usd, \
         charged_tokens, charged_millis, charged_micro_usd, admitted_at) \
         VALUES (?, ?, ?, ?, ?, 'reserved', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            Param::Text(request.id.clone()),
            Param::Int(attempt),
            Param::Text(request.run.clone()),
            Param::Text(request.model.clone()),
            Param::Text(serde_json::to_string(&scopes).map_err(corrupt)?),
            Param::Int(r.tokens),
            Param::Int(r.millis),
            Param::Int(r.micro_usd),
            Param::Int(request.maximum.tokens),
            Param::Int(request.maximum.micro_usd),
            Param::Int(r.tokens),
            Param::Int(r.millis),
            Param::Int(r.micro_usd),
            Param::Int(now_ms()),
        ],
    )
    .await?;
    Ok(End::Commit(Admission::Admitted {
        request: request.id.clone(),
        attempt,
    }))
}

async fn settle_in(
    tx: &mut Tx<'_, '_>,
    request: &str,
    attempt: i64,
    how: Settlement,
) -> Result<End<Settled>> {
    let key = [Param::Text(request.to_owned()), Param::Int(attempt)];
    let Some(row) = tx
        .query_opt(
            "SELECT state, scopes, charged_tokens, charged_millis, charged_micro_usd \
             FROM atomic_usage_attempts WHERE request_id = ? AND attempt = ?",
            &key,
        )
        .await?
    else {
        return Err(corrupt(format!(
            "no attempt {attempt} at request {request} to settle"
        )));
    };
    if row.text("state")? != "reserved" {
        return Ok(End::Rollback(Settled::Already));
    }
    let scopes: Vec<String> = serde_json::from_str(&row.text("scopes")?).map_err(corrupt)?;
    let charged = Charge {
        tokens: row.int("charged_tokens")?,
        millis: row.int("charged_millis")?,
        micro_usd: row.int("charged_micro_usd")?,
    };
    let (state, actual, usage) = match how {
        Settlement::Unknown => return Ok(End::Rollback(Settled::Uncertain)),
        Settlement::NotDispatched => ("released", Charge::default(), None),
        Settlement::Reported {
            input_tokens,
            output_tokens,
            millis,
            micro_usd,
        } => (
            "settled",
            Charge {
                tokens: input_tokens.max(0).saturating_add(output_tokens.max(0)),
                millis: millis.max(0),
                micro_usd: micro_usd.max(0),
            },
            Some((input_tokens.max(0), output_tokens.max(0))),
        ),
    };
    // Only a reserved attempt changes, so two settlements cannot both apply.
    let changed = tx
        .execute(
            "UPDATE atomic_usage_attempts SET state = ?, input_tokens = CAST(? AS BIGINT), \
             output_tokens = CAST(? AS BIGINT), charged_tokens = ?, charged_millis = ?, \
             charged_micro_usd = ?, settled_at = ? \
             WHERE request_id = ? AND attempt = ? AND state = 'reserved'",
            &[
                Param::Text(state.to_owned()),
                usage.map_or(Param::Null, |(i, _)| Param::Int(i)),
                usage.map_or(Param::Null, |(_, o)| Param::Int(o)),
                Param::Int(actual.tokens),
                Param::Int(actual.millis),
                Param::Int(actual.micro_usd),
                Param::Int(now_ms()),
                Param::Text(request.to_owned()),
                Param::Int(attempt),
            ],
        )
        .await?;
    if changed != 1 {
        return Ok(End::Rollback(Settled::Already));
    }
    add_charge(
        tx,
        &scopes,
        Charge {
            tokens: actual.tokens - charged.tokens,
            millis: actual.millis - charged.millis,
            micro_usd: actual.micro_usd - charged.micro_usd,
        },
    )
    .await?;
    Ok(End::Commit(Settled::Now))
}

impl Ledger {
    pub(crate) fn new(db: Reads) -> Self {
        Self { db }
    }

    /// Run `body` in one short transaction, committing or rolling back as
    /// it says. An error rolls back.
    async fn transact<T, F>(&self, body: F) -> Result<T>
    where
        T: Send + 'static,
        F: for<'t, 'c> Fn(Tx<'t, 'c>) -> TxFuture<'t, End<T>> + Send + Sync + 'static,
    {
        match self.db.transaction(body).await? {
            End::Commit(v) | End::Rollback(v) => Ok(v),
        }
    }

    /// Admit `request` before its call is dispatched: number a new attempt
    /// and reserve its bound against every allowance in its chain, or refuse
    /// it and write nothing.
    pub async fn admit(&self, request: Request) -> Result<Admission> {
        let request = std::sync::Arc::new(request);
        self.transact(move |mut tx| {
            let request = request.clone();
            Box::pin(async move { admit_in(&mut tx, &request).await })
        })
        .await
    }

    /// Settle attempt `attempt` at `request` once. Settling it again, or
    /// settling a released attempt, changes nothing.
    pub async fn settle(&self, request: &str, attempt: i64, how: Settlement) -> Result<Settled> {
        // Where a crash test kills the process: after the provider was
        // called, before its usage is recorded.
        failpoint("usage:before-settle");
        let request = request.to_owned();
        self.transact(move |mut tx| {
            let request = request.clone();
            Box::pin(async move { settle_in(&mut tx, &request, attempt, how).await })
        })
        .await
    }

    /// The allowance `scope`, if it exists.
    pub async fn allowance(&self, scope: &str) -> Result<Option<Allowance>> {
        let scope = scope.to_owned();
        self.transact(move |mut tx| {
            let scope = scope.clone();
            Box::pin(async move { Ok(End::Rollback(read_allowance(&mut tx, &scope).await?)) })
        })
        .await
    }

    /// Every attempt journaled for run `run`, by request and attempt.
    pub async fn attempts(&self, run: &str) -> Result<Vec<Attempt>> {
        let run = run.to_owned();
        self.transact(move |mut tx| {
            let run = run.clone();
            Box::pin(async move {
                let rows = tx
                    .query_all(
                        &format!(
                            "SELECT {ATTEMPT_COLUMNS} FROM atomic_usage_attempts WHERE run = ? \
                             ORDER BY request_id, attempt"
                        ),
                        &[Param::Text(run)],
                    )
                    .await?;
                let attempts = rows
                    .iter()
                    .map(attempt_from_row)
                    .collect::<Result<Vec<_>>>()?;
                Ok(End::Rollback(attempts))
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests;
