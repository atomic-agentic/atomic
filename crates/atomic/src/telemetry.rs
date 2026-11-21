// SPDX-License-Identifier: Apache-2.0

//! Basic OpenTelemetry spans: a run exports a span for its workflow, one for
//! each step it executes, and one for each model call.
//!
//! The spans are `tracing` spans, made where the work happens: the workflow
//! and its steps in this crate, model calls in atomic-core. Here they are
//! exported over OTLP when the standard variables configure an endpoint
//! (`OTEL_EXPORTER_OTLP_ENDPOINT` or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, with
//! `OTEL_EXPORTER_OTLP_PROTOCOL` and the exporter's other `OTEL_*`
//! variables); with none set, or `OTEL_SDK_DISABLED=true`, nothing is
//! installed and nothing is exported.
//!
//! Only atomic's own spans are exported. Codex, which atomic-core embeds,
//! makes spans of its own and installs no tracer: atomic drives Codex's
//! threads directly, with telemetry left off in Codex's configuration, so
//! this is the one tracer provider in the process, and it is not installed as
//! the global one.
//!
//! Every exported span passes through [`atomic_core::redact`] first: its name,
//! string attributes, events and status message, so a secret that reached a
//! span, such as in a provider's error, is not exported.
//!
//! A step or run that fails ends its span with the failure's class and a short
//! reason, not the failure whole: what a model call that failed in that step
//! said of its own cause (for a lite atom, a provider's status and the type of
//! error it named, none of its body; for a Codex atom, its redacted and bounded
//! error line, which can quote the provider), or, when nothing so near a model call failed, the
//! failure's message cut short (see [`mark_failed`]). A step notes on a handle
//! of its own, which the run takes up only when that step failed. The whole
//! failure stays in the run's stored error, where a person reads it.
//!
//! Names follow the GenAI semantic conventions where they fit, with atomic's
//! own under `atomic.*` (see [`attributes`]).

use atomic_durable::DurableError;
use opentelemetry::trace::{Status, TracerProvider as _};
use opentelemetry::{KeyValue, Value};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};
use std::time::Duration;
use tracing::field::Empty;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

/// The span and attribute names atomic uses, in one place: the GenAI
/// conventions are not stable, so a change to them is a change here.
pub mod attributes {
    /// A workflow run's span.
    pub const WORKFLOW_SPAN: &str = "invoke_workflow";
    /// An atom step's span.
    pub const AGENT_SPAN: &str = "invoke_agent";
    /// A gate step's span.
    pub const GATE_SPAN: &str = "atomic.gate";
}

/// The span of one workflow run's execution in this process.
pub fn workflow_span(workflow: &str, run: &str) -> tracing::Span {
    tracing::info_span!(
        "invoke_workflow",
        otel.name = %format!("{} {workflow}", attributes::WORKFLOW_SPAN),
        error.type = Empty,
        gen_ai.operation.name = attributes::WORKFLOW_SPAN,
        atomic.workflow = %workflow,
        atomic.workflow.id = %run,
        atomic.stopped = Empty,
        gen_ai.conversation.id = %run,
    )
}

/// The span of one execution of atom step `step`, which runs `atom`.
pub fn step_span(atom: &str, step: &str, run: &str) -> tracing::Span {
    tracing::info_span!(
        "invoke_agent",
        otel.name = %format!("{} {atom}", attributes::AGENT_SPAN),
        error.type = Empty,
        gen_ai.operation.name = attributes::AGENT_SPAN,
        gen_ai.agent.name = %atom,
        atomic.step = %step,
        atomic.workflow.id = %run,
        atomic.stopped = Empty,
    )
}

/// The span of a gate step `gate` while this process waits at it.
pub fn gate_span(gate: &str, run: &str) -> tracing::Span {
    tracing::info_span!(
        "atomic.gate",
        otel.name = %format!("{} {gate}", attributes::GATE_SPAN),
        error.type = Empty,
        atomic.gate = %gate,
        atomic.step = %gate,
        atomic.workflow.id = %run,
        atomic.stopped = Empty,
    )
}

/// The class of a durable failure, for `error.type`, or `None` for an
/// outcome that is not a failure (a run that parks is waiting).
fn error_type(e: &DurableError) -> Option<&'static str> {
    match e {
        DurableError::Parked(_) => None,
        DurableError::Failed(_) => Some("atomic.failed"),
        DurableError::Interrupted(_) => Some("atomic.interrupted"),
        _ => Some("atomic.error"),
    }
}

