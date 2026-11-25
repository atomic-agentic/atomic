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
//! `Authorization` are ignored: the executor sets those itself. An operation
//! whose required request body is declared under no media type atomic may send
//! is kept all the same, marked [`Operation::unsupported`].
//!
//! A response body is read as JSON from a JSON media type (`application/json`,
//! with parameters or not, or a `+json` type of any tree). A response is held
//! to a schema the same way, and additionally to the schema of a wildcard
//! media type (`*/*`, `application/*`) when no JSON media type names one: such
//! a response declares a body that may be JSON, so atomic reads it as JSON and
//! checks it. Every JSON media type that names a schema is kept for a
//! response, not only one of them, because a reply says which of them it is by
//! its `Content-Type`. Of several wildcards the most specific declared one
//! governs — `application/*` before `*/*` — because the narrower promise is
//! the one the spec meant for the bodies it covers.
//!
//! A request body is read more narrowly, because atomic sends every request
//! body as JSON. It prefers `application/json` and otherwise selects a
//! unique declared application subtype ending in `+json`. Ambiguous
//! representations leave the operation unsupported. The Content-Type it
//! sends is the one whose schema it checked. A `+json` type of another
//! tree (`text/vnd.acme.echo+json`) describes a reply but not a body atomic
//! may send, so a required body declared only under one leaves the operation
//! [`Operation::unsupported`]. A wildcard describes no body atomic may send
//! either, so a request body declared only under a wildcard does the same.
//!
//! Each response a success can be checked against says which of three things
//! it declares about its body — no `content` map at all, a `content` map with
//! no schema to check against, or a schema under each media type it names one
//! under — as [`ResponseBody`]. Only the first says there is no body to read;
//! only the last says what shape the body has.
//!
//! A tool's `output` describes what a reply may look like, which is more than
//! one response's schema whenever the operation's successes describe
//! different bodies: it is every distinct schema of every response a success
//! can reach, one per media type that response names a schema under, since a
//! reply says which of them it is by its `Content-Type` — one schema where they
//! all say the same thing and `anyOf` of them where they differ. A response
//! declaring no schema contributes nothing to it. A `default` describes the
//! output only when the spec declares no code from `200` to `299` and no `2XX`
//! range beside it: behind a named success a `default` is what that operation's
//! failures fall back to — still the response a success the spec gives no code
//! for is checked against, but no longer one a spec describes — while a spec
//! declaring no success at all means its `default` as the shape a success
//! takes. Where the union holds more than one schema, each member is a schema
//! document of its own, because the union is a document nothing compiles a
//! reply against and a reference inside a member — `#`, or `#/$defs/…` into the
//! `$defs` the member carries — must mean the member it is written in. A member
//! the spec gave an `$id` keeps it, and one the spec gave none is given a
//! `urn:atomic:response:…` of its own; a reference naming a member's own `$id`
//! is how such a member points at itself, so that name is kept too.
//!
//! A response declared for a
//! status that is not a success is kept as a key with nothing beside it: what
//! it declares about its body is never read, so its schema is never converted
//! and can never be what makes the import of the spec fail. A key of
//! `responses` under the `x-` prefix is the spec's own note rather than part
//! of the operation, and declares no response at all.

use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

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
    /// check against: it names only media types atomic reads no body from —
    /// `text/plain`, `application/octet-stream`, and the like, whether or not
    /// one of them carries a `schema`, which is a shape written for a body
    /// atomic never reads — wildcards that name no schema (`*/*`,
    /// `application/*`), or only JSON media types with no `schema`. A body is
    /// declared, and atomic holds no schema of its own to a reply of it.
    NoJsonSchema,
    /// It declares one or more schemas, each under a media type its body may
    /// arrive with: under one or more JSON media types, or, where no JSON
    /// media type names a schema at all, under its most specific wildcard
    /// media type. Every media type that names a schema is kept, keyed by how
    /// the spec writes it, because a reply says which of them it is by its
    /// `Content-Type`, and what atomic does when the reply names none of them
    /// is the executor's to decide.
    Json {
        /// Each schema it declares, keyed by the media type its body may
        /// arrive with, as the `content` map writes it.
        schemas: std::collections::BTreeMap<String, Value>,
        /// The other media types the same `content` map declares, as it writes
        /// them, in sorted order: those that name no schema, and those atomic
        /// reads no body from. A media type of that last kind may name a schema
        /// all the same — a `text/plain` of `type: string` does — and atomic
        /// reads no schema from a media type it reads no body from, so none of
        /// these says a shape atomic holds a reply to: a reply whose
        /// `Content-Type` names one is not governed by another media type's
        /// schema, least of all a wildcard written beside them, and the
        /// executor holds it to no schema at all, as it holds a reply of a
        /// response naming no schema.
        declared_media: Vec<String>,
    },
}

