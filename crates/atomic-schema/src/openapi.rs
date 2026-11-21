// SPDX-License-Identifier: Apache-2.0

//! OpenAPI 3.0 and 3.1 specs as toolsets: each operation becomes a tool
//! with one JSON Schema 2020-12 input, its parameters and its JSON body.
//!
//! OpenAPI 3.1 schemas are already JSON Schema 2020-12. A 3.0 spec's schema
//! dialect is converted first: `nullable: true` adds `null` to the type,
//! boolean `exclusiveMinimum`/`exclusiveMaximum` become the numeric form,
//! and `example` becomes `examples`. Component schemas become `$defs`, and
//! references to component parameters, request bodies and responses are
//! resolved in place, so a 3.0 and a 3.1 spec of one API give the same tools.
//!
//! As OpenAPI requires, header parameters named `Accept`, `Content-Type` or
//! `Authorization` are ignored: the executor sets those itself. Request and
//! response bodies are read from a JSON media type (`application/json`,
//! with parameters or not, or a `+json` type). An operation whose required
//! body has no JSON media type is kept, marked [`Operation::unsupported`].
//!
//! A response is held to a schema the same way, and additionally to the
//! schema of a wildcard media type (`*/*`, `application/*`) when no JSON
//! media type names one: such a response declares a body that may be JSON,
//! so atomic reads it as JSON and checks it. Every JSON media type that
//! names a schema is kept for a response, not only one of them, because a
//! reply says which of them it is by its `Content-Type`. A request body
//! keeps only the first, which changes nothing about it: atomic sends one
//! JSON body under one JSON media type, never under a wildcard, so a request
//! body declared only under a wildcard still leaves a required body
//! unsupported.
//!
//! Each response a success can be checked against says which of three things
//! it declares about its body — no `content` map at all, a `content` map with
//! no schema to check against, or a schema under each media type it names one
//! under — as [`ResponseBody`]. Only the first says there is no body to read;
//! only the last says what shape the body has. A response declared for a
//! status that is not a success is kept as a key with nothing beside it: what
//! it declares about its body is never read, so its schema is never converted
//! and can never be what makes the import of the spec fail. A key of
//! `responses` under the `x-` prefix is the spec's own note rather than part
//! of the operation, and declares no response at all.

use serde_json::{Map, Value, json};

/// What one response of a spec declares about its body, as
/// [`Operation::responses`] keeps it.
///
/// The three cases are not the same claim. Only [`ResponseBody::NoBody`]
/// says there is no body to read; the other two declare a body and simply
/// say different amounts about it.
#[derive(Clone, Debug, PartialEq)]
pub enum ResponseBody {
    /// It declares no `content` map at all, so it declares no body: a `204`,
    /// or a `200` written with only a description.
    NoBody,
    /// It declares a `content` map, but one that gives atomic no schema to
    /// check against: it names only media types atomic does not read and
    /// wildcards that name no schema (`text/plain`,
    /// `application/octet-stream`, `*/*`, `application/*`), or only JSON
    /// media types with no `schema`. A body is declared, of a shape nothing
    /// in the spec describes.
    NoJsonSchema,
    /// It declares one or more schemas, each under a media type its body may
    /// arrive with: under one or more JSON media types, or, where no JSON
    /// media type names a schema at all, under one wildcard media type. Every
    /// media type that names a schema is kept, keyed by how the spec writes
    /// it, because a reply says which of them it is by its `Content-Type`,
    /// and what atomic does when the reply names none of them is the
    /// executor's to decide.
    Json(std::collections::BTreeMap<String, Value>),
}

impl ResponseBody {
    /// The first schema it declares, in the sorted order of the media types
    /// they are declared under, when it declares one. That one is what the
    /// tool's `output` describes; a reply is checked against whichever of
    /// them its `Content-Type` names, see [`ResponseBody::schemas`].
    pub fn schema(&self) -> Option<&Value> {
        match self {
            ResponseBody::Json(schemas) => schemas.values().next(),
            ResponseBody::NoBody | ResponseBody::NoJsonSchema => None,
        }
    }

    /// Every schema it declares, with the media type each is declared under
    /// as the spec writes it, in that order. Empty for a response that
    /// declares none to check a body against.
    pub fn schemas(&self) -> impl Iterator<Item = (&String, &Value)> {
        match self {
            ResponseBody::Json(schemas) => Some(schemas.iter()),
            ResponseBody::NoBody | ResponseBody::NoJsonSchema => None,
        }
        .into_iter()
        .flatten()
    }

