// SPDX-License-Identifier: Apache-2.0

//! The built-in HTTP executor for OpenAPI toolsets: atomic makes the call,
//! never the model or an atom's code.
//!
//! A call names an operation and gives its arguments as one JSON object (see
//! [`atomic_schema::openapi::Operation`]). Before any request:
//!
//! - the arguments are validated against the operation's full input schema,
//!   whatever subset of it a model was shown;
//! - the URL is built from the toolset's base URL and the operation's path,
//!   and its host must be one the toolset allows, so a typo or a redirect
//!   cannot reach another host.
//!
//! Safe methods (GET, HEAD, OPTIONS) are retried after a connection failure,
//! a 429 or a 5xx; others are sent once. Responses are bounded.

use atomic_schema::openapi::Operation;
use reqwest::Url;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::time::Duration;

/// The largest response body read, in bytes.
pub const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

/// A toolset's operations and where they may be called.
#[derive(Clone, Debug)]
pub struct HttpToolset {
    /// The base URL operation paths are joined to.
    pub base_url: Url,
    /// Hosts (with a port when it is not the scheme's default) calls may go
    /// to; the base URL's host must be one of them.
    pub allow: Vec<String>,
    /// Operations by name.
    pub operations: BTreeMap<String, Operation>,
    /// Headers every request carries, such as a scoped credential the host
    /// resolved for this call. They are never logged.
    pub headers: Vec<(String, String)>,
    /// Attempts for a safe method, at least one.
    pub attempts: u32,
    /// The time limit for each attempt.
    pub timeout: Duration,
}

/// The request a call sends, built and checked.
#[derive(Clone, Debug)]
pub struct Request {
    /// The HTTP method.
    pub method: reqwest::Method,
    /// The URL, on an allowed host.
    pub url: Url,
    /// Headers, the toolset's own included.
    pub headers: Vec<(String, String)>,
    /// The JSON body, if any.
    pub body: Option<Value>,
}

/// Why a call produced no result.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// No operation by that name.
    #[error("toolset has no operation `{0}`")]
    Unknown(String),
    /// The arguments break the operation's input schema; nothing was sent.
    #[error("arguments for `{operation}` do not fit its schema: {errors}")]
    Arguments {
        /// The operation.
        operation: String,
        /// Each violation, with its path.
        errors: String,
    },
    /// The request would go to a host the toolset does not allow; nothing
    /// was sent.
    #[error("refused before any request: {0}")]
    Refused(String),
    /// The server answered with a status that is not success.
    #[error("`{operation}` returned {status}: {body}")]
    Status {
        /// The operation.
        operation: String,
        /// The HTTP status.
        status: u16,
        /// The start of the response body.
        body: String,
    },
    /// The request failed, or the response was too large or not JSON.
    #[error("`{operation}`: {message}")]
    Transport {
        /// The operation.
        operation: String,
        /// What failed.
        message: String,
    },
}

