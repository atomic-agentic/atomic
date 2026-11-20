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

    /// Every name path the expression reads from its roots, such as
    /// `["steps", "fetch", "output", "city"]`; names bound inside the
    /// expression (comprehension variables) are left out. A literal reads
    /// nothing.
    pub fn references(&self) -> Vec<Vec<String>> {
        fn visit(e: &IdedExpr, locals: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
            match &e.expr {
                Expr::Ident(_) | Expr::Select(_) => {
                    if let Some((_, root, path)) = path_of(e) {
                        if !locals.contains(&root) {
                            let mut names = vec![root];
                            names.extend(path.into_iter().map(|(_, field)| field));
                            out.push(names);
                        }
                    } else if let Expr::Select(s) = &e.expr {
                        visit(&s.operand, locals, out);
                    }
                }
                Expr::Call(c) => {
                    if let Some(t) = &c.target {
                        visit(t, locals, out);
                    }
                    c.args.iter().for_each(|a| visit(a, locals, out));
                }
                Expr::Comprehension(c) => {
                    visit(&c.iter_range, locals, out);
                    visit(&c.accu_init, locals, out);
                    let before = locals.len();
                    locals.push(c.iter_var.clone());
                    if let Some(v2) = &c.iter_var2 {
                        locals.push(v2.clone());
                    }
                    locals.push(c.accu_var.clone());
                    visit(&c.loop_cond, locals, out);
                    visit(&c.loop_step, locals, out);
                    visit(&c.result, locals, out);
                    locals.truncate(before);
                }
                Expr::List(l) => l.elements.iter().for_each(|x| visit(x, locals, out)),
                Expr::Map(m) => m.entries.iter().for_each(|entry| match &entry.expr {
                    EntryExpr::MapEntry(me) => {
                        visit(&me.key, locals, out);
                        visit(&me.value, locals, out);
                    }
                    EntryExpr::StructField(f) => visit(&f.value, locals, out),
                }),
                Expr::Struct(s) => s.entries.iter().for_each(|entry| match &entry.expr {
                    EntryExpr::MapEntry(me) => {
                        visit(&me.key, locals, out);
                        visit(&me.value, locals, out);
                    }
                    EntryExpr::StructField(f) => visit(&f.value, locals, out),
                }),
                Expr::Literal(_) | Expr::Unspecified => {}
            }
        }
        let mut out = Vec::new();
        visit(&self.ast, &mut Vec::new(), &mut out);
        out
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

/// What is known about the type of a value bound in an expression. It
/// decides what CEL type each JSON number becomes, so that the type depends
/// on the contract and never on how the number happened to be written.
#[derive(Clone, Debug)]
pub enum Shape<'a> {
    /// Nothing: every number is a `double`.
    Untyped,
    /// A self-contained JSON Schema; its local references resolve within it.
    Schema(&'a Value),
    /// An object whose named fields have shapes of their own (a workflow's
    /// `steps`, say); any other field is untyped.
    Fields(BTreeMap<String, Shape<'a>>),
}

impl<'a> Shape<'a> {
    /// A [`Shape::Fields`] from `(name, shape)` pairs.
    pub fn fields<K: Into<String>>(fields: impl IntoIterator<Item = (K, Shape<'a>)>) -> Self {
        Shape::Fields(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
}

/// Evaluate `source` with each name bound to its JSON value, typed by its
/// shape, and return the result as JSON. Evaluation reads only those values:
/// CEL has no side effects, so a workflow mapping can be evaluated again on
/// replay and give the same answer.
///
/// This is the one place atomic evaluates CEL over JSON. Every caller, from
/// workflow mappings to eval assertions, goes through it, so each sees the
/// same conversions: [`cel_value`] in and [`json_value`] out.
pub fn evaluate(source: &str, values: &[(&str, &Value, &Shape<'_>)]) -> Result<Value, String> {
    let program = cel::Program::compile(source).map_err(|e| e.to_string())?;
    let mut context = cel::Context::default();
    for (name, value, shape) in values {
        let value = cel_value(value, shape).map_err(|e| format!("`{name}`{e}"))?;
        context.add_variable_from_value(*name, value);
    }
    let result = program
        .execute(&context)
        .map_err(|e| format!("`{source}`: {e}"))?;
    json_value(&result).map_err(|e| format!("`{source}` does not give a JSON value: {e}"))
}

/// `value` as a CEL value, its numbers typed by `shape`.
///
/// Built by hand rather than through serde (`Context::add_variable`):
/// serde_json's `arbitrary_precision` feature is on in this workspace, and
/// under it a number serializes as a private one-entry map.
///
/// The type depends on the schema, never on how the number was written:
///
/// - a field only integers can fill (its `type`, a local `$ref`, any `allOf`
///   member, or every `anyOf` or `oneOf` branch that allows a number says
///   `integer`) is an `int`, or a `uint` above `i64::MAX`, whether written
///   `1`, `1.0` or `1e0`; one neither can hold is refused;
/// - a field typed `number` is a `double`; an integer a double cannot hold
///   exactly is refused, with advice to type the field `integer`;
/// - a field the schema does not type keeps an integer as an `int` (or a
///   `uint`), and anything else as a `double`;
/// - a number `f64` cannot hold (`1e400`) is refused.
pub fn cel_value(value: &Value, shape: &Shape<'_>) -> Result<cel::Value, String> {
    to_cel(value, &Ty::from_shape(shape), &mut String::new())
}

/// A CEL result as JSON. A `double` that is not finite is refused, and an
/// object's keys come out sorted, so a result is the same on every replay.
pub fn json_value(value: &cel::Value) -> Result<Value, String> {
    use cel::objects::Key;
    Ok(match value {
        cel::Value::Null => Value::Null,
        cel::Value::Bool(b) => Value::Bool(*b),
        cel::Value::Int(i) => Value::from(*i),
        cel::Value::UInt(u) => Value::from(*u),
        cel::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .ok_or_else(|| format!("{f} is not a finite number"))?,
        cel::Value::String(s) => Value::String(s.to_string()),
        cel::Value::List(items) => Value::Array(
            items
                .iter()
                .map(json_value)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        cel::Value::Map(map) => {
            let mut entries = map
                .map
                .iter()
                .map(|(key, item)| {
                    // JSON keys are strings; `{1: 'a', '1': 'b'}` has two.
                    let Key::String(key) = key else {
                        return Err(format!("a map key `{key:?}` is not a string"));
                    };
                    Ok((key.to_string(), json_value(item)?))
                })
                .collect::<Result<Vec<_>, String>>()?;
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().collect())
        }
        other => other.json().map_err(|e| e.to_string())?,
    })
}

/// How many schema nodes typing one number may visit before it gives up and
/// treats the number as untyped. Bounds unions of references that would
/// otherwise multiply.
const TYPE_BUDGET: usize = 4096;

/// What a schema says a number at one position is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Every number valid there is an integer.
    Integer,
    /// The schema types it `number`.
    Number,
    /// The schema does not say.
    Unknown,
}

/// A shape narrowed to one position in a value: the schemas that all apply
/// there (`All`), or that one of applies (`Any`).
#[derive(Clone)]
enum Ty<'a> {
    Untyped,
    Node(&'a Value, &'a Value),
    Fields(&'a BTreeMap<String, Shape<'a>>),
    All(Vec<Ty<'a>>),
    Any(Vec<Ty<'a>>),
}

/// One step into a value.
#[derive(Clone, Copy)]
enum Step<'k> {
    Field(&'k str),
    Item(usize),
}

impl Step<'_> {
    /// The JSON type a value must have to take this step.
    fn container(self) -> &'static str {
        match self {
            Step::Field(_) => "object",
            Step::Item(_) => "array",
        }
    }
}

/// The work one typing question may do: a budget of nodes, and the
/// references being followed, so a reference cycle stops rather than repeats.
struct Walk {
    budget: usize,
    path: Vec<*const Value>,
}

impl Walk {
    fn new() -> Self {
        Self {
            budget: TYPE_BUDGET,
            path: Vec::new(),
        }
    }

    fn spend(&mut self) -> bool {
        if self.budget == 0 {
            return false;
        }
        self.budget -= 1;
        true
    }

    /// Follow a reference to `target`, unless it is already being followed.
    fn enter(&mut self, target: &Value) -> bool {
        let ptr = std::ptr::from_ref(target);
        if self.path.contains(&ptr) {
            return false;
        }
        self.path.push(ptr);
        true
    }

    fn leave(&mut self) {
        self.path.pop();
    }
}

/// `parts` that must all hold, with nested conjunctions flattened.
fn all_of(parts: Vec<Ty<'_>>) -> Ty<'_> {
    let mut flat = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            Ty::All(inner) => flat.extend(inner),
            other => flat.push(other),
        }
    }
    if flat.len() == 1 {
        flat.pop().expect("one part")
    } else {
        Ty::All(flat)
    }
}

/// `parts` one of which holds, with nested alternatives flattened.
fn any_of(parts: Vec<Ty<'_>>) -> Ty<'_> {
    let mut flat = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            Ty::Any(inner) => flat.extend(inner),
            other => flat.push(other),
        }
    }
    if flat.len() == 1 {
        flat.pop().expect("one part")
    } else {
        Ty::Any(flat)
    }
}

/// The JSON types `node` allows, following a chain of local references, or
/// `None` when it does not say.
fn types_of<'a>(root: &'a Value, node: &'a Value, walk: &mut Walk) -> Option<Vec<&'a str>> {
    match node.get("type") {
        Some(Value::String(t)) => Some(vec![t.as_str()]),
        Some(Value::Array(ts)) => Some(ts.iter().filter_map(Value::as_str).collect()),
        _ => {
            let target = local_ref(root, node)?;
            if !walk.spend() || !walk.enter(target) {
                return None;
            }
            let types = types_of(root, target, walk);
            walk.leave();
            types
        }
    }
}

impl<'a> Ty<'a> {
    fn from_shape(shape: &'a Shape<'a>) -> Self {
        match shape {
            Shape::Untyped => Ty::Untyped,
            Shape::Schema(root) => Ty::Node(root, root),
            Shape::Fields(fields) => Ty::Fields(fields),
        }
    }

    /// What a number here is.
    fn kind(&self) -> Kind {
        if self.integer(&mut Walk::new()) {
            Kind::Integer
        } else if self.number(&mut Walk::new()) {
            Kind::Number
        } else {
            Kind::Unknown
        }
    }

    /// Whether no number but an integer can be valid here. A schema whose
    /// `type` allows no number at all (`null`, `object`) says so vacuously, so
    /// `anyOf: [{type: integer}, {type: null}]` is an integer.
    fn integer(&self, walk: &mut Walk) -> bool {
        if !walk.spend() {
            return false;
        }
        match self {
            Ty::Untyped | Ty::Fields(_) => false,
            Ty::All(parts) => parts.iter().any(|t| t.integer(walk)),
            Ty::Any(parts) => !parts.is_empty() && parts.iter().all(|t| t.integer(walk)),
            Ty::Node(root, node) => {
                let typed = match node.get("type") {
                    Some(Value::String(t)) => t != "number",
                    Some(Value::Array(ts)) => !ts.iter().any(|t| t == "number"),
                    _ => false,
                };
                typed
                    || local_ref(root, node).is_some_and(|t| {
                        walk.enter(t) && {
                            let integer = Ty::Node(root, t).integer(walk);
                            walk.leave();
                            integer
                        }
                    })
                    || node
                        .get("allOf")
                        .and_then(Value::as_array)
                        .is_some_and(|ms| ms.iter().any(|m| Ty::Node(root, m).integer(walk)))
                    || ["anyOf", "oneOf"].iter().any(|k| {
                        node.get(*k).and_then(Value::as_array).is_some_and(|bs| {
                            !bs.is_empty() && bs.iter().all(|b| Ty::Node(root, b).integer(walk))
                        })
                    })
            }
        }
    }

    /// Whether some schema here types a number `number`.
    fn number(&self, walk: &mut Walk) -> bool {
        if !walk.spend() {
            return false;
        }
        match self {
            Ty::Untyped | Ty::Fields(_) => false,
            Ty::All(parts) | Ty::Any(parts) => parts.iter().any(|t| t.number(walk)),
            Ty::Node(root, node) => {
                let typed = match node.get("type") {
                    Some(Value::String(t)) => t == "number",
                    Some(Value::Array(ts)) => ts.iter().any(|t| t == "number"),
                    _ => false,
                };
                typed
                    || local_ref(root, node).is_some_and(|t| {
                        walk.enter(t) && {
                            let number = Ty::Node(root, t).number(walk);
                            walk.leave();
                            number
                        }
                    })
                    || ["allOf", "anyOf", "oneOf"].iter().any(|k| {
                        node.get(*k)
                            .and_then(Value::as_array)
                            .is_some_and(|ms| ms.iter().any(|m| Ty::Node(root, m).number(walk)))
                    })
            }
        }
    }

    /// Whether a value here may be of JSON type `container`.
    fn may_be(&self, container: &str, walk: &mut Walk) -> bool {
        match self {
            Ty::Node(root, node) => {
                types_of(root, node, walk).is_none_or(|types| types.contains(&container))
            }
            _ => true,
        }
    }

    /// The type one step into a value here.
    fn child(&self, step: Step<'_>) -> Ty<'a> {
        self.step(step, &mut Walk::new())
    }

    fn step(&self, step: Step<'_>, walk: &mut Walk) -> Ty<'a> {
        if !walk.spend() {
            return Ty::Untyped;
        }
        match self {
            Ty::Untyped => Ty::Untyped,
            Ty::Fields(map) => match step {
                Step::Field(key) => map.get(key).map(Ty::from_shape).unwrap_or(Ty::Untyped),
                Step::Item(_) => Ty::Untyped,
            },
            Ty::All(parts) => all_of(parts.iter().map(|t| t.step(step, walk)).collect()),
            Ty::Any(parts) => {
                let mut children = Vec::new();
                for part in parts {
                    // An alternative that cannot be an object (or a list) says
                    // nothing about a field (or an item).
                    if part.may_be(step.container(), walk) {
                        children.push(part.step(step, walk));
                    }
                }
                any_of(children)
            }
            Ty::Node(root, node) => {
                let mut parts: Vec<Ty<'a>> = Vec::new();
                let direct = match step {
                    Step::Field(key) => match node.get("properties").and_then(|p| p.get(key)) {
                        Some(s) => Some(s),
                        // Beside patternProperties, additionalProperties may
                        // not be what applies to `key`.
                        None => node
                            .get("additionalProperties")
                            .filter(|s| s.is_object() && node.get("patternProperties").is_none()),
                    },
                    Step::Item(index) => match node.get("prefixItems").and_then(|p| p.get(index)) {
                        Some(s) => Some(s),
                        None => node.get("items").filter(|s| s.is_object()),
                    },
                };
                parts.extend(direct.map(|s| Ty::Node(root, s)));
                if let Some(target) = local_ref(root, node)
                    && walk.enter(target)
                {
                    parts.push(Ty::Node(root, target).step(step, walk));
                    walk.leave();
                }
                if let Some(members) = node.get("allOf").and_then(Value::as_array) {
                    for m in members {
                        parts.push(Ty::Node(root, m).step(step, walk));
                    }
                }
                for k in ["anyOf", "oneOf"] {
                    if let Some(branches) = node.get(k).and_then(Value::as_array) {
                        let branches = branches.iter().map(|b| Ty::Node(root, b)).collect();
                        parts.push(Ty::Any(branches).step(step, walk));
                    }
                }
                all_of(parts)
            }
        }
    }
}

/// The subschema a local `$ref` (`#` or `#/...`) names.
fn local_ref<'a>(root: &'a Value, node: &Value) -> Option<&'a Value> {
    let r = node.get("$ref")?.as_str()?;
    root.pointer(r.strip_prefix('#')?)
}

/// Whether `value` converts to CEL under `schema`, as every expression that
/// later reads it will convert it. Run where a value is accepted, so that one
/// the conversion refuses is an error there, not in every later expression.
pub fn convertible(value: &Value, schema: &Value) -> Result<(), String> {
    cel_value(value, &Shape::Schema(schema)).map(drop)
}

fn to_cel(value: &Value, ty: &Ty<'_>, at: &mut String) -> Result<cel::Value, String> {
    Ok(match value {
        Value::Null => cel::Value::Null,
        Value::Bool(b) => cel::Value::Bool(*b),
        Value::Number(n) => number(n, ty.kind()).map_err(|e| {
            format!(
                "{}: {e}",
                if at.is_empty() {
                    "the value"
                } else {
                    at.as_str()
                }
            )
        })?,
        Value::String(s) => cel::Value::String(std::sync::Arc::new(s.clone())),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                let len = at.len();
                at.push_str(&format!("[{i}]"));
                out.push(to_cel(item, &ty.child(Step::Item(i)), at)?);
                at.truncate(len);
            }
            cel::Value::List(std::sync::Arc::new(out))
        }
        Value::Object(map) => {
            let mut out = std::collections::HashMap::with_capacity(map.len());
            for (key, item) in map {
                let len = at.len();
                at.push('.');
                at.push_str(key);
                out.insert(key.clone(), to_cel(item, &ty.child(Step::Field(key)), at)?);
                at.truncate(len);
            }
            out.into()
        }
    })
}

/// A number as CEL, by what its schema says it is.
fn number(n: &serde_json::Number, kind: Kind) -> Result<cel::Value, String> {
    let exact = exact_integer(&n.to_string());
    let as_int = |value: i128| {
        i64::try_from(value)
            .map(cel::Value::Int)
            .or_else(|_| u64::try_from(value).map(cel::Value::UInt))
            .ok()
    };
    let double = || {
        n.as_f64()
            .filter(|f| f.is_finite())
            .map(cel::Value::Float)
            .ok_or_else(|| format!("{n} is not a finite number"))
    };
    match (kind, exact) {
        (Kind::Integer, Some(value)) => {
            as_int(value).ok_or_else(|| format!("{n} is an integer neither int nor uint can hold"))
        }
        (Kind::Integer, None) => Err(format!("{n} is not an integer CEL can hold")),
        // An integer the schema does not type stays an integer, as it was
        // written; one too large for int or uint is a double if it is exact.
        (Kind::Unknown, Some(value)) => match as_int(value) {
            Some(v) => Ok(v),
            None => exact_double(n, value),
        },
        (Kind::Number, Some(value)) => exact_double(n, value),
        (_, None) => double(),
    }
}

/// An integer as a CEL double, only if the double is exactly that integer.
fn exact_double(n: &serde_json::Number, value: i128) -> Result<cel::Value, String> {
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let f = value as f64;
    #[allow(clippy::cast_possible_truncation)]
    if f.is_finite() && f as i128 == value {
        Ok(cel::Value::Float(f))
    } else {
        Err(format!(
            "{n} is an integer a double cannot hold exactly; type the field `integer`"
        ))
    }
}

/// The integer a JSON number's text denotes, if it denotes one that fits in
/// an `i128`. Read exactly from the text, so `1.0`, `1e0` and `10e-1` are all
/// `1`, and nothing is rounded through `f64` on the way.
fn exact_integer(text: &str) -> Option<i128> {
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(i) => (&rest[..i], rest[i + 1..].parse::<i64>().ok()?),
        None => (rest, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits = format!("{whole}{fraction}");
    // Zero is zero whatever its exponent: `0e40`, `0.0e-400`.
    if digits.bytes().all(|b| b == b'0') {
        return Some(0);
    }
    let shift = exponent.checked_sub(i64::try_from(fraction.len()).ok()?)?;
    if shift >= 0 {
        // More than 39 digits cannot fit in an i128 anyway.
        let significant = i64::try_from(digits.trim_start_matches('0').len()).ok()?;
        if significant.checked_add(shift)? > 39 {
            return None;
        }
        digits.extend(std::iter::repeat_n('0', usize::try_from(shift).ok()?));
    } else {
        let cut = usize::try_from(shift.unsigned_abs()).ok()?;
        // Only zeros may be shifted out, and the digits are not all zeros.
        if cut > digits.len() {
            return None;
        }
        let (kept, dropped) = digits.split_at(digits.len() - cut);
        if !dropped.bytes().all(|b| b == b'0') {
            return None;
        }
        digits = kept.to_owned();
    }
    let digits = digits.trim_start_matches('0');
    if digits.len() > 39 {
        return None;
    }
    let magnitude: i128 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    Some(if negative { -magnitude } else { magnitude })
}

#[cfg(test)]
mod tests;
