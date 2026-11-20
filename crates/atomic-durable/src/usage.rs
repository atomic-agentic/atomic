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
//! its bound in tokens and dollars. Its active time can always be measured:
//! it is charged as what passed since admission, at most what was reserved,
//! when it settles as unknown, or, after the process that made it died,
//! when its atom invocation admits its next call. Recovery never releases it: a re-run of the step is a new
//! attempt at the same request with its own reservation, and when the
//! allowance runs out admission refuses, and the run parks. Only a call
//! known never to have been dispatched is released.
//!
//! Caps are fixed when an allowance is first created. Admitting again with
//! larger caps, as a resumed run with an edited budget would, changes
//! nothing, so resuming alone never raises a cap. Only [`Ledger::raise`],
//! a person's action, raises one.
//!
//! An attempt left `reserved` can be reconciled later, once its real usage
//! is known from a usage query or a person ([`Settlement::Reconciled`]), or
//! released once it is confirmed unused ([`Ledger::release`]);
//! [`Ledger::unsettled`] lists them.
//!
//! Charges are integers: tokens, milliseconds of active execution and
//! micro-dollars (millionths of a US dollar), each at most [`MOST`], so no
//! sum the ledger keeps can overflow.

use crate::failpoint;
use crate::runtime::{DurableError, End, Param, Reads, Result, Row, Tx, TxFuture};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// The most any cap, reservation or reported amount may be: 2^53. Larger
/// reported usage is held to it.
pub const MOST: i64 = 1 << 53;

/// How an absent amount is bound, for `NULLIF(?, -1)`: binding NULL instead
/// would type the parameter as text on PostgreSQL, and a statement prepared
/// that way then misreads a number bound to it later.
const NONE: i64 = -1;

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
    /// Whether it is a whole agent turn: it reserves all the active time the
    /// chain has left, the least any allowance in it has, in place of
    /// `reserve.millis`, and is cut off when that runs out. Less time left
    /// than `reserve.millis`, one request's bound, refuses it.
    pub turn: bool,
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
        /// The active milliseconds reserved: the call's time limit.
        millis: i64,
        /// For a turn, the allowance whose time left set that limit.
        bounded_by: Option<String>,
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
    /// No usage is known: the tokens and dollars stay charged at their
    /// bound until a usage query reconciles them. The active time, which can
    /// always be measured, is charged as what passed since admission, at
    /// most what was reserved.
    Unknown,
    /// The call was never dispatched: the reservation is released.
    NotDispatched,
    /// Its usage, learned after the fact from a usage query or a person,
    /// for an attempt that was never settled: it replaces the reservation.
    /// The active time is what passed since admission, at most what was
    /// reserved, unless it was settled already.
    Reconciled {
        /// Input tokens.
        input_tokens: i64,
        /// Output tokens.
        output_tokens: i64,
        /// What it cost, in micro-dollars.
        micro_usd: i64,
    },
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
    /// Milliseconds since the Unix epoch, for admission, heartbeats and
    /// settlement times.
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
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

/// Why `a` has no room for `r`, one reason per cap it would pass; empty
/// when it has room.
fn shortfall(a: &Allowance, r: Charge) -> Vec<String> {
    let over =
        |charged: i64, cap: i64, reserve: i64| charged.checked_add(reserve).is_none_or(|n| n > cap);
    let mut short = Vec::new();
    if over(a.charged.tokens, a.caps.tokens, r.tokens) {
        short.push(format!(
            "{} of {} tokens charged, and the call reserves {}",
            a.charged.tokens, a.caps.tokens, r.tokens
        ));
    }
    if over(a.charged.millis, a.caps.millis, r.millis) {
        short.push(format!(
            "{} of {} ms of active time charged, and the call reserves {}",
            a.charged.millis, a.caps.millis, r.millis
        ));
    }
    if let Some(cap) = a.caps.micro_usd
        && over(a.charged.micro_usd, cap, r.micro_usd)
    {
        short.push(format!(
            "{} of {} micro-dollars charged, and the call reserves {}",
            a.charged.micro_usd, cap, r.micro_usd
        ));
    }
    short
}

