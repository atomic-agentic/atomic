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
//! - plain integers whose spelling YAML 1.1 and YAML 1.2 do not read the same
//!   way: a leading zero (`0200` and `010`, which YAML 1.1 reads as the octal
//!   128 and 8), and a sign written after the base prefix (`0x-1F`, which the
//!   core schema reads as a string and a reader that takes a sign there reads
//!   as -31);
//! - a base-8 or base-16 integer past signed 64 bits, which the core schema
//!   reads as an integer this loader cannot read as a number at all, where a
//!   decimal that large is still read as one;
//! - `.inf` and `.nan`, which JSON has no form for.
//!
//! Only `true` and `false` are booleans; `null`, `~` and an empty value are
//! null; integers and floats follow the core schema, except where a bullet
//! above refuses the spelling. The loader does not take a sign before a base
//! prefix, so `-0x1F` and `+0o17` are strings: write `-31` and `15`. Quoting
//! any refused spelling gives the string it is read as elsewhere.

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
                    match core_scalar(&text) {
                        Core::Text => Ok((text.into_owned(), at)),
                        // Only when the number's string form is the source
                        // text, which leaves `0` and `-?[1-9][0-9]*`, read the
                        // same by YAML 1.1 and 1.2. Any other number reads as
                        // a key that would be renamed to the value it is read
                        // as, so `0x1F:` and `"0x1F":` would be two different
                        // keys; it is refused below.
                        Core::Value(Value::Number(n))
                            if (n.is_i64() || n.is_u64()) && n.to_string() == *text =>
                        {
                            Ok((text.into_owned(), at))
                        }
                        Core::Value(other) => Err(self.error(
                            at,
                            format!(
                                "keys must be a string or a plain decimal integer, \
                                 and this one reads as {other}; quote it"
                            ),
                        )),
                        // A spelling core_int refuses, such as `0200:`, is
                        // refused as a key for the same reason it is refused
                        // as a value, and quoting it is the same fix either
                        // way, so the reason is given as it stands.
                        Core::Refused(why) => Err(self.error(
                            at,
                            format!("keys must be a string or a plain decimal integer, and {why}"),
                        )),
                    }
                } else {
                    Ok((text.into_owned(), at))
                }
            }
            Event::Alias(_) => Err(self.error(at, "aliases (*name) are not allowed")),
            // Not "quote it": a mapping or a sequence is no key here whatever
            // its own text, so the first clause is the whole advice.
            _ => Err(self.error(
                at,
                "keys must be a string or a plain decimal integer, not a mapping or a sequence",
            )),
        }
    }

    fn scalar(&self, at: Position, text: &str, style: ScalarStyle) -> Result<Value, YamlError> {
        if style != ScalarStyle::Plain {
            return Ok(Value::String(text.to_owned()));
        }
        match core_scalar(text) {
            Core::Text => Ok(Value::String(text.to_owned())),
            Core::Value(value) => Ok(value),
            Core::Refused(why) => Err(self.error(at, why)),
        }
    }
}

/// What the YAML 1.2 core schema reads a plain (unquoted) scalar as, and what
/// this loader does with the spellings it will not take.
enum Core {
    /// Neither a null, a bool nor a number: the text, read as a string.
    Text,
    /// A null, bool or number that YAML 1.1, YAML 1.2 and JSON read alike.
    Value(Value),
    /// A spelling this loader refuses, because versions do not read it alike or
    /// because it cannot be read as a number at all. The message says which,
    /// and how to write what the author meant.
    Refused(String),
}

fn core_scalar(text: &str) -> Core {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Core::Value(Value::Null),
        "true" | "True" | "TRUE" => return Core::Value(Value::Bool(true)),
        "false" | "False" | "FALSE" => return Core::Value(Value::Bool(false)),
        _ => {}
    }
    if matches!(
        text.to_ascii_lowercase().as_str(),
        "yes" | "no" | "on" | "off"
    ) {
        return Core::Refused(format!(
            "unquoted {text:?} is a boolean in YAML 1.1 and a string in YAML 1.2; \
             write true or false for a boolean, or quote it (\"{text}\") for the string"
        ));
    }
    match core_int(text) {
        CoreInt::Number(n) => return Core::Value(Value::Number(n)),
        CoreInt::Refused(why) => return Core::Refused(why),
        CoreInt::NotAnInt => {}
    }
    if let Some(f) = core_float(text) {
        return match Number::from_f64(f) {
            Some(n) => Core::Value(Value::Number(n)),
            None => Core::Refused(format!("{text} cannot be written in JSON")),
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
        return Core::Refused(format!("{text} cannot be written in JSON"));
    }
    Core::Text
}

