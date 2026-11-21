// SPDX-License-Identifier: Apache-2.0

//! The restricted YAML loader for atom frontmatter, workflows and schemas.
//!
//! YAML here is a way to write JSON: what JSON cannot express, and a plain
//! scalar one YAML version reads as a number of one kind where the other reads
//! a number of another kind or a string, is refused at load with the file, line
//! and column. The loader uses the YAML 1.2 core schema and rejects:
//!
//! - duplicate keys, and keys that are neither a string nor a canonical
//!   decimal integer (`true:`, `null:`, `1.5:`, and equally `+2:`, `007:` --
//!   refused for its leading zero --, `0x1F:` and `0o17:`, which would be
//!   renamed to the number they read as). A canonical integer key is read as
//!   its own text, so `200:` and `"200":` are one key and both in one map are
//!   a duplicate; OpenAPI specs write status codes unquoted;
//! - explicit tags (`!!str`, `!custom`), anchors (`&a`) and aliases (`*a`);
//! - more than one document in a file;
//! - plain (unquoted) `yes`, `no`, `on` and `off` in any letter case, which
//!   YAML 1.1 reads as booleans: quote them to mean the string;
//! - plain numbers whose spelling YAML 1.1 and YAML 1.2 do not read the same
//!   way: a leading zero (`0200` and `010`, which YAML 1.1 reads as 128 and
//!   8), a sign written on either side of a base marker (`0x-1F` and `-0o17`,
//!   which neither version reads as a number, and `-0x1F`, which only 1.1
//!   reads, as -31), and a number YAML 1.1 alone reads: binary (`0b11`), an
//!   underscore in a whole number (`1_000`, `0x_1F`) and the sexagesimal
//!   (`12:30`), which 1.1 reads as 3, 1000, 31 and 750 and 1.2 and JSON read
//!   as strings;
//! - the same shapes in a float, which only YAML 1.1's float type takes: an
//!   underscore among a float's digits (`1_000.5`, `.5_0`, `01_2.5`) and a
//!   sexagesimal with a fraction (`1:30.5`), which 1.1 reads as 1000.5, 0.5,
//!   12.5 and 90.5 and the core schema and JSON read as strings;
//! - a base-8 or base-16 integer past signed 64 bits, which the core schema
//!   reads as an integer this loader does not read as a number at all -- it
//!   keeps the digits it was given, and only a decimal keeps its own digits
//!   past 64 bits;
//! - `.inf` and `.nan`, which JSON has no form for.
//!
//! Only `true` and `false` are booleans; `null`, `~` and an empty value are
//! null; integers and floats follow the core schema, except where a bullet
//! above refuses the spelling.
//!
//! Three spellings are read as the core schema reads them although YAML 1.1
//! reads something else, and each is kept on purpose:
//!
//! - `0o17` is 15 and `0o200` is 128, where 1.1 writes an octal `017` and
//!   `0200` and has no marker with an `o` in it, so PyYAML reads both as
//!   strings;
//! - `1e3` and `1.5e3` are the floats 1000 and 1500, as JSON and the core
//!   schema read an exponent with no sign, where 1.1 takes an exponent only
//!   with a sign and PyYAML reads both texts as strings;
//! - `2001-12-14` stays the string it is written as, where 1.1 resolves an
//!   unquoted date to a timestamp.
//!
//! Quoting any refused spelling gives the string it is read as elsewhere.

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
                        // Digits that are their own text and say no dot and no
                        // exponent, so they are a decimal, and no 64-bit integer
                        // holds them: a decimal that long is still a number here,
                        // the key would be renamed to it, and the same digits
                        // quoted would be a second key. A float keeps the message
                        // below, which says what it reads as and nothing about
                        // how big a number a key may be.
                        Core::Value(Value::Number(n))
                            if n.to_string() == *text && !text.contains(['.', 'e', 'E']) =>
                        {
                            Err(self.error(
                                at,
                                format!(
                                    "keys must be a string or a plain decimal integer a 64-bit \
                                     integer can hold, and this one reads as the number {n}; \
                                     quote it"
                                ),
                            ))
                        }
                        Core::Value(other) => Err(self.error(
                            at,
                            format!(
                                "keys must be a string or a plain decimal integer, \
                                 and this one reads as {other}; quote it"
                            ),
                        )),
                        // A spelling the loader refuses, such as `0200:`, is
                        // refused as a key for the same reason it is refused as
                        // a value. Quoting is the only fix a key has, so the
                        // reason is given without the advice a value is given.
                        Core::Refused(why) => Err(self.error(
                            at,
                            format!(
                                "keys must be a string or a plain decimal integer, and {}",
                                why.as_key()
                            ),
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
            Core::Refused(why) => Err(self.error(at, why.as_value())),
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
    /// because it cannot be read as a number at all.
    Refused(Refusal),
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
        return Core::Refused(Refusal::new(
            format!("unquoted {text:?} is a boolean in YAML 1.1 and a string in YAML 1.2"),
            format!("write true or false for a boolean, or quote it (\"{text}\") for the string"),
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
            None => Core::Refused(no_json(text)),
        };
    }
    // Asked only once neither reading has made a number of the text, so that a
    // float both readings agree on (`00.5`, `1.5e3`) is read as one before it
    // can be refused: what this arm then takes is a shape only YAML 1.1 reads
    // as a number, whether a whole one or a float.
    if let Some(why) = changed_across_yaml_versions(text) {
        return Core::Refused(why);
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
        return Core::Refused(no_json(text));
    }
    Core::Text
}

/// The refusal of a scalar JSON has no form for, which quoting settles.
fn no_json(text: &str) -> Refusal {
    Refusal::new(
        format!("{text} cannot be written in JSON"),
        format!("quote it (\"{text}\") for the string"),
    )
}

/// What the core schema reads a plain scalar written like an integer as.
enum CoreInt {
    /// Not an integer under the core schema: `1.5`, `1_000`, `0xZZ`.
    NotAnInt,
    /// An integer, read as a number.
    Number(Number),
    /// A spelling of an integer that versions do not read alike, or one too
    /// large to read.
    Refused(Refusal),
}

/// Why a plain scalar is refused: what the YAML versions read differently, and
/// what to write instead. A value gets both, because the author meant one of
/// the two. A key is fixed only by quoting, so the key rule names the key,
/// quotes the reason and drops the advice.
#[derive(Debug)]
pub struct Refusal {
    /// What the versions read differently, a clause that reads after "and".
    problem: String,
    /// How to write what the author meant.
    fix: String,
}

impl Refusal {
    fn new(problem: impl Into<String>, fix: impl Into<String>) -> Refusal {
        Refusal {
            problem: problem.into(),
            fix: fix.into(),
        }
    }

    /// The message for a value: the difference, then both ways to fix it. Also
    /// what a caller outside a file says, having no key and no position to
    /// point at.
    pub fn as_value(&self) -> String {
        format!("{}; {}", self.problem, self.fix)
    }

    /// The message for a key: the difference, then the only fix a key has.
    fn as_key(&self) -> String {
        format!("{}; quote it", self.problem)
    }
}

/// The sign a plain number spelling starts with, and the rest of it.
fn split_sign(text: &str) -> (Option<char>, &str) {
    match text.strip_prefix('-') {
        Some(rest) => (Some('-'), rest),
        None => match text.strip_prefix('+') {
            Some(rest) => (Some('+'), rest),
            None => (None, text),
        },
    }
}

/// The sign as text. A plus is no part of the number to write back, so only a
/// minus is written.
fn sign_text(sign: Option<char>) -> &'static str {
    if sign == Some('-') { "-" } else { "" }
}

