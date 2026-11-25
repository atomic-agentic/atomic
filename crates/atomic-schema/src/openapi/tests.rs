// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::registry::Registry;
use std::path::Path;

fn yaml(text: &str) -> Value {
    crate::yaml::load(Path::new("spec.yaml"), text)
        .expect("yaml")
        .value
}

/// What a response that names exactly one media type keeps: that schema,
/// under the media type as the spec wrote it, and no other media type beside
/// it.
fn one(media: &str, schema: Value) -> ResponseBody {
    ResponseBody::Json {
        schemas: [(media.to_owned(), schema)].into_iter().collect(),
        declared_media: Vec::new(),
    }
}

/// The media types a response keeps a schema under, as it keeps them.
fn kept_media(body: &ResponseBody) -> Vec<&str> {
    body.schemas().map(|(media, _)| media.as_str()).collect()
}

/// The other media types a response declares, the ones it keeps no schema for,
/// as it keeps them.
fn unkept_media(body: &ResponseBody) -> &[String] {
    body.declared_media()
}

/// What a tool's `output` describes, one item per schema it says a reply may
/// have: the schema itself where it says one, the distinct schemas of each
/// success that declares one where it says several. What a member carries to
/// stand as a schema document of its own — the `$defs` its references reach
/// into, and the `$id` that makes a reference inside it mean that member and
/// not the union — is dropped from each, being nothing about the shape a schema
/// describes. The tests that care about them say so of the `output` itself.
fn described(output: Option<&Value>) -> Vec<Value> {
    let Some(output) = output else {
        return Vec::new();
    };
    let parts = |schema: &Value| {
        let mut schema = schema.clone();
        if let Value::Object(map) = &mut schema {
            map.remove("$defs");
            map.remove("$id");
        }
        schema
    };
    match output.get("anyOf").and_then(Value::as_array) {
        Some(members) => members.iter().map(parts).collect(),
        None => vec![parts(output)],
    }
}

/// What `op` keeps beside the response key `code`, which must be there and
/// must keep something: only a status whose reply atomic never reads keeps
/// nothing beside its key.
fn kept<'a>(op: &'a Operation, code: &str) -> &'a ResponseBody {
    op.responses
        .get(code)
        .unwrap_or_else(|| panic!("`{code}` is not declared"))
        .as_ref()
        .unwrap_or_else(|| panic!("`{code}` keeps nothing beside its key"))
}

/// One forecast API, written once for OpenAPI 3.0 and once for 3.1, in
/// each version's own dialect.
const V30: &str = r##"openapi: 3.0.3
info: { title: Forecast, version: "1" }
paths:
  /v1/forecast:
    parameters:
      - { name: latitude, in: query, required: true, schema: { type: number, minimum: -90, maximum: 90 } }
    get:
      operationId: getForecast
      summary: Current conditions for one place.
      parameters:
        - { name: longitude, in: query, required: true, schema: { type: number, minimum: -180, maximum: 180 } }
        - { name: units, in: query, schema: { type: string, enum: [metric, imperial], nullable: true } }
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: { $ref: "#/components/schemas/Forecast" }
  /v1/reports/{city}:
    post:
      operationId: postReport
      parameters:
        - $ref: "#/components/parameters/City"
      requestBody:
        $ref: "#/components/requestBodies/Report"
      responses:
        "201": { $ref: "#/components/responses/Created" }
