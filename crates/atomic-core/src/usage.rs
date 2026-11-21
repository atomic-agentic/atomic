// SPDX-License-Identifier: Apache-2.0

//! Metering model calls, and an exec atom's command.
//!
//! Every model call an atom makes, the first turn and any repair turn alike,
//! and every exec atom's command (a [`Call`] that is [`Call::work`]), passes
//! through a [`Meter`]: [`Meter::admit`] before the call is
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
    /// Whether it is not a model call but other active work, such as an exec
    /// atom's command: it uses no tokens and costs nothing, and, like a
    /// turn, reserves the active time left and is cut off when it runs out.
    pub work: bool,
    /// For a turn or work, the longest it can take when it has a limit of
    /// its own, such as a command's: it reserves no more of the time left
    /// than that.
    pub longest: Option<Duration>,
}

/// The model name active work that is not a model call is accounted under.
pub const WORK_MODEL: &str = "(command)";

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
pub type Admitted = (Option<Ticket>, Limit);

/// How a call ended, for its settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The provider answered and reported this usage. A failed or rejected
    /// answer still spent it.
    Reported(Usage),
    /// The call may have reached the provider, but no usage is known: it
    /// stays charged at its bound: atomic has no lookup of one call's usage,
    /// and OpenAI, Azure OpenAI and local servers, as atomic calls them,
    /// offer none.
    Unknown,
    /// Part of what the call spent is known: it reported `reported`, and
    /// was then stopped with a provider request in flight, which reported
    /// nothing. The request in flight is taken to be as large as one
    /// request's bound, or as `last`, the last request that reported, if
    /// that was larger: a turn's requests grow as it goes. It is charged the
    /// reported usage and that estimate, at least its reservation, and the
    /// charge keeps that value, as an unknown one does.
    Partial {
        /// What it reported.
        reported: Usage,
        /// What its last request that reported used, if any did.
        last: Option<Usage>,
    },
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

    /// Record what the call of `ticket`, still running, has reported so far,
    /// as an [`Outcome::Partial`]: so that a call whose process stops or dies
    /// before it is settled stays charged at least that, and the request it
    /// may have in flight. Recording it again with more replaces it; the
    /// call's settlement replaces it in turn. A meter that keeps nothing
    /// between admission and settlement records nothing.
    fn progress<'a>(&'a self, ticket: &'a Ticket, outcome: Outcome) -> MeterFuture<'a, ()> {
        let _ = (ticket, outcome);
        Box::pin(async { Ok(()) })
    }
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
    pub async fn admit(&self, call: Call) -> Result<Admitted, MeterError> {
        match &self.0 {
            Some(meter) => meter.admit(call).await.map(|t| {
                let limit = t.limit;
                (Some(t), limit)
            }),
            None => Ok((None, Limit::default())),
        }
    }

    /// Record what the call of `ticket` has reported so far, if it is
    /// metered (see [`Meter::progress`]).
    pub async fn progress(
        &self,
        ticket: Option<&Ticket>,
        outcome: Outcome,
    ) -> Result<(), MeterError> {
        match (&self.0, ticket) {
            (Some(meter), Some(ticket)) => meter.progress(ticket, outcome).await,
            _ => Ok(()),
        }
    }

    /// Settle `ticket`, if there is one.
    pub async fn settle(&self, ticket: Option<Ticket>, outcome: Outcome) -> Result<(), MeterError> {
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

/// The run and step a model call is made for, which its span names. Set by
/// the workflow code that runs a step, around the step's work; see
/// [`for_step`].
#[derive(Clone, Debug)]
pub struct Caller {
    /// The run's workflow ID.
    pub run: String,
    /// The step's ID.
    pub step: String,
}

tokio::task_local! {
    static CALLER: Caller;
}

/// Run `work` as step `step` of run `run`: model calls it makes name them on
/// their spans.
pub async fn for_step<F: std::future::Future>(
    run: impl Into<String>,
    step: impl Into<String>,
    work: F,
) -> F::Output {
    let caller = Caller {
        run: run.into(),
        step: step.into(),
    };
    CALLER.scope(caller, work).await
}

/// The caller the current task runs for, to carry into a task it spawns.
pub(crate) fn caller() -> Option<Caller> {
    CALLER.try_with(Caller::clone).ok()
}

/// Run `work` for `caller`, as the task that spawned it was.
pub(crate) async fn with_caller<F: std::future::Future>(
    caller: Option<Caller>,
    work: F,
) -> F::Output {
    match caller {
        Some(caller) => CALLER.scope(caller, work).await,
        None => work.await,
    }
}

/// The span of one model call, named as the GenAI conventions name a chat
/// call, a child of the step's span. Its usage is recorded on it when the
/// provider reports any. A Codex atom's call is one turn, which may make
/// several requests to the provider: its span covers them all.
pub(crate) fn call_span(model: &str, provider: &str) -> tracing::Span {
    let caller = caller();
    let span = tracing::info_span!(
        "chat",
        otel.name = %format!("chat {model}"),
        otel.kind = "client",
        otel.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty,
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = %provider,
        gen_ai.request.model = %model,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        error.message = tracing::field::Empty,
        http.response.status_code = tracing::field::Empty,
        atomic.workflow.id = tracing::field::Empty,
        atomic.step = tracing::field::Empty,
        atomic.stopped = tracing::field::Empty,
    );
    if let Some(caller) = caller {
        span.record("atomic.workflow.id", caller.run.as_str());
        span.record("atomic.step", caller.step.as_str());
    }
    span
}

/// The provider a base URL names, for `gen_ai.provider.name`: OpenAI's own
/// API, or another that speaks its protocol.
pub(crate) fn provider_name(base_url: &str) -> &'static str {
    if crate::lite::is_openai_api(base_url) {
        "openai"
    } else {
        "openai_compatible"
    }
}

