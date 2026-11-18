// SPDX-License-Identifier: Apache-2.0

//! CEL expressions, checked against schemas at load.
//!
//! CEL is atomic's one expression language: `with` mappings, `validate`
//! rules, gate conditions, `{% if %}` tags and eval assertions. It has no side
//! effects. An expression is parsed when its file loads, and every field path
//! it reads from a known root (`input`, `steps`, …) is resolved against that
//! root's schema, so a misspelt field fails the load with its column instead
//! of quietly evaluating to an error at run time.
//!
//! The check follows `properties`, `items`, local `$ref`s and `anyOf`
//! branches. An object schema that lists `properties` is read as the full
//! list of its fields, unless it sets `additionalProperties`; an object with
//! no `properties` is open, and fields below it are not checked. Loop
//! variables of `all`, `exists`, `map` and `filter` are typed from their
//! list's `items`.

use cel::common::ast::{EntryExpr, Expr, IdedExpr, SourceInfo};
use cel::parser::Parser;
use serde_json::Value;
use std::collections::BTreeMap;

/// A parsed expression.
#[derive(Debug)]
pub struct Expression {
    source: String,
    ast: IdedExpr,
    info: SourceInfo,
}

/// Why an expression was refused, with the 1-based column it starts at in
/// the expression's own text.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("column {column}: {message}")]
pub struct ExprError {
    /// Column in the expression, from 1.
    pub column: usize,
    /// What is wrong.
    pub message: String,
}

/// A schema and the document its local `$ref`s resolve in.
#[derive(Clone, Debug)]
struct Typed {
    schema: Value,
    doc: Value,
}

/// The names an expression may read, each with its schema (`None` when its
/// shape is unknown and fields below it are not checked).
#[derive(Clone, Debug, Default)]
pub struct Scope<'a> {
    roots: BTreeMap<String, Option<&'a Value>>,
}

impl<'a> Scope<'a> {
    /// An empty scope.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allow `name`, typed by `schema`.
    pub fn with(mut self, name: &str, schema: &'a Value) -> Self {
        self.roots.insert(name.to_owned(), Some(schema));
        self
    }

    /// Allow `name` with an unknown shape.
    pub fn with_any(mut self, name: &str) -> Self {
        self.roots.insert(name.to_owned(), None);
        self
    }
}

/// Parse `source` as CEL.
pub fn parse(source: &str) -> Result<Expression, ExprError> {
    match Parser::new().parse_with_source_info(source) {
        Ok((ast, info)) => Ok(Expression {
            source: source.to_owned(),
            ast,
            info,
        }),
        Err(errors) => {
            let first = errors.errors.first();
            Err(ExprError {
                column: first.map_or(1, |e| usize::try_from(e.pos.1).unwrap_or(0) + 1),
                message: first.map_or_else(|| "not a CEL expression".to_owned(), |e| e.msg.clone()),
            })
        }
    }
}

/// One name in scope while walking: a loop variable, or a root.
enum Binding {
    Unknown,
    Shapeless,
    Typed(Typed),
}

impl Expression {
    /// The expression as written.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Check every name and field path against `scope`.
    pub fn check(&self, scope: &Scope<'_>) -> Result<(), ExprError> {
        let mut locals = Vec::new();
        self.walk(&self.ast, scope, &mut locals)
    }

    fn column(&self, id: u64) -> usize {
        let offset = self
            .info
            .offset_for(id)
            .map_or(0, |(start, _)| start as usize);
        self.source
            .char_indices()
            .take_while(|(i, _)| *i < offset)
            .count()
            + 1
    }

    /// The column of a field's name: a field access's node starts at its `.`.
    fn field_column(&self, id: u64) -> usize {
        let c = self.column(id);
        match self.source.chars().nth(c - 1) {
            Some('.') => c + 1,
            _ => c,
        }
    }

    fn binding(
        &self,
        name: &str,
        scope: &Scope<'_>,
        locals: &[(String, Option<Typed>)],
    ) -> Binding {
        if let Some((_, t)) = locals.iter().rev().find(|(n, _)| n == name) {
            return t.clone().map_or(Binding::Shapeless, Binding::Typed);
        }
        match scope.roots.get(name) {
            None => Binding::Unknown,
            Some(None) => Binding::Shapeless,
            Some(Some(s)) => Binding::Typed(Typed {
                schema: (*s).clone(),
                doc: (*s).clone(),
            }),
        }
    }