components:
  parameters:
    City: { name: city, in: path, description: The city's name., schema: { type: string } }
  requestBodies:
    Report:
      required: true
      content:
        application/json:
          schema:
            type: object
            required: [text]
            properties:
              text: { type: string, example: Mild and breezy. }
              wind: { type: number, minimum: 0, exclusiveMinimum: true }
  responses:
    Created:
      description: created
      content:
        application/json:
          schema: { type: object, properties: { id: { type: string } } }
  schemas:
    Forecast:
      type: object
      required: [temperature_c]
      properties:
        temperature_c: { type: number }
        note: { type: string, nullable: true }
"##;

const V31: &str = r##"openapi: 3.1.0
info: { title: Forecast, version: "1" }
paths:
  /v1/forecast:
    parameters:
      - { name: latitude, in: query, required: true, schema: { type: number, minimum: -90, maximum: 90 } }
    get:
      operationId: getForecast
      summary: Current conditions for one place.
      parameters:
        - { name: longitude, in: query, required: true, schema: { type: number, minimum: -180, maximum: 180 } }
        - { name: units, in: query, schema: { type: [string, "null"], enum: [metric, imperial, null] } }
      responses:
        "200":
          description: ok
          content:
            application/json:
              schema: { $ref: "#/components/schemas/Forecast" }
  /v1/reports/{city}:
    post:
      operationId: postReport
      parameters:
        - $ref: "#/components/parameters/City"
      requestBody:
        $ref: "#/components/requestBodies/Report"
      responses:
        "201": { $ref: "#/components/responses/Created" }
components:
  parameters:
    City: { name: city, in: path, description: The city's name., schema: { type: string } }
  requestBodies:
    Report:
      required: true
      content:
        application/json:
          schema:
            type: object
            required: [text]
            properties:
              text: { type: string, examples: [Mild and breezy.] }
              wind: { type: number, exclusiveMinimum: 0 }
  responses:
    Created:
      description: created
      content:
        application/json:
          schema: { type: object, properties: { id: { type: string } } }
  schemas:
    Forecast:
      type: object
      required: [temperature_c]
      properties:
        temperature_c: { type: number }
        note: { type: [string, "null"] }
"##;

#[test]
fn a_3_0_and_a_3_1_spec_of_one_api_give_the_same_tools() {
    let v30 = operations(&yaml(V30)).expect("3.0");
    let v31 = operations(&yaml(V31)).expect("3.1");
    assert_eq!(v30, v31);
    let names: Vec<_> = v30.iter().map(|op| op.name.as_str()).collect();
    assert_eq!(names, ["getForecast", "postReport"]);
}

#[test]
fn unquoted_status_codes_import_the_same_as_quoted_ones() {
    // Specs commonly write the responses map with the status codes unquoted.
    let unquoted = V31.replace("\"200\":", "200:").replace("\"201\":", "201:");
    assert_ne!(unquoted, V31, "the fixture holds no status code to unquote");
    let quoted = operations(&yaml(V31)).expect("quoted codes");
    let same = operations(&yaml(&unquoted)).expect("unquoted codes");
    assert_eq!(same, quoted);
    // The code is still the string a caller looks a status up by.
    assert_eq!(
        same[0]
            .responses
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["200"]
    );
    assert_eq!(same[0].response(200).map(|(k, _)| k), Some("200"));
    assert_eq!(same[1].response(201).map(|(k, _)| k), Some("201"));
}

#[test]
fn an_operation_becomes_one_input_object_with_its_parameters_and_body() {
    let ops = operations(&yaml(V30)).expect("spec");
    let forecast = &ops[0];
    assert_eq!(
        (forecast.method.as_str(), forecast.path.as_str()),
        ("get", "/v1/forecast")
    );
    assert_eq!(
        forecast.description.as_deref(),
        Some("Current conditions for one place.")
    );
    assert_eq!(forecast.input["required"], json!(["latitude", "longitude"]));
    assert_eq!(forecast.locations["units"], "query");
    assert_eq!(
        forecast.input["properties"]["units"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        forecast.output.as_ref().expect("output")["$ref"],
        "#/$defs/Forecast"
    );
    let report = &ops[1];
    assert_eq!(report.input["required"], json!(["city", "body"]));
    assert_eq!(report.locations["city"], "path");
    assert_eq!(
        report.input["properties"]["city"]["description"],
        "The city's name."
    );
    assert_eq!(
        report.input["properties"]["body"]["properties"]["wind"]["exclusiveMinimum"],
        0
    );
    // Every schema compiles, and the input validates what the API takes.
    let registry = Registry::new();
    for op in &ops {
        let validator = registry.validator_for(&op.input).expect("input schema");
        if op.name == "getForecast" {
            assert!(
                validator.is_valid(&json!({ "latitude": 38.7, "longitude": -9.1, "units": null }))
            );
            assert!(!validator.is_valid(&json!({ "latitude": 98, "longitude": -9.1 })));
            assert!(!validator.is_valid(&json!({ "latitude": 1, "longitude": 1, "extra": 1 })));
        }
        for schema in op.output.iter() {
            registry.validator_for(schema).expect("output schema");
        }
        // Every schema a response declares, not only the first: the executor
        // checks a reply against whichever media type it names.
        for body in op.responses.values().flatten() {
            for (_, schema) in body.schemas() {
                registry.validator_for(schema).expect("response schema");
            }
        }
    }
}

#[test]
fn unsupported_specs_and_ambiguous_operations_are_refused() {
    for (spec, says) in [
        ("swagger: '2.0'\npaths: {}\n", "Swagger 2.0"),
        ("openapi: 4.0.0\npaths: {}\n", "3.0.x or 3.1.x"),
        (
            "openapi: 3.1.0\npaths:\n  /a:\n    get: { operationId: x }\n  /b:\n    get: { operationId: x }\n",
            "two operations are named `x`",
        ),
        (
            "openapi: 3.1.0\npaths:\n  /a:\n    get:\n      parameters:\n        - { $ref: '#/components/parameters/Missing' }\n",
            "does not resolve",
        ),
        (
            "openapi: 3.0.3\npaths:\n  /a:\n    get:\n      parameters:\n        - { name: n, in: query, schema: { type: number, exclusiveMinimum: true } }\n",
            "without `minimum`",
        ),
    ] {
        let error = operations(&yaml(spec)).expect_err(spec);
        assert!(error.message.contains(says), "{spec}: {error}");
    }
    // An operation with no operationId is named from its method and path.
    let ops = operations(&yaml(
        "openapi: 3.1.0\npaths:\n  /v1/items/{id}:\n    delete: {}\n",
    ))
    .expect("spec");
    assert_eq!(ops[0].name, "delete_v1_items__id");
}

#[test]
fn reserved_headers_styles_and_media_types_follow_openapi() {
    let spec = yaml(
        r#"openapi: 3.1.0
paths:
  /a:
    post:
      operationId: a
      parameters:
        - { name: Authorization, in: header, schema: { type: string } }
        - { name: accept, in: header, schema: { type: string } }
        - { name: tags, in: query, schema: { type: array } }
        - { name: ids, in: query, style: pipeDelimited, explode: false, schema: { type: array } }
        - { name: id, in: path, required: true, schema: { type: string } }
      requestBody: { required: true, content: { application/vnd.api+json: { schema: { type: object } } } }
      responses:
        "200": { description: ok, content: { "application/json; charset=utf-8": { schema: { type: integer } } } }
  /b:
    put:
      operationId: b
      requestBody: { required: true, content: { text/plain: { schema: { type: string } } } }
"#,
    );
    let ops = operations(&spec).expect("spec");
    let a = &ops[0];
    assert!(a.locations.get("Authorization").is_none() && a.locations.get("accept").is_none());
    assert_eq!(
        a.styles["tags"],
        json!({ "style": "form", "explode": true })
    );
    assert_eq!(
        a.styles["ids"],
        json!({ "style": "pipeDelimited", "explode": false })
    );
    assert_eq!(
        a.styles["id"],
        json!({ "style": "simple", "explode": false })
    );
    assert_eq!(a.locations["body"], "body");
    assert_eq!(a.output, Some(json!({ "type": "integer" })));
    assert_eq!(a.unsupported, None);
    assert!(
        ops[1]
            .unsupported
            .as_deref()
            .is_some_and(|r| r.contains("text/plain"))
    );
    let bad = yaml(
        "openapi: 3.1.0\npaths:\n  /a:\n    get:\n      parameters:\n        - { name: n, in: header, style: form }\n",
    );
    assert!(
        operations(&bad)
            .expect_err("style")
            .message
            .contains("cannot have style")
    );
}

#[test]
fn each_response_is_kept_by_its_status_and_a_success_picks_its_own() {
    let spec = yaml(
        r##"openapi: 3.1.0
info: { title: Jobs, version: "1" }
paths:
  /jobs:
    post:
      operationId: submit
      responses:
        "200": { description: done, content: { application/json: { schema: { type: object, required: [result], properties: { result: { type: string } } } } } }
        "202": { $ref: "#/components/responses/Queued" }
        "204": { description: nothing to do }
        "2xx": { description: other, content: { application/json: { schema: { type: string } } } }
        "404": { description: no such job, content: { application/json: { schema: { type: object } } } }
        default: { description: error, content: { application/json: { schema: { type: number } } } }
components:
  schemas:
    Ticket: { type: object, required: [ticket], properties: { ticket: { type: integer } } }
  responses:
    Queued: { description: queued, content: { application/json: { schema: { $ref: "#/components/schemas/Ticket" } } } }
"##,
    );
    let ops = operations(&spec).expect("spec");
    let op = &ops[0];
    // Every declared response is kept, a failure's too, but a failure keeps
    // nothing beside its key: its schema is never read, so it is never what
    // makes the import fail.
    assert_eq!(
        op.responses.keys().map(String::as_str).collect::<Vec<_>>(),
        ["200", "202", "204", "2xx", "404", "default"]
    );
    assert_eq!(
        op.responses.get("404"),
        Some(&None),
        "the 404's schema is not kept"
    );
    assert_eq!(
        kept(op, "200").schema().map(|s| s["required"].clone()),
        Some(json!(["result"])),
        "the success's schema is kept"
    );
    let picked = |status: u16| op.response(status).map(|(key, body)| (key, body.clone()));
    assert_eq!(
        picked(200).map(|(k, b)| (k, b.schema().map(|s| s["required"].clone()))),
        Some(("200", Some(json!(["result"]))))
    );
    assert_eq!(
        picked(204),
        Some(("204", ResponseBody::NoBody)),
        "no content at all"
    );
    assert_eq!(
        picked(201).map(|(k, b)| (k, b.schema().map(|s| s["type"].clone()))),
        Some(("2xx", Some(json!("string")))),
        "a range, in either case"
    );
    // 299 has no code of its own, but it is inside the `2xx` range, which is
    // what picks it; only a success outside every code and every range takes
    // `default`, as the spec after this one shows. A failure is never picked,
    // whatever it declares.
    assert_eq!(picked(299).map(|(k, _)| k), Some("2xx"));
    let only_codes = operations(&yaml(
        r#"openapi: 3.1.0
info: { title: Jobs, version: "1" }
paths:
  /jobs:
    get:
      responses:
        "200": { description: ok }
        default: { description: error, content: { application/json: { schema: { type: number } } } }
"#,
    ))
    .expect("spec");
    assert_eq!(only_codes[0].response(201).map(|(k, _)| k), Some("default"));
    assert_eq!(op.response(404), None);
    assert_eq!(op.response(500), None);
    // A response by reference resolves to its schema, which reaches a
    // component schema through $defs that the response carries.
    let (key, queued) = op.response(202).expect("202");
    assert_eq!(key, "202");
    let queued = queued.schema().expect("a schema");
    assert!(queued.get("$defs").is_some(), "{queued}");
    let validator = Registry::new().validator_for(queued).expect("compiles");
    assert!(validator.is_valid(&json!({ "ticket": 7 })));
    assert!(!validator.is_valid(&json!({ "result": "done" })));
    // The tool's output describes every success a reply can be checked
    // against, not only the first: its own `200`, the `2xx` a 201 or a 299
    // falls to, and the `202`, in status order. The `204` declares no body and
    // so says nothing about a shape, and no success reaches `default` while a
    // `2XX` range covers them, so its number describes nothing here.
    assert_eq!(
        described(op.output.as_ref()),
        vec![
            json!({
                "type": "object",
                "required": ["result"],
                "properties": { "result": { "type": "string" } }
            }),
            json!({ "type": "string" }),
            json!({ "$ref": "#/$defs/Ticket" }),
        ],
        "{}",
        op.output.as_ref().expect("a union of the successes")
    );
}

// What a response declares about its body comes in three kinds, and they
// are not the same claim. Only a response with no `content` key says there
// is no body to read; one whose content names no schema atomic reads a body
// against — a media type atomic does not read, whether or not it names a
// schema of its own, a wildcard with no `schema`, a JSON media type with
// no `schema` — declares a body atomic has no schema to check. The two are
// kept apart because only the first lets atomic refuse a body. A wildcard
// that does name a schema is the response's own claim about its body, which
// may be JSON, so it gives atomic one to check against.
#[test]
fn a_response_says_whether_it_declares_no_body_no_schema_or_a_schema() {
    let ops = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Kinds, version: "1" }
paths:
  /kinds:
    get:
      operationId: kinds
      responses:
        "200": { description: no content key at all }
        "201": { description: any media type, content: { "*/*": { schema: { type: object } } } }
        "202": { description: plain text, content: { text/plain: { schema: { type: string } } } }
        "203": { description: bytes, content: { application/octet-stream: { schema: { type: string } } } }
        "204": { description: json with no schema, content: { application/json: {} } }
        "205": { description: any application body, content: { "application/*": { schema: { type: array } } } }
        "206": { description: a json schema, content: { application/json: { schema: { type: object } } } }
        "207": { description: any application body, no schema, content: { "application/*; charset=utf-8": {} } }
"##,
    ))
    .expect("spec");
    let op = &ops[0];
    let declared = |code: &str| kept(op, code);
    assert_eq!(*declared("200"), ResponseBody::NoBody);
    assert_eq!(
        *declared("201"),
        one("*/*", json!({ "type": "object" })),
        "`*/*` names the schema its body has to fit"
    );
    assert_eq!(
        *declared("202"),
        ResponseBody::NoJsonSchema,
        "text/plain is not JSON"
    );
    assert_eq!(
        *declared("203"),
        ResponseBody::NoJsonSchema,
        "octet-stream is not JSON"
    );
    assert_eq!(
        *declared("204"),
        ResponseBody::NoJsonSchema,
        "a JSON media type with no `schema`"
    );
    assert_eq!(
        *declared("205"),
        one("application/*", json!({ "type": "array" })),
        "`application/*` names the schema its body has to fit"
    );
    assert_eq!(
        declared("206").schema(),
        Some(&json!({ "type": "object" })),
        "a JSON media type with a schema gives one"
    );
    assert_eq!(
        *declared("207"),
        ResponseBody::NoJsonSchema,
        "a wildcard with no `schema` declares a body nothing describes"
    );
    // Selection by status is the test above; what it hands over is one of
    // these three. What the tool's output describes is all of them together:
    // the two responses naming a schema describe a reply of either shape, and
    // the five that name none — the one with no `content` and the four with a
    // `content` map that names no schema — add nothing, so they neither
    // describe a shape nor widen what the output accepts.
    assert_eq!(
        described(op.output.as_ref()),
        vec![json!({ "type": "object" }), json!({ "type": "array" })],
        "{}",
        op.output.as_ref().expect("a union of what it describes")
    );
}