    /// Every schema to change in place, when there are any;
    /// [`ResponseBody::schema`] borrows the first read-only.
    fn schemas_mut(&mut self) -> impl Iterator<Item = &mut Value> {
        match self {
            ResponseBody::Json(schemas) => Some(schemas.values_mut()),
            ResponseBody::NoBody | ResponseBody::NoJsonSchema => None,
        }
        .into_iter()
        .flatten()
    }
}

/// One operation of a spec, as a tool.
#[derive(Clone, Debug, PartialEq)]
pub struct Operation {
    /// Its `operationId`, or `<method>_<path>` when it has none.
    pub name: String,
    /// The HTTP method, lowercase.
    pub method: String,
    /// The path template, such as `/forecast/{city}`.
    pub path: String,
    /// Its summary or description.
    pub description: Option<String>,
    /// Where each input field goes: `path`, `query`, `header`, `cookie` or
    /// `body`.
    pub locations: Map<String, Value>,
    /// How each parameter is serialized: `{"style": ..., "explode": ...}`,
    /// with OpenAPI's defaults filled in.
    pub styles: Map<String, Value>,
    /// The tool's input: one object, a field per parameter and `body` for a
    /// JSON request body.
    pub input: Value,
    /// The JSON schema of its success response, when it declares one: the
    /// first schema the first success code declares, else `default`'s. A
    /// tool's output, for what describes it; what a reply is checked against
    /// is [`Operation::response`], and within it whichever schema the reply's
    /// `Content-Type` names.
    pub output: Option<Value>,
    /// Each response it declares, by the key the spec lists it under: a code
    /// such as `200` or `404`, a range such as `2XX`, or `default`. A success's,
    /// and `default`'s, says what it declares about its body, see
    /// [`ResponseBody`]; any other key is kept with nothing beside it, because
    /// atomic reads no such reply's body and so holds it to no schema. A key
    /// under the `x-` prefix is the spec's own note rather than part of the
    /// operation, names no response, and is not among these.
    pub responses: std::collections::BTreeMap<String, Option<ResponseBody>>,
    /// Why atomic cannot call it, when it cannot: a required request body
    /// with no JSON media type.
    pub unsupported: Option<String>,
}

impl Operation {
    /// The response it declares for a success with `status`, and the key it
    /// is declared under, as OpenAPI picks one: the status's own code, else
    /// its range (`2XX`, in either case), else `default`. `None` for a status
    /// that is not a success (outside 200 to 299), whose body atomic never
    /// checks, and for a success it declares none of them for. Every response
    /// a success can reach keeps what it declares about its body, the
    /// [`ResponseBody`] it comes with: a status can select one that declares
    /// no body as well as one whose body has a schema. A response that
    /// declares a schema under more than one media type leaves which of them
    /// governs a reply to the reply's `Content-Type`, which is not known here.
    pub fn response(&self, status: u16) -> Option<(&str, &ResponseBody)> {
        if !(200..=299).contains(&status) {
            return None;
        }
        let range = format!("{}XX", status / 100);
        let code = status.to_string();
        let (key, body) = self
            .responses
            .get_key_value(&code)
            .or_else(|| {
                self.responses
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(&range))
            })
            .or_else(|| self.responses.get_key_value("default"))?;
        // A key kept with nothing beside it names no schema for a reply to
        // fit, and never lends another response's. Only a failure's key is
        // kept that way, so no status an operation answers reaches this.
        Some((key.as_str(), body.as_ref()?))
    }
}

/// The styles OpenAPI allows in each location; the first is the default.
fn styles_for(place: &str) -> &'static [&'static str] {
    match place {
        "path" => &["simple", "label", "matrix"],
        "query" => &["form", "spaceDelimited", "pipeDelimited", "deepObject"],
        "header" => &["simple"],
        _ => &["form"],
    }
}

/// Whether `media` is a JSON media type: `application/json`, with or without
/// parameters, or a structured `+json` type of any tree (`application/`,
/// `text/`, vendor or standards-prefixed trees alike), which is what RFC 6839
/// says a `+json` suffix means.
pub fn is_json_media(media: &str) -> bool {
    let essence = media_essence(media);
    essence == "application/json" || (essence.contains('/') && essence.ends_with("+json"))
}

