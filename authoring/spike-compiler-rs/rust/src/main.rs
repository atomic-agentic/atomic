// SPDX-License-Identifier: Apache-2.0
//! Parse `.astro` files with Astro's Rust parser and print one JSON line per
//! file: the ESTree AST the npm binding returns (without its comment list),
//! the diagnostics, and the median parse time over `ITERATIONS` runs.
//!
//! The serialization follows compiler-rs's `astro_napi::parse_astro_impl`
//! (MIT, Astro contributors).

use std::time::Instant;

use oxc_allocator::Allocator;
use oxc_ast_visit::{VisitMut, utf8_to_utf16::Utf8ToUtf16};
use oxc_estree::{CompactTSSerializer, ESTree};
use oxc_parser::{ParseOptions, Parser};
use oxc_span::SourceType;

fn parse_once(source: &str) -> (String, Vec<serde_like::Diag>) {
    let allocator = Allocator::default();
    let mut ret = Parser::new(&allocator, source, SourceType::astro())
        .with_options(ParseOptions::default())
        .parse_astro();
    let diags = ret
        .errors
        .iter()
        .map(|error| serde_like::Diag {
            message: error.message.to_string(),
            labels: error
                .labels
                .iter()
                .flatten()
                .map(|label| (label.offset(), label.len()))
                .collect(),
        })
        .collect();
    if let Some(mut converter) = Utf8ToUtf16::new(source).converter() {
        converter.visit_astro_root(&mut ret.root);
    }
    let mut serializer = CompactTSSerializer::new(false);
    ret.root.serialize(&mut serializer);
    (serializer.into_string(), diags)
}

fn parse_only(source: &str) -> usize {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source, SourceType::astro())
        .with_options(ParseOptions::default())
        .parse_astro();
    ret.root.body.len() + ret.errors.len()
}

mod serde_like {
    pub struct Diag {
        pub message: String,
        pub labels: Vec<(usize, usize)>,
    }
}

fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn main() {
    let iterations: usize = std::env::var("ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    for path in std::env::args().skip(1) {
        let source = std::fs::read_to_string(&path).expect("read source");
        let (ast, diags) = parse_once(&source);
        let mut samples = Vec::with_capacity(iterations);
        let mut sink = 0;
        for _ in 0..iterations {
            let start = Instant::now();
            sink += parse_only(&source);
            samples.push(start.elapsed().as_nanos());
        }
        samples.sort_unstable();
        let median = samples.get(samples.len() / 2).copied().unwrap_or(0);
        let diagnostics: Vec<String> = diags
            .iter()
            .map(|d| {
                let labels: Vec<String> =
                    d.labels.iter().map(|(o, l)| format!("[{o},{l}]")).collect();
                format!("{{\"message\":{},\"labels\":[{}]}}", quote(&d.message), labels.join(","))
            })
            .collect();
        println!(
            "{{\"file\":{},\"medianNs\":{median},\"sink\":{sink},\"diagnostics\":[{}],\"ast\":{ast}}}",
            quote(&path),
            diagnostics.join(",")
        );
    }
}
