// SPDX-License-Identifier: Apache-2.0

//! The restricted YAML loader for atom frontmatter, workflows and schemas.
//!
//! YAML here is a way to write JSON: anything that JSON cannot express, or that
//! different YAML versions read differently, is refused at load with the file,
//! line and column. The loader uses the YAML 1.2 core schema and rejects:
//!
//! - duplicate keys, and keys that are neither a string nor a canonical
//!   decimal integer (`true:`, `null:`, `1.5:`, and equally `+2:`, `007:`,
//!   `0x1F:` and `0o17:`, which would be renamed to the number they read as).
//!   A canonical integer key is read as its own text, so `200:` and `"200":`
//!   are one key and both in one map are a duplicate; OpenAPI specs write
//!   status codes unquoted;
//! - explicit tags (`!!str`, `!custom`), anchors (`&a`) and aliases (`*a`);
//! - more than one document in a file;
//! - plain (unquoted) `yes`, `no`, `on` and `off` in any letter case, which
//!   YAML 1.1 reads as booleans: quote them to mean the string;
//! - `.inf` and `.nan`, which JSON has no form for.
//!
//! Only `true` and `false` are booleans; `null`, `~` and an empty value are
//! null; integers and floats follow the core schema.

use saphyr_parser::{Event, Parser, ScalarStyle, Span};
use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A position in a source file, both 1-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    /// Line, from 1.
    pub line: usize,
    /// Column, from 1.
    pub column: usize,
}

/// A loaded YAML document: its JSON value, and where each node of it starts,
/// keyed by JSON Pointer (`""` is the root, `/steps/0/atom` a nested value).
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    /// The document as JSON.
    pub value: Value,
    /// Where each value starts in the file.
    pub positions: BTreeMap<String, Position>,
}

impl Document {
    /// Where the value at `pointer` starts, if it exists.
    pub fn position(&self, pointer: &str) -> Option<Position> {
        self.positions.get(pointer).copied()
    }
}

/// Why a YAML file was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{}:{}:{}: {message}", file.display(), at.line, at.column)]
pub struct YamlError {
    /// The file.
    pub file: PathBuf,
    /// Where the problem is.
    pub at: Position,
    /// What is wrong, and how to fix it when that is not obvious.
    pub message: String,
}

/// Load `text`, the contents of `file`, under the rules above. An empty file is
/// a null document.
pub fn load(file: &Path, text: &str) -> Result<Document, YamlError> {
    let mut loader = Loader {
        file: file.to_owned(),
        parser: Parser::new_from_str(text),
        positions: BTreeMap::new(),
        peeked: None,
    };
    loader.document()
}

struct Loader<'a> {
    file: PathBuf,
    parser: Parser<'a, saphyr_parser::StrInput<'a>>,
    positions: BTreeMap<String, Position>,
    peeked: Option<(Event<'a>, Span)>,
}

fn position(span: &Span) -> Position {
    Position {
        line: span.start.line(),
        column: span.start.col() + 1,
    }
}

/// Escape one JSON Pointer reference token (RFC 6901).
fn token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

impl<'a> Loader<'a> {
    fn error(&self, at: Position, message: impl Into<String>) -> YamlError {
        YamlError {
            file: self.file.clone(),
            at,
            message: message.into(),
        }
    }

    fn next(&mut self) -> Result<(Event<'a>, Span), YamlError> {
        if let Some(e) = self.peeked.take() {
            return Ok(e);
        }
        match self.parser.next_event() {
            Some(Ok(e)) => Ok(e),
            Some(Err(e)) => {
                let m = e.marker();
                Err(self.error(
                    Position {
                        line: m.line(),
                        column: m.col() + 1,
                    },
                    e.info().to_string(),
                ))
            }
            None => Err(self.error(Position { line: 1, column: 1 }, "unexpected end of input")),
        }
    }

    fn peek(&mut self) -> Result<&(Event<'a>, Span), YamlError> {
        if self.peeked.is_none() {
            let e = self.next()?;
            self.peeked = Some(e);
        }
        Ok(self.peeked.as_ref().expect("just filled"))
    }

