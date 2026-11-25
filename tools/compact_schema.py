# SPDX-License-Identifier: Apache-2.0
"""Compiles the compact schema form to JSON Schema, for the example checker.

A mirror of atomic's own compiler (atomic_schema::compact), kept for the
offline catalog checks. The syntax follows Dotprompt's Picoschema, with one
difference: a `?` field is optional and is not also nullable.

    name: type, description          a required scalar (string, integer, number, boolean, any)
    name?: type                      an optional one
    tags(array, description): string           a list of scalars
    lines(array):                              a list of objects
      sku: string
    address(object):                           a nested object
      city: string
    status(enum, description): [open, closed]  one of the listed values
    (*): string                                any other property, of this type
"""

from __future__ import annotations

import re

SCALARS = {"string", "integer", "number", "boolean", "null", "any"}
# A root with any of these is full JSON Schema, as atomic decides.
JSON_SCHEMA_KEYWORDS = {"$ref", "$schema", "$id", "$defs", "$anchor", "$comment", "$dynamicRef",
    "$dynamicAnchor", "$vocabulary", "type", "properties", "patternProperties", "additionalProperties",
    "unevaluatedProperties", "propertyNames", "required", "enum", "const", "anyOf", "oneOf", "allOf",
    "not", "if", "then", "else", "items", "prefixItems", "unevaluatedItems", "contains",
    "dependentSchemas"}
# Constraints and annotations: full JSON Schema unless the value is a compact field.
VALUE_KEYWORDS = {"multipleOf", "maximum", "exclusiveMaximum", "minimum", "exclusiveMinimum",
    "maxLength", "minLength", "pattern", "maxItems", "minItems", "uniqueItems", "maxContains",
    "minContains", "maxProperties", "minProperties", "dependentRequired", "format", "contentEncoding",
    "contentMediaType", "contentSchema", "title", "description", "default", "deprecated", "readOnly",
    "writeOnly", "examples", "definitions", "dependencies"}
KEY = re.compile(r"(\(\*\)|[A-Za-z_][\w-]*)(\?)?(?:\((\w+)(?:,\s*(.*))?\))?")


def _is_field(value):
    return isinstance(value, str) and value.partition(",")[0].strip() in SCALARS


def _marks_json_schema(key, value):
    return key in JSON_SCHEMA_KEYWORDS or (key in VALUE_KEYWORDS and not _is_field(value))


def is_compact(schema):
    """A non-empty mapping with no JSON Schema keyword at its root, counting a
    constraint or annotation keyword only when its value is not a compact field."""
    return isinstance(schema, dict) and bool(schema) and not any(
        _marks_json_schema(key, value) for key, value in schema.items())


def _fail(where, message):
    raise ValueError(f"{where}: compact schema: {message}")


def _scalar(text, where):
    if not isinstance(text, str):
        _fail(where, f"expected 'type, description', found {text!r}; constraints need full JSON Schema")
    kind, _, description = text.partition(",")
    kind = kind.strip()
    if kind not in SCALARS:
        _fail(where, f"unknown type {kind!r}; constraints and formats need full JSON Schema")
    schema = {} if kind == "any" else {"type": kind}
    if description.strip():
        schema["description"] = description.strip()
    return schema


def compile_compact(mapping, where):
    if not isinstance(mapping, dict) or not mapping:
        _fail(where, "expected a mapping of fields")
    properties, required, additional = {}, [], False
    for key, value in mapping.items():
        match = KEY.fullmatch(str(key))
        if not match:
            _fail(where, f"cannot read field {key!r}")
        name, optional, kind, description = match.groups()
        if kind is None:
            schema = _scalar(value, where)
        elif kind == "array":
            schema = {"type": "array", "items": compile_compact(value, where) if isinstance(value, dict) else _scalar(value, where)}
        elif kind == "object":
            schema = compile_compact(value, where)
        elif kind == "enum":
            if not isinstance(value, list) or not value:
                _fail(where, f"{name}: an enum needs a list of values")
            schema = {"enum": value}
        else:
            _fail(where, f"{name}: unknown kind ({kind})")
        if description:
            schema["description"] = description.strip()
        if name == "(*)":
            additional = schema
        else:
            properties[name] = schema
            if not optional:
                required.append(name)
    compiled = {"type": "object", "additionalProperties": additional}
    if required:
        compiled["required"] = required
    compiled["properties"] = properties
    return compiled


def to_json_schema(schema, where="schema"):
    """The schema as JSON Schema: compiled when compact. A root that mixes a
    structural keyword with a key that only reads as a compact field is refused."""
    if is_compact(schema):
        return compile_compact(schema, where)
    if isinstance(schema, dict):
        keyword = next((key for key, value in schema.items() if _marks_json_schema(key, value)), None)
        if keyword is not None:
            for key, value in schema.items():
                known = key in JSON_SCHEMA_KEYWORDS or key in VALUE_KEYWORDS or key.startswith("x-")
                if not known and (key.endswith("?") or "(" in key or _is_field(value)):
                    _fail(where, f"{key!r} reads as a compact field beside the JSON Schema keyword {keyword!r}")
    return schema