// Which media type's schema a response is held to. A response declaring both
// a specific JSON media type and a wildcard is held to the specific one's;
// one whose JSON media type names no schema is held to the wildcard's, since
// that is the only schema it declares. The wildcard's schema never reaches a
// request body: atomic sends a JSON request body under a JSON media type, so
// an operation whose request body is declared only under `*/*` stays
// unsupported, with no `body` in its input.
#[test]
fn a_specific_json_media_type_beats_a_wildcard_and_a_wildcard_names_no_request_body() {
    let ops = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Mixed, version: "1" }
paths:
  /both:
    get:
      operationId: both
      responses:
        "200":
          description: a json media type and a wildcard, each with a schema
          content:
            "*/*": { schema: { type: string } }
            application/json: { schema: { type: object, required: [specific] } }
  /jsonless:
    get:
      operationId: jsonless
      responses:
        "200":
          description: a json media type with no schema, a wildcard with one
          content:
            application/json: {}
            "application/*": { schema: { type: integer } }
  /wild:
    post:
      operationId: wild
      requestBody: { required: true, content: { "*/*": { schema: { type: object } } } }
"##,
    ))
    .expect("spec");
    let specific = json!({ "type": "object", "required": ["specific"] });
    let both = &ops[0];
    assert_eq!(
        both.response(200).expect("200").1.schema(),
        Some(&specific),
        "the specific JSON media type wins over the wildcard"
    );
    assert_eq!(both.output, Some(specific), "and is the tool's output");
    let jsonless = &ops[1];
    assert_eq!(
        jsonless.response(200).expect("200").1.schema(),
        Some(&json!({ "type": "integer" })),
        "the wildcard names the only schema it declares"
    );
    let wild = &ops[2];
    assert!(
        wild.unsupported
            .as_deref()
            .is_some_and(|why| why.contains("*/*")),
        "{:?}",
        wild.unsupported
    );
    assert!(
        wild.input["properties"].get("body").is_none(),
        "{}",
        wild.input
    );
}

// Where a response declares a schema under more than one wildcard, the most
// specific declared key governs. `application/*` says its body is of one group
// and `*/*` says it is of any, so the closer promise is the `application/*`
// one — which is not what sorting the media types by name says, since `*/*`
// sorts first. A specific JSON media type still beats both wildcards, and a
// response declaring only `*/*` is still held to that one.
#[test]
fn the_most_specific_wildcard_declared_governs_a_response() {
    let ops = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Wildcards, version: "1" }
paths:
  /both:
    get:
      operationId: both
      responses:
        "200":
          description: a schema under each wildcard
          content:
            "*/*": { schema: { type: string } }
            "application/*": { schema: { type: object, required: [narrow] } }
  /spelled:
    get:
      operationId: spelled
      responses:
        "200":
          description: the same two, written with parameters and the other way round
          content:
            "application/*; charset=utf-8": { schema: { type: object, required: [narrow] } }
            "*/*": { schema: { type: string } }
  /staronly:
    get:
      operationId: staronly
      responses:
        "200": { description: any body, content: { "*/*": { schema: { type: string } } } }
  /jsontoo:
    get:
      operationId: jsontoo
      responses:
        "200":
          description: a json media type beside both wildcards
          content:
            "*/*": { schema: { type: string } }
            "application/*": { schema: { type: object, required: [narrow] } }
            application/json: { schema: { type: object, required: [specific] } }