/// Mark `span`, which `why` speaks for, failed with `e`: ERROR status with why
/// it failed and its class (redacted on export).
///
/// What the span says of why is what the calls it covers noted — for a lite
/// atom's call the provider answered with an error status, that status and the
/// type of error it named, none of the provider's own text; for a Codex atom,
/// its redacted and bounded error line, which can quote the provider (see
/// [`atomic_core::usage::Why`]) — or, when nothing
/// noted anything, `e` itself, redacted and cut to
/// [`atomic_core::usage::MAX_SPAN_ERROR`] characters, which is how a model
/// call's own span cuts it. A span never carries the whole failure: that
/// travels in `e` and stays in the run's stored error, where a person reads
/// it. A step's span and its run's span say the same thing, because a durable
/// step's failure reaches the run only as its error's text — and only for an
/// execution of the run that ran the step: a run resumed after a restart has
/// nothing noted, and its span says what the failure it recorded says, cut
/// short. When `span` is one of the steps of `run`, what it noted goes to the
/// run's note too, so the run's span says the same as this one did; a span
/// that speaks for the run itself (`run: None`) and an outcome that is not a
/// failure — a step that parks — leave the run's note as it is.
pub fn mark_failed(
    span: &tracing::Span,
    why: &atomic_core::usage::Why,
    run: Option<&atomic_core::usage::Why>,
    e: &DurableError,
) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let Some(kind) = error_type(e) else {
        return;
    };
    let message = why.noted().unwrap_or_else(|| {
        atomic_core::redact::bounded(&e.to_string(), atomic_core::usage::MAX_SPAN_ERROR - 1)
    });
    span.set_status(Status::error(message));
    span.record("error.type", kind);
    if let Some(run) = run {
        run.note_step(why);
    }
}

/// A future run inside a span, which ends when the future does, when the
/// future is dropped, or when the process [`flush`]es before it exits,
/// whichever comes first (see [`atomic_core::spans`]). A failure the future
/// returns marks the span failed, as [`mark_failed`] has it, and — for a step
/// made with [`traced_step`] — says the same on the run's spans.
pub struct Traced<F> {
    inner: atomic_core::spans::InHeld<F>,
    /// What this span says of a call of its own that failed.
    why: atomic_core::usage::Why,
    /// The run's own note, which takes `why`'s when this span's work fails:
    /// `None` for a span that speaks for the run itself.
    run: Option<atomic_core::usage::Why>,
}

/// Run `future` inside `span`, which speaks for the run `why` notes for (see
/// [`Traced`]).
pub fn traced<F, T>(span: tracing::Span, why: atomic_core::usage::Why, future: F) -> Traced<F>
where
    F: Future<Output = Result<T, DurableError>>,
{
    Traced {
        inner: atomic_core::spans::hold(span).run(future),
        why,
        run: None,
    }
}

/// Run `future` as one step of the run `run_why` notes for, inside the step's
/// `span`, which `why` speaks for: `why` is what the step's model calls note
/// on, its span says the same, and when the step fails the run's spans say it
/// too (see [`mark_failed`] and [`atomic_core::usage::Why`]). A step that
/// succeeds or parks leaves nothing on them.
pub fn traced_step<F, T>(
    span: tracing::Span,
    why: atomic_core::usage::Why,
    run_why: &atomic_core::usage::Why,
    future: F,
) -> Traced<F>
where
    F: Future<Output = Result<T, DurableError>>,
{
    Traced {
        inner: atomic_core::spans::hold(span).run(future),
        why,
        run: Some(run_why.clone()),
    }
}

impl<F, T> Future for Traced<F>
where
    F: Future<Output = Result<T, DurableError>>,
{
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let polled = Pin::new(&mut self.inner).poll(cx);
        if let Poll::Ready(out) = &polled {
            let held = self.inner.held();
            if let (Some(span), Err(e)) = (held.span(), out) {
                // A step hands its note to the run it failed, which is what
                // `run` is for; a span that speaks for the run has none.
                mark_failed(&span, &self.why, self.run.as_ref(), e);
            }
            // Ended now, not when the future's owner drops it.
            held.end();
        }
        polled
    }
}