fn host_of(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

/// `value` as one path segment: everything but unreserved characters is
/// percent-encoded, so a value cannot add segments or a query.
fn segment(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

impl HttpToolset {
    /// Checks the toolset itself: an http(s) base URL whose host is allowed.
    pub fn check(&self) -> Result<(), CallError> {
        if !matches!(self.base_url.scheme(), "http" | "https") {
            return Err(CallError::Refused(format!(
                "base URL {} is not http or https",
                self.base_url
            )));
        }
        let host = host_of(&self.base_url).unwrap_or_default();
        if !self.allow.contains(&host) {
            return Err(CallError::Refused(format!(
                "base URL host {host} is not in the toolset's allowlist"
            )));
        }
        Ok(())
    }

    /// The request a call would send, checked: its method, URL, headers and
    /// body. Nothing is sent.
    pub fn request(&self, name: &str, args: &Value) -> Result<Request, CallError> {
        self.check()?;
        let operation = self
            .operations
            .get(name)
            .ok_or_else(|| CallError::Unknown(name.to_owned()))?;
        let validator = atomic_schema::registry::Registry::new()
            .validator_for(&operation.input)
            .map_err(|e| CallError::Arguments {
                operation: name.to_owned(),
                errors: e.to_string(),
            })?;
        let errors: Vec<String> = validator
            .iter_errors(args)
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect();
        if !errors.is_empty() {
            return Err(CallError::Arguments {
                operation: name.to_owned(),
                errors: errors.join("; "),
            });
        }
        let empty = Map::new();
        let fields = args.as_object().unwrap_or(&empty);
        let mut path = operation.path.clone();
        let mut query = Vec::new();
        let mut headers = self.headers.clone();
        let mut cookies = Vec::new();
        let mut body = None;
        for (field, value) in fields {
            match operation.locations.get(field).and_then(Value::as_str) {
                Some("path") => {
                    let value = text(value);
                    // `.` and `..` are dot segments: URL parsing removes
                    // them (and `..` the segment before it), and so would
                    // their encoded spelling `%2E`, so they are refused
                    // rather than encoded. An empty value would leave an
                    // empty segment.
                    if matches!(value.as_str(), "" | "." | "..") {
                        return Err(CallError::Arguments {
                            operation: name.to_owned(),
                            errors: format!(
                                "/{field}: {value:?} is not a path segment; it would change the operation's path"
                            ),
                        });
                    }
                    path = path.replace(&format!("{{{field}}}"), &segment(&value));
                }
                Some("query") => query.push((field.clone(), text(value))),
                Some("header") => headers.push((field.clone(), text(value))),
                Some("cookie") => cookies.push(format!("{field}={}", text(value))),
                Some("body") => body = Some(value.clone()),
                _ => {}
            }
        }
        if path.contains('{') {
            return Err(CallError::Arguments {
                operation: name.to_owned(),
                errors: format!("path {path} still has an unfilled parameter"),
            });
        }
        if !cookies.is_empty() {
            headers.push(("Cookie".into(), cookies.join("; ")));
        }
        let mut url = self
            .base_url
            .join(path.trim_start_matches('/'))
            .map_err(|e| CallError::Refused(format!("cannot build the URL: {e}")))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        // The joined URL can name another host (`//elsewhere/...` in a path).
        let host = host_of(&url).unwrap_or_default();
        if !self.allow.contains(&host) || url.scheme() != self.base_url.scheme() {
            return Err(CallError::Refused(format!(
                "{url} is outside the toolset's allowlist"
            )));
        }
        let method = reqwest::Method::from_bytes(operation.method.to_uppercase().as_bytes())
            .map_err(|e| CallError::Refused(e.to_string()))?;
        Ok(Request {
            method,
            url,
            headers,
            body,
        })
    }

    /// Calls `name` with `args`, and returns its JSON response (or `null`
    /// for an empty one).
    pub async fn call(&self, name: &str, args: &Value) -> Result<Value, CallError> {
        let Request {
            method,
            url,
            headers,
            body,
        } = self.request(name, args)?;
        let transport = |message: String| CallError::Transport {
            operation: name.to_owned(),
            message,
        };
        // Redirects are not followed: the next host would not be checked.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(self.timeout)
            .build()
            .map_err(|e| transport(e.to_string()))?;
        let safe = matches!(
            method,
            reqwest::Method::GET | reqwest::Method::HEAD | reqwest::Method::OPTIONS
        );
        let attempts = if safe { self.attempts.max(1) } else { 1 };
        let mut last = None;
        for attempt in 0..attempts {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100 * 2u64.pow(attempt.min(6)))).await;
            }
            let mut request = client.request(method.clone(), url.clone());
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(e) => {
                    last = Some(transport(e.to_string()));
                    continue;
                }
            };
            let status = response.status();
            let bytes = read_bounded(response).await.map_err(&transport)?;
            if status.is_success() {
                if bytes.is_empty() {
                    return Ok(Value::Null);
                }
                return serde_json::from_slice(&bytes)
                    .map_err(|e| transport(format!("the response is not JSON: {e}")));
            }
            let error = CallError::Status {
                operation: name.to_owned(),
                status: status.as_u16(),
                body: String::from_utf8_lossy(&bytes[..bytes.len().min(2000)]).into_owned(),
            };
            if !(status.as_u16() == 429 || status.is_server_error()) {
                return Err(error);
            }
            last = Some(error);
        }
        Err(last.unwrap_or_else(|| transport("no attempt was made".into())))
    }
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > RESPONSE_LIMIT {
            return Err(format!(
                "the response is larger than {RESPONSE_LIMIT} bytes"
            ));
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;
