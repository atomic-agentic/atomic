// SPDX-License-Identifier: Apache-2.0

//! Lite atoms: one schema-constrained model call, with no thread, sandbox or
//! tools.
//!
//! A lite atom keeps the contract every atom has (the lowered output schema
//! constrains the answer, the answer is validated against the full contract,
//! and one repair turn follows a misfit), but it runs as a plain HTTP call
//! through atomic's own client rather than as a Codex thread. That client
//! speaks one of two OpenAI-compatible wires, chosen per endpoint:
//!
//! - **Chat Completions**, with `response_format: json_schema`. Local servers
//!   built on llama.cpp, vLLM or Ollama enforce the schema in decoding on this
//!   wire, while some of them accept the Responses API's `text.format` and
//!   ignore it.
//! - **The Responses API**, with `text.format: json_schema`, for OpenAI's own
//!   API.
//!
//! The repair turn is a second call that carries the first exchange plus the
//! correction, so a lite atom makes one call when its answer fits and two
//! when it needs the repair.

use crate::codex::{ModelEndpoint, ThreadReport, TurnSpec};
use crate::typed::AtomConfig;
use crate::usage::{Call, Limit, Outcome, Usage, meter_error};
use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use std::time::Duration;

/// How long one call may take before it fails, unless the runtime says
/// otherwise. Reasoning models on local hardware can think for minutes.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// The name a structured-output schema is sent under.
const SCHEMA_NAME: &str = "output";

/// The API a lite atom's call goes through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Wire {
    /// `POST {base}/chat/completions` with `response_format`.
    #[default]
    ChatCompletions,
    /// `POST {base}/responses` with `text.format`.
    Responses,
}

impl Wire {
    /// The wire for an endpoint that does not name one: the Responses API on
    /// OpenAI's own API, Chat Completions everywhere else, since that is the
    /// wire on which OpenAI-compatible servers enforce a schema.
    pub fn for_base_url(base_url: &str) -> Self {
        if is_openai_api(base_url) {
            Self::Responses
        } else {
            Self::ChatCompletions
        }
    }
}

/// Whether `base_url` is OpenAI's own API, the one endpoint known to enforce
/// a Responses API structured-output schema in decoding. Other servers may
/// accept the schema on that API and ignore it.
pub fn is_openai_api(base_url: &str) -> bool {
    let host = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest)
        .split(['/', ':'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    host == "api.openai.com"
}

/// Where and with what model lite atoms run.
#[derive(Clone, Debug)]
pub struct LiteRuntime {
    /// The model endpoint.
    pub endpoint: ModelEndpoint,
    /// The API to call it through.
    pub wire: Wire,
    /// The project's default run settings. Only the model applies to a lite
    /// atom: it has no sandbox, network or tools.
    pub defaults: AtomConfig,
    /// How long one call may take.
    pub timeout: Duration,
    /// The artifact store outputs' references are checked against.
    pub artifacts: crate::runtime::Artifacts,
    /// What admits and settles each call (see [`crate::usage`]).
    pub meter: crate::usage::Metering,
    /// The request field the endpoint reads a call's output limit from, or
    /// `None` to tell from the endpoint's URL ([`LimitField::for_endpoint`]).
    /// Set it for a gateway or a server the URL does not identify.
    pub limit_field: Option<LimitField>,
}

impl LiteRuntime {
    /// A runtime on `endpoint`, with the wire [`Wire::for_base_url`] picks,
    /// the default timeout and no artifact store.
    pub fn new(endpoint: ModelEndpoint, defaults: AtomConfig) -> Self {
        Self {
            wire: Wire::for_base_url(&endpoint.base_url),
            endpoint,
            defaults,
            timeout: DEFAULT_TIMEOUT,
            artifacts: crate::runtime::Artifacts::none(),
            meter: crate::usage::Metering::none(),
            limit_field: None,
        }
    }
}

/// One message of the exchange so far.
#[derive(Clone, Copy)]
enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// What one call returned.
struct Answer {
    text: String,
    id: String,
}

/// How one call ended: its answer or why there is none, and the usage the
/// provider reported, read before the answer is judged, so a refusal or a
/// cut-off answer still settles what it spent. `None` when nothing that can
/// be read was reported: such a call is settled as unknown, never as free.
struct Called {
    answer: anyhow::Result<Answer>,
    usage: Option<Usage>,
}

/// The request field a call's output limit is sent in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitField {
    /// `max_tokens`: Chat Completions on llama.cpp, vLLM, Ollama and other
    /// OpenAI-compatible servers.
    MaxTokens,
    /// `max_completion_tokens`: Chat Completions on OpenAI's and Azure
    /// OpenAI's own APIs, where reasoning models refuse `max_tokens`.
    MaxCompletionTokens,
    /// `max_output_tokens`: the Responses API.
    MaxOutputTokens,
}