"##,
    ))
    .expect("spec");
    let op = |name: &str| {
        ops.iter()
            .find(|op| op.name == name)
            .unwrap_or_else(|| panic!("no operation named `{name}`"))
    };
    let narrow = json!({ "type": "object", "required": ["narrow"] });
    assert_eq!(
        kept_media(kept(op("both"), "200")),
        ["application/*"],
        "`application/*` is the more specific of the two, though `*/*` sorts first"
    );
    assert_eq!(
        kept(op("both"), "200").schema(),
        Some(&narrow),
        "and the schema under it is the one the response is held to"
    );
    assert_eq!(
        kept(op("both"), "200").declared_media(),
        Vec::<String>::new(),
        "the wildcard it passed over names a schema, so it is not among the \
         media types declared with no schema"
    );
    assert_eq!(
        kept_media(kept(op("spelled"), "200")),
        ["application/*; charset=utf-8"],
        "the same, however the more specific one is spelled or written"
    );
    assert_eq!(
        kept_media(kept(op("staronly"), "200")),
        ["*/*"],
        "where only `*/*` is declared it governs its own response"
    );
    assert_eq!(
        kept_media(kept(op("jsontoo"), "200")),
        ["application/json"],
        "and a specific JSON media type beats both wildcards, as it always did"
    );
}

// A `content` map declares bodies it keeps no schema for: media types that name
// no schema, and media types atomic reads no body from at all. They are declared
// all the same, and the executor cannot tell a reply naming one of them from a
// reply naming a media type the response never mentions without being told — the
// first is a body the spec declares and gives atomic no schema to check —
// a `text/plain` of `type: string` is such a media type, its schema describing
// a body atomic never reads — and another media
// type's schema, a wildcard's above all, is not its contract.
#[test]
fn a_response_keeps_the_media_types_it_declares_that_keep_no_schema() {
    let ops = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Declared, version: "1" }
paths:
  /mixed:
    get:
      operationId: mixed
      responses:
        "200":
          description: plain text with nothing beside it, and any body with a schema
          content:
            text/plain: {}
            "application/*; charset=utf-8": { schema: { type: object } }
        "201":
          description: a json media type with no schema, and one with
          content:
            application/json: {}
            application/problem+json: { schema: { type: object } }
        "202":
          description: two media types, each naming a schema of its own
          content:
            text/plain: { schema: { type: string } }
            application/json: { schema: { type: object } }
        "203":
          description: media types that name no schema, only
          content:
            text/plain: {}
            "application/*": {}
"##,
    ))
    .expect("spec");
    let op = &ops[0];
    assert_eq!(
        kept_media(kept(op, "200")),
        ["application/*; charset=utf-8"],
        "the only schema it declares"
    );
    assert_eq!(
        unkept_media(kept(op, "200")),
        ["text/plain"],
        "the media type it declares with no schema, as the spec writes it"
    );
    assert_eq!(
        unkept_media(kept(op, "201")),
        ["application/json"],
        "a JSON media type with no `schema` is declared too"
    );
    assert_eq!(
        unkept_media(kept(op, "202")),
        ["text/plain"],
        "and so is one whose schema atomic never reads, because it reads no \
         body of that media type"
    );
    // A response that keeps no schema at all keeps no list either: nothing
    // describes any of its replies' shapes, so the executor holds none of them
    // to anything and never has to tell one declared media type from another.
    assert_eq!(*kept(op, "203"), ResponseBody::NoJsonSchema);
    assert_eq!(
        kept(op, "203").declared_media(),
        Vec::<String>::new(),
        "and it says so"
    );
}

// A response may declare the shape of its body under more than one JSON media
// type — what it returns normally and the `problem+json` its failures carry,
// say. Every schema it declares is kept, under the media type as the spec
// writes it, because a reply says which of them it is by its `Content-Type`:
// keeping only one would leave a reply of another declared type checked against
// a schema nothing wrote for it. Every one of them describes the tool's output
// too, because a reply may arrive as any of them, and a component schema
// reference resolves in every one of them, not only in the first.
#[test]
fn every_json_media_types_schema_is_kept_for_a_response() {
    let ops = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Many, version: "1" }
paths:
  /thing:
    get:
      operationId: thing
      responses:
        "200":
          description: three json media types, each with its own schema
          content:
            application/json: { schema: { $ref: "#/components/schemas/Thing" } }
            application/problem+json: { schema: { $ref: "#/components/schemas/Problem" } }
            "text/vnd.acme.echo+json; charset=utf-8": { schema: { type: string } }
        "201":
          description: a json media type with no schema beside one with, and a wildcard
          content:
            application/json: {}
            application/vnd.acme.ack+json: { schema: { type: object } }
            "application/*": { schema: { type: integer } }
        "202":
          description: the same type, twice, however the spec spells it
          content:
            "Application/JSON; charset=utf-8": { schema: { type: boolean } }
            application/problem+json: { schema: { type: integer } }
        "203":
          description: a json media type with no schema, only
          content:
            application/json: {}
  /echo:
    post:
      operationId: echoThing
      requestBody: { required: true, content: { "text/vnd.acme.echo+json": { schema: { type: object } } } }
  /ack:
    post:
      operationId: sendAck
      requestBody: { required: true, content: { "application/vnd.acme.ack+json": { schema: { type: object } } } }
components:
  schemas:
    Thing: { type: object, required: [thing], properties: { thing: { type: string } } }
    Problem: { type: object, required: [title], properties: { title: { type: string } } }