impl ResponseBody {
    /// The first schema it declares, in the sorted order of the media types
    /// they are declared under, when it declares one. What the tool's `output`
    /// describes is every schema it declares, and what a reply is held to is
    /// whichever of them its `Content-Type` names, see
    /// [`ResponseBody::schemas`].
    pub fn schema(&self) -> Option<&Value> {
        match self {
            ResponseBody::Json { schemas, .. } => schemas.values().next(),
            ResponseBody::NoBody | ResponseBody::NoJsonSchema => None,
        }
    }

    /// Every schema it declares, with the media type each is declared under
    /// as the spec writes it, in that order. Empty for a response that
    /// declares none to check a body against.
    pub fn schemas(&self) -> impl Iterator<Item = (&String, &Value)> {
        match self {
            ResponseBody::Json { schemas, .. } => Some(schemas.iter()),
            ResponseBody::NoBody | ResponseBody::NoJsonSchema => None,
        }
        .into_iter()
        .flatten()
    }

    /// The media types its `content` map declares that keep no schema of their
    /// own, as it writes them. Empty for a response that declares no `content`
    /// map or no schema at all; otherwise the declared media types
    /// [`ResponseBody::schemas`] leaves out — the ones a reply may name while
    /// atomic holds it to no shape, a media type of its own that names a schema
    /// for a body atomic never reads included.
    pub fn declared_media(&self) -> &[String] {
        match self {
            ResponseBody::Json { declared_media, .. } => declared_media,
            ResponseBody::NoBody | ResponseBody::NoJsonSchema => &[],
        }
    }

    /// Every schema to change in place, when there are any;
    /// [`ResponseBody::schema`] borrows the first read-only.
    fn schemas_mut(&mut self) -> impl Iterator<Item = &mut Value> {
        match self {
            ResponseBody::Json { schemas, .. } => Some(schemas.values_mut()),
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
    /// The Content-Type whose schema is used for the request body.
    pub request_media: Option<String>,
    /// The tool's input: one object, a field per parameter and `body` for a
    /// JSON request body.
    pub input: Value,
    /// What its successes describe a reply's body as: every schema each one's
    /// response declares, one per media type it names one under, distinct and
    /// in status order — that one schema where they agree, `anyOf` of them
    /// where they differ, `None` when none of them declares one. A `default`
    /// is asked only when no code from `200` to `299` and no `2XX` range is
    /// declared beside it. Where it is an `anyOf`, each member is a schema
    /// document of its own — the `$id` the spec gave it, or one of atomic's
    /// where the spec gave none — so a reference inside it means that member.
    /// A tool's output, for what describes it; what a reply is checked against
    /// is [`Operation::response`], and within it whichever schema the reply's
    /// `Content-Type` names. `declared_output` says which responses are asked
    /// and what each contributes.
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
        response_declared(&self.responses, status)
    }
}

/// What [`Operation::response`] does, over one operation's responses, so that
/// what a tool's output describes is built from exactly the responses a reply
/// can be checked against.
fn response_declared(
    responses: &std::collections::BTreeMap<String, Option<ResponseBody>>,
    status: u16,
) -> Option<(&str, &ResponseBody)> {
    if !(200..=299).contains(&status) {
        return None;
    }
    let range = format!("{}XX", status / 100);
    let code = status.to_string();
    let (key, body) = responses
        .get_key_value(&code)
        .or_else(|| {
            responses
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(&range))
        })
        .or_else(|| responses.get_key_value("default"))?;
    // A key kept with nothing beside it names no schema for a reply to fit,
    // and never lends another response's. Only a failure's key is kept that
    // way, so no status an operation answers reaches this.
    Some((key.as_str(), body.as_ref()?))
}

