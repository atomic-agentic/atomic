// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::registry::Registry;
use std::path::Path;

fn yaml(text: &str) -> Value {
    crate::yaml::load(Path::new("spec.yaml"), text)
        .expect("yaml")
        .value
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
        for schema in op
            .output
            .iter()
            .chain(op.responses.values().filter_map(ResponseBody::schema))
        {
            registry.validator_for(schema).expect("response schema");
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
    // Every declared response is kept, a failure's too.
    assert_eq!(
        op.responses.keys().map(String::as_str).collect::<Vec<_>>(),
        ["200", "202", "204", "2xx", "404", "default"]
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
    // A success outside every code and range takes default; a failure is
    // never picked, whatever it declares.
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
    // The tool's output is the first success's, as before.
    assert_eq!(
        op.output.as_ref().map(|s| s["required"].clone()),
        Some(json!(["result"]))
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
    let declared = |code: &str| op.responses.get(code).expect("declared");
    assert_eq!(*declared("200"), ResponseBody::NoBody);
    assert_eq!(
        *declared("201"),
        ResponseBody::Json(json!({ "type": "object" })),
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
        ResponseBody::Json(json!({ "type": "array" })),
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
    // these three. The tool's output is still the first success's
    // declaration, and that one declares no content, so the tool describes
    // no output.
    assert_eq!(op.output, None);
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