"##,
    ))
    .expect("spec");
    let op = ops.iter().find(|op| op.name == "thing").expect("the get");
    let thing = kept(op, "200");
    assert_eq!(
        kept_media(thing),
        [
            "application/json",
            "application/problem+json",
            "text/vnd.acme.echo+json; charset=utf-8",
        ],
        "every media type that names a schema, each as the spec writes it"
    );
    // Each is its own schema, compiles, and carries the components its
    // reference needs — none of them is the first one twice.
    let registry = Registry::new();
    for (_, schema) in thing.schemas() {
        registry.validator_for(schema).expect("compiles");
        assert!(schema.get("$defs").is_some(), "{schema}");
    }
    let (_, problem) = thing
        .schemas()
        .find(|(media, _)| media.as_str() == "application/problem+json")
        .expect("the problem media type keeps its own schema");
    let validator = registry.validator_for(problem).expect("compiles");
    assert!(validator.is_valid(&json!({ "title": "no such thing" })));
    assert!(
        !validator.is_valid(&json!({ "thing": "x" })),
        "the two schemas are the same schema"
    );
    // Every media type of every success describes the output, in status order
    // and then in the order the response names them: a `200` answers as its
    // Thing, its problem or its echo, a `201` as its ack, a `202` as its
    // boolean or its integer. The `203` names no schema, so it describes
    // nothing, and the `application/*` of the `201` is preempted by the JSON
    // media type beside it, so it describes nothing either.
    assert_eq!(
        described(op.output.as_ref()),
        vec![
            json!({ "$ref": "#/$defs/Thing" }),
            json!({ "$ref": "#/$defs/Problem" }),
            json!({ "type": "string" }),
            json!({ "type": "object" }),
            json!({ "type": "boolean" }),
            json!({ "type": "integer" }),
        ],
        "{}",
        op.output
            .as_ref()
            .expect("a union of every success's media")
    );
    // A JSON media type that names no schema adds nothing to keep; the one
    // beside it is kept, and the wildcard stays preempted by any JSON media
    // type that names a schema.
    assert_eq!(
        kept_media(kept(op, "201")),
        ["application/vnd.acme.ack+json"],
        "the only schema it declares"
    );
    // The media type is kept as written, so a spec that spells the same type
    // two ways keeps both, in the order the executor walks them.
    assert_eq!(
        kept_media(kept(op, "202")),
        [
            "Application/JSON; charset=utf-8",
            "application/problem+json"
        ],
        "sorted by what the spec wrote, so the uppercase spelling first"
    );
    // A response whose only JSON media type names no schema still declares a
    // body nothing describes.
    assert_eq!(*kept(op, "203"), ResponseBody::NoJsonSchema);
    // A `+json` type is a JSON media type for a reply whichever tree it sits
    // under, which is what kept the `text/vnd.acme.echo+json` schema above. A
    // request body is read more narrowly: atomic sends every one as
    // `application/json`, so a body declared only under a `+json` type of the
    // `text/` tree is one it would send as something the spec never declared,
    // and the operation stays unsupported with no `body` in its input. A `+json`
    // type of the `application/` tree is one it sends within that family, so
    // that operation stays callable with the schema as its `body`.
    let echo = ops
        .iter()
        .find(|op| op.name == "echoThing")
        .expect("the post");
    assert!(
        echo.unsupported
            .as_deref()
            .is_some_and(|why| why.contains("text/vnd.acme.echo+json")),
        "{:?}",
        echo.unsupported
    );
    assert!(
        echo.input["properties"].get("body").is_none(),
        "{}",
        echo.input
    );
    let ack = ops
        .iter()
        .find(|op| op.name == "sendAck")
        .expect("the other post");
    assert_eq!(ack.unsupported, None);
    assert_eq!(ack.input["properties"]["body"], json!({ "type": "object" }));
}

// atomic never reads the body of a reply that is not a success, so a failure
// response keeps no schema: nothing of it is converted, resolved or compiled.
// That is what keeps a `404` whose reference does not resolve from refusing
// the whole spec — and the operation's own `200`, which is what a reply is
// checked against, is kept and compiles all the same. `default` is the
// exception: a success the spec gives no code for is checked against it, so a
// schema that cannot be resolved there still refuses the spec.
#[test]
fn a_failure_response_keeps_no_schema_so_its_broken_schema_cannot_refuse_the_spec() {
    let ops = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Orders, version: "1" }
paths:
  /orders/{id}:
    get:
      operationId: order
      responses:
        "200":
          description: the order
          content:
            application/json: { schema: { $ref: "#/components/schemas/Order" } }
        "404":
          description: no such order, described by a schema that is not here
          content:
            application/json: { schema: { $ref: "#/components/schemas/Nowhere" } }
        "5XX":
          description: a range, with a body no schema describes
          content:
            application/json: {}
        default:
          description: the problem it carries
          content:
            application/problem+json: { schema: { $ref: "#/components/schemas/Problem" } }
components:
  schemas:
    Order: { type: object, required: [order], properties: { order: { type: string } } }
    Problem: { type: object, required: [title], properties: { title: { type: string } } }
"##,
    ))
    .expect("a broken failure schema does not refuse the spec");
    let op = &ops[0];
    assert_eq!(
        op.responses.keys().map(String::as_str).collect::<Vec<_>>(),
        ["200", "404", "5XX", "default"],
        "each is still declared, so a status the spec gives no success for is refused"
    );
    assert_eq!(op.responses.get("404"), Some(&None));
    assert_eq!(op.responses.get("5XX"), Some(&None));
    assert_eq!(op.response(404), None, "a failure is never checked");
    // What a success is checked against is untouched: its own `200`, and
    // `default` for a success no code names.
    let order = op.response(200).expect("200").1.schema().expect("a schema");
    let registry = Registry::new();
    let validator = registry.validator_for(order).expect("the 200 compiles");
    assert!(validator.is_valid(&json!({ "order": "A-1" })));
    assert!(!validator.is_valid(&json!({ "title": "not an order" })));
    let default = kept(op, "default");
    let validator = registry
        .validator_for(default.schema().expect("a schema"))
        .expect("default compiles");
    assert!(validator.is_valid(&json!({ "title": "no such order" })));
    assert!(!validator.is_valid(&json!({ "order": "A-1" })));
    // The output describes the `200` alone. A `default` describes what a reply
    // may look like only while no success is named beside it; here the `200`
    // names one, so the `default` is what this operation's *failures* fall back
    // to — an error body, not a described reply — even though a success the
    // spec gives no code for is still checked against it, as above. What it
    // does not describe either is the `404`'s: that schema is not kept at all,
    // broken or not.
    assert_eq!(
        described(op.output.as_ref()),
        vec![json!({ "$ref": "#/$defs/Order" })],
        "{}",
        op.output
            .as_ref()
            .expect("the `200`'s schema, not a union with the `default`")
    );
    assert!(
        op.output
            .as_ref()
            .expect("the `200`'s schema")
            .get("anyOf")
            .is_none(),
        "one described schema stands as it is: {}",
        op.output.as_ref().expect("one schema")
    );
    // A 3.0 failure schema is not converted either: what the 3.0 dialect
    // cannot convert stays out of the import, as long as it sits under a
    // failure. The same schema under `default` is refused, because a success
    // is checked against `default`.
    let broken = r##"openapi: 3.0.3
info: { title: Orders, version: "1" }
paths:
  /orders/{id}:
    get:
      operationId: order
      responses:
        "200": { description: ok, content: { application/json: { schema: { type: object } } } }
        "422": { description: unprocessable, content: { application/json: { schema: { type: number, exclusiveMinimum: true } } } }
"##;
    let ops = operations(&yaml(broken)).expect("an unconvertible failure schema is kept out");
    assert_eq!(ops[0].responses.get("422"), Some(&None));
    let error = operations(&yaml(
        r##"openapi: 3.0.3
info: { title: Orders, version: "1" }
paths:
  /orders/{id}:
    get:
      operationId: order
      responses:
        default: { description: unprocessable, content: { application/json: { schema: { type: number, exclusiveMinimum: true } } } }
"##,
    ))
    .expect_err("the same schema under default is converted, and cannot be");
    assert!(
        error.message.contains("without `minimum`"),
        "{error} — a default schema is still converted"
    );
}

// An `x-` key under `responses` is the spec's own note about the operation:
// `x-` is the one prefix OpenAPI writes for a key that is not part of the
// operation, so an extension declares no response. Counting one would make an
// operation that declares only extensions declare a response after all, which
// is what refuses a success the spec never described. Any other key counts,
// however the spec spells it, and one naming no status keeps no schema.
#[test]
fn an_extension_key_under_responses_is_not_a_declared_response() {
    let ops = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Notes, version: "1" }
paths:
  /notes:
    get:
      operationId: onlyExtensions
      responses:
        x-internal: true
        x-openapi: { primaryMediaTypes: [application/json] }
        "200": { description: ok, content: { application/json: { schema: { type: object } } } }
  /nothing:
    get:
      operationId: nothingYet
      responses:
        x-internal: true
  /odd:
    get:
      operationId: oddKey
      responses:
        latest: { description: no status spells this }
