// SPDX-License-Identifier: Apache-2.0

//! Metering model calls.
//!
//! Every model call an atom makes, the first turn and any repair turn alike,
//! passes through a [`Meter`]: [`Meter::admit`] before the call is
//! dispatched, and [`Meter::settle`] once, after it, with what the provider
//! reported. A meter that refuses admission stops the atom before the call
//! goes out, with [`Exhausted`]; the atom's run fails with
//! [`crate::AtomError::Budget`], which a durable workflow turns into a parked
//! run.
//!
//! The meter itself is the caller's: a durable workflow gives each atom step a
//! meter backed by its usage ledger, which reserves the call's whole bound
//! before dispatch and settles it by a stable request and attempt id. A run
//! with no meter ([`Metering::none`]) is not accounted, which suits tests and
//! one-off diagnostics only.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Tokens one model call used, as the provider reported them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    /// Input tokens, including any cached input.
    pub input_tokens: i64,
    /// Output tokens, including reasoning.
    pub output_tokens: i64,
}

impl Usage {
    /// Input and output together.
    pub fn total(&self) -> i64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// The call about to be dispatched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// Its position among the atom run's calls, from 0: the first turn is 0,
    /// a repair turn 1. Stable across a re-run of the same step.
    pub index: u32,
    /// The model it goes to.
    pub model: String,
    /// An upper bound on its input tokens, when the caller can tell: the
    /// size of the request in bytes, since no token is shorter than a byte.
    /// The meter records it in the call's declared maximum.
    pub input_bound: Option<u64>,
    /// Whether it is a whole agent turn, such as a Codex turn, that may make
    /// several provider requests and run tools between them: it reserves
    /// the atom's remaining active time, not one request's.
    pub turn: bool,
}

/// What an admitted call may use: the runtime holds the call to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limit {
    /// The output tokens it may return, sent to the provider as the call's
    /// output limit.
    pub output_tokens: Option<u64>,
    /// How long it may take: it is cut off after this.
    pub time: Option<Duration>,
}

impl Limit {
    /// `default`, or this limit's time if it is shorter.
    pub fn time_within(&self, default: Duration) -> Duration {
        self.time.map_or(default, |t| t.min(default))
    }
}

/// A call the meter admitted, to be settled once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    /// The meter's id for the request.
    pub request: String,
    /// Which attempt at the request this is, from 1.
    pub attempt: i64,
    /// The model the call goes to.
    pub model: String,
    /// When it was admitted, for the active time it took.
    pub admitted: Instant,
    /// What the call may use: its reservation.
    pub limit: Limit,
    /// For a turn, the allowance whose time left set its time limit, such
    /// as `atom:<run>:<step>`.
    pub bounded_by: Option<String>,
}

/// An admission: the ticket to settle, if the call is metered, and the
/// limit to hold it to.
pub(crate) type Admitted = (Option<Ticket>, Limit);

/// How a call ended, for its settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The provider answered and reported this usage. A failed or rejected
    /// answer still spent it.
    Reported(Usage),
    /// The call may have reached the provider, but no usage is known: it
    /// stays charged at its bound until a usage query reconciles it (none is
    /// built yet, so it stays charged at its bound).
    Unknown,
    /// Part of what the call spent is known: it reported this usage, and
    /// was then stopped with a provider request in flight, which reported
    /// nothing. It is charged this usage and one more request's bound, at
    /// least its reservation, and stays open for a usage query, which is not
    /// built yet: until one is, the charge keeps that value.
    Partial(Usage),
    /// The call certainly never reached the provider, so nothing was spent.
    NotDispatched,
}

/// Admission was refused: a cap leaves no room for the call's bound.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Exhausted(pub String);

/// Why the meter could not admit or settle a call.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MeterError {
    /// A cap leaves no room: the run parks.
    #[error(transparent)]
    Exhausted(#[from] Exhausted),
    /// The meter's store failed. The call is not dispatched; an admitted one
    /// stays charged at its bound.
    #[error("the usage ledger: {0}")]
    Store(String),
    /// The meter cannot account for the call as its budget requires, such
    /// as a call to a model with no price under a dollar cap. The call is
    /// not dispatched, and the atom fails.
    #[error("{0}")]
    Unaccountable(String),
}

/// A boxed future, for the object-safe [`Meter`].
pub type MeterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, MeterError>> + Send + 'a>>;

/// What admits and settles an atom's model calls.
pub trait Meter: Send + Sync {
    /// Reserve `call`'s bound before it is dispatched, or refuse it.
    fn admit(&self, call: Call) -> MeterFuture<'_, Ticket>;

    /// Settle `ticket` with how its call ended. Settling a ticket again
    /// changes nothing.
    fn settle(&self, ticket: Ticket, outcome: Outcome) -> MeterFuture<'_, ()>;
}

/// The meter a runtime's calls go through, if any.
#[derive(Clone, Default)]
pub struct Metering(Option<Arc<dyn Meter>>);

impl Metering {
    /// No meter: calls are not accounted.
    pub fn none() -> Self {
        Self(None)
    }

    /// Calls go through `meter`.
    pub fn new(meter: Arc<dyn Meter>) -> Self {
        Self(Some(meter))
    }

    /// Whether calls are accounted.
    pub fn is_metered(&self) -> bool {
        self.0.is_some()
    }

    /// Admit `call`; with no meter, no ticket and no limit.
    pub(crate) async fn admit(&self, call: Call) -> Result<Admitted, MeterError> {
        match &self.0 {
            Some(meter) => meter.admit(call).await.map(|t| {
                let limit = t.limit;
                (Some(t), limit)
            }),
            None => Ok((None, Limit::default())),
        }
    }

    /// Settle `ticket`, if there is one.
    pub(crate) async fn settle(
        &self,
        ticket: Option<Ticket>,
        outcome: Outcome,
    ) -> Result<(), MeterError> {
        match (&self.0, ticket) {
            (Some(meter), Some(ticket)) => meter.settle(ticket, outcome).await,
            _ => Ok(()),
        }
    }
}

impl std::fmt::Debug for Metering {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Metering(metered)"
        } else {
            "Metering(none)"
        })
    }
}

/// An error that stops a conversation because the meter refused or failed,
/// so the caller can tell it from a provider failure.
pub(crate) fn meter_error(e: MeterError) -> anyhow::Error {
    anyhow::Error::new(e)
}

/// The meter's refusal or failure inside `e`, if that is what stopped it.
pub(crate) fn meter_failure(e: &anyhow::Error) -> Option<&MeterError> {
    e.chain().find_map(|c| c.downcast_ref::<MeterError>())
}