/// A media type with its parameters dropped and its case folded, as media
/// types are compared.
pub fn media_essence(media: &str) -> String {
    media
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Whether `media` is a wildcard media type: `*/*` or `application/*`, with
/// parameters or not. Neither names JSON, but each says its body is anything
/// of the group it stands for, and a schema written under one says which part
/// of that the API returns — so a body under one is read as JSON, which is
/// what `response_content` does when no JSON media type names a schema.
fn is_wildcard_media(media: &str) -> bool {
    let essence = media_essence(media);
    essence == "*/*" || essence == "application/*"
}

/// Whether a response key names a reply whose body atomic reads and so keeps
/// a schema for: a code from `200` to `299`, a `2XX` range in either case, or
/// `default`, which is what a success the spec gives no code for falls back
/// to. Any other key names a reply atomic never reads the body of — another
/// range, any other code — or names no status at all, so nothing is converted
/// or compiled for it.
fn success_key(code: &str) -> bool {
    if code == "default" {
        return true;
    }
    if is_range_key(code) {
        return code.as_bytes()[0] == b'2';
    }
    match code.parse::<u16>() {
        Ok(status) => (200..=299).contains(&status),
        Err(_) => false,
    }
}

/// Whether a response key is a range such as `2XX` or `4xx`, in either case.
fn is_range_key(code: &str) -> bool {
    let bytes = code.as_bytes();
    bytes.len() == 3 && bytes[0].is_ascii_digit() && bytes[1..].eq_ignore_ascii_case(b"xx")
}

/// Whether a key of a spec's `responses` object lists a response. `x-` is the
/// one prefix OpenAPI gives a key that is the spec's own note rather than part
/// of the operation, so an `x-internal` or `x-openapi` key declares no
/// response: counting one would make an operation that declares only
/// extensions declare a response after all. Any other key counts, however the
/// spec spells it — a key naming no status keeps nothing beside it, see
/// [`success_key`], and still stands as a response the operation declared.
fn is_response_key(code: &str) -> bool {
    !code.starts_with("x-")
}

/// What a response object declares about its body.
///
/// A `content` map is kept as declared even when atomic can get no schema
/// out of it: media types it does not read, a wildcard or a JSON media type
/// with no `schema`, a map that is not a map at all. Those declare a body of
/// an unknown shape, which is not the same as declaring no body, so they are
/// kept apart from a response with no `content` key.
fn declared_body(response: &Value) -> ResponseBody {
    let Some(content) = response.get("content") else {
        return ResponseBody::NoBody;
    };
    match response_content(content) {
        schemas if schemas.is_empty() => ResponseBody::NoJsonSchema,
        schemas => ResponseBody::Json(schemas),
    }
}

/// The schemas a response's body is held to under a `content` map: every JSON
/// media type's, as `json_schemas` keeps them, which a wildcard never
/// overrides; else the first wildcard media type's that names a schema, so
/// that a response declaring a schema only under `*/*` or `application/*`
/// holds its body to it. A content map with neither declares a body of a
/// shape nothing describes.
fn response_content(content: &Value) -> std::collections::BTreeMap<String, Value> {
    let schemas = json_schemas(content);
    if schemas.is_empty() {
        return wildcard_content(content);
    }
    schemas
}

/// The schema under a `content` map's first JSON media type, if any — which
/// is what a request body is held to, since atomic sends one JSON body under
/// one JSON media type. `None` when the first one names no `schema`, even
/// where a later one does, which is what leaves such a request body
/// unsupported.
fn json_content(content: &Value) -> Option<&Value> {
    let content = content.as_object()?;
    let mut keys: Vec<&String> = content.keys().filter(|k| is_json_media(k)).collect();
    keys.sort();
    keys.first().and_then(|k| content[k.as_str()].get("schema"))
}

/// Every schema under a `content` map's JSON media types, each keyed by the
/// media type as the map writes it, because a reply says which of them it is
/// by its `Content-Type`. A JSON media type naming no `schema` is kept apart
/// from these: it names nothing to check a body against.
fn json_schemas(content: &Value) -> std::collections::BTreeMap<String, Value> {
    let Some(content) = content.as_object() else {
        return std::collections::BTreeMap::new();
    };
    let mut types: Vec<&String> = content.keys().filter(|k| is_json_media(k)).collect();
    types.sort();
    types
        .iter()
        .filter_map(|k| {
            content[k.as_str()]
                .get("schema")
                .map(|schema| ((*k).clone(), schema.clone()))
        })
        .collect()
}

/// The schema under a `content` map's first wildcard media type that names
/// one, if any, the media types in sorted order, keyed by that media type.
fn wildcard_content(content: &Value) -> std::collections::BTreeMap<String, Value> {
    let mut out = std::collections::BTreeMap::new();
    let Some(content) = content.as_object() else {
        return out;
    };
    let mut keys: Vec<&String> = content.keys().filter(|k| is_wildcard_media(k)).collect();
    keys.sort();
    if let Some((key, schema)) = keys
        .iter()
        .find_map(|k| content[k.as_str()].get("schema").map(|s| ((*k).clone(), s)))
    {
        out.insert(key, schema.clone());
    }
    out
}

/// Why a spec cannot become a toolset.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("OpenAPI spec at {path}: {message}")]
pub struct OpenApiError {
    /// A JSON Pointer into the spec.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

fn fail(path: &str, message: impl Into<String>) -> OpenApiError {
    OpenApiError {
        path: if path.is_empty() {
            "/".into()
        } else {
            path.into()
        },
        message: message.into(),
    }
}

const METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// The spec's operations, in path and method order.
pub fn operations(spec: &Value) -> Result<Vec<Operation>, OpenApiError> {
    let version = spec["openapi"].as_str().unwrap_or_default();
    let legacy = if version.starts_with("3.0.") {
        true
    } else if version.starts_with("3.1.") {
        false
    } else if spec.get("swagger").is_some() {
        return Err(fail(
            "/swagger",
            "Swagger 2.0 specs are not supported yet; convert it to OpenAPI 3",
        ));
    } else {
        return Err(fail(
            "/openapi",
            format!(
                "`openapi` must be 3.0.x or 3.1.x, not {:?}",
                spec["openapi"]
            ),
        ));
    };
    let convert = |schema: &Value, at: &str| -> Result<Value, OpenApiError> {
        let schema = if legacy {
            dialect(schema, at)?
        } else {
            schema.clone()
        };
        Ok(references(&schema))
    };
    let mut defs = Map::new();
    if let Some(schemas) = spec
        .pointer("/components/schemas")
        .and_then(Value::as_object)
    {
        for (name, schema) in schemas {
            defs.insert(
                name.clone(),
                convert(schema, &format!("/components/schemas/{name}"))?,
            );
        }
    }
    let mut out = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    let Some(paths) = spec.get("paths").and_then(Value::as_object) else {
        return Ok(out);
    };
    let mut ordered: Vec<_> = paths.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));
    for (path, item) in ordered {
        let item = resolve(spec, item, &format!("/paths/{}", escape(path)))?;
        let shared = item
            .get("parameters")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for method in METHODS {
            let Some(operation) = item.get(*method) else {
                continue;
            };
            let at = format!("/paths/{}/{method}", escape(path));
            let name = operation["operationId"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    let slug: String = path
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                        .collect();
                    format!("{method}{}", slug.trim_end_matches('_'))
                });
            if !names.insert(name.clone()) {
                return Err(fail(&at, format!("two operations are named `{name}`")));
            }
            let mut properties = Map::new();
            let mut required = Vec::new();
            let mut locations = Map::new();
            let mut styles = Map::new();
            let mut unsupported = None;
            // Operation parameters override path-level ones of the same name and place.
            let mut parameters: Vec<Value> = Vec::new();
            for parameter in shared
                .iter()
                .chain(operation["parameters"].as_array().into_iter().flatten())
            {
                let parameter = resolve(spec, parameter, &at)?;
                let key = (parameter["name"].clone(), parameter["in"].clone());
                parameters.retain(|p: &Value| (p["name"].clone(), p["in"].clone()) != key);
                parameters.push(parameter);
            }
            for parameter in &parameters {
                let field = parameter["name"]
                    .as_str()
                    .ok_or_else(|| fail(&at, "a parameter has no name"))?;
                let place = parameter["in"]
                    .as_str()
                    .ok_or_else(|| fail(&at, format!("parameter `{field}` has no `in`")))?;
                if !["path", "query", "header", "cookie"].contains(&place) {
                    return Err(fail(&at, format!("parameter `{field}` is in `{place}`")));
                }
                if place == "header"
                    && ["accept", "content-type", "authorization"]
                        .contains(&field.to_ascii_lowercase().as_str())
                {
                    continue;
                }
                let allowed = styles_for(place);
                let style = match parameter.get("style") {
                    None => allowed[0],
                    Some(style) => {
                        let style = style.as_str().unwrap_or_default();
                        allowed.iter().copied().find(|s| *s == style).ok_or_else(|| {
                            fail(&at, format!("parameter `{field}` in `{place}` cannot have style `{style}`"))
                        })?
                    }
                };
                let explode = parameter["explode"].as_bool().unwrap_or(style == "form");
                styles.insert(
                    field.to_owned(),
                    json!({ "style": style, "explode": explode }),
                );
                if properties.contains_key(field) {
                    return Err(fail(&at, format!("two parameters are named `{field}`")));
                }
                let mut schema = match parameter.get("schema") {
                    Some(schema) => convert(schema, &at)?,
                    None => json!({}),
                };
                if let (Some(text), Value::Object(map)) =
                    (parameter["description"].as_str(), &mut schema)
                {
                    map.entry("description").or_insert(json!(text));
                }
                if place == "path" || parameter["required"] == true {
                    required.push(json!(field));
                }
                properties.insert(field.to_owned(), schema);
                locations.insert(field.to_owned(), json!(place));
            }
            if let Some(body) = operation.get("requestBody") {
                let body = resolve(spec, body, &at)?;
                let json = body.get("content").and_then(json_content);
                if json.is_none() && body["required"] == true {
                    let mut types: Vec<&String> = body["content"]
                        .as_object()
                        .map(|c| c.keys().collect())
                        .unwrap_or_default();
                    types.sort();
                    let types: Vec<&str> = types.iter().map(|t| t.as_str()).collect();
                    unsupported = Some(format!(
                        "its required request body is {}; atomic sends JSON request bodies only",
                        if types.is_empty() {
                            "untyped".to_owned()
                        } else {
                            types.join(", ")
                        }
                    ));
                }
                if let Some(schema) = json {
                    if properties.contains_key("body") {
                        return Err(fail(
                            &at,
                            "a parameter named `body` collides with the request body",
                        ));
                    }
                    let mut schema = convert(schema, &format!("{at}/requestBody"))?;
                    if let (Some(text), Value::Object(map)) =
                        (body["description"].as_str(), &mut schema)
                    {
                        map.entry("description").or_insert(json!(text));
                    }
                    properties.insert("body".into(), schema);
                    locations.insert("body".into(), json!("body"));
                    if body["required"] == true {
                        required.push(json!("body"));
                    }
                }
            }
            let mut input = json!({ "type": "object", "additionalProperties": false, "properties": properties });
            if !required.is_empty() {
                input["required"] = Value::Array(required);
            }
            // Every response the spec lists, each with what it declares for
            // its body: a reply is checked against the one for its status,
            // never another's, and against the schema of the media type its
            // reply names, never another one's. A key naming no status atomic
            // reads a body for keeps nothing beside it, and a key naming no
            // response at all is not kept.
            let mut kept = std::collections::BTreeMap::new();
            if let Some(responses) = operation.get("responses").and_then(Value::as_object) {
                for (code, response) in responses {
                    if !is_response_key(code) {
                        continue;
                    }
                    if !success_key(code) {
                        // Only a success's body, or the body of the default a
                        // success falls back to, is ever checked, so this
                        // response's schema is never converted and never
                        // compiled: a reference in a `404` that does not
                        // resolve cannot stop the import of the operation,
                        // whose `200` is still checked.
                        kept.insert(code.clone(), None);
                        continue;
                    }
                    let response = resolve(spec, response, &at)?;
                    let mut body = declared_body(&response);
                    for schema in body.schemas_mut() {
                        *schema = convert(schema, &format!("{at}/responses/{code}"))?;
                    }
                    kept.insert(code.clone(), Some(body));
                }
            }
            if !defs.is_empty() {
                input["$defs"] = Value::Object(defs.clone());
                for body in kept.values_mut().flatten() {
                    for schema in body.schemas_mut() {
                        if let Value::Object(map) = schema {
                            map.insert("$defs".into(), Value::Object(defs.clone()));
                        }
                    }
                }
            }
            // The schema of the first key beginning with `2`, in the map's
            // sorted order, else `default`'s: the first success, for any spec
            // that spells its codes as OpenAPI says. Only a key naming a
            // success keeps a schema, so an odd key such as a bare `2`
            // describes no output.
            let output = kept
                .iter()
                .find(|(code, _)| code.starts_with('2'))
                .or_else(|| kept.get_key_value("default"))
                .and_then(|(_, body)| body.as_ref())
                .and_then(|body| body.schema().cloned());
            let description = operation["summary"]
                .as_str()
                .or_else(|| operation["description"].as_str())
                .map(str::to_owned);
            out.push(Operation {
                name,
                method: (*method).to_owned(),
                path: path.clone(),
                description,
                locations,
                styles,
                input,
                output,
                responses: kept,
                unsupported,
            });
        }
    }
    Ok(out)
}

