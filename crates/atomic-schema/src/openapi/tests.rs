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
        if let Some(output) = &op.output {
            registry.validator_for(output).expect("output schema");
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
fn each_success_response_is_kept_by_its_status() {
    let spec = yaml(
        r#"openapi: 3.1.0
info: { title: Jobs, version: "1" }
paths:
  /jobs:
    post:
      operationId: submit
      responses:
        "200": { description: done, content: { application/json: { schema: { type: object, required: [result], properties: { result: { type: string } } } } } }
        "202": { description: queued, content: { application/json: { schema: { type: object, required: [ticket], properties: { ticket: { type: integer } } } } } }
        "204": { description: nothing to do }
        "2XX": { description: other, content: { application/json: { schema: { type: string } } } }
        "404": { description: no such job, content: { application/json: { schema: { type: object } } } }
        default: { description: error, content: { application/json: { schema: { type: number } } } }
"#,
    );
    let ops = operations(&spec).expect("spec");
    let op = &ops[0];
    assert_eq!(
        op.responses.keys().map(String::as_str).collect::<Vec<_>>(),
        ["200", "202", "204", "2XX", "default"],
        "a failure's response is not a success's"
    );
    let required = |status: u16| {
        op.response(status)
            .and_then(Option::as_ref)
            .map(|s| s["required"].clone())
    };
    assert_eq!(required(200), Some(json!(["result"])));
    assert_eq!(required(202), Some(json!(["ticket"])));
    assert_eq!(op.response(204), Some(&None), "no content");
    assert_eq!(
        op.response(201).cloned().flatten(),
        Some(json!({ "type": "string" })),
        "a range"
    );
    assert_eq!(
        op.response(304).cloned().flatten(),
        Some(json!({ "type": "number" })),
        "default"
    );
    // The tool's output is the first success's, as before.
    assert_eq!(
        op.output.as_ref().map(|s| s["required"].clone()),
        Some(json!(["result"]))
    );
}