    fn document(&mut self) -> Result<Document, YamlError> {
        let (event, span) = self.next()?;
        if !matches!(event, Event::StreamStart) {
            return Err(self.error(position(&span), "expected the start of a YAML stream"));
        }
        let value = match self.next()? {
            (Event::StreamEnd, _) => {
                self.positions
                    .insert(String::new(), Position { line: 1, column: 1 });
                return Ok(Document {
                    value: Value::Null,
                    positions: std::mem::take(&mut self.positions),
                });
            }
            (Event::DocumentStart(_), _) => {
                let value = self.node(String::new())?;
                match self.next()? {
                    (Event::DocumentEnd, _) => {}
                    (_, span) => {
                        return Err(self.error(position(&span), "expected the end of the document"));
                    }
                }
                value
            }
            (_, span) => return Err(self.error(position(&span), "expected a YAML document")),
        };
        match self.next()? {
            (Event::StreamEnd, _) => Ok(Document {
                value,
                positions: std::mem::take(&mut self.positions),
            }),
            (Event::DocumentStart(_), span) => Err(self.error(
                position(&span),
                "a second YAML document: a file holds exactly one",
            )),
            (_, span) => Err(self.error(position(&span), "unexpected content after the document")),
        }
    }

    fn reject_extras(
        &self,
        at: Position,
        anchor: usize,
        tag: &Option<impl std::fmt::Display>,
    ) -> Result<(), YamlError> {
        if anchor != 0 {
            return Err(self.error(
                at,
                "anchors (&name) are not allowed; repeat the value instead",
            ));
        }
        if let Some(tag) = tag {
            return Err(self.error(at, format!("tags are not allowed (found {tag})")));
        }
        Ok(())
    }

    fn node(&mut self, pointer: String) -> Result<Value, YamlError> {
        let (event, span) = self.next()?;
        let at = position(&span);
        self.positions.insert(pointer.clone(), at);
        match event {
            Event::Alias(_) => Err(self.error(
                at,
                "aliases (*name) are not allowed; repeat the value instead",
            )),
            Event::Scalar(text, style, anchor, tag) => {
                self.reject_extras(at, anchor, &tag)?;
                self.scalar(at, &text, style)
            }
            Event::SequenceStart(anchor, tag) => {
                self.reject_extras(at, anchor, &tag)?;
                let mut items = Vec::new();
                loop {
                    if matches!(self.peek()?.0, Event::SequenceEnd) {
                        self.next()?;
                        break;
                    }
                    let child = format!("{pointer}/{}", items.len());
                    items.push(self.node(child)?);
                }
                Ok(Value::Array(items))
            }
            Event::MappingStart(anchor, tag) => {
                self.reject_extras(at, anchor, &tag)?;
                let mut map = Map::new();
                let mut seen: BTreeMap<String, Position> = BTreeMap::new();
                loop {
                    if matches!(self.peek()?.0, Event::MappingEnd) {
                        self.next()?;
                        break;
                    }
                    let (key, key_at) = self.key()?;
                    if let Some(first) = seen.get(&key) {
                        return Err(self.error(
                            key_at,
                            format!("duplicate key {key:?} (first at line {})", first.line),
                        ));
                    }
                    seen.insert(key.clone(), key_at);
                    let child = format!("{pointer}/{}", token(&key));
                    let value = self.node(child)?;
                    map.insert(key, value);
                }
                Ok(Value::Object(map))
            }
            _ => Err(self.error(at, "unexpected YAML event")),
        }
    }

