// SPDX-License-Identifier: Apache-2.0

//! The built-in HTTP executor for OpenAPI toolsets: atomic makes the call,
//! never the model or an atom's code.
//!
//! A call names an operation and gives its arguments as one JSON object (see
//! [`atomic_schema::openapi::Operation`]). Before any request:
//!
//! - the arguments are validated against the operation's full input schema,
//!   whatever subset of it a model was shown;
//! - the URL is the toolset's base URL (its whole path) followed by the
//!   operation's path, and its host must be one the toolset allows, so a
//!   typo or a redirect cannot reach another host;
//! - parameters are serialized by their OpenAPI style, and no argument can
//!   set a header the toolset sets itself (its credential among them) or a
//!   header that frames the request.
//!
//! Safe methods (GET, HEAD, OPTIONS) are retried after a connection failure,
//! a 429 or a 5xx; others are sent once. Responses are bounded. A success
//! is checked against the response the operation declares for its status
//! (its own code, else its range, else `default`): its schema, if it has
//! one, and no body at all only where that response declares no `content`
//! at all, as a `204` does. A response that declares `content` atomic has
//! no JSON schema for — a `*/*` or `text/plain` media type, or a JSON media
//! type with no `schema` — declares a body whose shape nothing describes, so
//! it is passed through unchecked. Because `default` is selected by every
//! success no code or range declares, a contentless `default` refuses an
//! undeclared success that comes back with a body; that is the restrictive
//! reading, and it is deliberate. A success with a status the operation
//! declares no response for, when it declares any, is refused; it was
//! answered, so it is not sent again.

use atomic_schema::openapi::{Operation, ResponseBody};
use reqwest::Url;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::time::Duration;

/// The largest response body read, in bytes.
pub const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

/// A toolset's operations and where they may be called.
#[derive(Clone)]
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
#[derive(Clone)]
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
    /// The response breaks the contract the operation declares for its
    /// status: it does not fit that response's schema, or it carries a body
    /// where that response declares no `content` at all. A response that
    /// declares content with no JSON schema to check against is not a
    /// violation either way; its body is passed through unchecked.
    #[error(
        "`{operation}` returned {status}, which does not fit its response `{response}`: {errors}"
    )]
    Response {
        /// The operation.
        operation: String,
        /// The HTTP status.
        status: u16,
        /// The response it was checked against, as the spec declares it:
        /// `200`, `2XX` or `default`.
        response: String,
        /// Each violation, with its path.
        errors: String,
    },
    /// The server answered a success with a status the operation declares
    /// no response for. The request was made and answered, so it is not
    /// sent again.
    #[error("`{operation}` returned {status}, a status its spec declares no response for")]
    Undeclared {
        /// The operation.
        operation: String,
        /// The HTTP status.
        status: u16,
    },
}

/// Header names, written in lowercase, as `***` for each value: headers can
/// carry credentials, which are never logged.
fn redacted(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .map(|(name, _)| format!("{}: ***", name.to_ascii_lowercase()))
        .collect()
}

/// Whether a body carries nothing where the selected response declares no
/// content: no bytes, or JSON's own whitespace only. A reply told to carry
/// nothing may still frame an empty one, and whitespace carries nothing, so
/// both read as no result. Form feed is not JSON whitespace, so a body of it
/// is not blank here, and stays the parse failure it has always been.
fn is_blank_body(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
}

impl std::fmt::Debug for HttpToolset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpToolset")
            .field("base_url", &self.base_url.as_str())
            .field("allow", &self.allow)
            .field("operations", &self.operations.keys().collect::<Vec<_>>())
            .field("headers", &redacted(&self.headers))
            .field("attempts", &self.attempts)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("url", &self.url.as_str())
            .field("headers", &redacted(&self.headers))
            .field("body", &self.body.is_some())
            .finish()
    }
}

/// Headers an argument may never set: they frame the request, or atomic
/// sets them.
const RESERVED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "cookie",
    "accept",
    "content-type",
    "authorization",
];

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