async fn admit_in(tx: &mut Tx<'_, '_>, request: &Request, now: i64) -> Result<End<Admission>> {
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
    let mut r = request.reserve;
    let amounts =
        [r.tokens, r.millis, r.micro_usd]
            .into_iter()
            .chain(request.chain.iter().flat_map(|s| {
                [s.caps.tokens, s.caps.millis]
                    .into_iter()
                    .chain(s.caps.micro_usd)
            }));
    if amounts.into_iter().any(|n| !(0..=MOST).contains(&n)) {
        return Err(corrupt(format!(
            "request {} reserves or caps an amount below zero or past {MOST}",
            request.id
        )));
    }
    // Create what is missing, root first, with the caps given now. An
    // allowance that exists keeps the caps it was created with.
    let mut parent: Option<&str> = None;
    for scope in &request.chain {
        // A NULL binds as text on PostgreSQL, hence the cast for the parent;
        // an absent dollar cap is bound as NONE.
        tx.execute(
            "INSERT INTO atomic_usage_allowances (scope, parent, tokens_cap, millis_cap, \
             micro_usd_cap, tokens_charged, millis_charged, micro_usd_charged) \
             VALUES (?, CAST(? AS TEXT), ?, ?, NULLIF(?, -1), 0, 0, 0) \
             ON CONFLICT (scope) DO NOTHING",
            &[
                Param::Text(scope.id.clone()),
                parent.map_or(Param::Null, |p| Param::Text(p.to_owned())),
                Param::Int(scope.caps.tokens),
                Param::Int(scope.caps.millis),
                Param::Int(scope.caps.micro_usd.unwrap_or(NONE)),
            ],
        )
        .await?;
        parent = Some(&scope.id);
    }
    // Reserve against each, root first. Each row is locked before it is
    // read (a write that changes nothing), so the room the read finds is
    // the room the reservation takes, and a refusal's reason comes from the
    // same state as the refusal itself.
    let mut allowances = Vec::with_capacity(request.chain.len());
    for scope in &request.chain {
        tx.execute(
            "UPDATE atomic_usage_allowances SET tokens_cap = tokens_cap WHERE scope = ?",
            &[Param::Text(scope.id.clone())],
        )
        .await?;
        let Some(a) = read_allowance(tx, &scope.id).await? else {
            return Err(corrupt(format!("allowance {} vanished", scope.id)));
        };
        allowances.push(a);
    }
    // A turn takes all the active time left, the least any allowance has.
    let mut bounded_by = None;
    if request.turn {
        let (least, left) = allowances
            .iter()
            .map(|a| (a, a.caps.millis.saturating_sub(a.charged.millis)))
            .min_by_key(|(_, left)| *left)
            .ok_or_else(|| corrupt("an empty chain"))?;
        // Less time than one request's bound is not worth a turn: it would
        // be cut off at once and charged its whole token bound.
        if left < r.millis.max(1) {
            return refuse(
                &least.scope,
                format!(
                    "the allowance {} has less active time left than one request's bound for \
                     request {}: {} of {} ms charged, and a request reserves {}",
                    least.scope, request.id, least.charged.millis, least.caps.millis, r.millis
                ),
            );
        }
        bounded_by = Some(least.scope.clone());
        r.millis = left;
    }
    for (scope, a) in request.chain.iter().zip(&allowances) {
        let short = shortfall(a, r);
        if !short.is_empty() {
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
            return Err(corrupt(format!(
                "allowance {} changed under its lock",
                scope.id
            )));
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
            Param::Int(now),
        ],
    )
    .await?;
    Ok(End::Commit(Admission::Admitted {
        request: request.id.clone(),
        attempt,
        millis: r.millis,
        bounded_by,
    }))
}

/// How often a running call's meter records that it is still alive: a
/// tenth of what it reserved, between 100 ms and 5 s.
pub fn heartbeat_every(reserved_millis: i64) -> i64 {
    (reserved_millis / 10).clamp(100, 5_000)
}

/// Whose measure of an attempt's active time settles it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Measure {
    /// The process that made the call, as the call ends: what passed since
    /// admission. A time settled before, by someone who took the call for
    /// dead, gives way to it if it is larger.
    Owner,
    /// Anyone else, for a call whose process died: what passed until a
    /// heartbeat after the last one it recorded, so time the process was
    /// down is never charged.
    Dead,
}