    fn walk(
        &self,
        e: &IdedExpr,
        scope: &Scope<'_>,
        locals: &mut Vec<(String, Option<Typed>)>,
    ) -> Result<(), ExprError> {
        match &e.expr {
            Expr::Ident(_) | Expr::Select(_) => {
                if let Some((root_id, root, path)) = path_of(e) {
                    return self
                        .resolve(root_id, &root, &path, scope, locals)
                        .map(|_| ());
                }
                if let Expr::Select(s) = &e.expr {
                    self.walk(&s.operand, scope, locals)?;
                }
                Ok(())
            }
            Expr::Call(c) => {
                if let Some(t) = &c.target {
                    self.walk(t, scope, locals)?;
                }
                c.args.iter().try_for_each(|a| self.walk(a, scope, locals))
            }
            Expr::Comprehension(c) => {
                self.walk(&c.iter_range, scope, locals)?;
                self.walk(&c.accu_init, scope, locals)?;
                let item = match path_of(&c.iter_range) {
                    Some((root_id, root, path)) => self
                        .resolve(root_id, &root, &path, scope, locals)?
                        .and_then(|t| {
                            let items = deref(&t.doc, &t.schema).get("items")?.clone();
                            Some(Typed {
                                schema: deref(&t.doc, &items).clone(),
                                doc: t.doc,
                            })
                        }),
                    None => None,
                };
                let pushed = 2 + usize::from(c.iter_var2.is_some());
                locals.push((c.iter_var.clone(), item));
                if let Some(v2) = &c.iter_var2 {
                    locals.push((v2.clone(), None));
                }
                locals.push((c.accu_var.clone(), None));
                let result = self
                    .walk(&c.loop_cond, scope, locals)
                    .and_then(|()| self.walk(&c.loop_step, scope, locals))
                    .and_then(|()| self.walk(&c.result, scope, locals));
                locals.truncate(locals.len() - pushed);
                result
            }
            Expr::List(l) => l
                .elements
                .iter()
                .try_for_each(|x| self.walk(x, scope, locals)),
            Expr::Map(m) => m
                .entries
                .iter()
                .try_for_each(|entry| self.entry(&entry.expr, scope, locals)),
            Expr::Struct(s) => s
                .entries
                .iter()
                .try_for_each(|entry| self.entry(&entry.expr, scope, locals)),
            Expr::Literal(_) | Expr::Unspecified => Ok(()),
        }
    }

    fn entry(
        &self,
        e: &EntryExpr,
        scope: &Scope<'_>,
        locals: &mut Vec<(String, Option<Typed>)>,
    ) -> Result<(), ExprError> {
        match e {
            EntryExpr::MapEntry(me) => {
                self.walk(&me.key, scope, locals)?;
                self.walk(&me.value, scope, locals)
            }
            EntryExpr::StructField(f) => self.walk(&f.value, scope, locals),
        }
    }

    /// Resolve `root.path…` and return its schema, when it is known.
    fn resolve(
        &self,
        root_id: u64,
        root: &str,
        path: &[(u64, String)],
        scope: &Scope<'_>,
        locals: &[(String, Option<Typed>)],
    ) -> Result<Option<Typed>, ExprError> {
        let mut cur = match self.binding(root, scope, locals) {
            Binding::Unknown => {
                let known: Vec<&str> = scope.roots.keys().map(String::as_str).collect();
                return Err(ExprError {
                    column: self.column(root_id),
                    message: format!("unknown name `{root}` (known: {})", known.join(", ")),
                });
            }
            Binding::Shapeless => return Ok(None),
            Binding::Typed(t) => t,
        };
        let mut walked = root.to_owned();
        for (field_id, field) in path {
            match field_schema(&cur.doc, &cur.schema, field) {
                Field::Declared(next) => cur.schema = next,
                Field::Open => return Ok(None),
                Field::Missing => {
                    let fields = known_fields(&cur.doc, &cur.schema);
                    return Err(ExprError {
                        column: self.field_column(*field_id),
                        message: format!(
                            "`{walked}` has no field `{field}` (fields: {})",
                            if fields.is_empty() {
                                "none".to_owned()
                            } else {
                                fields.join(", ")
                            }
                        ),
                    });
                }
            }
            walked = format!("{walked}.{field}");
        }
        Ok(Some(cur))
    }
}