impl LimitField {
    /// The field for `wire` at `base_url`.
    pub fn for_endpoint(wire: Wire, base_url: &str) -> Self {
        match wire {
            Wire::Responses => Self::MaxOutputTokens,
            Wire::ChatCompletions if reads_max_completion_tokens(base_url) => {
                Self::MaxCompletionTokens
            }
            Wire::ChatCompletions => Self::MaxTokens,
        }
    }

    /// Its name in the request body.
    pub fn name(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
            Self::MaxOutputTokens => "max_output_tokens",
        }
    }
}

/// Whether `base_url` is OpenAI's own API, in any region, or an Azure
/// OpenAI or Azure AI resource, in any cloud: the endpoints whose Chat
/// Completions read `max_completion_tokens`, and whose reasoning models
/// refuse `max_tokens`.
fn reads_max_completion_tokens(base_url: &str) -> bool {
    let host = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest)
        .split(['/', ':'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    host == "api.openai.com"
        || host.ends_with(".api.openai.com")
        || [
            ".openai.azure.com",
            ".openai.azure.us",
            ".openai.azure.cn",
            ".cognitiveservices.azure.com",
            ".services.ai.azure.com",
        ]
        .iter()
        .any(|suffix| host.ends_with(suffix))
}

/// Call the model with `first`, then, after each answer, with the follow-up
/// turn `next` returns, carrying the exchange so far, until it returns `None`.
/// Returns the last answer and a report whose `thread_id` is the provider's id
/// for the last response. As with [`crate::codex::converse`], follow-up turns
/// keep the first turn's instructions and model.
pub async fn converse<F>(
    runtime: &LiteRuntime,
    first: &TurnSpec,
    mut next: F,
) -> anyhow::Result<(String, ThreadReport)>
where
    F: FnMut(&str) -> Option<TurnSpec>,
{
    converse_async(runtime, first, |answer| std::future::ready(next(&answer))).await
}

/// As [`converse`], with an asynchronous check between turns, such as an
/// atom's validators running in the compiler worker.
pub async fn converse_async<F, Fut>(
    runtime: &LiteRuntime,
    first: &TurnSpec,
    mut next: F,
) -> anyhow::Result<(String, ThreadReport)>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Option<TurnSpec>>,
{
    if runtime.endpoint.sign_in.is_some() {
        bail!(
            "a lite atom calls a model API itself, and a ChatGPT sign-in serves Codex atoms \
             only: set ATOMIC_MODEL_BASE_URL and ATOMIC_MODEL to an OpenAI-compatible endpoint \
             for it, and ATOMIC_MODEL_API_KEY_ENV to the variable holding its key if it needs one"
        );
    }
    let model = first
        .model
        .clone()
        .unwrap_or_else(|| runtime.endpoint.model.clone());
    let key = match &runtime.endpoint.api_key_env {
        Some(var) => Some(
            std::env::var(var)
                .with_context(|| format!("read the API key from the environment variable {var}"))?,
        ),
        None => None,
    };
    let client = reqwest::Client::builder()
        .timeout(runtime.timeout)
        .build()
        .context("build the HTTP client")?;

    let mut exchange = vec![(Role::User, first.prompt.clone())];
    let mut schema = first.output_schema.clone();
    let mut report = ThreadReport {
        thread_id: String::new(),
        turns: 0,
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
    };
    let mut index = 0;
    loop {
        // The request without its output limit, for an upper bound on its
        // input tokens; the limit adds a few bytes the slack covers.
        let unlimited = request_body(
            runtime,
            &model,
            first.developer_instructions.as_deref(),
            &exchange,
            schema.as_ref(),
            None,
        );
        let input_bound = serde_json::to_vec(&unlimited)
            .map(|b| b.len() as u64 + 64)
            .ok();
        // Admitted before it is dispatched, settled once after.
        let (ticket, limit) = runtime
            .meter
            .admit(Call {
                index,
                model: model.clone(),
                input_bound,
                turn: false,
            })
            .await
            .map_err(meter_error)?;
        index += 1;
        // Opened once the call is admitted, so a refused call exports none.
        let span = crate::spans::hold(crate::usage::call_span(
            &model,
            crate::usage::provider_name(&runtime.endpoint.base_url),
        ));
        let called = span
            .run(call(
                &client,
                runtime,
                key.as_deref(),
                &model,
                first.developer_instructions.as_deref(),
                &exchange,
                schema.as_ref(),
                &limit,
            ))
            .await;
        if let (Some(usage), Some(span)) = (&called.usage, span.span()) {
            crate::usage::record_usage(&span, usage);
        }
        if let (Err(e), Some(span)) = (&called.answer, span.span()) {
            crate::usage::record_error(&span, e);
        }
        span.end();
        let outcome = match (&called.answer, called.usage) {
            // What the provider reported, whatever it answered.
            (_, Some(usage)) => Outcome::Reported(usage),
            // A connection that was never made sent nothing.
            (Err(e), None) if never_sent(e) => Outcome::NotDispatched,
            // Anything else spent something unknown: it stays charged at its
            // reservation.
            (_, None) => Outcome::Unknown,
        };
        runtime
            .meter
            .settle(ticket, outcome)
            .await
            .map_err(meter_error)?;
        let answer = called.answer?;
        report.thread_id = answer.id;
        report.turns += 1;
        if let Some(usage) = called.usage {
            report.input_tokens += usage.input_tokens;
            report.output_tokens += usage.output_tokens;
            report.total_tokens += usage.total();
        }
        match next(answer.text.clone()).await {
            Some(follow_up) => {
                exchange.push((Role::Assistant, answer.text));
                exchange.push((Role::User, follow_up.prompt));
                schema = follow_up.output_schema;
            }
            None => return Ok((answer.text, report)),
        }
    }
}

/// Whether `e` shows the request never left this process: the connection
/// to the provider was never made.
fn never_sent(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<reqwest::Error>())
        .any(reqwest::Error::is_connect)
}

