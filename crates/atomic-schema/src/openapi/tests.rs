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
/// success that declares one where it says several. The component schemas the
/// output carries are dropped from each, being the same map everywhere and not
/// part of what a schema describes.
fn described(output: Option<&Value>) -> Vec<Value> {
    let Some(output) = output else {
        return Vec::new();
    };
    let parts = |schema: &Value| {
        let mut schema = schema.clone();
        if let Value::Object(map) = &mut schema {
            map.remove("$defs");
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
// is no body to read; one whose content names no schema — a media type
// atomic does not read, a wildcard with no `schema`, a JSON media type with
// no `schema` — declares a body whose shape nothing describes. The two are
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
// first is a body the spec declares and describes no shape for, and another media
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
// a schema nothing wrote for it. What one response contributes to the tool's
// `output` is still the first in sorted order — the media type a reply naming
// no `Content-Type` is held to — and a component schema reference resolves in
// every one of them, not only in the first.
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
    // Each success describes the output by the first media type it names one
    // under, and the tool describes all three: a `200`'s Thing, the ack a `201`
    // answers with, the boolean a `202` does. The `203` names no schema, so it
    // describes nothing, and a `204` has no response to describe.
    assert_eq!(
        described(op.output.as_ref()),
        vec![
            json!({ "$ref": "#/$defs/Thing" }),
            json!({ "type": "object" }),
            json!({ "type": "boolean" }),
        ],
        "{}",
        op.output.as_ref().expect("a union of the successes")
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
    // The output describes both, because a success is checked against both:
    // its own `200` for a 200, `default` for any other success the spec gives
    // no code for. What it does not describe is the `404`'s — that schema is
    // not kept at all, broken or not.
    assert_eq!(
        described(op.output.as_ref()),
        vec![
            json!({ "$ref": "#/$defs/Order" }),
            json!({ "$ref": "#/$defs/Problem" }),
        ],
        "{}",
        op.output.as_ref().expect("a union of Order and default")
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
    // the spec never wrote is described too.
    assert_eq!(
        by("twoShapes").output,
        Some(json!({ "anyOf": [result.clone(), ticket] })),
        "{:?}",
        by("twoShapes").output
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
// exactly one of the shapes the successes declare. A component schema reference
// resolves inside a member of it — a member points at the document it sits in,
// which is now the union, so the union carries the components beside its
// members — while each member keeps its own copy, because what a reply is held
// to is one member alone.
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
        union["$defs"], components,
        "the union carries the components its members reference"
    );
    assert_eq!(
        union["anyOf"],
        json!([
            { "$ref": "#/$defs/Order", "$defs": components.clone() },
            { "$ref": "#/$defs/Queued", "$defs": components },
        ]),
        "each member keeps its own copy too, being what a reply is held to alone"
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