/// Settle the active time of attempt `attempt` at `request`, while it is
/// still reserved: time can always be measured, so it is charged as
/// `measure` says, at most what was reserved, while its tokens and dollars
/// stay charged at their bound. The row is locked first, and nothing is
/// charged unless the update changed it, so two settlements cannot both
/// apply. Returns the attempt's charged time after, or `None` if it is not
/// reserved.
async fn settle_time(
    tx: &mut Tx<'_, '_>,
    request: &str,
    attempt: i64,
    now: i64,
    measure: Measure,
) -> Result<Option<i64>> {
    let key = [Param::Text(request.to_owned()), Param::Int(attempt)];
    let locked = tx
        .execute(
            "UPDATE atomic_usage_attempts SET attempt = attempt \
             WHERE request_id = ? AND attempt = ? AND state = 'reserved'",
            &key,
        )
        .await?;
    if locked != 1 {
        return Ok(None);
    }
    let row = tx
        .query_one(
            "SELECT a.scopes, a.reserved_millis, a.charged_millis, a.admitted_at, a.settled_at, \
             h.seen_at FROM atomic_usage_attempts a LEFT JOIN atomic_usage_heartbeats h \
             ON h.request_id = a.request_id AND h.attempt = a.attempt \
             WHERE a.request_id = ? AND a.attempt = ?",
            &key,
        )
        .await?;
    let (charged, reserved, admitted) = (
        row.int("charged_millis")?,
        row.int("reserved_millis")?,
        row.int("admitted_at")?,
    );
    let settled = row.int_opt("settled_at")?.is_some();
    let measured = match measure {
        Measure::Owner => {
            let took = (now - admitted).clamp(0, reserved);
            if settled { took.max(charged) } else { took }
        }
        Measure::Dead if settled => return Ok(Some(charged)),
        Measure::Dead => {
            let seen = row.int_opt("seen_at")?.unwrap_or(admitted);
            let until = (seen + heartbeat_every(reserved)).min(now);
            (until - admitted).clamp(0, reserved)
        }
    };
    if settled && measured == charged {
        return Ok(Some(charged));
    }
    let changed = tx
        .execute(
            "UPDATE atomic_usage_attempts SET charged_millis = ?, settled_at = ? \
             WHERE request_id = ? AND attempt = ? AND state = 'reserved' AND charged_millis = ?",
            &[
                Param::Int(measured),
                Param::Int(now),
                Param::Text(request.to_owned()),
                Param::Int(attempt),
                Param::Int(charged),
            ],
        )
        .await?;
    if changed != 1 {
        return Ok(Some(charged));
    }
    let scopes: Vec<String> = serde_json::from_str(&row.text("scopes")?).map_err(corrupt)?;
    add_charge(
        tx,
        &scopes,
        Charge {
            tokens: 0,
            millis: measured - charged,
            micro_usd: 0,
        },
    )
    .await?;
    Ok(Some(measured))
}

/// Settle the time of every attempt that died charged to `request`'s own
/// atom invocation (the last scope of its chain): an invocation makes its
/// calls one at a time, so when it admits another, none of its earlier
/// ones still reserved is running. Each is charged until a heartbeat after
/// the last it recorded, at most what it reserved.
async fn settle_dead_in(tx: &mut Tx<'_, '_>, request: &Request, now: i64) -> Result<End<()>> {
    let Some(own) = request.chain.last() else {
        return Ok(End::Rollback(()));
    };
    let earlier = tx
        .query_all(
            "SELECT request_id, attempt, scopes FROM atomic_usage_attempts WHERE run = ? \
             AND state = 'reserved' AND settled_at IS NULL ORDER BY request_id, attempt",
            &[Param::Text(request.run.clone())],
        )
        .await?;
    let mut settled = false;
    for row in earlier {
        let scopes: Vec<String> = serde_json::from_str(&row.text("scopes")?).map_err(corrupt)?;
        if scopes.last() == Some(&own.id) {
            settle_time(
                tx,
                &row.text("request_id")?,
                row.int("attempt")?,
                now,
                Measure::Dead,
            )
            .await?;
            settled = true;
        }
    }
    Ok(if settled {
        End::Commit(())
    } else {
        End::Rollback(())
    })
}