/// What a tool's `output` describes: every distinct schema of every response a
/// success can reach, which is one schema per media type that response names
/// one under, as [`ResponseBody::schemas`] keeps them — a reply says which of
/// them it is by its `Content-Type`, so each describes a reply the tool may
/// get.
///
/// Each success status is asked, and selects its response exactly as
/// [`Operation::response`] does, and each response is asked once however many
/// statuses select it. A `default` is asked only when no code from `200` to
/// `299` and no `2XX` range is declared beside it: behind a named success a
/// `default` describes what that operation's failures carry, which is not a
/// reply the tool describes, even though a success the spec gives no code for
/// is still checked against it; while a spec declaring nothing but a `default`
/// means it as the shape every success takes. A response that declares no schema — no
/// `content` map, or a map that names none — contributes nothing, so it neither
/// describes the output nor widens it. The same schema written under two media
/// types or two statuses is one schema. Where the successes agree, that one
/// schema is the output as it stands; where they differ, `anyOf` of them is, in
/// status order and then in the order the response names its media types, so a
/// reply is described whichever of them it answers with; and where none declares
/// a shape, there is no output at all. Each member of that `anyOf` is a schema
/// resource of its own — the `$id` the spec gave it, or `urn:atomic:response:…`
/// where the spec gave it none — so a reference written inside a member means
/// that member, as it does when that member is compiled alone.
fn declared_output(
    responses: &std::collections::BTreeMap<String, Option<ResponseBody>>,
) -> Option<Value> {
    // What a `200`-`299` code or a `2XX` range declares shades the `default`:
    // every success is covered by one or the other, and only a spec that names
    // no success at all leaves one to fall back to the `default`.
    let named = responses
        .keys()
        .any(|code| code != "default" && success_key(code));
    let mut schemas: Vec<Value> = Vec::new();
    let mut asked = std::collections::BTreeSet::new();
    for status in 200..300u16 {
        let Some((code, body)) = response_declared(responses, status) else {
            continue;
        };
        if code == "default" && named {
            continue;
        }
        // One response answers every status that selects it, so it is asked
        // once. A response that declares no schema — no `content` map at all,
        // or a map naming none — says nothing about a shape, so it contributes
        // nothing to the union, and not even a schema that would accept
        // anything in its place.
        if !asked.insert(code) {
            continue;
        }
        for (_, schema) in body.schemas() {
            if !schemas.contains(schema) {
                schemas.push(schema.clone());
            }
        }
    }
    match schemas.len() {
        0 => None,
        1 => schemas.pop(),
        _ => {
            // The union is a document nothing else holds a reply to, so each
            // member has to be a document of its own: a reference inside a
            // member — `#`, or `#/$defs/…` into the `$defs` it carries beside
            // its own components — then means the member it is written in,
            // exactly as it does when the executor holds a reply to that member
            // alone. What names nothing is named here; what the spec already
            // named keeps its name, because a reference written as that name is
            // how such a member points at itself. A boolean schema holds no
            // keyword and so no id, and declares nothing a reference could
            // reach.
            let members: Vec<Value> = schemas
                .into_iter()
                .enumerate()
                .map(|(index, schema)| match schema {
                    Value::Object(mut member) => {
                        let unnamed = member.get("$id").is_none();
                        if unnamed {
                            member.insert(
                                "$id".into(),
                                json!(format!("urn:atomic:response:{index}")),
                            );
                        }
                        Value::Object(member)
                    }
                    other => other,
                })
                .collect();
            let mut union = Map::new();
            union.insert("anyOf".into(), Value::Array(members));
            Some(Value::Object(union))
        }
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

/// Whether `media` names a body atomic reads as JSON in a reply:
/// `application/json`, with or without parameters, or a structured `+json`
/// type of any tree (`application/`, `text/`, vendor or standards-prefixed
/// trees alike), which is what RFC 6839 says a `+json` suffix means. A
/// request body must use one of the application media types accepted by
/// `is_json_request_media`.
pub fn is_json_media(media: &str) -> bool {
    let essence = media_essence(media);
    essence == "application/json" || (essence.contains('/') && essence.ends_with("+json"))
}

/// Whether `media` names a request body atomic can send: `application/json`,
/// with or without parameters, or a `+json` type of the `application/` tree.
/// Request body serialization supports application JSON representations. A
/// `+json` type of another tree, such as `text/vnd.acme.echo+json`, is left to
/// [`Operation::unsupported`].
fn is_json_request_media(media: &str) -> bool {
    let essence = media_essence(media);
    let application_tree = essence.starts_with("application/");
    essence == "application/json" || (application_tree && essence.ends_with("+json"))
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
/// what `response_content` does when no JSON media type names a schema. A
/// schema declared only under a wildcard still governs a reply of a media type
/// the response never names; the reply it does not govern is one naming a
/// media type the response names while keeping no schema for it, see
/// [`ResponseBody::declared_media`].
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
    let declared = response_content(content);
    if declared.schemas.is_empty() {
        return ResponseBody::NoJsonSchema;
    }
    ResponseBody::Json {
        schemas: declared.schemas,
        declared_media: declared.declared_media,
    }
}

/// What one response's `content` map declares: the schemas a reply is held
/// to, and the media types it declares that keep no schema of their own.
struct Declared {
    schemas: std::collections::BTreeMap<String, Value>,
    declared_media: Vec<String>,
}

/// What a response's body is held to under a `content` map: every JSON media
/// type's schema, as `json_schemas` keeps them, which a wildcard never
/// overrides; else the schema of the most specific wildcard media type that
/// names one, so that a response declaring a schema only under `*/*` or
/// `application/*` holds its body to it. A content map with neither declares
/// a body of a shape nothing describes. Either way the media types that name
/// no schema are kept along with them, because they are declared all the same.
fn response_content(content: &Value) -> Declared {
    let mut schemas = json_schemas(content);
    if schemas.is_empty() {
        schemas = wildcard_content(content);
    }
    Declared {
        schemas,
        declared_media: media_with_no_kept_schema(content),
    }
}

/// The media types a `content` map declares that keep no schema for atomic:
/// those whose entry names no schema, and those atomic reads no body from at
/// all (`text/plain`, whatever it names — a `text/plain` of `type: string`
/// names a schema for a body atomic never reads, and is kept out all the
/// same). Wildcards are never among them — no reply names one, and a wildcard
/// is the schema kept for exactly the media types the map does not name. A
/// reply whose `Content-Type` names one of these is a reply the spec declares
/// without giving atomic a shape to hold it to, which is not the same as a
/// reply the spec never mentions: the executor decides what to hold each of
/// them to.
fn media_with_no_kept_schema(content: &Value) -> Vec<String> {
    let Some(content) = content.as_object() else {
        return Vec::new();
    };
    let mut media: Vec<String> = content
        .iter()
        .filter(|(media, entry)| {
            !is_wildcard_media(media) && (!is_json_media(media) || entry.get("schema").is_none())
        })
        .map(|(media, _)| (*media).clone())
        .collect();
    media.sort();
    media
}

/// Select one request representation and keep its declared Content-Type.
/// Prefer plain application/json, then a parameterized application/json,
/// then an application subtype with the +json suffix. Without a unique
/// choice at that priority the tool cannot decide which contract to use.
fn json_content(content: &Value) -> Result<Option<(&str, &Value)>, String> {
    let Some(entries) = content.as_object() else {
        return Ok(None);
    };
    let mut choices: Vec<_> = entries
        .iter()
        .filter(|(media, _)| is_json_request_media(media))
        .map(|(media, entry)| {
            let rank = match (media_essence(media).as_str(), media.contains(';')) {
                ("application/json", false) => 0,
                ("application/json", true) => 1,
                _ => 2,
            };
            (rank, media, entry)
        })
        .collect();
    choices.sort_by_key(|(rank, _, _)| *rank);
    let Some(&(rank, media, entry)) = choices.first() else {
        return Ok(None);
    };
    if choices.get(1).is_some_and(|next| next.0 == rank) {
        return Err("request body has several equally preferred JSON media types; select one representation in the spec".into());
    }
    Ok(entry.get("schema").map(|schema| (media.as_str(), schema)))
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
    types.sort(); // in a stable order, so one schema per media type is kept
    types
        .iter()
        .filter_map(|k| {
            content[k.as_str()]
                .get("schema")
                .map(|schema| ((*k).clone(), schema.clone()))
        })
        .collect()
}

/// How specific one wildcard media type is, for picking which of several a
/// response's body is held to: `application/*` says the body is of one group,
/// `*/*` says it is of any, so a schema written under the first is the closer
/// promise. The smaller the rank, the closer. `None` for a media type that is
/// no wildcard at all.
fn wildcard_rank(media: &str) -> Option<u8> {
    let essence = media_essence(media);
    let application_group = essence == "application/*";
    if application_group {
        Some(0)
    } else if essence == "*/*" {
        Some(1)
    } else {
        None
    }
}

/// The schema under the most specific wildcard media type in a `content` map
/// that names one, keyed by that media type as the map writes it. Sorted by
/// name alone, `*/*` comes before `application/*`, which is the loosest order:
/// the wildcards are ranked by how specific they are, so a map declaring a
/// schema under both holds its body to the `application/*` one. Where two
/// wildcards rank the same, the media type as written decides.
fn wildcard_content(content: &Value) -> std::collections::BTreeMap<String, Value> {
    let mut out = std::collections::BTreeMap::new();
    let Some(content) = content.as_object() else {
        return out;
    };
    let mut keys: Vec<&String> = content.keys().filter(|k| is_wildcard_media(k)).collect();
    keys.sort();
    keys.sort_by_key(|k| wildcard_rank(k).unwrap_or(u8::MAX));
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
            let mut request_media = None;
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
                let mut schema = if let Some(content) = parameter.get("content") {
                    if ["schema", "style", "explode"]
                        .iter()
                        .any(|key| parameter.get(*key).is_some())
                    {
                        return Err(fail(
                            &at,
                            format!(
                                "parameter `{field}` mixes content with schema or style serialization"
                            ),
                        ));
                    }
                    let entries = content.as_object().filter(|entries| entries.len() == 1)
                        .ok_or_else(|| fail(&at, format!("parameter `{field}` must declare exactly one content media type")))?;
                    let (media, entry) = entries.iter().next().expect("one media type");
                    if !is_json_media(media) {
                        unsupported = Some(format!(
                            "parameter `{field}` uses unsupported content media type `{media}`"
                        ));
                    }
                    styles.insert(field.to_owned(), json!({"content": media}));
                    convert(entry.get("schema").unwrap_or(&json!({})), &at)?
                } else {
                    match parameter.get("schema") {
                        Some(schema) => convert(schema, &at)?,
                        None => json!({}),
                    }
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
                let json = match json_content(&body["content"]) {
                    Ok(selected) => selected,
                    Err(reason) => {
                        unsupported = Some(reason);
                        None
                    }
                };
                if unsupported.is_none() && json.is_none() && body["required"] == true {
                    let mut declared: Vec<&String> = body["content"]
                        .as_object()
                        .map(|c| c.keys().collect())
                        .unwrap_or_default();
                    declared.sort();
                    let declared: Vec<&str> = declared.iter().map(|t| t.as_str()).collect();
                    unsupported = Some(format!(
                        "its required request body is {}; atomic sends JSON request bodies only",
                        if declared.is_empty() {
                            "untyped".to_owned()
                        } else {
                            declared.join(", ")
                        }
                    ));
                }
                if let Some((media, schema)) = json {
                    request_media = Some(media.to_owned());
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
            // What every success a reply can reach describes, not only the
            // first: a `200` and a `202` of different shapes both describe the
            // tool's output, as `anyOf`. A key naming no status, such as a
            // bare `2`, is what no status selects, so it describes nothing.
            project_direction(&mut input, "readOnly").map_err(|message| fail(&at, message))?;
            for body in kept.values_mut().flatten() {
                for schema in body.schemas_mut() {
                    project_direction(schema, "writeOnly").map_err(|message| fail(&at, message))?;
                }
            }
            let output = declared_output(&kept);
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
                request_media,
                input,
                output,
                responses: kept,
                unsupported,
            });
        }
    }
    Ok(out)
}