/// The body of one call on `runtime`'s wire, with `output_limit` as its
/// output limit, in the field its endpoint reads, if one is given.
fn request_body(
    runtime: &LiteRuntime,
    model: &str,
    developer: Option<&str>,
    exchange: &[(Role, String)],
    schema: Option<&Value>,
    output_limit: Option<u64>,
) -> Value {
    let mut body = match runtime.wire {
        Wire::ChatCompletions => chat_request(model, developer, exchange, schema),
        Wire::Responses => responses_request(model, developer, exchange, schema),
    };
    if let Some(limit) = output_limit {
        let field = runtime
            .limit_field
            .unwrap_or_else(|| LimitField::for_endpoint(runtime.wire, &runtime.endpoint.base_url));
        body[field.name()] = json!(limit);
    }
    body
}

/// Make one call on the runtime's wire, held to `limit`: its output limit is
/// sent with it, and it is cut off when its time runs out.
#[allow(clippy::too_many_arguments)]
async fn call(
    client: &reqwest::Client,
    runtime: &LiteRuntime,
    key: Option<&str>,
    model: &str,
    developer: Option<&str>,
    exchange: &[(Role, String)],
    schema: Option<&Value>,
    limit: &Limit,
) -> Called {
    let base = runtime.endpoint.base_url.trim_end_matches('/');
    let url = match runtime.wire {
        Wire::ChatCompletions => format!("{base}/chat/completions"),
        Wire::Responses => format!("{base}/responses"),
    };
    let body = request_body(
        runtime,
        model,
        developer,
        exchange,
        schema,
        limit.output_tokens,
    );
    let time = limit.time_within(runtime.timeout);
    let mut request = client.post(&url).json(&body);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    // The whole exchange, the body's read included, is held to the time.
    let exchanged = tokio::time::timeout(time, async {
        let response = request
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .with_context(|| format!("read the response from {url}"))?;
        anyhow::Ok((status, text))
    })
    .await;
    let failed = |answer: anyhow::Error| Called {
        answer: Err(answer),
        usage: None,
    };
    let (status, text) = match exchanged {
        Ok(Ok(done)) => done,
        Ok(Err(e)) => return failed(e),
        Err(_) => {
            return failed(anyhow!(
                "POST {url}: cut off after {} ms, the call's reserved time",
                time.as_millis()
            ));
        }
    };
    let value: Option<Value> = serde_json::from_str(&text).ok();
    // The usage first: whatever the answer is, what it spent is settled.
    let usage = value.as_ref().and_then(|v| match runtime.wire {
        Wire::ChatCompletions => usage_of(v, "prompt_tokens", "completion_tokens"),
        Wire::Responses => usage_of(v, "input_tokens", "output_tokens"),
    });
    let answer = if !status.is_success() {
        Err(ProviderError {
            url: url.clone(),
            status: status.to_string(),
            code: status.as_u16(),
            kind: value.as_ref().and_then(error_kind),
            excerpt: excerpt(&text),
        }
        .into())
    } else {
        match &value {
            None => Err(anyhow!(
                "POST {url}: the response is not JSON: {}",
                excerpt(&text)
            )),
            Some(value) => match runtime.wire {
                Wire::ChatCompletions => chat_answer(value),
                Wire::Responses => responses_answer(value),
            }
            .with_context(|| format!("POST {url}")),
        }
    };
    Called { answer, usage }
}