/// A path `a.b.c`: the root's node id and name, then each field's node id and
/// name.
type NamePath = (u64, String, Vec<(u64, String)>);

/// `a.b.c` as root `a` and fields `b`, `c`, with each node's id.
fn path_of(e: &IdedExpr) -> Option<NamePath> {
    let mut fields = Vec::new();
    let mut cur = e;
    loop {
        match &cur.expr {
            Expr::Ident(name) => {
                fields.reverse();
                return Some((cur.id, name.clone(), fields));
            }
            Expr::Select(s) => {
                fields.push((cur.id, s.field.clone()));
                cur = &s.operand;
            }
            _ => return None,
        }
    }
}

/// Follow local `$ref`s.
fn deref<'v>(doc: &'v Value, s: &'v Value) -> &'v Value {
    let mut cur = s;
    for _ in 0..32 {
        match cur.get("$ref").and_then(Value::as_str) {
            Some("#") => cur = doc,
            Some(r) if r.starts_with("#/") => match doc.pointer(&r[1..]) {
                Some(t) => cur = t,
                None => return cur,
            },
            _ => return cur,
        }
    }
    cur
}

enum Field {
    Declared(Value),
    Open,
    Missing,
}

fn field_schema(doc: &Value, s: &Value, field: &str) -> Field {
    let s = deref(doc, s);
    if let Some(Value::Array(branches)) = s.get("anyOf") {
        let mut open = false;
        for b in branches {
            match field_schema(doc, b, field) {
                Field::Declared(found) => return Field::Declared(found),
                Field::Open => open = true,
                Field::Missing => {}
            }
        }
        return if open { Field::Open } else { Field::Missing };
    }
    match s.get("properties").and_then(Value::as_object) {
        Some(props) => match props.get(field) {
            Some(p) => Field::Declared(deref(doc, p).clone()),
            None => match s.get("additionalProperties") {
                None | Some(Value::Bool(false)) => Field::Missing,
                Some(_) => Field::Open,
            },
        },
        None => Field::Open,
    }
}

fn known_fields(doc: &Value, s: &Value) -> Vec<String> {
    let s = deref(doc, s);
    let mut out: Vec<String> = s
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(Value::Array(bs)) = s.get("anyOf") {
        for b in bs {
            out.extend(known_fields(doc, b));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Evaluate `source` with each name in `values` bound to its JSON value, and
/// return the result as JSON. Evaluation reads only those values: CEL has no
/// side effects, so a workflow mapping can be evaluated again on replay and
/// give the same answer.
pub fn evaluate(source: &str, values: &[(&str, &Value)]) -> Result<Value, String> {
    let program = cel::Program::compile(source).map_err(|e| e.to_string())?;
    let mut context = cel::Context::default();
    for (name, value) in values {
        context.add_variable_from_value(*name, cel_value(value));
    }
    let result = program
        .execute(&context)
        .map_err(|e| format!("`{source}`: {e}"))?;
    result
        .json()
        .map_err(|e| format!("`{source}` does not give a JSON value: {e}"))
}

/// `value` as a CEL value. Built by hand rather than through serde, which
/// would turn a number into serde_json's private wrapper when its
/// arbitrary-precision feature is on, as it is in this workspace.
fn cel_value(value: &Value) -> cel::Value {
    match value {
        Value::Null => cel::Value::Null,
        Value::Bool(b) => cel::Value::Bool(*b),
        Value::Number(n) => n
            .as_i64()
            .map(cel::Value::Int)
            .or_else(|| n.as_u64().map(cel::Value::UInt))
            .unwrap_or_else(|| cel::Value::Float(n.as_f64().unwrap_or(f64::NAN))),
        Value::String(s) => cel::Value::String(std::sync::Arc::new(s.clone())),
        Value::Array(items) => {
            cel::Value::List(std::sync::Arc::new(items.iter().map(cel_value).collect()))
        }
        Value::Object(map) => map
            .iter()
            .map(|(key, item)| (key.clone(), cel_value(item)))
            .collect::<std::collections::HashMap<String, cel::Value>>()
            .into(),
    }
}

#[cfg(test)]
mod tests;