/// Project OpenAPI property visibility into an ordinary JSON Schema contract.
/// A forbidden property is optional and has the false schema, so an open
/// object cannot accept it as an additional property.
fn project_direction(schema: &mut Value, forbidden: &str) -> Result<(), String> {
    let root = schema.clone();
    directional_schema(schema, &root, forbidden, &BTreeSet::new())
}

fn directional_schema(
    schema: &mut Value,
    root: &Value,
    forbidden: &str,
    inherited: &BTreeSet<String>,
) -> Result<(), String> {
    let mut hidden = inherited.clone();
    add_hidden_properties(schema, root, forbidden, &mut hidden);
    // A shared requirement belongs to every caller of the component. Reject
    // the conflicting use instead of weakening requirements for other callers.
    if let Some(target) = local_target(schema, root)
        && !hidden.is_empty()
    {
        let mut own = BTreeSet::new();
        add_hidden_properties(target, root, forbidden, &mut own);
        for part in conjuncts(target, root) {
            let required = part.get("required").and_then(Value::as_array);
            if let Some(name) = required
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .find(|name| hidden.contains(*name) && !own.contains(*name))
            {
                return Err(format!(
                    "a referenced component requires `{name}`, marked {forbidden} outside that component; put its visibility and requirement in the same component"
                ));
            }
        }
    }
    let Some(result) = schema.as_object_mut() else {
        return Ok(());
    };
    let separate = BTreeSet::new();
    for (key, value) in result.iter_mut() {
        match key.as_str() {
            "properties" | "patternProperties" | "dependentSchemas" | "$defs" | "definitions" => {
                if let Some(fields) = value.as_object_mut() {
                    for (name, child) in fields {
                        if key == "properties" && hidden.contains(name) {
                            *child = Value::Bool(false);
                        } else {
                            directional_schema(child, root, forbidden, &separate)?;
                        }
                    }
                }
            }
            "allOf" | "oneOf" | "anyOf" | "prefixItems" => {
                if let Some(children) = value.as_array_mut() {
                    for child in children {
                        let same_object = if key == "allOf" { &hidden } else { &separate };
                        directional_schema(child, root, forbidden, same_object)?;
                    }
                }
            }
            "items"
            | "not"
            | "additionalProperties"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "contains"
            | "if"
            | "then"
            | "else"
            | "propertyNames" => {
                directional_schema(value, root, forbidden, &separate)?;
            }
            _ => {}
        }
    }
    if !hidden.is_empty()
        && let Some(required) = result.get_mut("required").and_then(Value::as_array_mut)
    {
        required.retain(|name| name.as_str().is_none_or(|name| !hidden.contains(name)));
    }
    Ok(())
}