/// A cookie value: bytes outside RFC 6265's cookie-octet, and `%`, are
/// percent-encoded, so a value cannot add a cookie or attribute.
fn cookie_value(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        let octet = matches!(byte, 0x21 | 0x23..=0x2B | 0x2D..=0x3A | 0x3C..=0x5B | 0x5D..=0x7E);
        if octet && byte != b'%' {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A parameter's value as a list of scalar texts, or as key and value
/// pairs for an object; nested values cannot be serialized.
enum Shape {
    Scalar(String),
    List(Vec<String>),
    Pairs(Vec<(String, String)>),
}

fn shape(value: &Value) -> Result<Shape, String> {
    let scalar = |value: &Value| match value {
        Value::Array(_) | Value::Object(_) => {
            Err("a nested array or object cannot be sent as a parameter".to_owned())
        }
        other => Ok(text(other)),
    };
    Ok(match value {
        Value::Array(items) => Shape::List(items.iter().map(scalar).collect::<Result<_, _>>()?),
        Value::Object(map) => Shape::Pairs(
            map.iter()
                .map(|(k, v)| Ok((k.clone(), scalar(v)?)))
                .collect::<Result<_, String>>()?,
        ),
        other => Shape::Scalar(scalar(other)?),
    })
}

/// A `simple`-style value (path and header parameters): lists and objects
/// joined with commas, `key=value` for an exploded object. `encode` is
/// applied to each part, never to the separators.
fn simple(shape: Shape, explode: bool, encode: impl Fn(&str) -> String) -> String {
    match shape {
        Shape::Scalar(text) => encode(&text),
        Shape::List(items) => items
            .iter()
            .map(|i| encode(i))
            .collect::<Vec<_>>()
            .join(","),
        Shape::Pairs(pairs) => pairs
            .iter()
            .map(|(k, v)| {
                if explode {
                    format!("{}={}", encode(k), encode(v))
                } else {
                    format!("{},{}", encode(k), encode(v))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// Query pairs for one parameter, by its style.
fn query_pairs(
    field: &str,
    shape: Shape,
    style: &str,
    explode: bool,
) -> Result<Vec<(String, String)>, String> {
    Ok(match (style, shape) {
        (_, Shape::Scalar(text)) => vec![(field.to_owned(), text)],
        ("form" | "spaceDelimited" | "pipeDelimited", Shape::List(items)) if explode => {
            items.into_iter().map(|i| (field.to_owned(), i)).collect()
        }
        ("form" | "spaceDelimited" | "pipeDelimited", Shape::List(items)) => {
            let separator = match style {
                "spaceDelimited" => " ",
                "pipeDelimited" => "|",
                _ => ",",
            };
            vec![(field.to_owned(), items.join(separator))]
        }
        ("form", Shape::Pairs(pairs)) if explode => pairs,
        ("form", Shape::Pairs(pairs)) => vec![(
            field.to_owned(),
            pairs
                .into_iter()
                .flat_map(|(k, v)| [k, v])
                .collect::<Vec<_>>()
                .join(","),
        )],
        ("deepObject", Shape::Pairs(pairs)) => pairs
            .into_iter()
            .map(|(k, v)| (format!("{field}[{k}]"), v))
            .collect(),
        (style, _) => return Err(format!("style `{style}` cannot carry this value")),
    })
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
        if let Some(reason) = &operation.unsupported {
            return Err(CallError::Refused(format!("`{name}`: {reason}")));
        }
        let invalid = |errors: String| CallError::Arguments {
            operation: name.to_owned(),
            errors,
        };
        let empty = Map::new();
        let fields = args.as_object().unwrap_or(&empty);
        let mut path = operation.path.clone();
        let mut query = Vec::new();
        let mut headers = self.headers.clone();
        let mut cookies = Vec::new();
        let mut body = None;
        for (field, value) in fields {
            let Some(place) = operation.locations.get(field).and_then(Value::as_str) else {
                continue;
            };
            if place == "body" {
                body = Some(value.clone());
                continue;
            }
            // OpenAPI's defaults, for an operation built without styles.
            let declared = operation.styles.get(field);
            let style = declared.and_then(|s| s["style"].as_str()).unwrap_or(
                if matches!(place, "path" | "header") {
                    "simple"
                } else {
                    "form"
                },
            );
            let explode = declared
                .and_then(|s| s["explode"].as_bool())
                .unwrap_or(style == "form");
            if value.is_null() {
                if place == "path" {
                    return Err(invalid(format!(
                        "/{field}: a path parameter cannot be null"
                    )));
                }
                continue;
            }
            let shape = shape(value).map_err(|e| invalid(format!("/{field}: {e}")))?;
            match place {
                "path" => {
                    if style != "simple" {
                        return Err(CallError::Refused(format!(
                            "`{name}`: path style `{style}` is not supported"
                        )));
                    }
                    let value = simple(shape, explode, segment);
                    // `.` and `..` are dot segments: URL parsing removes
                    // them (and `..` the segment before it), and so would
                    // their encoded spelling `%2E`, so they are refused
                    // rather than encoded. An empty value would leave an
                    // empty segment.
                    if matches!(value.as_str(), "" | "." | "..") {
                        return Err(invalid(format!(
                            "/{field}: {value:?} is not a path segment; it would change the operation's path"
                        )));
                    }
                    path = path.replace(&format!("{{{field}}}"), &value);
                }
                "query" => query.extend(
                    query_pairs(field, shape, style, explode)
                        .map_err(|e| invalid(format!("/{field}: {e}")))?,
                ),
                "header" => {
                    let lower = field.to_ascii_lowercase();
                    if RESERVED_HEADERS.contains(&lower.as_str())
                        || self
                            .headers
                            .iter()
                            .any(|(h, _)| h.eq_ignore_ascii_case(field))
                    {
                        return Err(CallError::Refused(format!(
                            "`{name}`: an argument cannot set the `{field}` header"
                        )));
                    }
                    let value = simple(shape, explode, str::to_owned);
                    if value.chars().any(|c| c.is_control()) {
                        return Err(invalid(format!(
                            "/{field}: a header value cannot hold control characters"
                        )));
                    }
                    headers.push((field.clone(), value));
                }
                "cookie" => match shape {
                    Shape::List(items) if explode => cookies.extend(
                        items
                            .iter()
                            .map(|i| format!("{}={}", cookie_value(field), cookie_value(i))),
                    ),
                    shape => cookies.push(format!(
                        "{}={}",
                        cookie_value(field),
                        simple(shape, false, cookie_value)
                    )),
                },
                _ => {}
            }
        }
        if path.contains('{') {
            return Err(invalid(format!(
                "path {path} still has an unfilled parameter"
            )));
        }
        if !cookies.is_empty() {
            headers.push(("Cookie".into(), cookies.join("; ")));
        }
        // The operation's path follows the base URL's whole path: without a
        // trailing slash, joining would replace the base's last segment. The
        // `./` keeps a first segment with a colon from reading as a scheme.
        let mut base = self.base_url.clone();
        if !base.path().ends_with('/') {
            let with_slash = format!("{}/", base.path());
            base.set_path(&with_slash);
        }
        let mut url = base
            .join(&format!("./{}", path.trim_start_matches('/')))
            .map_err(|e| CallError::Refused(format!("cannot build the URL: {e}")))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        // The joined URL must stay on an allowed host, under the base path.
        let host = host_of(&url).unwrap_or_default();
        if !self.allow.contains(&host)
            || url.scheme() != base.scheme()
            || !url.path().starts_with(base.path())
        {
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
                let code = status.as_u16();
                let declared = self
                    .operations
                    .get(name)
                    .and_then(|operation| operation.response(code));
                // Only a response that declares no `content` at all declares
                // no body: a `204`, or a `200` written with only a
                // description. Anything that carries something with it
                // breaks the contract, and is refused as such before it is
                // parsed. A response that declares content atomic has no
                // JSON schema for declares a body too, of a shape nothing
                // describes, so what comes back is not held against it.
                let mut nothing_declared = false;
                if let Some((key, ResponseBody::NoBody)) = declared {
                    nothing_declared = true;
                    if !is_blank_body(&bytes) {
                        return Err(CallError::Response {
                            operation: name.to_owned(),
                            status: code,
                            response: key.to_owned(),
                            errors: format!(
                                "its response declares no body, and {} bytes were returned",
                                bytes.len()
                            ),
                        });
                    }
                }
                // An empty body is read as `null`. Reaching here with no
                // content declared, the body is blank, and blank is no
                // result either: the reply was told to carry nothing. Where
                // a body was declared — a JSON schema, or a media type
                // atomic does not read — only no bytes at all read as null;
                // whitespace is not JSON, and stays the transport failure it
                // has always been.
                let value = if bytes.is_empty() || nothing_declared {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes)
                        .map_err(|e| transport(format!("the response is not JSON: {e}")))?
                };
                return self.check_response(name, code, value);
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

impl HttpToolset {
    /// `value`, the body of a success with `status`, if it fits the schema
    /// the operation declares for that status (see
    /// [`atomic_schema::openapi::Operation::response`]), or the operation
    /// declares none to fit: a response that declares content with no JSON
    /// schema declares a body nothing describes, so any body fits; a
    /// response with no content at all, such as a `204`, has nothing to
    /// check here, because its caller has already refused one that carried a
    /// body; and an operation that declares no response at all takes any. A
    /// success whose status the operation does not declare, when it declares
    /// others, is refused.
    fn check_response(&self, name: &str, status: u16, value: Value) -> Result<Value, CallError> {
        let Some(operation) = self.operations.get(name) else {
            return Ok(value);
        };
        let (key, schema) = match operation.response(status) {
            Some((key, ResponseBody::Json(schema))) => (key, schema),
            // Declares a body, declares no schema to fit it: passed through.
            Some((_, _)) => return Ok(value),
            None if operation.responses.is_empty() => return Ok(value),
            None => {
                return Err(CallError::Undeclared {
                    operation: name.to_owned(),
                    status,
                });
            }
        };
        let response = |errors: String| CallError::Response {
            operation: name.to_owned(),
            status,
            response: key.to_owned(),
            errors,
        };
        let validator = atomic_schema::registry::Registry::new()
            .validator_for(schema)
            .map_err(|e| response(e.to_string()))?;
        let errors: Vec<String> = validator
            .iter_errors(&value)
            .map(|e| format!("{}: {e}", e.instance_path()))
            .collect();
        if errors.is_empty() {
            Ok(value)
        } else {
            Err(response(errors.join("; ")))
        }
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