/// The installed exporter, kept so that [`flush`] can send what is buffered
/// before the process exits.
static PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// The OTLP protocol the variables ask for: the traces one, else the general
/// one, else `http/protobuf`, the specification's default.
fn protocol() -> String {
    [
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
        "OTEL_EXPORTER_OTLP_PROTOCOL",
    ]
    .iter()
    .find_map(|v| std::env::var(v).ok().filter(|p| !p.is_empty()))
    .unwrap_or_else(|| "http/protobuf".into())
}

/// Whether the variables ask for spans to be exported: an endpoint is set,
/// and neither `OTEL_SDK_DISABLED=true` nor `OTEL_TRACES_EXPORTER=none` turns
/// export off.
fn configured() -> bool {
    let set = |v: &str| std::env::var(v).is_ok_and(|s| !s.is_empty());
    let is = |v: &str, value: &str| std::env::var(v).is_ok_and(|s| s.eq_ignore_ascii_case(value));
    !is("OTEL_SDK_DISABLED", "true")
        && !is("OTEL_TRACES_EXPORTER", "none")
        && (set("OTEL_EXPORTER_OTLP_ENDPOINT") || set("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"))
}

/// Install the exporter the standard variables configure, if they configure
/// one, as this process's tracing subscriber. Call it once, on the process's
/// Tokio runtime, which the gRPC exporter runs on; call [`flush`] before the
/// process exits.
pub fn init_from_env() -> anyhow::Result<()> {
    if !configured() {
        return Ok(());
    }
    use opentelemetry_otlp::{Protocol, WithExportConfig};
    let builder = SdkTracerProvider::builder();
    let builder = match protocol().as_str() {
        // tonic needs a Tokio reactor for its connection and for every
        // export, so its batches are sent from a task on the runtime.
        "grpc" => {
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .build()?;
            let processor =
                opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor::builder(
                    Redacting(exporter),
                    opentelemetry_sdk::runtime::Tokio,
                )
                .build();
            builder.with_span_processor(processor)
        }
        // HTTP batches are sent from the processor's own thread, with a
        // blocking client.
        "http/json" => builder.with_batch_exporter(Redacting(
            opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpJson)
                .build()?,
        )),
        "http/protobuf" => builder.with_batch_exporter(Redacting(
            opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .with_protocol(Protocol::HttpBinary)
                .build()?,
        )),
        other => anyhow::bail!(
            "OTEL_EXPORTER_OTLP_PROTOCOL is {other:?}; atomic exports grpc, http/protobuf or \
             http/json"
        ),
    };
    install(builder)
}

/// Install `builder`'s provider, with atomic's resource, as this process's
/// tracing subscriber.
fn install(builder: opentelemetry_sdk::trace::TracerProviderBuilder) -> anyhow::Result<()> {
    let provider = builder
        .with_resource(Resource::builder().with_service_name("atomic").build())
        .build();
    tracing::subscriber::set_global_default(subscriber(&provider))?;
    let _ = PROVIDER.set(provider);
    Ok(())
}

/// A subscriber that sends atomic's spans to `provider`.
pub fn subscriber(provider: &SdkTracerProvider) -> impl tracing::Subscriber + Send + Sync {
    let layer = tracing_opentelemetry::layer()
        .with_tracer(provider.tracer("atomic"))
        .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
            meta.target().starts_with("atomic")
        }));
    tracing_subscriber::registry().with(layer)
}

/// End the spans still open, send what is buffered and stop exporting. Call
/// it before the process exits; afterwards nothing is exported.
pub fn flush() {
    // The gRPC processor's task runs on this runtime: wait for it on a
    // thread that may block.
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(flush_blocking)
        }
        _ => flush_blocking(),
    }
}

/// As [`flush`], unless a signal comes first: then it stops waiting and
/// returns `true`, so a collector that never answers holds up the exit
/// until Ctrl-C or [`FLUSH_DEADLINE`], whichever is first. Call it as a
/// command ends normally.
pub async fn flush_unless_signalled() -> bool {
    if PROVIDER.get().is_none() {
        atomic_core::spans::end_all();
        return false;
    }
    let flushing = tokio::task::spawn_blocking(flush_blocking);
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let (Ok(mut interrupt), Ok(mut terminate)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) {
            return tokio::select! {
                _ = flushing => false,
                _ = interrupt.recv() => true,
                _ = terminate.recv() => true,
            };
        }
    }
    let _ = flushing.await;
    false
}