/// A provider's answer that is not a success: its status, the start of its
/// body, and the type of error the body names, if it names one.
#[derive(Debug, thiserror::Error)]
#[error("POST {url}: {status}: {excerpt}")]
pub(crate) struct ProviderError {
    url: String,
    /// The status, as `401 Unauthorized`.
    status: String,
    /// The status's code.
    pub(crate) code: u16,
    /// The error's type, from the body (see [`error_kind`]).
    kind: Option<String>,
    /// The start of the body, redacted.
    excerpt: String,
}

impl ProviderError {
    /// What a trace says of it: the status and the error's type, none of
    /// the provider's own text.
    pub(crate) fn summary(&self) -> String {
        match &self.kind {
            Some(kind) => format!("{}: {kind}", self.status),
            None => self.status.clone(),
        }
    }
}

/// The type of error an OpenAI-style body names, `error.type` or else
/// `error.code`, when it reads as one: see [`is_error_kind`]. Anything
/// else is the provider's own text.
fn error_kind(body: &Value) -> Option<String> {
    let error = body.get("error")?;
    ["type", "code"].iter().find_map(|field| {
        let kind = error.get(*field)?.as_str()?;
        is_error_kind(kind).then(|| kind.to_owned())
    })
}

/// Whether `kind` reads as an error's type, such as `invalid_request_error`,
/// `rate_limit_exceeded` or `overloaded`, and not as a key: a lower-case
/// letter first, at most 40 lower-case letters, digits and `_`, an
/// underscore if it has digits, and at most three digits. A key's mixed
/// case, dashed prefix or run of digits fails it; one of letters alone
/// passes any test of shape, and the redaction of the span's summary is
/// what catches it.
fn is_error_kind(kind: &str) -> bool {
    let b = kind.as_bytes();
    let lower = |c: &u8| c.is_ascii_lowercase();
    let digits = b.iter().filter(|c| c.is_ascii_digit()).count();
    b.first().is_some_and(lower)
        && b.len() <= 40
        && b.iter()
            .all(|c| lower(c) || c.is_ascii_digit() || *c == b'_')
        && (b.contains(&b'_') || digits == 0)
        && digits <= 3
}