"##,
    ))
    .expect("spec");
    let op = &ops[0];
    assert_eq!(
        op.responses.keys().map(String::as_str).collect::<Vec<_>>(),
        ["200"],
        "the extensions are not among what it declares"
    );
    assert_eq!(
        *kept(op, "200"),
        one("application/json", json!({ "type": "object" })),
        "the response it does declare is kept as it was"
    );
    // An operation whose only `responses` key is an extension declares no
    // response at all, which is what an operation with no `responses` map
    // declares too.
    assert!(
        ops[1].responses.is_empty(),
        "{:?}",
        ops[1].responses.keys().collect::<Vec<_>>()
    );
    // A key that is no extension names no status either, but it is still a key
    // of `responses`, so the operation declares a response and a success the
    // spec gives none is refused rather than passed through. It keeps no
    // schema, because it names no reply whose body atomic reads.
    assert_eq!(
        ops[2]
            .responses
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["latest"],
        "only an extension key is skipped"
    );
    assert_eq!(ops[2].responses.get("latest"), Some(&None));
}

// A tool's `output` describes what a reply may look like, and an operation may
// answer with more than one shape: a `200` that carries the finished result and
// a `202` that carries a ticket are both successes, and a reply of either one is
// described. So the output is the distinct schema of every success a reply can
// be checked against — one schema where they agree, unwrapped as it always was,
// `anyOf` of them where they differ. A success that declares no body says
// nothing about a shape: it contributes nothing, and in particular it does not
// make the output accept anything. What a reply is *held to* is still the one
// response its status selects, by the media type its `Content-Type` names; this
// is the description the model and an atom's checks see.
#[test]
fn a_tools_output_is_the_union_of_its_success_responses() {
    let result = json!({
        "type": "object",
        "required": ["result"],
        "properties": { "result": { "type": "string" } }
    });
    let ticket = json!({
        "type": "object",
        "required": ["ticket"],
        "properties": { "ticket": { "type": "integer" } }
    });
    let ops = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Jobs, version: "1" }
paths:
  /jobs:
    post:
      operationId: twoShapes
      responses:
        "200":
          description: done at once
          content:
            application/json: { schema: { type: object, required: [result], properties: { result: { type: string } } } }
        "202":
          description: queued instead
          content:
            application/json: { schema: { type: object, required: [ticket], properties: { ticket: { type: integer } } } }
  /echo:
    post:
      operationId: sameShape
      responses:
        "200":
          description: done at once
          content:
            application/json: { schema: { type: object, required: [result], properties: { result: { type: string } } } }
        "202":
          description: queued, and it describes the same shape
          content:
            application/json: { schema: { type: object, required: [result], properties: { result: { type: string } } } }
  /nothing:
    post:
      operationId: noContent
      responses:
        "204":
          description: accepted, with nothing to return
        "200":
          description: the only success that has a body
          content:
            application/json: { schema: { type: object, required: [result], properties: { result: { type: string } } } }
"##,
    ))
    .expect("spec");
    let by = |name: &str| ops.iter().find(|op| op.name == name).expect(name);
    assert_eq!(
        ops.iter().map(|op| op.name.as_str()).collect::<Vec<_>>(),
        ["sameShape", "twoShapes", "noContent"],
        "three operations, one per shape claim"
    );
    // Two successes, two shapes: both describe the output, in status order,
    // and nothing else does — a third alternative would say a reply of a shape
    // the spec never wrote is described too. Each member is a document of its
    // own, which is what `described` drops; the union holds nothing beside them.
    assert_eq!(
        described(by("twoShapes").output.as_ref()),
        vec![result.clone(), ticket.clone()],
        "{:?}",
        by("twoShapes").output
    );
    assert_eq!(
        by("twoShapes")
            .output
            .as_ref()
            .expect("a union")
            .get("$defs"),
        None,
        "the union carries no `$defs` of its own"
    );
    // Two successes describing one shape: that schema, as it stands, not an
    // `anyOf` holding it once.
    assert_eq!(
        by("sameShape").output,
        Some(result.clone()),
        "{:?}",
        by("sameShape").output
    );
    // A success declaring no body describes no shape, so the one that does is
    // the whole output — the `204` neither adds a member nor widens it.
    assert_eq!(
        by("noContent").output,
        Some(result.clone()),
        "{:?}",
        by("noContent").output
    );
}

// The union is a schema like any other: it must compile, and what it accepts is
// exactly one of the shapes the successes declare. Each member keeps the
// components it references, and an `$id` stands as their base — one of atomic's
// here, because the spec named none; a member the spec did name keeps that
// name, which the last test in this file shows. Either way a reference written
// inside a member means that member, never the union
// it sits in, so the union holds no `$defs` for a member to reach by accident.
// What a reply is held to is one member alone, where it is the whole document
// already.
#[test]
fn the_union_of_component_schemas_validates_a_reply_of_either_shape() {
    let ops = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Orders, version: "1" }
paths:
  /orders:
    post:
      operationId: order
      responses:
        "200":
          description: the order
          content:
            application/json: { schema: { $ref: "#/components/schemas/Order" } }
        "202":
          description: the order, still being made
          content:
            application/json: { schema: { $ref: "#/components/schemas/Queued" } }
components:
  schemas:
    Order: { type: object, required: [order], properties: { order: { type: string } } }
    Queued: { type: object, required: [ticket], properties: { ticket: { type: integer } } }
"##,
    ))
    .expect("spec");
    let op = &ops[0];
    let union = op.output.as_ref().expect("a union of the two successes");
    let components = json!({
        "Order": { "type": "object", "required": ["order"], "properties": { "order": { "type": "string" } } },
        "Queued": { "type": "object", "required": ["ticket"], "properties": { "ticket": { "type": "integer" } } },
    });
    assert_eq!(
        union["anyOf"],
        json!([
            { "$id": "urn:atomic:response:0", "$ref": "#/$defs/Order", "$defs": components.clone() },
            { "$id": "urn:atomic:response:1", "$ref": "#/$defs/Queued", "$defs": components.clone() },
        ]),
        "each member keeps its own copy of the components, and an id of its own \
         to reference them from"
    );
    assert_eq!(
        union.get("$defs"),
        None,
        "the union holds no components of its own, so a member that resolved a \
         reference against the union rather than itself would not compile"
    );
    let validator = Registry::new()
        .validator_for(union)
        .expect("the union compiles, references and all");
    assert!(
        validator.is_valid(&json!({ "order": "A-1" })),
        "the shape a `200` answers with"
    );
    assert!(
        validator.is_valid(&json!({ "ticket": 7 })),
        "the shape a `202` answers with"
    );
    assert!(
        !validator.is_valid(&json!({ "other": 1 })),
        "a body matching neither shape is not described by them"
    );
    // What a reply is held to is unchanged: the response its status selects,
    // not the union that describes them all.
    let held = |status: u16| {
        let schema = op
            .response(status)
            .expect("a response")
            .1
            .schema()
            .expect("a schema");
        let validator = Registry::new().validator_for(schema).expect("compiles");
        validator.is_valid(&json!({ "order": "A-1" }))
    };
    assert!(held(200), "a `200` body fits the `200`");
    assert!(!held(202), "the same body does not fit the `202`");
}