fn local_target<'a>(schema: &Value, root: &'a Value) -> Option<&'a Value> {
    let pointer = schema.get("$ref")?.as_str()?.strip_prefix('#')?;
    root.pointer(pointer)
}

/// Every unconditional conjunct describes the same instance, including a
/// referenced component. A reference can be recursive, so follow each target once.
fn conjuncts<'a>(schema: &'a Value, root: &'a Value) -> impl Iterator<Item = &'a Value> {
    let mut pending = vec![schema];
    let mut followed = BTreeSet::new();
    std::iter::from_fn(move || {
        let schema = pending.pop()?;
        if let Some(target) = local_target(schema, root)
            && followed.insert(std::ptr::from_ref(target))
        {
            pending.push(target);
        }
        if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
            pending.extend(parts);
        }
        Some(schema)
    })
}

fn add_hidden_properties(
    schema: &Value,
    root: &Value,
    forbidden: &str,
    hidden: &mut BTreeSet<String>,
) {
    for part in conjuncts(schema, root) {
        if let Some(properties) = part.get("properties").and_then(Value::as_object) {
            for (name, property) in properties {
                if conjuncts(property, root).any(|s| s.get(forbidden) == Some(&Value::Bool(true))) {
                    hidden.insert(name.clone());
                }
            }
        }
    }
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
