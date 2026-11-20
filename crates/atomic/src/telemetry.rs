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
//! Names follow the GenAI semantic conventions where they fit, with atomic's
//! own under `atomic.*` (see [`attributes`]).

use opentelemetry::trace::{Status, TracerProvider as _};
use opentelemetry::{KeyValue, Value};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
use std::borrow::Cow;
use std::sync::OnceLock;
use std::time::Duration;
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
        gen_ai.operation.name = attributes::WORKFLOW_SPAN,
        atomic.workflow = %workflow,
        atomic.workflow.id = %run,
        gen_ai.conversation.id = %run,
    )
}

/// The span of one execution of atom step `step`, which runs `atom`.
pub fn step_span(atom: &str, step: &str, run: &str) -> tracing::Span {
    tracing::info_span!(
        "invoke_agent",
        otel.name = %format!("{} {atom}", attributes::AGENT_SPAN),
        gen_ai.operation.name = attributes::AGENT_SPAN,
        gen_ai.agent.name = %atom,
        atomic.step = %step,
        atomic.workflow.id = %run,
    )
}

/// The span of a gate step `gate` while this process waits at it.
pub fn gate_span(gate: &str, run: &str) -> tracing::Span {
    tracing::info_span!(
        "atomic.gate",
        otel.name = %format!("{} {gate}", attributes::GATE_SPAN),
        atomic.gate = %gate,
        atomic.step = %gate,
        atomic.workflow.id = %run,
    )
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

/// Whether the variables ask for spans to be exported.
fn configured() -> bool {
    let set = |v: &str| std::env::var(v).is_ok_and(|s| !s.is_empty());
    let disabled = std::env::var("OTEL_SDK_DISABLED").is_ok_and(|s| s.eq_ignore_ascii_case("true"));
    !disabled && (set("OTEL_EXPORTER_OTLP_ENDPOINT") || set("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"))
}

/// Install the exporter the standard variables configure, if they configure
/// one, as this process's tracing subscriber. Call it once, early; call
/// [`flush`] before the process exits.
pub fn init_from_env() -> anyhow::Result<()> {
    if !configured() {
        return Ok(());
    }
    use opentelemetry_otlp::{Protocol, WithExportConfig};
    let exporter = match protocol().as_str() {
        "grpc" => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .build()?,
        "http/json" => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpJson)
            .build()?,
        "http/protobuf" => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()?,
        other => anyhow::bail!(
            "OTEL_EXPORTER_OTLP_PROTOCOL is {other:?}; atomic exports grpc, http/protobuf or \
             http/json"
        ),
    };
    install(SdkTracerProvider::builder().with_batch_exporter(Redacting(exporter)))
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

/// Send the spans buffered so far and stop exporting. Call it before the
/// process exits; afterwards nothing is exported.
pub fn flush() {
    if let Some(provider) = PROVIDER.get() {
        let _ = provider.force_flush();
        let _ = provider.shutdown_with_timeout(Duration::from_secs(5));
    }
}

/// An exporter that redacts every span before `E` exports it.
#[derive(Debug)]
pub struct Redacting<E>(pub E);

fn redact_value(value: &mut Value) {
    match value {
        Value::String(s) => {
            if let Cow::Owned(r) = atomic_core::redact::text(s.as_str()) {
                *value = Value::String(r.into());
            }
        }
        Value::Array(opentelemetry::Array::String(items)) => {
            for item in items {
                if let Cow::Owned(r) = atomic_core::redact::text(item.as_str()) {
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

/// `span` with every string it carries redacted.
pub fn redact_span(span: &mut SpanData) {
    if let Cow::Owned(r) = atomic_core::redact::text(&span.name) {
        span.name = r.into();
    }
    redact_attributes(&mut span.attributes);
    for event in &mut span.events.events {
        if let Cow::Owned(r) = atomic_core::redact::text(&event.name) {
            event.name = r.into();
        }
        redact_attributes(&mut event.attributes);
    }
    if let Status::Error { description } = &mut span.status
        && let Cow::Owned(r) = atomic_core::redact::text(description)
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

    #[test]
    fn an_exported_span_carries_no_secret() {
        let memory = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(Redacting(memory.clone()))
            .build();
        let secret = concat!("sk-ant-", "api03-SEEDEDsecretVALUE0123456789abcdef");
        tracing::subscriber::with_default(subscriber(&provider), || {
            let span = tracing::info_span!(
                target: "atomic_app::telemetry",
                "chat",
                otel.name = "chat synthetic",
                error.message = format!("401: {{\"error\": \"bad key {secret}\"}}"),
            );
            let _entered = span.enter();
            tracing::info!(target: "atomic_app::telemetry", "the provider said {secret}");
        });
        provider.force_flush().expect("flush");
        let spans = memory.get_finished_spans().expect("spans");
        assert_eq!(spans.len(), 1, "{spans:?}");
        let shown = format!("{spans:?}");
        assert!(!shown.contains(secret), "{shown}");
        assert!(shown.contains("[REDACTED]"), "{shown}");
        assert!(shown.contains("chat synthetic"), "{shown}");
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