/// How long a process waits, as it exits, for its last spans to be sent.
/// Spans not sent by then are dropped, so a collector that accepts the
/// connection and never answers holds up an exit by about this, however long
/// the exporter's own timeout.
pub const FLUSH_DEADLINE: Duration = Duration::from_secs(2);

/// As [`flush`], on a thread that may block, such as one of
/// `spawn_blocking`'s: a stopping process runs it there so that another
/// signal can still stop it at once (see `runs::stop`). It returns by
/// [`FLUSH_DEADLINE`].
pub fn flush_blocking() {
    atomic_core::spans::end_all();
    if let Some(provider) = PROVIDER.get()
        && !flush_within(provider.clone(), FLUSH_DEADLINE)
    {
        atomic_core::log_line!(
            "atomic: the trace collector did not take the last spans within {} s; they are \
             dropped",
            FLUSH_DEADLINE.as_secs()
        );
    }
}

/// Send what `provider` holds and shut it down, on a thread of its own that
/// is left behind if it has not finished within `deadline`. Whether it
/// finished.
fn flush_within(provider: SdkTracerProvider, deadline: Duration) -> bool {
    let started = std::time::Instant::now();
    let (done, finished) = std::sync::mpsc::channel();
    let flushing = std::thread::Builder::new()
        .name("atomic-flush".into())
        .spawn(move || {
            let _ = provider.force_flush();
            let left = deadline.saturating_sub(started.elapsed());
            let _ = provider.shutdown_with_timeout(left.max(Duration::from_millis(50)));
            let _ = done.send(());
        });
    flushing.is_ok() && finished.recv_timeout(deadline).is_ok()
}

/// An exporter that redacts every span before `E` exports it.
#[derive(Debug)]
pub struct Redacting<E>(pub E);

/// The most of one span string exported: longer ones are cut before they
/// are redacted, so redaction stays quick before the export's deadline.
pub const MAX_SPAN_TEXT: usize = 4096;

/// `s` bounded and redacted, if that changes it.
fn scrub(s: &str) -> Option<String> {
    let out = atomic_core::redact::bounded(s, MAX_SPAN_TEXT);
    (out != s).then_some(out)
}

fn redact_value(value: &mut Value) {
    match value {
        Value::String(s) => {
            if let Some(r) = scrub(s.as_str()) {
                *value = Value::String(r.into());
            }
        }
        Value::Array(opentelemetry::Array::String(items)) => {
            for item in items {
                if let Some(r) = scrub(item.as_str()) {
                    *item = r.into();
                }
            }
        }
        _ => {}
    }
}

fn redact_attributes(attributes: &mut [KeyValue]) {
    for kv in attributes {
        redact_value(&mut kv.value);
    }
}

/// `span` with every string it carries bounded and redacted.
pub fn redact_span(span: &mut SpanData) {
    if let Some(r) = scrub(&span.name) {
        span.name = r.into();
    }
    redact_attributes(&mut span.attributes);
    for event in &mut span.events.events {
        if let Some(r) = scrub(&event.name) {
            event.name = r.into();
        }
        redact_attributes(&mut event.attributes);
    }
    if let Status::Error { description } = &mut span.status
        && let Some(r) = scrub(description)
    {
        *description = r.into();
    }
}

impl<E: SpanExporter> SpanExporter for Redacting<E> {
    fn export(
        &self,
        mut batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        batch.iter_mut().for_each(redact_span);
        self.0.export(batch)
    }