/// Whether `text` is not empty and every character of it satisfies `keep`.
/// Named for what it asks and not for a digit, because what it is asked of the
/// characters is not always that they are digits: an underscore counts among
/// YAML 1.1's digits and no radix parse takes one.
fn all_chars(text: &str, keep: impl FnMut(char) -> bool) -> bool {
    !text.is_empty() && text.chars().all(keep)
}

/// Whether `digits` is not empty and every character of it is a digit this
/// radix has. An underscore is no digit, so this says no to it.
fn all_digits(digits: &str, radix: u32) -> bool {
    !digits.is_empty() && digits.chars().all(|c| c.is_digit(radix))
}

/// The base these digits are written in, as its radix, what to call it in a
/// sentence, the marker an author writes before it, and the digits after that
/// marker. Decimal is what is left when there is no marker.
fn base_of(body: &str) -> (u32, &'static str, &'static str, &str) {
    if let Some(digits) = body.strip_prefix("0o") {
        return (8, "octal", "0o", digits);
    }
    if let Some(digits) = body.strip_prefix("0x") {
        return (16, "hexadecimal", "0x", digits);
    }
    (10, "decimal", "", body)
}

/// The number these digits say with `sign` in front, or nothing when no signed
/// 64-bit number holds them or when nothing but underscores is left.
/// Underscores are no part of the number, and a decimal's leading zeros are
/// only how the digits were written.
fn signed_number(sign: Option<char>, digits: &str, radix: u32) -> Option<i64> {
    let mut bare = digits.replace('_', "");
    if radix == 10 {
        bare = bare.trim_start_matches('0').to_owned();
    }
    if bare.is_empty() {
        return None;
    }
    i64::from_str_radix(&format!("{}{bare}", sign_text(sign)), radix).ok()
}