    fn key(&mut self) -> Result<(String, Position), YamlError> {
        let (event, span) = self.next()?;
        let at = position(&span);
        match event {
            Event::Scalar(text, style, anchor, tag) => {
                self.reject_extras(at, anchor, &tag)?;
                if style == ScalarStyle::Plain {
                    // A plain key must read as a string under the core schema,
                    // or as a canonical decimal integer, which is then read as
                    // its own text: OpenAPI specs write status codes unquoted,
                    // so `200:` is the key `"200"`, and the mapping's duplicate
                    // check sees the two spellings as one key.
                    match self.scalar(at, &text, style)? {
                        Value::String(s) => Ok((s, at)),
                        // Only when the number's string form is the source
                        // text, which leaves `0` and `-?[1-9][0-9]*`, read the
                        // same by YAML 1.1 and 1.2. Any other spelling that
                        // core_int takes for an integer, such as `+2`, `0200`,
                        // `007`, `0x1F`, `0o17` or `-0`, would be renamed to
                        // the value it reads as, so `007:` and `"007":` would
                        // be two different keys; it is refused below.
                        Value::Number(n)
                            if (n.is_i64() || n.is_u64()) && n.to_string() == *text =>
                        {
                            Ok((text.into_owned(), at))
                        }
                        other => Err(self.error(
                            at,
                            format!(
                                "keys must be a string or a plain decimal integer, \
                                 and this one reads as {other}; quote it"
                            ),
                        )),
                    }
                } else {
                    Ok((text.into_owned(), at))
                }
            }
            Event::Alias(_) => Err(self.error(at, "aliases (*name) are not allowed")),
            _ => Err(self.error(
                at,
                "keys must be a string or a plain decimal integer, \
                 not a mapping or a sequence; quote it",
            )),
        }
    }

    fn scalar(&self, at: Position, text: &str, style: ScalarStyle) -> Result<Value, YamlError> {
        if style != ScalarStyle::Plain {
            return Ok(Value::String(text.to_owned()));
        }
        match text {
            "" | "~" | "null" | "Null" | "NULL" => return Ok(Value::Null),
            "true" | "True" | "TRUE" => return Ok(Value::Bool(true)),
            "false" | "False" | "FALSE" => return Ok(Value::Bool(false)),
            _ => {}
        }
        if matches!(
            text.to_ascii_lowercase().as_str(),
            "yes" | "no" | "on" | "off"
        ) {
            return Err(self.error(
                at,
                format!(
                    "unquoted {text:?} is a boolean in YAML 1.1 and a string in YAML 1.2; \
                     write true or false for a boolean, or quote it (\"{text}\") for the string"
                ),
            ));
        }
        if let Some(n) = core_int(text) {
            return Ok(Value::Number(n));
        }
        if let Some(f) = core_float(text) {
            return match Number::from_f64(f) {
                Some(n) => Ok(Value::Number(n)),
                None => Err(self.error(at, format!("{text} cannot be written in JSON"))),
            };
        }
        if matches!(
            text,
            ".inf"
                | ".Inf"
                | ".INF"
                | "+.inf"
                | "+.Inf"
                | "+.INF"
                | "-.inf"
                | "-.Inf"
                | "-.INF"
                | ".nan"
                | ".NaN"
                | ".NAN"
        ) {
            return Err(self.error(at, format!("{text} cannot be written in JSON")));
        }
        Ok(Value::String(text.to_owned()))
    }
}

/// Core schema integers: decimal with an optional sign, `0o` octal, `0x` hex.
fn core_int(text: &str) -> Option<Number> {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        // An integer too large for 64 bits is still an integer, as JSON
        // reads it, never a string.
        return text
            .parse::<i64>()
            .map(Number::from)
            .ok()
            .or_else(|| text.parse::<u64>().map(Number::from).ok())
            .or_else(|| text.trim_start_matches('+').parse::<Number>().ok());
    }
    if let Some(oct) = text.strip_prefix("0o") {
        return i64::from_str_radix(oct, 8).ok().map(Number::from);
    }
    if let Some(hex) = text.strip_prefix("0x") {
        return i64::from_str_radix(hex, 16).ok().map(Number::from);
    }
    None
}

/// Core schema floats: `[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?`.
fn core_float(text: &str) -> Option<f64> {
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some((int, frac)) => {
            (!int.is_empty() || !frac.is_empty())
                && int.bytes().all(|b| b.is_ascii_digit())
                && frac.bytes().all(|b| b.is_ascii_digit())
                && !(int.is_empty() && frac.is_empty())
        }
        None => !mantissa.is_empty() && mantissa.bytes().all(|b| b.is_ascii_digit()),
    };
    let exponent_ok = match exponent {
        None => true,
        Some(e) => {
            let d = e.strip_prefix(['-', '+']).unwrap_or(e);
            !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
        }
    };
    if mantissa_ok && exponent_ok && (mantissa.contains('.') || exponent.is_some()) {
        text.parse::<f64>().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