    fn shutdown_with_timeout(&mut self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn force_flush(&mut self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::trace::InMemorySpanExporter;

    /// An exporter whose exports never end, as one sending to a collector
    /// that never answers.
    #[derive(Debug)]
    struct Hanging;

    impl SpanExporter for Hanging {
        fn export(
            &self,
            _: Vec<SpanData>,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            std::future::pending()
        }
    }

    /// A flush whose export never ends gives up at its deadline.
    #[test]
    fn a_flush_to_an_exporter_that_never_answers_ends_at_its_deadline() {
        use opentelemetry::trace::{Tracer, TracerProvider as _};
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(Hanging)
            .build();
        provider.tracer("atomic").in_span("held", |_| {});
        let (done, flushed) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            let _ = done.send(flush_within(provider, Duration::from_millis(300)));
        });
        let finished = flushed
            .recv_timeout(Duration::from_secs(20))
            .expect("the flush waited for the exporter for good");
        assert!(!finished, "an export that never ends finished");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn an_exported_span_carries_no_seeded_key() {
        let memory = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(Redacting(memory.clone()))
            .build();
        let seeded = concat!("sk-ant-", "api03-SEEDEDsampleVALUE0123456789abcdef");
        tracing::subscriber::with_default(subscriber(&provider), || {
            let span = tracing::info_span!(
                target: "atomic_app::telemetry",
                "chat",
                otel.name = "chat synthetic",
                error.message = format!("401: {{\"error\": \"bad key {seeded}\"}}"),
            );
            let _entered = span.enter();
            tracing::info!(target: "atomic_app::telemetry", "the provider said {seeded}");
        });
        provider.force_flush().expect("flush");
        let spans = memory.get_finished_spans().expect("spans");
        assert_eq!(spans.len(), 1, "{spans:?}");
        let shown = format!("{spans:?}");
        assert!(!shown.contains(seeded), "{shown}");
        assert!(shown.contains("[REDACTED]"), "{shown}");
        assert!(shown.contains("chat synthetic"), "{shown}");
    }

    /// A step's span and its run's say why they failed by the provider's
    /// status and the type of error it named, and carry none of the
    /// provider's own text however long its body: what the model call said
    /// reaches both spans through the step's own note, although workflow code
    /// is handed the run's failure as text alone, and a step's span is marked
    /// before its run's.
    #[tokio::test]
    async fn a_steps_span_and_its_runs_say_a_provider_failure_by_status_and_type() {
        const RUN: &str = "run:provider-body";
        const URL: &str = "http://127.0.0.1:9/v1/chat/completions";
        // Long enough that its end is past what a span may carry, with a type
        // the provider named.
        let body = format!(
            "{{\"error\": {{\"message\": \"{} PASTTHECAP\", \"type\": \"server_error\"}}}}",
            "detail ".repeat(50)
        );
        let failure = atomic_core::testing::provider_failure(URL, 500, &body);
        let whole = format!("{failure:#}");
        assert!(
            whole.contains("detail") && whole.contains("PASTTHECAP"),
            "the failure a run keeps holds the provider's body: {whole}"
        );
        // What the model call that failed says of itself, on the note of the
        // step it was made in (see `note_call`).
        let step_why = atomic_core::usage::Why::for_step();
        step_why.note_call(&failure);
        let memory = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(memory.clone())
            .build();
        let run_why = atomic_core::usage::Why::default();
        let failed = DurableError::Failed(whole);
        let entered = tracing::subscriber::set_default(subscriber(&provider));
        // The step as the workflow that ran it runs it, then its run, which
        // is handed only the text the step's failure travelled as.
        let of_the_step = traced_step(
            step_span("rank-note", "rank", RUN),
            step_why,
            &run_why,
            async { Err::<(), _>(failed.clone()) },
        )
        .await;
        let of_the_run = traced(workflow_span("triage", RUN), run_why, async {
            Err::<(), _>(failed)
        })
        .await;
        drop(entered);
        assert!(of_the_step.is_err() && of_the_run.is_err());
        provider.force_flush().expect("flush");
        let spans = memory.get_finished_spans().expect("spans");
        assert_eq!(spans.len(), 2, "{spans:?}");
        for name in ["invoke_agent rank-note", "invoke_workflow triage"] {
            let data = spans
                .iter()
                .find(|s| s.name.as_ref() == name)
                .unwrap_or_else(|| panic!("no {name} was exported: {spans:?}"));
            let Status::Error { description } = &data.status else {
                panic!("a span that failed: {data:?}");
            };
            assert_eq!(
                description.to_string(),
                "500 Internal Server Error: server_error",
                "{name} says the status and the type, and nothing else"
            );
            let class = data
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == "error.type")
                .map(|kv| kv.value.as_str().to_string());
            assert_eq!(
                class.as_deref(),
                Some("atomic.failed"),
                "{name} does not name its class: {data:?}"
            );
        }
    }