fn escape(path: &str) -> String {
    path.replace('~', "~0").replace('/', "~1")
}

/// A component reference other than a schema, followed in place.
fn resolve(spec: &Value, value: &Value, at: &str) -> Result<Value, OpenApiError> {
    let mut current = value.clone();
    for _ in 0..16 {
        let Some(target) = current.get("$ref").and_then(Value::as_str) else {
            return Ok(current);
        };
        let pointer = target
            .strip_prefix('#')
            .ok_or_else(|| fail(at, format!("reference `{target}` is outside the spec")))?;
        current = spec
            .pointer(pointer)
            .cloned()
            .ok_or_else(|| fail(at, format!("reference `{target}` does not resolve")))?;
    }
    Err(fail(at, "references nest too deeply"))
}

/// Component schema references become `$defs` references.
fn references(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = match (key.as_str(), value.as_str()) {
                        ("$ref", Some(target)) => {
                            match target.strip_prefix("#/components/schemas/") {
                                Some(name) => json!(format!("#/$defs/{name}")),
                                None => value.clone(),
                            }
                        }
                        _ => references(value),
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(references).collect()),
        other => other.clone(),
    }
}

/// An OpenAPI 3.0 schema in JSON Schema 2020-12.
fn dialect(schema: &Value, at: &str) -> Result<Value, OpenApiError> {
    let Value::Object(map) = schema else {
        return Ok(schema.clone());
    };
    let mut out = Map::new();
    for (key, value) in map {
        let value = match key.as_str() {
            "properties" | "patternProperties" | "$defs" | "definitions" => match value {
                Value::Object(children) => Value::Object(
                    children
                        .iter()
                        .map(|(name, child)| Ok((name.clone(), dialect(child, at)?)))
                        .collect::<Result<_, OpenApiError>>()?,
                ),
                other => other.clone(),
            },
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => match value {
                Value::Array(items) => Value::Array(
                    items
                        .iter()
                        .map(|item| dialect(item, at))
                        .collect::<Result<_, _>>()?,
                ),
                other => other.clone(),
            },
            "items" | "additionalProperties" | "not" => dialect(value, at)?,
            _ => value.clone(),
        };
        out.insert(key.clone(), value);
    }
    if let Some(example) = out.remove("example") {
        out.entry("examples").or_insert(json!([example]));
    }
    for (flag, bound) in [
        ("exclusiveMinimum", "minimum"),
        ("exclusiveMaximum", "maximum"),
    ] {
        if let Some(Value::Bool(exclusive)) = out.get(flag).cloned() {
            out.remove(flag);
            if exclusive {
                let limit = out
                    .remove(bound)
                    .ok_or_else(|| fail(at, format!("`{flag}: true` without `{bound}`")))?;
                out.insert(flag.into(), limit);
            }
        }
    }
    if out.remove("nullable") == Some(Value::Bool(true)) {
        match out.get("type").cloned() {
            Some(Value::String(kind)) => {
                out.insert("type".into(), json!([kind, "null"]));
            }
            Some(Value::Array(mut kinds)) => {
                if !kinds.contains(&json!("null")) {
                    kinds.push(json!("null"));
                }
                out.insert("type".into(), Value::Array(kinds));
            }
            _ => {
                let inner = Value::Object(std::mem::take(&mut out));
                out.insert("anyOf".into(), json!([inner, { "type": "null" }]));
            }
        }
        if let Some(Value::Array(values)) = out.get_mut("enum")
            && !values.contains(&Value::Null)
        {
            values.push(Value::Null);
        }
    }
    for openapi_only in ["discriminator", "xml", "externalDocs"] {
        out.remove(openapi_only);
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests;