async fn settle_in(
    tx: &mut Tx<'_, '_>,
    request: &str,
    attempt: i64,
    how: Settlement,
    now: i64,
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
    // Usage that is not reported still took time: settle that much now.
    if matches!(how, Settlement::Unknown | Settlement::Reconciled { .. }) {
        // The owner settles an unknown call as it ends; a reconciliation
        // comes after, for a call whose process may be long gone.
        let measure = match how {
            Settlement::Unknown => Measure::Owner,
            _ => Measure::Dead,
        };
        settle_time(tx, request, attempt, now, measure).await?;
        if matches!(how, Settlement::Unknown) {
            return Ok(End::Commit(Settled::Uncertain));
        }
    }
    let row = tx
        .query_one(
            "SELECT state, scopes, charged_tokens, charged_millis, charged_micro_usd \
             FROM atomic_usage_attempts WHERE request_id = ? AND attempt = ?",
            &key,
        )
        .await?;
    let scopes: Vec<String> = serde_json::from_str(&row.text("scopes")?).map_err(corrupt)?;
    let charged = Charge {
        tokens: row.int("charged_tokens")?,
        millis: row.int("charged_millis")?,
        micro_usd: row.int("charged_micro_usd")?,
    };
    // Reported amounts are held to [0, MOST], so the sums below cannot
    // overflow.
    let held = |n: i64| n.clamp(0, MOST);
    let (state, actual, usage) = match how {
        Settlement::Unknown => return Ok(End::Commit(Settled::Uncertain)),
        Settlement::NotDispatched => ("released", Charge::default(), None),
        Settlement::Reported {
            input_tokens,
            output_tokens,
            millis,
            micro_usd,
        } => (
            "settled",
            Charge {
                tokens: held(input_tokens) + held(output_tokens),
                millis: held(millis),
                micro_usd: held(micro_usd),
            },
            Some((held(input_tokens), held(output_tokens))),
        ),
        Settlement::Reconciled {
            input_tokens,
            output_tokens,
            micro_usd,
        } => (
            "settled",
            Charge {
                tokens: held(input_tokens) + held(output_tokens),
                millis: charged.millis,
                micro_usd: held(micro_usd),
            },
            Some((held(input_tokens), held(output_tokens))),
        ),
    };
    // Only a reserved attempt changes, so two settlements cannot both apply.
    let changed = tx
        .execute(
            "UPDATE atomic_usage_attempts SET state = ?, input_tokens = NULLIF(?, -1), \
             output_tokens = NULLIF(?, -1), charged_tokens = ?, charged_millis = ?, \
             charged_micro_usd = ?, settled_at = ? \
             WHERE request_id = ? AND attempt = ? AND state = 'reserved'",
            &[
                Param::Text(state.to_owned()),
                Param::Int(usage.map_or(NONE, |(i, _)| i)),
                Param::Int(usage.map_or(NONE, |(_, o)| o)),
                Param::Int(actual.tokens),
                Param::Int(actual.millis),
                Param::Int(actual.micro_usd),
                Param::Int(now),
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
        Self {
            db,
            clock: Arc::new(now_ms),
        }
    }

    /// The same ledger reading the time from `clock`, in milliseconds since
    /// the Unix epoch: for tests that need time to pass.
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
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
        let request = Arc::new(request);
        // The time of the invocation's dead calls first, in a transaction of
        // its own: it stands whether or not this call is admitted.
        let (dead, clock) = (request.clone(), self.clock.clone());
        self.transact(move |mut tx| {
            let (request, now) = (dead.clone(), clock());
            Box::pin(async move { settle_dead_in(&mut tx, &request, now).await })
        })
        .await?;
        let clock = self.clock.clone();
        self.transact(move |mut tx| {
            let (request, now) = (request.clone(), clock());
            Box::pin(async move { admit_in(&mut tx, &request, now).await })
        })
        .await
    }

    /// Record that the call of attempt `attempt` at `request` is still
    /// running, so that if its process dies, the time charged for it stops
    /// here rather than when someone finds it dead. A running call's meter
    /// calls it every [`heartbeat_every`] of its reservation.
    pub async fn heartbeat(&self, request: &str, attempt: i64) -> Result<()> {
        let (request, clock) = (request.to_owned(), self.clock.clone());
        self.transact(move |mut tx| {
            let (request, now) = (request.clone(), clock());
            Box::pin(async move {
                tx.execute(
                    "INSERT INTO atomic_usage_heartbeats (request_id, attempt, seen_at) \
                     VALUES (?, ?, ?) ON CONFLICT (request_id, attempt) \
                     DO UPDATE SET seen_at = excluded.seen_at",
                    &[Param::Text(request), Param::Int(attempt), Param::Int(now)],
                )
                .await?;
                Ok(End::Commit(()))
            })
        })
        .await
    }

    /// Settle attempt `attempt` at `request` as unknown because its process
    /// is stopping before the call ended: its tokens and dollars stay
    /// charged at their bound, its time is what passed. As
    /// [`Settlement::Unknown`], without the crash tests' failpoint.
    pub async fn interrupt(&self, request: &str, attempt: i64) -> Result<Settled> {
        self.settle_once(request, attempt, Settlement::Unknown)
            .await
    }

    /// Settle attempt `attempt` at `request` once. Settling it again, or
    /// settling a released attempt, changes nothing.
    pub async fn settle(&self, request: &str, attempt: i64, how: Settlement) -> Result<Settled> {
        // Where a crash test kills the process: after the provider was
        // called, before its usage is recorded.
        if crate::failpoints::armed() {
            failpoint(&format!("usage:before-settle:{request}"));
        }
        self.settle_once(request, attempt, how).await
    }

    /// Release attempt `attempt` at `request`, left `reserved`, once it is
    /// confirmed that it spent nothing: its reservation is no longer
    /// charged. Releasing a settled or released attempt changes nothing.
    pub async fn release(&self, request: &str, attempt: i64) -> Result<Settled> {
        self.settle_once(request, attempt, Settlement::NotDispatched)
            .await
    }

    /// The attempts journaled for run `run` that are still `reserved`: each
    /// charged at its bound until it is settled, reconciled or released.
    pub async fn unsettled(&self, run: &str) -> Result<Vec<Attempt>> {
        Ok(self
            .attempts(run)
            .await?
            .into_iter()
            .filter(|a| a.state == "reserved")
            .collect())
    }

    /// Raise allowance `scope`'s caps to `caps`, each only where it is
    /// larger: a cap is never lowered here, and a `micro_usd` of `None`
    /// leaves the dollar cap as it is. A person's action, from a human
    /// credential only; resuming a run never calls it. Returns the
    /// allowance as it stands after, or `None` if there is no such one.
    pub async fn raise(&self, scope: &str, caps: Caps) -> Result<Option<Allowance>> {
        if [caps.tokens, caps.millis]
            .into_iter()
            .chain(caps.micro_usd)
            .any(|n| !(0..=MOST).contains(&n))
        {
            return Err(corrupt(format!(
                "a cap for {scope} is below zero or past {MOST}"
            )));
        }
        let scope = scope.to_owned();
        self.transact(move |mut tx| {
            let scope = scope.clone();
            Box::pin(async move {
                // Locked before it is read, so a concurrent raise cannot be
                // undone by this one.
                tx.execute(
                    "UPDATE atomic_usage_allowances SET tokens_cap = tokens_cap WHERE scope = ?",
                    &[Param::Text(scope.clone())],
                )
                .await?;
                let Some(a) = read_allowance(&mut tx, &scope).await? else {
                    return Ok(End::Rollback(None));
                };
                let raised = Caps {
                    tokens: a.caps.tokens.max(caps.tokens),
                    millis: a.caps.millis.max(caps.millis),
                    micro_usd: match (a.caps.micro_usd, caps.micro_usd) {
                        (Some(old), Some(new)) => Some(old.max(new)),
                        (old, _) => old,
                    },
                };
                tx.execute(
                    "UPDATE atomic_usage_allowances SET tokens_cap = ?, millis_cap = ?, \
                     micro_usd_cap = NULLIF(?, -1) WHERE scope = ?",
                    &[
                        Param::Int(raised.tokens),
                        Param::Int(raised.millis),
                        Param::Int(raised.micro_usd.unwrap_or(NONE)),
                        Param::Text(scope.clone()),
                    ],
                )
                .await?;
                Ok(End::Commit(read_allowance(&mut tx, &scope).await?))
            })
        })
        .await
    }

    async fn settle_once(&self, request: &str, attempt: i64, how: Settlement) -> Result<Settled> {
        let (request, clock) = (request.to_owned(), self.clock.clone());
        self.transact(move |mut tx| {
            let (request, now) = (request.clone(), clock());
            Box::pin(async move { settle_in(&mut tx, &request, attempt, how, now).await })
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

    /// Every attempt journaled for run `run`, by request and attempt. A
    /// read: it takes no write lock.
    pub async fn attempts(&self, run: &str) -> Result<Vec<Attempt>> {
        let run = run.to_owned();
        self.db
            .read(move |mut tx| {
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
                    Ok(attempts)
                })
            })
            .await
    }
}

#[cfg(test)]
mod tests;
