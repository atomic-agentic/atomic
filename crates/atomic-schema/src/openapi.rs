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

use serde_json::{Map, Value, json};

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
    /// first success code's, else `default`'s. A tool's output, for what
    /// describes it; a response is checked against [`Operation::response`].
    pub output: Option<Value>,
    /// Each response it declares for a success, by the status it is
    /// declared for as the spec writes it: a code such as `200`, a range
    /// such as `2XX`, or `default`. Its JSON schema, or `None` where it
    /// declares no JSON body, as a `204` does.
    pub responses: std::collections::BTreeMap<String, Option<Value>>,
    /// Why atomic cannot call it, when it cannot: a required request body
    /// with no JSON media type.
    pub unsupported: Option<String>,
}

impl Operation {
    /// The response it declares for a reply with `status`, as OpenAPI picks
    /// one: the status's own code, else its range (`2XX`), else `default`.
    /// `None` when it declares none of them.
    pub fn response(&self, status: u16) -> Option<&Option<Value>> {
        let range = format!("{}XX", status / 100);
        self.responses
            .get(&status.to_string())
            .or_else(|| {
                self.responses
                    .iter()
                    .find(|(code, _)| code.eq_ignore_ascii_case(&range))
                    .map(|(_, schema)| schema)
            })
            .or_else(|| self.responses.get("default"))
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

/// Whether `media` is a JSON media type: `application/json`, with or
/// without parameters, or a structured `+json` type.
pub fn is_json_media(media: &str) -> bool {
    let essence = media
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "application/json"
        || (essence.starts_with("application/") && essence.ends_with("+json"))
}

/// The schema under a `content` map's first JSON media type, if any.
fn json_content(content: &Value) -> Option<&Value> {
    let content = content.as_object()?;
    let mut keys: Vec<&String> = content.keys().filter(|k| is_json_media(k)).collect();
    keys.sort();
    keys.first().and_then(|k| content[k.as_str()].get("schema"))
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
            // Every success response it declares, and `default`, each with
            // its own schema: a reply is checked against the one for its
            // status, never another's.
            let mut kept = std::collections::BTreeMap::new();
            if let Some(responses) = operation.get("responses").and_then(Value::as_object) {
                for (code, response) in responses {
                    if !(code.starts_with('2') || code == "default") {
                        continue;
                    }
                    let response = resolve(spec, response, &at)?;
                    let schema = match response.get("content").and_then(json_content) {
                        Some(schema) => Some(convert(schema, &format!("{at}/responses/{code}"))?),
                        None => None,
                    };
                    kept.insert(code.clone(), schema);
                }
            }
            if !defs.is_empty() {
                input["$defs"] = Value::Object(defs.clone());
                for schema in kept.values_mut().flatten() {
                    if let Value::Object(map) = schema {
                        map.insert("$defs".into(), Value::Object(defs.clone()));
                    }
                }
            }
            let mut codes: Vec<&String> =
                kept.keys().filter(|code| code.starts_with('2')).collect();
            codes.sort();
            let output = codes
                .first()
                .copied()
                .or_else(|| kept.keys().find(|k| *k == "default"))
                .and_then(|code| kept[code].clone());
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