// A `default` describes what a reply may look like only when nothing names a
// success beside it. OpenAPI falls back to it for any success the spec gives no
// code for, so a spec that declares no success at all means its `default` as the
// shape a success takes, and every media type it names one under describes the
// output. Once the spec names a success — a code from `200` to `299`, or a `2XX`
// range, either case — every success is covered by one of those, and the
// `default` is what the spec's *failures* fall back to: an error body, which no
// reply the tool describes carries. It is still the response a success no code
// names is checked against, so what it declares is still converted, kept and
// compiled all the same; it just describes nothing.
#[test]
fn a_default_describes_the_output_only_when_no_success_is_named() {
    let spec = |responses: &str| {
        let text = format!(
            r##"openapi: 3.1.0
info: {{ title: Shaded, version: "1" }}
paths:
  /thing:
    get:
      operationId: thing
      responses:
{responses}
"##
        );
        operations(&yaml(&text)).expect("spec")
    };
    let named = |responses: &str| {
        let ops = spec(responses);
        let op = &ops[0];
        (
            described(op.output.as_ref()),
            op.responses
                .get("default")
                .expect("the default is declared")
                .as_ref()
                .expect("and kept")
                .schemas()
                .map(|(media, _)| media.to_owned())
                .collect::<Vec<String>>(),
        )
    };
    // Nothing names a success, so the `default` is what every success takes,
    // and both the media types it declares describe a reply.
    let (said, medias) = named(
        "        default:\n          description: the thing, or the problem\n          content:\n            application/json: { schema: { type: object } }\n            application/problem+json: { schema: { type: string } }\n",
    );
    assert_eq!(
        said,
        vec![json!({ "type": "object" }), json!({ "type": "string" })],
        "the `default` is the whole output, media type by media type"
    );
    assert_eq!(medias, ["application/json", "application/problem+json"]);
    // A code names a success, so the `default` — the error its failures carry —
    // is out of what a reply may look like, though it stays kept for the success
    // the spec gives no code for.
    let (said, medias) = named(
        "        \"200\":\n          description: the thing\n          content:\n            application/json: { schema: { type: object } }\n        default:\n          description: the problem it carries\n          content:\n            application/problem+json: { schema: { type: string } }\n",
    );
    assert_eq!(
        said,
        vec![json!({ "type": "object" })],
        "the named `200` describes the output and the `default` does not"
    );
    assert_eq!(
        medias,
        ["application/problem+json"],
        "and it is still kept, because a 201 is checked against it"
    );
    // A range names every success at once, which shades the `default` the same
    // way, in either case.
    for range in ["2XX", "2xx"] {
        let (said, _) = named(&format!(
            "        \"{range}\":\n          description: the thing\n          content:\n            application/json: {{ schema: {{ type: object }} }}\n        default:\n          description: the problem\n          content:\n            application/problem+json: {{ schema: {{ type: string }} }}\n"
        ));
        assert_eq!(
            said,
            vec![json!({ "type": "object" })],
            "a `{range}` range covers every success, so the `default` describes nothing"
        );
    }
    // A response key that is no success at all shades nothing: a `default`
    // beside a bare `404` still describes what a success takes.
    let (said, _) = named(
        "        \"404\":\n          description: no such thing\n          content:\n            application/json: { schema: { type: number } }\n        default:\n          description: the thing\n          content:\n            application/json: { schema: { type: object } }\n",
    );
    assert_eq!(
        said,
        vec![json!({ "type": "object" })],
        "a failure names no success, so the `default` still describes the reply"
    );
}

// The union `output` is a document nothing else compiles a reply against, so it
// cannot be the document its members' references resolve in: two responses whose
// schemas define their own `$defs` under one name would each reach the other's,
// and a member's `#` — the schema it is, which a recursive component uses — would
// name the union. Each member is therefore given an `$id`, which makes it a
// schema resource of its own, exactly the position it is in when the executor
// holds one reply to one response's schema alone. The union holds no `$defs` of
// its own, so a member that resolved a reference against the union instead of
// itself would not compile at all.
#[test]
fn each_member_of_the_output_union_is_its_own_schema_document() {
    // Two successes of one 3.1 spec, each declaring a `$defs` of its own under
    // the same name, `Leaf`: a `200` requires a `thing`, a `202` a `ticket`.
    let ops = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Inline, version: "1" }
paths:
  /thing:
    get:
      operationId: thing
      responses:
        "200":
          description: a thing, defined beside the response that returns it
          content:
            application/json:
              schema:
                $ref: "#/$defs/Leaf"
                $defs:
                  Leaf: { type: object, required: [thing], properties: { thing: { type: string } } }
        "202":
          description: a ticket, defined the same way and under the same name
          content:
            application/json:
              schema:
                $ref: "#/$defs/Leaf"
                $defs:
                  Leaf: { type: object, required: [ticket], properties: { ticket: { type: integer } } }
"##,
    ))
    .expect("spec");
    let op = &ops[0];
    let union = op.output.as_ref().expect("a union of the two successes");
    assert_eq!(
        union.get("$defs"),
        None,
        "the union holds no `$defs`, so a member reaching for one finds itself or nothing"
    );
    let validator = Registry::new()
        .validator_for(union)
        .expect("the union of two inline `$defs` of one name compiles");
    assert_eq!(
        union["anyOf"]
            .as_array()
            .expect("members")
            .iter()
            .map(|member| member["$id"].as_str().expect("an id").to_owned())
            .collect::<Vec<_>>(),
        ["urn:atomic:response:0", "urn:atomic:response:1"],
        "each member is a resource of its own, so neither reads the other's `$defs`"
    );
    assert!(
        validator.is_valid(&json!({ "thing": "x" })),
        "the `200`'s own `Leaf`"
    );
    assert!(
        validator.is_valid(&json!({ "ticket": 7 })),
        "the `202`'s own `Leaf`, of the same name"
    );
    // One member's `Leaf` is not the other's: each shape is accepted exactly
    // once, and neither accepts the other's body.
    let members = union["anyOf"].as_array().expect("two members");
    let member = |index: usize| {
        Registry::new()
            .validator_for(&members[index])
            .expect("the member compiles alone")
    };
    assert!(member(0).is_valid(&json!({ "thing": "x" })));
    assert!(
        !member(0).is_valid(&json!({ "ticket": 7 })),
        "the `200` reads its own `Leaf`, not the `202`'s"
    );
    assert!(member(1).is_valid(&json!({ "ticket": 7 })));
    assert!(
        !member(1).is_valid(&json!({ "thing": "x" })),
        "and the `202` reads its own"
    );
    // A member's `#` names that member, not the union. The `200`'s schema is
    // recursive through `#`, and only a `child` that fits the `200` again fits
    // it; were `#` the union, the `202`'s ticket would fit a `child` too.
    let recursive = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Recursive, version: "1" }
paths:
  /tree:
    get:
      operationId: tree
      responses:
        "200":
          description: a node, and under it the same node again
          content:
            application/json: { schema: { $ref: "#/components/schemas/Node" } }
        "202":
          description: a ticket instead
          content:
            application/json: { schema: { $ref: "#/components/schemas/Ticket" } }
components:
  schemas:
    Node:
      type: object
      required: [node]
      properties:
        node: { type: string }
        child: { $ref: "#" }
      additionalProperties: false
    Ticket: { type: object, required: [ticket], properties: { ticket: { type: integer } } }