/// The number these digits say, as the decimal text to write back, or nothing
/// when no signed 64-bit number holds it.
fn number_text(sign: Option<char>, digits: &str, radix: u32) -> Option<String> {
    signed_number(sign, digits, radix).map(|n| n.to_string())
}

/// Core schema integers: decimal with an optional sign, `0o` octal, `0x` hex.
/// A leading zero is refused rather than read because one version reads an
/// octal where the other reads a decimal, and a sign written on either side of
/// a base marker is refused because at most one version reads such a spelling
/// as a number. A base-8 or base-16 integer past signed 64 bits is refused
/// because this loader cannot read it as a number.
fn core_int(text: &str) -> CoreInt {
    let (sign, body) = split_sign(text);
    if all_chars(body, |c| c.is_ascii_digit()) {
        if body.len() > 1 && body.starts_with('0') {
            // YAML 1.2 reads the digits as decimal; YAML 1.1 reads them as an
            // octal, or as a string when a digit is no octal digit. The two do
            // not always land on a different number (`007` is 7 both ways), but
            // the spelling is a decimal in one version and an octal in the
            // other, so it is refused whatever it lands on.
            // The octal number, when these digits say one a number can hold:
            // that is what 1.1 reads, and its spelling is a fix worth naming.
            let octal = number_text(sign, body, 8).filter(|_| all_digits(body, 8));
            let read_as = match &octal {
                Some(o) => o.clone(),
                // Octal digits, but too many for any number to name.
                None if all_digits(body, 8) => "an octal number".to_owned(),
                // An 8 or a 9 is no octal digit, so YAML 1.1 reads the text as
                // a string rather than as an integer of either base.
                None => "a string".to_owned(),
            };
            // What the digits say without the leading zeros, which is the
            // decimal to write; all zeros say zero.
            let bare = match body.trim_start_matches('0') {
                "" => "0",
                bare => bare,
            };
            let sign = sign_text(sign);
            let octal_fix = if octal.is_some() {
                format!(", or {sign}0o{bare} for octal")
            } else {
                String::new()
            };
            return CoreInt::Refused(Refusal::new(
                format!(
                    "unquoted {text:?} has a leading zero, which YAML 1.1 reads as {read_as} \
                     and YAML 1.2 as decimal"
                ),
                format!("write {sign}{bare}{octal_fix}, or quote it (\"{text}\")"),
            ));
        }
        // An integer too large for 64 bits is still an integer, as JSON reads
        // it: numbers are read here with a precision that holds any decimal, so
        // digits are never read as a string and only a base marker can make a
        // refusal of one. The arm that says string is one serde_json cannot
        // reach for digits; it stays because a loader answers with a string or
        // an error, never a panic.
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
    let (radix, name, marker, digits) = base_of(body);
    if radix == 10 || digits.is_empty() {
        // Nothing here is a base-8 or base-16 spelling, or the marker has no
        // digits after it: either way this is a string, as the core schema
        // reads it. `all_chars` says no to an empty body, so `-` and `+` on
        // their own end up here too.
        return CoreInt::NotAnInt;
    }
    // Which side of the marker the sign sits on decides the message: only a
    // sign before a `0x` is a number in YAML 1.1, whose integer rules take
    // `[-+]?0x...` and have no class an `o` sits in; every other sign around a
    // marker is no number in either version.
    let (inner_sign, inner_digits) = split_sign(digits);
    if inner_sign.is_some() && all_digits(inner_digits, radix) {
        return CoreInt::Refused(sign_around_prefix(
            text,
            sign,
            inner_sign,
            radix,
            name,
            marker,
            inner_digits,
        ));
    }
    if sign.is_some() && all_digits(digits, radix) {
        return CoreInt::Refused(sign_around_prefix(
            text, sign, None, radix, name, marker, digits,
        ));
    }
    match i64::from_str_radix(digits, radix) {
        Ok(n) => CoreInt::Number(Number::from(n)),
        // The radix parse fails on a digit the base has no such thing as, which
        // the core schema reads as a string, and on a value past the signed 64
        // bits a number here is read as.
        Err(_) if !all_digits(digits, radix) => CoreInt::NotAnInt,
        Err(_) => CoreInt::Refused(Refusal::new(
            format!(
                "{text} is a {name} integer past signed 64 bits, which this loader does not \
                 read as a number"
            ),
            // The number to write in decimal is the one these digits say, which
            // is what a decimal that long reads as: only a decimal keeps its own
            // digits past 64 bits. `u128` reaches that far; past even that the
            // digits are out of reach and only the base can be named.
            match u128::from_str_radix(digits, radix).ok() {
                Some(n) => format!("write {n} in decimal, or quote it (\"{text}\") for the string"),
                None => format!("write it in decimal, or quote it (\"{text}\")"),
            },
        )),
    }
}

/// Why a sign written on the wrong side of a base marker is refused. `outer` is
/// the sign before the marker and `inner` the one between the marker and its
/// digits, and one of the two is present. Only a sign before a `0x` makes a
/// number, in YAML 1.1 alone: its integer rules take `[-+]?0x[0-9a-fA-F_]+` and
/// have no class an `o` sits in, so `-0x1F` is -31 there and a string in the
/// core schema, while `-0o17`, `0x-1F` and `0o-17` are no number in either.
fn sign_around_prefix(
    text: &str,
    outer: Option<char>,
    inner: Option<char>,
    radix: u32,
    name: &str,
    marker: &str,
    digits: &str,
) -> Refusal {
    // The number the digits say with the sign that touches them. Two signs, one
    // on each side of the marker, say no one number, so none is named.
    let number = match (outer, inner) {
        (Some(_), Some(_)) => None,
        _ => number_text(inner.or(outer), digits, radix),
    };
    let problem = match (outer, inner) {
        (Some(_), Some(_)) => format!(
            "unquoted {text:?} puts a sign on each side of its {marker}, which is no number in \
             either YAML version"
        ),
        (Some(_), None) if radix == 16 => format!(
            "unquoted {text:?} puts a sign before its {name} prefix, which YAML 1.1 reads as {} \
             and YAML 1.2 as a string",
            number.as_deref().unwrap_or("a number")
        ),
        (Some(_), None) => format!(
            "unquoted {text:?} puts a sign before its {marker} marker, which is no number in \
             either YAML version"
        ),
        _ => format!(
            "unquoted {text:?} puts a sign after its {marker}, which is no number in either \
             YAML version"
        ),
    };
    let fix = match &number {
        Some(n) => format!("write {n} for the number, or quote it (\"{text}\") for the string"),
        // No number holds these digits, or two signs say which was meant, so
        // only the string can be offered.
        None => format!("quote it (\"{text}\") for the string"),
    };
    Refusal::new(problem, fix)
}

/// A number that only YAML 1.1 reads, and the number it reads: binary (`0b11`),
/// a whole number written with an underscore (`1_000`, `0x_1F`, `1000_`), the
/// sexagesimal `12:30`, or a float of the last two kinds (`1_000.5`, `.5_0`,
/// `1:30.5`). YAML 1.1's integer and float types take all four; the core schema
/// and JSON read them as strings.
enum Yaml11 {
    Binary {
        sign: Option<char>,
        digits: String,
    },
    /// Digits with an underscore in them, in the base the digits say: 1.1
    /// reads a leading zero as octal here too, so `010_7` is 71 and not 107.
    Underscored {
        sign: Option<char>,
        radix: u32,
        digits: String,
    },
    Sexagesimal {
        sign: Option<char>,
        value: Option<i128>,
    },
    /// A float, spelled with an underscore among its digits or as a
    /// sexagesimal with a fraction, which only YAML 1.1's float type reads.
    Float {
        sign: Option<char>,
        value: Option<f64>,
    },
}

impl Yaml11 {
    /// What YAML 1.1 reads this as, named in the message so that an author who
    /// meant a number has it, or nothing when no number can hold the digits --
    /// `0b` with 129 of them, say. This text is refused, never published, so
    /// the read is only ever to name it.
    fn value(&self) -> Option<String> {
        match self {
            Yaml11::Binary { sign, digits } => {
                let bare = digits.replace('_', "");
                u128::from_str_radix(&bare, 2)
                    .ok()
                    .map(|n| format!("{}{n}", sign_text(*sign)))
            }
            Yaml11::Underscored {
                sign,
                radix,
                digits,
            } => number_text(*sign, digits, *radix),
            Yaml11::Sexagesimal { sign, value } => {
                value.map(|v| format!("{}{v}", sign_text(*sign)))
            }
            Yaml11::Float { sign, value } => value.map(|v| format!("{}{v}", sign_text(*sign))),
        }
    }
}

/// The base-60 digit one group of a sexagesimal says, or nothing when the group
/// is no `[0-5]?[0-9]`: two digits at most, and no underscore.
fn sexagesimal_group(group: &str) -> Option<i128> {
    match group.as_bytes() {
        [d] if d.is_ascii_digit() => Some(i128::from(d - b'0')),
        [f, s] if (b'0'..=b'5').contains(f) && s.is_ascii_digit() => {
            Some(i128::from((f - b'0') * 10 + (s - b'0')))
        }
        _ => None,
    }
}

/// What YAML 1.1's sexagesimal counts out of `head`, its text with no sign and
/// nothing after the last group: the number it says, `None` where the head is no
/// sexagesimal, and `Some(None)` for one whose groups say more than an `i128`
/// can name. The head starts at 1 and counts every group after a colon in base
/// 60, so `01:30`, `1:70` and `12:3a` are none; an underscore may sit anywhere
/// after the first digit, never where the digit is, so `_1:30` is none either,
/// and `1:2_0` is none because a group takes no underscore.
fn sexagesimal(head: &str) -> Option<Option<i128>> {
    let (first, rest) = head.split_once(':')?;
    if !matches!(first.as_bytes().first(), Some(b'1'..=b'9')) {
        return None;
    }
    let bare = first.replace('_', "");
    if !all_chars(&bare, |c| c.is_ascii_digit()) {
        return None;
    }
    let mut value: Option<i128> = bare.parse().ok();
    for group in rest.split(':') {
        let group = sexagesimal_group(group)?;
        // A chain of groups Python's unbounded integers hold and no number here
        // can name: still 1.1's number, still refused, only unnamed in the
        // message.
        value = match value {
            Some(v) => v.checked_mul(60).and_then(|v| v.checked_add(group)),
            None => None,
        };
    }
    Some(value)
}

/// What YAML 1.1's float type makes of a plain scalar whose digits hold a dot,
/// given the text before the first dot and all of it after. `None` says 1.1
/// reads no float; otherwise the number it reads, or nothing when its groups say
/// more than a number here can name. PyYAML resolves a float with these three
/// alternatives, which take an exponent only with a sign and let an underscore
/// sit anywhere among the digits:
///
/// ```text
/// [-+]?[0-9][0-9_]*\.[0-9_]*([eE][-+][0-9]+)?
///   |  [-+]?\.[0-9_]+([eE][-+][0-9]+)?
///   |  [-+]?[0-9][0-9_]*(:[0-5]?[0-9])+\.[0-9_]*
/// ```
///
/// [`core_float`], which is asked first, reads every text the first two
/// alternatives say without an underscore in it, and reads the third's
/// sexagesimal as a string. So what is left for here — the shapes the two
/// readings disagree on — is an underscore among a float's digits and a
/// sexagesimal with a fraction. A text whose underscores change nothing, like
/// `._`, is a float to 1.1 by its type definition and one PyYAML cannot
/// construct; the loader refuses it and names no number.
fn yaml_11_float(sign: Option<char>, head: &str, tail: &str) -> Option<Yaml11> {
    // An exponent belongs to the fraction, and YAML 1.1 takes one only with a
    // sign: `1.5e3` is the float the core schema reads and YAML 1.1 reads as a
    // string, which is the other way round and is not refused here.
    let (frac, exponent) = match tail.find(['e', 'E']) {
        Some(i) => {
            // The sign is part of the shape: without it 1.1 reads no float and
            // the text is a string to both readings, which is not refused.
            let digits = tail[i + 1..].strip_prefix(['-', '+'])?;
            if !all_chars(digits, |c| c.is_ascii_digit()) {
                return None;
            }
            (&tail[..i], Some(&tail[i..]))
        }
        None => (tail, None),
    };
    // What counts as a float's digits: YAML 1.1 lets an underscore sit anywhere
    // among them, and a radix parse takes none of them, so they are stripped
    // before any number is read.
    let float_digits = |part: &str| all_chars(part, |c| c.is_ascii_digit() || c == '_');
    let bare = |part: &str| part.replace('_', "");
    if head.contains(':') {
        // The third alternative takes no exponent, and its fraction may be
        // empty. Groups that say more than a number here can name leave the
        // refusal to name only the difference.
        let value = sexagesimal(head)?;
        if exponent.is_some() || (!frac.is_empty() && !float_digits(frac)) {
            return None;
        }
        let fraction = if frac.is_empty() {
            0.0
        } else {
            match format!("0.{}", bare(frac)).parse::<f64>() {
                Ok(f) => f,
                Err(_) => return None,
            }
        };
        return Some(Yaml11::Float {
            sign,
            value: value.map(|v| v as f64 + fraction),
        });
    }
    if head.is_empty() {
        // The second alternative, `\.[0-9_]+`: no digits before the dot and at
        // least one after it. A fraction alone with no underscore at all is the
        // float `core_float` has already read.
        if frac.is_empty() || !float_digits(frac) {
            return None;
        }
    } else {
        // The first alternative: the whole part starts at a digit and may hold
        // underscores anywhere after it, and the fraction may be empty.
        let whole_ok =
            matches!(head.as_bytes().first(), Some(b'0'..=b'9')) && float_digits(&bare(head));
        if !whole_ok || (!frac.is_empty() && !float_digits(frac)) {
            return None;
        }
    }
    let text = format!("{}.{}{}", bare(head), bare(frac), exponent.unwrap_or(""));
    Some(Yaml11::Float {
        sign,
        value: text.parse::<f64>().ok(),
    })
}

/// What YAML 1.1's integer and float types read this text as, if either reads
/// it as a number at all. This follows the integer rules YAML 1.1 gives, which
/// is what PyYAML resolves with and what `tools/check_examples.py` mirrors:
///
/// ```text
/// [-+]?0b[0-1_]+  |  [-+]?0[0-7_]+  |  [-+]?(0|[1-9][0-9_]*)
///   |  [-+]?0x[0-9a-fA-F_]+  |  [-+]?[1-9][0-9_]*(:[0-5]?[0-9])+
/// ```
///
/// An underscore counts anywhere after the first digit, so `1000_` and `0x_1F`
/// are numbers there and strings here, and only a leading underscore makes a
/// string of both. A colon group is `[0-5]?[0-9]` and takes no underscore, and
/// there is no `o` marker in this list: `0o17` is the core schema's octal, which
/// [`core_int`] reads as 15. A dot goes to [`yaml_11_float`], since only YAML
/// 1.1's float type takes the dotted shapes that reach here. The shapes the core
/// schema reads as numbers too -- a plain decimal, a plain `0x` hex, a
/// leading-zero octal -- are answered by [`core_int`] as well, which is asked
/// first.
fn yaml_11_int(text: &str) -> Option<Yaml11> {
    let (sign, body) = split_sign(text);
    // A dot is asked about first, because the dotted shapes that get this far
    // are ones `core_float` read no number out of: an underscore among the
    // digits, or a sexagesimal with a fraction.
    if let Some(dot) = body.find('.')
        && let Some(one) = yaml_11_float(sign, &body[..dot], &body[dot + 1..])
    {
        return Some(one);
    }
    if body.contains('o') {
        // No class in those two lists holds an `o`, so nothing with one is a
        // number either of YAML 1.1's types reads. `0o17` is YAML 1.2's octal,
        // which [`core_int`] reads as 15, and `0o7_7` and `0o19.5` are strings
        // to both versions.
        return None;
    }
    match body.strip_prefix("0b") {
        Some(digits) => {
            // `0b2` is no binary number and `0B11` is no binary marker, so both
            // are strings to every version. An underscore alone after the
            // marker is in the class YAML 1.1 gives its binary integers, which
            // is why `0b_` is refused even though no number reads out of it.
            let binary = all_chars(digits, |c| c == '_' || c == '0' || c == '1');
            return binary.then(|| Yaml11::Binary {
                sign,
                digits: digits.to_owned(),
            });
        }
        None => {
            if body.is_empty() {
                return None;
            }
        }
    }
    if body.contains(':') {
        // `1:70`, `1:2_0`, `01:30` and `_1:30` are no sexagesimal and stay
        // strings for both versions, as PyYAML reads them too.
        return Some(Yaml11::Sexagesimal {
            sign,
            value: sexagesimal(body)?,
        });
    }
    if !body.contains('_') {
        return None;
    }
    let (_radix, _, marker, digits) = base_of(body);
    let first = digits.chars().next()?;
    let rest = &digits[first.len_utf8()..];
    // Where the first digit is decides the base these digits are read in, as
    // the three whole-number alternatives say: a leading zero is an octal, `0_`
    // is zero that way too, and only a first digit from 1 up is a decimal. An 8
    // or a 9 after the zero is no octal digit, so no version reads a number.
    // Hex takes a hex digit in any place, and an underscore after the `0x`
    // marker sits in its class, as it does after `0b`.
    let (read_as, head_ok) = if marker == "0x" {
        (16, all_chars(digits, |c| c == '_' || c.is_ascii_hexdigit()))
    } else if first == '0' {
        (8, true)
    } else {
        (10, first.is_ascii_digit())
    };
    // An underscore may then sit anywhere after the first digit, but nothing
    // else may, and the first digit's own base was the question above.
    let shaped = head_ok && rest.chars().all(|c| c == '_' || c.is_digit(read_as));
    shaped.then(|| Yaml11::Underscored {
        sign,
        radix: read_as,
        digits: digits.to_owned(),
    })
}

/// Why a number only one YAML version reads is refused, naming the number it
/// reads; nothing says so when both versions read the same thing.
fn changed_across_yaml_versions(text: &str) -> Option<Refusal> {
    let one = yaml_11_int(text)?;
    let fix = match one.value().as_deref() {
        Some(n) => format!("write {n} for the number, or quote it (\"{text}\") for the string"),
        // No number can hold these digits, so only the string can be offered.
        None => format!("quote it (\"{text}\") for the string"),
    };
    Some(Refusal::new(
        format!("unquoted {text:?} is a number in YAML 1.1 and a string in YAML 1.2 and JSON"),
        fix,
    ))
}

/// Why a plain (unquoted) scalar spelled like an integer is refused, with the
/// message the loader would give it. [`refused_one_version_number`] is the other
/// half: together the two are every number spelling [`core_scalar`] refuses.
///
/// A caller that reads one bare scalar — an environment value, say, where there
/// is no file to point at — and falls back to a string when the loader refuses
/// the spelling needs these. The fallback gives what YAML 1.2 and JSON read for
/// `yes`, which is a boolean in YAML 1.1, and for `.inf`, which both YAML
/// versions read as a float JSON has no form for; the caller may keep the string
/// for those. It may not for `0200`, whose fallback quietly turns the number 200
/// into a string. `None` says the text is no refused integer spelling, whether it
/// is a number, a string or a refusal of another kind.
pub fn refused_number(text: &str) -> Option<Refusal> {
    match core_int(text) {
        CoreInt::Refused(why) => Some(why),
        CoreInt::Number(_) | CoreInt::NotAnInt => None,
    }
}

/// Why a whole number or a float only YAML 1.1 reads is refused — `0b11`,
/// `1_000`, `12:30`, `1_000.5` — with the message the loader would give it. The
/// same caller needs this half too: its fallback reads these the way YAML 1.2
/// and JSON do, as the string, while YAML 1.1 reads 3, 1000, 750 and 1000.5, and
/// a file already refuses every one of them.
pub fn refused_one_version_number(text: &str) -> Option<Refusal> {
    changed_across_yaml_versions(text)
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
