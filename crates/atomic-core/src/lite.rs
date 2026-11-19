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
    /// The usage the provider reported, or `None` if it reported none that
    /// can be read: such a call is settled as unknown, never as free.
    usage: Option<Usage>,
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
            runtime.wire,
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
            })
            .await
            .map_err(meter_error)?;
        index += 1;
        let answer = call(
            &client,
            runtime,
            key.as_deref(),
            &model,
            first.developer_instructions.as_deref(),
            &exchange,
            schema.as_ref(),
            &limit,
        )
        .await;
        let outcome = match &answer {
            Ok(Answer {
                usage: Some(usage), ..
            }) => Outcome::Reported(*usage),
            // An answer with no usage spent something unknown: it stays
            // charged at its reservation.
            Ok(Answer { usage: None, .. }) => Outcome::Unknown,
            // A connection that was never made sent nothing.
            Err(e) if never_sent(e) => Outcome::NotDispatched,
            Err(_) => Outcome::Unknown,
        };
        runtime
            .meter
            .settle(ticket, outcome)
            .await
            .map_err(meter_error)?;
        let answer = answer?;
        report.thread_id = answer.id;
        report.turns += 1;
        if let Some(usage) = answer.usage {
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

/// The body of one call on `wire`, with `output_limit` as its output limit
/// if one is given.
fn request_body(
    wire: Wire,
    model: &str,
    developer: Option<&str>,
    exchange: &[(Role, String)],
    schema: Option<&Value>,
    output_limit: Option<u64>,
) -> Value {
    let mut body = match wire {
        Wire::ChatCompletions => chat_request(model, developer, exchange, schema),
        Wire::Responses => responses_request(model, developer, exchange, schema),
    };
    if let Some(limit) = output_limit {
        // `max_tokens` is the name every OpenAI-compatible server reads on
        // Chat Completions; the Responses API names it `max_output_tokens`.
        let field = match wire {
            Wire::ChatCompletions => "max_tokens",
            Wire::Responses => "max_output_tokens",
        };
        body[field] = json!(limit);
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
) -> anyhow::Result<Answer> {
    let base = runtime.endpoint.base_url.trim_end_matches('/');
    let url = match runtime.wire {
        Wire::ChatCompletions => format!("{base}/chat/completions"),
        Wire::Responses => format!("{base}/responses"),
    };
    let body = request_body(
        runtime.wire,
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
    let (status, text) = match exchanged {
        Ok(done) => done?,
        Err(_) => bail!(
            "POST {url}: cut off after {} ms, the call's reserved time",
            time.as_millis()
        ),
    };
    if !status.is_success() {
        bail!("POST {url}: {status}: {}", excerpt(&text));
    }
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("POST {url}: the response is not JSON: {}", excerpt(&text)))?;
    match runtime.wire {
        Wire::ChatCompletions => chat_answer(&value),
        Wire::Responses => responses_answer(&value),
    }
    .with_context(|| format!("POST {url}"))
}

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
        usage: usage_of(value, "prompt_tokens", "completion_tokens"),
    })
}

fn responses_answer(value: &Value) -> anyhow::Result<Answer> {
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
        usage: usage_of(value, "input_tokens", "output_tokens"),
    })
}

/// The start of a response body, for an error message.
fn excerpt(text: &str) -> String {
    const MAX: usize = 500;
    match text.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests;
