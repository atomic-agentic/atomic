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
JSON_SCHEMA_KEYWORDS = {"$ref", "$schema", "$id", "$defs", "$anchor", "$comment", "type", "properties",
    "patternProperties", "additionalProperties", "unevaluatedProperties", "required", "enum", "const",
    "anyOf", "oneOf", "allOf", "not", "if", "then", "else", "items", "prefixItems", "contains",
    "dependentSchemas"}
KEY = re.compile(r"(\(\*\)|[A-Za-z_][\w-]*)(\?)?(?:\((\w+)(?:,\s*(.*))?\))?")


def is_compact(schema):
    """A non-empty mapping with no JSON Schema keyword at its root."""
    return isinstance(schema, dict) and bool(schema) and not (set(schema) & JSON_SCHEMA_KEYWORDS)


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