/// Record on `span` what a call spent.
pub(crate) fn record_usage(span: &tracing::Span, usage: &Usage) {
    span.record("gen_ai.usage.input_tokens", usage.input_tokens);
    span.record("gen_ai.usage.output_tokens", usage.output_tokens);
}

/// The most characters of a failed call's error a span carries, with the
/// `…` that ends one that was cut. The run's stored error keeps the whole.
pub(crate) const MAX_SPAN_ERROR: usize = 200;

/// What a call's span says of why it failed, redacted and at most
/// [`MAX_SPAN_ERROR`] characters: for an error a provider answered with,
/// its status and the type of error it names, with its HTTP status code;
/// else what failed and its innermost cause, such as a connection's
/// `Connection refused`, rather than every context between, which can
/// repeat the URL.
pub(crate) fn span_error(error: &anyhow::Error) -> (String, Option<u16>) {
    let bounded = |text: &str| crate::redact::bounded(text, MAX_SPAN_ERROR - 1);
    match error
        .chain()
        .find_map(|e| e.downcast_ref::<crate::lite::ProviderError>())
    {
        Some(provider) => (bounded(&provider.summary()), Some(provider.code)),
        None if error.chain().count() > 1 => {
            (bounded(&format!("{error}: {}", error.root_cause())), None)
        }
        None => (bounded(&error.to_string()), None),
    }
}

/// Record on `span` why a call failed (see [`span_error`]).
pub(crate) fn record_error(span: &tracing::Span, error: &anyhow::Error) {
    span.record("otel.status_code", "ERROR");
    span.record(
        "error.type",
        if meter_failure(error).is_some() {
            "atomic.meter"
        } else {
            "atomic.model_call"
        },
    );
    let (message, status) = span_error(error);
    span.record("error.message", message.as_str());
    if let Some(status) = status {
        span.record("http.response.status_code", i64::from(status));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call's time is its own limit, or its default when that is shorter
    /// or it has none.
    #[test]
    fn a_limits_time_is_the_shorter_of_its_own_and_the_default() {
        let five = Duration::from_secs(300);
        let limit = |time: Option<Duration>| Limit {
            output_tokens: None,
            time,
        };
        assert_eq!(limit(None).time_within(five), five);
        assert_eq!(
            limit(Some(Duration::from_millis(700))).time_within(five),
            Duration::from_millis(700)
        );
        assert_eq!(
            limit(Some(Duration::from_secs(3600))).time_within(five),
            five
        );
    }
}