    /// A step's note reaches its run only through that step's own failure: a
    /// call that failed in a step whose output was taken anyway labels
    /// nothing, so the run says what the failure that stopped it says rather
    /// than what an earlier step explained.
    #[tokio::test]
    async fn a_step_that_was_gone_labels_nothing_on_its_runs_span() {
        const RUN: &str = "run:tolerated";
        const URL: &str = "http://127.0.0.1:9/v1/chat/completions";
        let failure = atomic_core::testing::provider_failure(
            URL,
            503,
            r#"{"error": {"message": "busy", "type": "server_error"}}"#,
        );
        let memory = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(memory.clone())
            .build();
        let run_why = atomic_core::usage::Why::default();
        let entered = tracing::subscriber::set_default(subscriber(&provider));
        // A first step whose call failed and whose answer was taken anyway.
        let first = atomic_core::usage::Why::for_step();
        first.note_call(&failure);
        let taken = traced_step(
            step_span("rank-note", "rank", RUN),
            first,
            &run_why,
            std::future::ready(Ok(())),
        )
        .await;
        assert!(taken.is_ok(), "the step's output was taken");
        // The run then failed on something no model call explained.
        let whole = format!("{} THE END", "detail ".repeat(80));
        let second = atomic_core::usage::Why::for_step();
        let ran = traced_step(
            step_span("summarize-note", "summarize", RUN),
            second,
            &run_why,
            async { Err::<(), _>(DurableError::Failed(whole.clone())) },
        )
        .await;
        assert!(ran.is_err());
        let of_the_run = traced(workflow_span("triage", RUN), run_why, async {
            Err::<(), _>(DurableError::Failed(whole))
        })
        .await;
        assert!(of_the_run.is_err());
        drop(entered);
        provider.force_flush().expect("flush");
        let spans = memory.get_finished_spans().expect("spans");
        let said = |name: &str| -> Option<String> {
            let data = spans
                .iter()
                .find(|s| s.name.as_ref() == name)
                .unwrap_or_else(|| panic!("no {name} was exported: {spans:?}"));
            match &data.status {
                Status::Error { description } => Some(description.to_string()),
                _ => None,
            }
        };
        // The step that was taken anyway says nothing of the call that failed
        // in it: it did not fail.
        assert_eq!(said("invoke_agent rank-note"), None, "not a failure");
        for name in ["invoke_agent summarize-note", "invoke_workflow triage"] {
            let shown = said(name).unwrap_or_else(|| panic!("{name} did not fail"));
            assert!(
                !shown.contains("503"),
                "{name} carries a failure the run went past: {shown}"
            );
            assert!(shown.ends_with('…'), "not cut short: {shown}");
            assert!(!shown.contains("THE END"), "its end was carried: {shown}");
        }
    }

    /// A failure no model call explained is cut to what a span may carry, and
    /// the `…` says it was: the whole stays in the run's stored error. This is
    /// also what a run resumed after a restart says, since nothing noted in
    /// the process that ran the step survives it.
    #[test]
    fn a_steps_span_cuts_a_failure_no_model_call_explained() {
        const RUN: &str = "run:cut-alone";
        let whole = format!("{} THE END", "detail ".repeat(80));
        let memory = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(memory.clone())
            .build();
        tracing::subscriber::with_default(subscriber(&provider), || {
            let span = step_span("rank-note", "rank", RUN);
            mark_failed(
                &span,
                &atomic_core::usage::Why::default(),
                None,
                &DurableError::Failed(whole),
            );
        });
        provider.force_flush().expect("flush");
        let spans = memory.get_finished_spans().expect("spans");
        assert_eq!(spans.len(), 1, "{spans:?}");
        let Status::Error { description } = &spans[0].status else {
            panic!("a span that failed: {:?}", spans[0]);
        };
        let shown = description.to_string();
        assert!(
            shown.chars().count() <= atomic_core::usage::MAX_SPAN_ERROR,
            "{} characters: {shown}",
            shown.chars().count()
        );
        assert!(shown.ends_with('…'), "not cut short: {shown}");
        assert!(!shown.contains("THE END"), "its end was carried: {shown}");
    }

    #[test]
    fn only_atomic_spans_are_exported() {
        let memory = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(memory.clone())
            .build();
        tracing::subscriber::with_default(subscriber(&provider), || {
            let _codex = tracing::info_span!(target: "codex_core::session", "turn").entered();
            let _ours = tracing::info_span!(target: "atomic_app::pipeline", "step").entered();
        });
        provider.force_flush().expect("flush");
        let names: Vec<String> = memory
            .get_finished_spans()
            .expect("spans")
            .iter()
            .map(|s| s.name.to_string())
            .collect();
        assert_eq!(names, ["step"]);
    }
}