"##,
    ))
    .expect("spec");
    let union = recursive[0].output.as_ref().expect("a union of both");
    let validator = Registry::new()
        .validator_for(union)
        .expect("the union of a recursive member compiles");
    assert!(
        validator.is_valid(&json!({ "node": "a", "child": { "node": "b" } })),
        "a child that is another node"
    );
    assert!(
        !validator.is_valid(&json!({ "node": "a", "child": { "ticket": 7 } })),
        "a child is the same node again, never the other response's shape: {union}"
    );
    // What a reply is held to is one member alone, and it reads its own `$defs`
    // there too, as it always did.
    let held = |status: u16| {
        let schema = op
            .response(status)
            .expect("a response")
            .1
            .schema()
            .expect("a schema");
        Registry::new()
            .validator_for(schema)
            .expect("the response's schema compiles alone")
    };
    assert!(held(200).is_valid(&json!({ "thing": "x" })));
    assert!(!held(202).is_valid(&json!({ "thing": "x" })));
}

// A response schema that names itself is written that way so that it can point
// at itself: a `$ref` of `https://example.com/node` inside the schema whose
// `$id` is that is that member's own `#`. The union must not rename such a
// member — its reference would then name a document nothing holds, and a
// response schema that compiles every other way would stop compiling inside the
// tool built from it — so a member keeps the name the spec gave it and takes a
// `urn:atomic:response:…` only where it has none.
#[test]
fn a_member_of_the_union_keeps_the_id_the_spec_gave_it() {
    let ops = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Node, version: "1" }
paths:
  /node:
    get:
      operationId: node
      responses:
        "200":
          description: a node, which names itself and points at itself
          content:
            application/json:
              schema:
                $id: "https://example.com/node"
                type: object
                properties: { child: { $ref: "https://example.com/node" } }
        "202":
          description: a count, which names nothing
          content:
            application/json: { schema: { type: integer } }
"##,
    ))
    .expect("spec");
    let op = &ops[0];
    let member = op
        .response(200)
        .expect("a response")
        .1
        .schema()
        .expect("a schema");
    Registry::new()
        .validator_for(member)
        .expect("the member compiles alone, self-reference and all");
    let union = op.output.as_ref().expect("a union of the two successes");
    assert_eq!(
        union["anyOf"],
        json!([
            {
                "$id": "https://example.com/node",
                "type": "object",
                "properties": { "child": { "$ref": "https://example.com/node" } },
            },
            { "$id": "urn:atomic:response:1", "type": "integer" },
        ]),
        "the member the spec named keeps that name, and the one it named nothing \
         takes an id of atomic's: {union}"
    );
    let validator = Registry::new()
        .validator_for(union)
        .expect("the union compiles, the self-reference naming the member it is in");
    assert!(
        validator.is_valid(&json!(7)),
        "the shape a `202` answers with"
    );
    assert!(
        validator.is_valid(&json!({ "child": { "child": {} } })),
        "a node nested in a node nested in a node"
    );
    assert!(
        !validator.is_valid(&json!({ "child": 5 })),
        "a `child` is the same node again, not anything at all: {union}"
    );
    // What a reply is held to is that member alone, which is the same document
    // here and says the same thing about a nested node.
    let held = Registry::new()
        .validator_for(member)
        .expect("the response's schema compiles alone");
    assert!(held.is_valid(&json!({ "child": {} })));
    assert!(!held.is_valid(&json!({ "child": 5 })));
}

// The union is compiled where no member is: a member that names its own
// document resolves what it writes against that name, so a reference written
// for whichever document it happens to be compiled into — a root-relative
// `$ref: "/"`, with no scheme and no fragment — resolves when that member is
// compiled on its own and does not resolve once it stands as a document of its
// own. This is the one thing the union's own compile asks that no member's
// does, and `atomic check` compiles the union, so it is said there.
#[test]
fn the_unions_own_compile_refuses_a_member_that_reaches_outside_itself() {
    let ops = operations(&yaml(
        r##"openapi: 3.1.0
info: { title: Rooted, version: "1" }
paths:
  /thing:
    get:
      operationId: thing
      responses:
        "200":
          description: a thing, reaching for a root nothing names
          content:
            application/json: { schema: { $ref: "/" } }
        "202":
          description: a count
          content:
            application/json: { schema: { type: string } }
"##,
    ))
    .expect("spec");
    let op = &ops[0];
    let member = op
        .response(200)
        .expect("a response")
        .1
        .schema()
        .expect("a schema");
    Registry::new()
        .validator_for(member)
        .expect("the member compiles alone, wherever it is asked to resolve `/`");
    let union = op.output.as_ref().expect("a union of the two successes");
    let error = Registry::new()
        .validator_for(union)
        .expect_err("the same reference does not resolve once the member names itself");
    let said = error.to_string();
    assert!(
        said.contains("Failed to resolve '/'") && said.contains("urn:atomic:response:0"),
        "the refusal should name the reference and the member's own base: {said}"
    );
}

#[test]
fn request_and_response_use_their_own_view_of_shared_properties() {
    let schema = json!({
        "type":"object", "required":["id","credential"],
        "properties":{"id":{"type":"integer","readOnly":true},
                      "credential":{"$ref":"#/components/schemas/Secret"}}
    });
    let media = json!({"application/json":{"schema":{"$ref":"#/components/schemas/Record"}}});
    let spec = json!({"openapi":"3.1.0", "components":{"schemas":{
        "Record":schema, "Secret":{"type":"string","writeOnly":true}
    }}, "paths":{"/":{"post":{
        "requestBody":{"required":true,"content":media},
        "responses":{"201":{"content":media}}
    }}}});
    let op = operations(&spec).unwrap().remove(0);
    let input = Registry::new().validator_for(&op.input).unwrap();
    let output = Registry::new()
        .validator_for(op.output.as_ref().unwrap())
        .unwrap();
    assert!(input.is_valid(&json!({"body":{"credential":"value"}})));
    assert!(!input.is_valid(&json!({"body":{"id":1,"credential":"value"}})));
    assert!(output.is_valid(&json!({"id":1})));
    assert!(!output.is_valid(&json!({"id":1,"credential":"value"})));
}

#[test]
fn content_parameters_require_one_declared_representation() {
    let build = |parameter: Value| {
        operations(&json!({"openapi":"3.1.0","paths":{"/":{"get":{"parameters":[parameter]}}}}))
    };
    let mut param = json!({"name":"q","in":"query","content":{"text/plain":{}}});
    let op = build(param.clone()).unwrap().remove(0);
    assert!(op.unsupported.unwrap().contains("text/plain"));
    param["content"]["application/json"] = json!({});
    assert!(
        build(param.clone())
            .unwrap_err()
            .message
            .contains("exactly one")
    );
    param["schema"] = json!({});
    assert!(build(param).unwrap_err().message.contains("mixes content"));
}

#[test]
fn ambiguous_request_media_are_refused_and_parameters_are_preserved() {
    let spec = |media: Value| json!({"openapi":"3.1.0","paths":{"/":{"post":{"requestBody":{"content":media}}}}});
    for media in [
        json!({"application/a+json":{},"application/b+json":{}}),
        json!({"application/json; profile=a":{},"application/json; profile=b":{}}),
    ] {
        let op = operations(&spec(media)).unwrap().remove(0);
        assert!(op.unsupported.unwrap().contains("equally preferred"));
        assert!(op.request_media.is_none());
    }
    let content_type = "application/json; profile=custom";
    let op = operations(&spec(json!({(content_type):{"schema":{"const":1}}})))
        .unwrap()
        .remove(0);
    assert_eq!(op.request_media.as_deref(), Some(content_type));
    assert_eq!(op.input["properties"]["body"]["const"], 1);
}