/// What the core schema reads a plain scalar written like an integer as.
enum CoreInt {
    /// Not an integer under the core schema: `1.5`, `1_000`, `0xZZ`.
    NotAnInt,
    /// An integer, read as a number.
    Number(Number),
    /// A spelling of an integer that versions do not read alike, or one too
    /// large to read; the message says which, and how to fix it.
    Refused(String),
}

/// Core schema integers: decimal with an optional sign, `0o` octal, `0x` hex.
/// A leading zero and a sign written after the base prefix are refused rather
/// than read, because YAML 1.1 and YAML 1.2 do not read them alike, and a
/// base-8 or base-16 integer past signed 64 bits is refused because this
/// loader cannot read it as a number.
fn core_int(text: &str) -> CoreInt {
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text.strip_prefix('+').unwrap_or(text)),
    };
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        if digits.len() > 1 && digits.starts_with('0') {
            // YAML 1.2 reads the digits as decimal; YAML 1.1 reads them as
            // octal, or as a string when a digit is not an octal digit. The
            // two do not always land on a different number (`007` is 7 both
            // ways), but the spelling is a decimal in one version and an
            // octal in the other, so it is refused whatever it lands on.
            let read_as = if !digits.bytes().all(|b| (b as char).is_digit(8)) {
                // An 8 or a 9 is no octal digit, so YAML 1.1 reads it as a
                // string rather than as an integer of either base.
                "a string".to_owned()
            } else {
                match i64::from_str_radix(digits, 8) {
                    // Name the octal number when a number can hold it.
                    Ok(o) => format!("the octal {sign}{o}"),
                    Err(_) => "an octal integer".to_owned(),
                }
            };
            return CoreInt::Refused(format!(
                "unquoted {text:?} has a leading zero, which YAML 1.1 reads as {read_as} and \
                 YAML 1.2 as decimal; quote it (\"{text}\")"
            ));
        }
        // An integer too large for 64 bits is still an integer, as JSON
        // reads it, never a string. A decimal too many digits for even a
        // float to hold is the string it reads as everywhere else.
        return match text
            .parse::<i64>()
            .map(Number::from)
            .ok()
            .or_else(|| text.parse::<u64>().map(Number::from).ok())
            .or_else(|| text.trim_start_matches('+').parse::<Number>().ok())
        {
            Some(n) => CoreInt::Number(n),
            None => CoreInt::NotAnInt,
        };
    }
    let base = match text.strip_prefix("0o") {
        Some(digits) => Some((8, 'o', "octal", digits)),
        None => text
            .strip_prefix("0x")
            .map(|digits| (16, 'x', "hexadecimal", digits)),
    };
    let Some((radix, marker, name, digits)) = base else {
        return CoreInt::NotAnInt;
    };
    if digits.is_empty() {
        // `0x` and `0o` with no digits after the prefix are strings, as the
        // core schema reads them.
        return CoreInt::NotAnInt;
    }
    // The core schema writes an integer's sign before its prefix, so it reads
    // `0x-1F` as a string; a reader that takes the sign where
    // `from_str_radix` takes it reads the same text as a number. Refused
    // rather than read as a string, so that a typo here cannot quietly change
    // the value's type.
    let sign_inside = match digits.strip_prefix(['-', '+']) {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| (b as char).is_digit(radix)),
        None => false,
    };
    if sign_inside {
        return CoreInt::Refused(format!(
            "unquoted {text:?} is not an integer in the YAML 1.2 core schema, which writes the \
             sign before 0{marker}; quote it (\"{text}\") for the string"
        ));
    }
    match i64::from_str_radix(digits, radix) {
        Ok(n) => CoreInt::Number(Number::from(n)),
        // The radix parse fails on a digit the base has no such thing as,
        // which the core schema reads as a string, and on a value past the
        // signed 64 bits a number here is read as.
        Err(_) if !digits.bytes().all(|b| (b as char).is_digit(radix)) => CoreInt::NotAnInt,
        Err(_) => CoreInt::Refused(format!(
            "{text} is a {name} integer past signed 64 bits, which this loader cannot read as \
             a number; quote it (\"{text}\") for the string"
        )),
    }
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
