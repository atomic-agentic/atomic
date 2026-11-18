// SPDX-License-Identifier: Apache-2.0

use super::*;
use serde_json::Map;

fn invalid(id: &str, error: impl ToString) -> SchemaError {
    SchemaError::Invalid {
        id: id.into(),
        message: error.to_string(),
    }
}

// Traverse schema positions, never literal data in const/enum/examples/default.
fn children(schema: &Value) -> Vec<&Value> {
    let mut values = Vec::new();
    for key in [
        "properties",
        "patternProperties",
        "dependentSchemas",
        "$defs",
        "definitions",
    ] {
        if let Some(map) = schema.get(key).and_then(Value::as_object) {
            values.extend(map.values());
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(array) = schema.get(key).and_then(Value::as_array) {
            values.extend(array);
        }
    }
    for key in [
        "items",
        "additionalItems",
        "contains",
        "additionalProperties",
        "unevaluatedProperties",
        "unevaluatedItems",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "contentSchema",
    ] {
        if let Some(value) = schema.get(key) {
            values.push(value);
        }
    }
    values
}

fn base_url(schema: &Value, base: &str) -> Result<url::Url, SchemaError> {
    let base = url::Url::parse(base).map_err(|e| invalid(base, e))?;
    schema
        .get("$id")
        .and_then(Value::as_str)
        .map_or(Ok(base.clone()), |id| {
            base.join(id).map_err(|e| invalid(id, e))
        })
}

fn insert(
    known: &mut BTreeMap<String, Value>,
    uri: String,
    schema: Value,
) -> Result<(), SchemaError> {
    if let Some(existing) = known.get(&uri)
        && *existing != schema
    {
        return Err(invalid(
            &uri,
            "conflicting schema retrieval URI or identifier",
        ));
    }
    known.insert(uri, schema);
    Ok(())
}

impl Registry {
    pub(super) fn known(&self) -> Arc<BTreeMap<String, Value>> {
        self.known_resources.clone()
    }

    fn resources(entry: &Entry) -> Result<BTreeMap<String, Value>, SchemaError> {
        let mut known = BTreeMap::new();
        insert(&mut known, entry.id.clone(), entry.schema.clone())?;
        fn nested(
            schema: &Value,
            base: &str,
            known: &mut BTreeMap<String, Value>,
            depth: usize,
        ) -> Result<(), SchemaError> {
            if depth > 128 {
                return Err(invalid(base, "schema nesting limit exceeded"));
            }
            let base = base_url(schema, base)?;
            if schema.get("$id").is_some() {
                let mut canonical = schema.clone();
                canonical["$id"] = Value::String(base.to_string());
                insert(
                    known,
                    base.to_string().trim_end_matches('#').into(),
                    canonical,
                )?;
            }
            for child in children(schema) {
                nested(child, base.as_str(), known, depth + 1)?;
            }
            Ok(())
        }
        if entry.file.is_absolute() {
            insert(&mut known, file_uri(&entry.file), entry.schema.clone())?;
        }
        let base = if entry.file.is_absolute() {
            file_uri(&entry.file)
        } else {
            entry.id.clone()
        };
        nested(&entry.schema, &base, &mut known, 0)?;
        Ok(known)
    }

    pub(super) fn index_entry(&mut self, entry: &Entry) -> Result<(), SchemaError> {
        let resources = Self::resources(entry)?;
        for (uri, value) in &resources {
            if self
                .known_resources
                .get(uri)
                .is_some_and(|existing| existing != value)
                && self
                    .resource_owners
                    .get(uri)
                    .is_some_and(|owners| owners.iter().any(|owner| owner != &entry.id))
            {
                return Err(invalid(
                    uri,
                    "conflicting schema retrieval URI or identifier",
                ));
            }
        }
        self.unindex(&entry.id);
        let known = Arc::make_mut(&mut self.known_resources);
        let mut keys = BTreeSet::new();
        for (uri, value) in resources {
            self.resource_owners
                .entry(uri.clone())
                .or_default()
                .insert(entry.id.clone());
            keys.insert(uri.clone());
            known.insert(uri, value);
        }
        self.document_resources.insert(entry.id.clone(), keys);
        self.files.insert(entry.file.clone(), entry.id.clone());
        #[cfg(test)]
        {
            self.indexed_documents += 1;
        }
        Ok(())
    }

    fn unindex(&mut self, id: &str) {
        if let Some(keys) = self.document_resources.remove(id) {
            let known = Arc::make_mut(&mut self.known_resources);
            for uri in keys {
                if let Some(owners) = self.resource_owners.get_mut(&uri) {
                    owners.remove(id);
                    if owners.is_empty() {
                        self.resource_owners.remove(&uri);
                        known.remove(&uri);
                    }
                }
            }
        }
        if let Some(entry) = self.entries.get(id) {
            self.files.remove(&entry.file);
        }
    }

    pub(super) fn rollback(
        &mut self,
        id: &str,
        previous: Option<Entry>,
    ) -> Result<(), SchemaError> {
        self.unindex(id);
        self.entries.remove(id);
        if let Some(entry) = previous {
            self.index_entry(&entry)?;
            self.entries.insert(id.into(), entry);
        }
        Ok(())
    }

    pub(super) fn load_local_references(
        &mut self,
        schema: &Value,
        base: &str,
        depth: usize,
    ) -> Result<(), SchemaError> {
        fn collect(
            schema: &Value,
            base: &str,
            out: &mut Vec<url::Url>,
            depth: usize,
        ) -> Result<(), SchemaError> {
            if depth > 128 {
                return Err(invalid(base, "schema nesting limit exceeded"));
            }
            let base = base_url(schema, base)?;
            for key in ["$ref", "$dynamicRef", "$recursiveRef"] {
                if let Some(reference) = schema.get(key).and_then(Value::as_str) {
                    let mut target = base.join(reference).map_err(|e| invalid(reference, e))?;
                    target.set_fragment(None);
                    if target.scheme() == "file" {
                        out.push(target);
                    }
                }
            }
            for child in children(schema) {
                collect(child, base.as_str(), out, depth + 1)?;
            }
            Ok(())
        }
        let mut references = Vec::new();
        collect(schema, base, &mut references, 0)?;
        let root = base_url(schema, base)?.to_string();
        for reference in references {
            if reference.as_str() == root || self.known_resources.contains_key(reference.as_str()) {
                continue;
            }
            let file = reference
                .to_file_path()
                .map_err(|()| invalid(reference.as_str(), "invalid local schema URI"))?;
            self.load_file_inner(&file, depth + 1)?;
        }
        Ok(())
    }

    /// Inline a registered contract using its retrieval URI and JSON Schema identifier scopes.
    /// Unsupported recursive/deep and dynamic references fail instead of being weakened.
    pub fn inline(&mut self, id: &str) -> Result<Value, SchemaError> {
        let entry = self
            .get(id)
            .ok_or_else(|| SchemaError::Unknown(id.into()))?
            .clone();
        self.inline_value(&entry.schema, &entry.file)
    }

    /// Inline a schema rooted at a file, preserving reference siblings and literal JSON data.
    /// Local dependencies load offline; every other resource must already be registered.
    pub fn inline_value(&mut self, schema: &Value, file: &Path) -> Result<Value, SchemaError> {
        let base = file_uri(file);
        self.load_local_references(schema, &base, 0)?;
        let known = self.known();
        let mut builder = jsonschema::Registry::new().retriever(Registered(known.clone()));
        builder = builder
            .add(&base, schema.clone())
            .map_err(|e| invalid(&base, e))?;
        let resources = builder.prepare().map_err(|e| invalid(&base, e))?;
        let resolver =
            resources.resolver(jsonschema::uri::from_str(&base).map_err(|e| invalid(&base, e))?);
        let root = resolver.lookup("#").map_err(|e| invalid(&base, e))?;
        expand(
            root.contents(),
            root.resolver().base_uri().as_str(),
            &resources,
            0,
            false,
            &mut EXPANSION_BUDGET.clone(),
        )
    }
}

fn merge_reference_siblings(target: &Value, siblings: &Map<String, Value>) -> Option<Value> {
    let mut merged = target.as_object()?.clone();
    for (key, value) in siblings {
        let annotation = matches!(
            key.as_str(),
            "title"
                | "description"
                | "$comment"
                | "default"
                | "examples"
                | "deprecated"
                | "readOnly"
                | "writeOnly"
        );
        let scalar = matches!(
            key.as_str(),
            "type"
                | "const"
                | "enum"
                | "minimum"
                | "maximum"
                | "exclusiveMinimum"
                | "exclusiveMaximum"
                | "multipleOf"
                | "minLength"
                | "maxLength"
                | "pattern"
                | "format"
                | "minItems"
                | "maxItems"
                | "uniqueItems"
                | "minProperties"
                | "maxProperties"
        );
        if !annotation && !scalar {
            return None;
        }
        if !annotation
            && let Some(previous) = merged.get(key)
            && previous != value
        {
            let bound = match key.as_str() {
                "minLength" | "minItems" | "minProperties" => {
                    previous.as_u64()?.max(value.as_u64()?)
                }
                "maxLength" | "maxItems" | "maxProperties" => {
                    previous.as_u64()?.min(value.as_u64()?)
                }
                _ => return None,
            };
            merged.insert(key.clone(), Value::from(bound));
        } else {
            merged.insert(key.clone(), value.clone());
        }
    }
    Some(Value::Object(merged))
}

/// How many subschemas one inlined contract may have. A reference that is
/// used twice is inlined twice, so a chain of such references grows as a power
/// of two; the budget stops that long before memory runs out.
const EXPANSION_BUDGET: usize = 20_000;

/// Merge `properties` and `required` beside a `$ref` into the target object
/// when that keeps the contract's meaning, so a schema that extends another
/// needs no `allOf`, which strict generation refuses. Returns `None` to keep
/// the exact `allOf` form, and an error for an extension the target forbids.
fn merge_object_siblings(
    target: &Value,
    siblings: &Map<String, Value>,
) -> Result<Option<Value>, String> {
    let mut rest = siblings.clone();
    let properties = rest.remove("properties");
    let required = rest.remove("required");
    if properties.is_none() && required.is_none() {
        return Ok(merge_reference_siblings(target, siblings));
    }
    let Some(target_object) = target.as_object() else {
        return Ok(None);
    };
    let closed = target_object.get("additionalProperties").cloned();
    // `additionalProperties: false` beside the reference closes the merged
    // object to the properties listed beside it. That means the same as the
    // allOf form only when those include every property of the target.
    let close = match rest.remove("additionalProperties") {
        None => false,
        Some(Value::Bool(false)) => {
            let listed = properties.as_ref().and_then(Value::as_object);
            let covered = target_object
                .get("properties")
                .and_then(Value::as_object)
                .is_none_or(|own| {
                    own.keys()
                        .all(|key| listed.is_some_and(|listed| listed.contains_key(key)))
                });
            if !covered || !matches!(closed, None | Some(Value::Bool(_))) {
                return Ok(None);
            }
            true
        }
        Some(_) => return Ok(None),
    };
    let Some(mut merged) = merge_reference_siblings(target, &rest) else {
        return Ok(None);
    };
    let Some(merged_object) = merged.as_object_mut() else {
        return Ok(None);
    };
    if let Some(properties) = properties {
        let Some(properties) = properties.as_object() else {
            return Ok(None);
        };
        let Some(known) = merged_object
            .entry("properties")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
        else {
            return Ok(None);
        };
        for (key, schema) in properties {
            match known.get(key) {
                Some(existing) if existing == schema => {}
                // A refinement of a property the target already has.
                Some(existing) => {
                    let Some(refinement) = schema.as_object() else {
                        return Ok(None);
                    };
                    let Some(value) = merge_reference_siblings(existing, refinement) else {
                        return Ok(None);
                    };
                    known.insert(key.clone(), value);
                }
                None => match &closed {
                    None | Some(Value::Bool(true)) => {
                        known.insert(key.clone(), schema.clone());
                    }
                    Some(Value::Bool(false)) => {
                        return Err(format!(
                            "property `{key}` beside `$ref` is refused by the referenced schema, \
                             which sets additionalProperties: false; add it to that schema"
                        ));
                    }
                    // Both the extra property and the target's
                    // additionalProperties would apply; only allOf says so.
                    Some(_) => return Ok(None),
                },
            }
        }
    }
    if let Some(required) = required {
        let Some(required) = required.as_array() else {
            return Ok(None);
        };
        let Some(list) = merged_object
            .entry("required")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
        else {
            return Ok(None);
        };
        for name in required {
            if !list.contains(name) {
                list.push(name.clone());
            }
        }
    }
    if close {
        merged_object.insert("additionalProperties".into(), Value::Bool(false));
    }
    Ok(Some(merged))
}

fn expand(
    schema: &Value,
    base: &str,
    resources: &jsonschema::Registry<'_>,
    depth: usize,
    apply_id: bool,
    budget: &mut usize,
) -> Result<Value, SchemaError> {
    if depth > 64 {
        return Err(invalid(
            base,
            "recursive or overly deep schema cannot be inlined",
        ));
    }
    if schema.is_object() {
        if *budget == 0 {
            return Err(invalid(
                base,
                format!(
                    "schema expands to more than {EXPANSION_BUDGET} subschemas when its references are inlined"
                ),
            ));
        }
        *budget -= 1;
    }
    let Some(object) = schema.as_object() else {
        return Ok(schema.clone());
    };
    if object.contains_key("$dynamicRef") || object.contains_key("$recursiveRef") {
        return Err(invalid(base, "dynamic schema references cannot be inlined"));
    }
    let base = if apply_id {
        base_url(schema, base)?.to_string()
    } else {
        base.into()
    };
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let resolver =
            resources.resolver(jsonschema::uri::from_str(&base).map_err(|e| invalid(&base, e))?);
        let resolved = resolver
            .lookup(reference)
            .map_err(|e| invalid(reference, e))?;
        let target = expand(
            resolved.contents(),
            resolved.resolver().base_uri().as_str(),
            resources,
            depth + 1,
            false,
            budget,
        )?;
        let mut siblings = object.clone();
        for key in [
            "$ref",
            "$id",
            "$schema",
            "$defs",
            "definitions",
            "$anchor",
            "$dynamicAnchor",
        ] {
            siblings.remove(key);
        }
        if siblings.is_empty() {
            return Ok(target);
        }
        let siblings = expand(
            &Value::Object(siblings),
            &base,
            resources,
            depth + 1,
            false,
            budget,
        )?;
        let mut combined = siblings
            .as_object()
            .ok_or_else(|| invalid(&base, "invalid reference siblings"))?
            .clone();
        // Scalar assertions can be intersected without introducing allOf.
        // Applicators and annotation-sensitive object keywords retain their
        // original scope below; flattening those could weaken the full contract.
        if let Some(merged) =
            merge_object_siblings(&target, &combined).map_err(|e| invalid(&base, e))?
        {
            return Ok(merged);
        }
        let mut clauses = combined
            .remove("allOf")
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        clauses.insert(0, target);
        combined.insert("allOf".into(), Value::Array(clauses));
        return Ok(Value::Object(combined));
    }
    let mut out = Map::new();
    for (key, value) in object {
        let expanded = match key.as_str() {
            "$id" | "$schema" | "$defs" | "definitions" | "$anchor" | "$dynamicAnchor" => continue,
            "properties" | "patternProperties" | "dependentSchemas" => {
                let Some(properties) = value.as_object() else {
                    return Err(invalid(&base, format!("invalid schema {key}")));
                };
                let values = properties
                    .iter()
                    .map(|(key, child)| {
                        Ok((
                            key.clone(),
                            expand(child, &base, resources, depth + 1, true, budget)?,
                        ))
                    })
                    .collect::<Result<Map<_, _>, SchemaError>>()?;
                Value::Object(values)
            }
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                let Some(array) = value.as_array() else {
                    return Err(invalid(&base, format!("invalid schema {key}")));
                };
                Value::Array(
                    array
                        .iter()
                        .map(|child| expand(child, &base, resources, depth + 1, true, budget))
                        .collect::<Result<_, _>>()?,
                )
            }
            "items"
            | "additionalItems"
            | "contains"
            | "additionalProperties"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "propertyNames"
            | "not"
            | "if"
            | "then"
            | "else"
            | "contentSchema" => expand(value, &base, resources, depth + 1, true, budget)?,
            _ => value.clone(),
        };
        out.insert(key.clone(), expanded);
    }
    Ok(Value::Object(out))
}