/// An answer the provider cut off at its output limit: not an answer that
/// misfits its schema, so it gets no repair turn.
#[derive(Debug, thiserror::Error)]
#[error(
    "the answer was cut off at the call's output limit ({0}); raise the call bound's \
     output tokens, or ask for less"
)]
pub struct Truncated(String);

fn chat_request(
    model: &str,
    developer: Option<&str>,
    exchange: &[(Role, String)],
    schema: Option<&Value>,
) -> Value {
    // `system`, not `developer`: every OpenAI-compatible server knows it.
    let messages: Vec<Value> = developer
        .map(|d| json!({"role": "system", "content": d}))
        .into_iter()
        .chain(
            exchange
                .iter()
                .map(|(role, text)| json!({"role": role.as_str(), "content": text})),
        )
        .collect();
    let mut body = json!({"model": model, "messages": messages, "stream": false});
    if let Some(schema) = schema {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {"name": SCHEMA_NAME, "strict": true, "schema": schema}
        });
    }
    body
}

fn responses_request(
    model: &str,
    developer: Option<&str>,
    exchange: &[(Role, String)],
    schema: Option<&Value>,
) -> Value {
    let input: Vec<Value> = exchange
        .iter()
        .map(|(role, text)| json!({"role": role.as_str(), "content": text}))
        .collect();
    let mut body = json!({"model": model, "input": input, "stream": false, "store": false});
    if let Some(developer) = developer {
        body["instructions"] = json!(developer);
    }
    if let Some(schema) = schema {
        body["text"] = json!({
            "format": {"type": "json_schema", "name": SCHEMA_NAME, "strict": true, "schema": schema}
        });
    }
    body
}

/// The usage in a response's `usage` object, read through its `input` and
/// `output` fields; `None` when the object or either field is missing or
/// not a count.
fn usage_of(value: &Value, input: &str, output: &str) -> Option<Usage> {
    let usage = value.get("usage")?;
    let count = |field: &str| usage.get(field).and_then(Value::as_i64).filter(|n| *n >= 0);
    Some(Usage {
        input_tokens: count(input)?,
        output_tokens: count(output)?,
    })
}

fn chat_answer(value: &Value) -> anyhow::Result<Answer> {
    if let Some(reason) = value
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .filter(|r| *r == "length")
    {
        return Err(Truncated(format!("finish_reason {reason}")).into());
    }
    let message = value
        .pointer("/choices/0/message")
        .ok_or_else(|| anyhow!("the response has no choices[0].message"))?;
    if let Some(refusal) = message.get("refusal").and_then(Value::as_str) {
        bail!("the model refused: {refusal}");
    }
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("choices[0].message has no text content"))?;
    Ok(Answer {
        text: text.to_owned(),
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

fn responses_answer(value: &Value) -> anyhow::Result<Answer> {
    if value.get("status").and_then(Value::as_str) == Some("incomplete") {
        let why = value
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        // Only an answer stopped at its output limit is truncated; any
        // other stop, such as a content filter, is the provider's own.
        if why == "max_output_tokens" {
            return Err(Truncated(format!("status incomplete: {why}")).into());
        }
        bail!("the provider stopped the answer before it was complete: {why}");
    }
    let mut text = String::new();
    let mut messages = 0;
    for item in value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|i| i.get("type").and_then(Value::as_str) == Some("message"))
    {
        messages += 1;
        for part in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match part.get("type").and_then(Value::as_str) {
                Some("output_text") => {
                    text.push_str(part.get("text").and_then(Value::as_str).unwrap_or_default());
                }
                Some("refusal") => bail!(
                    "the model refused: {}",
                    part.get("refusal")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                ),
                _ => {}
            }
        }
    }
    if messages == 0 {
        bail!(
            "the response has no message (status {})",
            value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
    }
    Ok(Answer {
        text,
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

/// The start of a response body, for an error message.
/// The start of a provider's response, for an error message, with any
/// secret it echoes redacted first, so the cut cannot hide one.
fn excerpt(text: &str) -> String {
    // Bounded, redacted (as JSON when it is JSON), then cut.
    crate::redact::excerpt(text, 500)
}

#[cfg(test)]
mod tests;
