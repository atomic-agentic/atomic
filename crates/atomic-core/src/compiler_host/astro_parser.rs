// SPDX-License-Identifier: Apache-2.0

//! Lower Astro's native parser tree to the nodes consumed by the declarative renderer.
//! Source slices and offsets are UTF-8. Columns in diagnostics are UTF-16 for editors.

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::Visit;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};
use serde_json::{Value, json};

struct Source<'a> {
    text: &'a str,
    // Byte and UTF-16 offsets of each line, and the cumulative byte excess after
    // each multibyte character. Lookup stays logarithmic even on a single long line.
    lines: Vec<(usize, usize)>,
    excess: Vec<(usize, usize)>,
}

impl<'a> Source<'a> {
    fn new(text: &'a str) -> Self {
        let mut lines = vec![(0, 0)];
        let mut excess = Vec::new();
        let mut units = 0;
        for (byte, ch) in text.char_indices() {
            units += ch.len_utf16();
            let end = byte + ch.len_utf8();
            if !ch.is_ascii() {
                excess.push((end, end - units));
            }
            if ch == '\n' {
                lines.push((end, units));
            }
        }
        Self {
            text,
            lines,
            excess,
        }
    }

    fn point(&self, offset: u32) -> Value {
        let offset = offset as usize;
        let line = self.lines.partition_point(|(b, _)| *b <= offset);
        let column = self.utf16(offset) - self.lines[line - 1].1 + 1;
        json!({"line":line, "column":column, "offset":offset})
    }

    fn utf16(&self, offset: usize) -> usize {
        let excess = self.excess.partition_point(|(b, _)| *b <= offset);
        let excess = excess.checked_sub(1).map_or(0, |i| self.excess[i].1);
        offset - excess
    }

    fn position(&self, span: Span) -> Value {
        json!({"start":self.point(span.start), "end":self.point(span.end)})
    }

    fn slice(&self, span: Span) -> &'a str {
        span.source_text(self.text)
    }

    fn text_node(&self, span: Span) -> Value {
        json!({"type":"text", "value":self.slice(span), "position":self.position(span)})
    }

    fn child(&self, node: &JSXChild<'_>) -> Value {
        match node {
            JSXChild::Element(n) => self.element(n),
            JSXChild::Fragment(n) => self.fragment(n),
            JSXChild::Text(n) => self.text_node(n.span),
            JSXChild::AstroComment(n) => {
                json!({"type":"comment","value":n.value.as_str(),"position":self.position(n.span)})
            }
            JSXChild::AstroDoctype(n) => json!({"type":"doctype","position":self.position(n.span)}),
            JSXChild::AstroScript(n) => self.text_node(n.program.span),
            JSXChild::Spread(n) => json!({"type":"spread","position":self.position(n.span)}),
            JSXChild::ExpressionContainer(n) => {
                let mut parts = Parts {
                    source: self,
                    cursor: n.span.start + 1,
                    children: Vec::new(),
                };
                parts.visit_jsx_expression(&n.expression);
                parts.text_to(n.span.end - 1);
                json!({"type":"expression", "children":parts.children, "position":self.position(n.span)})
            }
        }
    }

    fn children(&self, children: &[JSXChild<'_>]) -> Vec<Value> {
        children.iter().map(|child| self.child(child)).collect()
    }

    fn fragment(&self, node: &JSXFragment<'_>) -> Value {
        json!({"type":"fragment","name":"","attributes":[],"children":self.children(&node.children),"position":self.position(node.span)})
    }

    fn element(&self, node: &JSXElement<'_>) -> Value {
        let open = &node.opening_element;
        let name = self.slice(open.name.span());
        let kind = if name == "Fragment" {
            "fragment"
        } else if name.contains('.') || name.starts_with(|c: char| c.is_ascii_uppercase()) {
            "component"
        } else if name.contains('-') {
            "custom-element"
        } else {
            "element"
        };
        let attrs: Vec<_> = open.attributes.iter().map(|a| self.attribute(a)).collect();
        json!({"type":kind,"name":name,"attributes":attrs,"children":self.children(&node.children),"position":self.position(node.span)})
    }

    fn attribute(&self, node: &JSXAttributeItem<'_>) -> Value {
        let (name, kind, value) = match node {
            JSXAttributeItem::SpreadAttribute(a) => (self.slice(a.argument.span()), "spread", ""),
            JSXAttributeItem::Attribute(a) => {
                let name = self.slice(a.name.span());
                let (kind, value) = match &a.value {
                    None => ("empty", ""),
                    Some(JSXAttributeValue::StringLiteral(s)) => {
                        let raw = self.slice(s.span);
                        match raw.as_bytes().first() {
                            Some(quote @ (b'\'' | b'"'))
                                if raw.len() >= 2 && raw.as_bytes().last() == Some(quote) =>
                            {
                                ("quoted", &raw[1..raw.len() - 1])
                            }
                            _ => ("quoted", raw),
                        }
                    }
                    Some(JSXAttributeValue::ExpressionContainer(e))
                        if e.span.start == a.span.start =>
                    {
                        ("shorthand", "")
                    }
                    Some(JSXAttributeValue::ExpressionContainer(e)) => {
                        let kind = if matches!(&e.expression, JSXExpression::TemplateLiteral(t) if t.span.start == e.span.start)
                        {
                            "template-literal"
                        } else {
                            "expression"
                        };
                        (
                            kind,
                            self.slice(Span::new(e.span.start + 1, e.span.end - 1)),
                        )
                    }
                    Some(other) => ("unsupported", self.slice(other.span())),
                };
                (name, kind, value)
            }
        };
        json!({"type":"attribute","name":name,"kind":kind,"value":value,"position":self.position(node.span())})
    }
}

// An expression is source text interleaved with its outermost embedded JSX nodes.
// The visitor stops at each such node; its children are lowered exactly once.
struct Parts<'s, 'a> {
    source: &'s Source<'a>,
    cursor: u32,
    children: Vec<Value>,
}

impl Parts<'_, '_> {
    fn text_to(&mut self, end: u32) {
        if self.cursor < end {
            self.children
                .push(self.source.text_node(Span::new(self.cursor, end)));
        }
    }
}

impl<'ast> Visit<'ast> for Parts<'_, '_> {
    fn visit_jsx_element(&mut self, node: &JSXElement<'ast>) {
        self.text_to(node.span.start);
        self.children.push(self.source.element(node));
        self.cursor = node.span.end;
    }

    fn visit_jsx_fragment(&mut self, node: &JSXFragment<'ast>) {
        self.text_to(node.span.start);
        self.children.push(self.source.fragment(node));
        self.cursor = node.span.end;
    }
}

pub(super) fn parse(text: &str) -> Value {
    let arena = Allocator::default();
    let parsed = Parser::new(&arena, text, SourceType::astro()).parse_astro();
    let source = Source::new(text);
    let mut diagnostics: Vec<_> = parsed
        .errors
        .iter()
        .map(|e| {
            let offset = e.labels.iter().flatten().next().map_or(0, |l| l.offset());
            json!({"severity":1,"text":e.message,"location":source.point(offset as u32)})
        })
        .collect();
    let root = parsed.root;
    let frontmatter = root.frontmatter.as_ref().filter(|fm| !fm.span.is_empty());
    let consumed = root
        .body
        .last()
        .map(|n| n.span().end)
        .or_else(|| frontmatter.map(|fm| fm.span.end))
        .unwrap_or(0);
    if diagnostics.is_empty() && !text[consumed as usize..].trim().is_empty() {
        diagnostics.push(json!({"severity":1,"text":"unconsumed Astro source","location":source.point(consumed)}));
    }
    let mut children = Vec::new();
    if diagnostics.is_empty() {
        if let Some(fm) = frontmatter {
            let raw = source.slice(fm.span);
            let raw = raw.strip_prefix("---").unwrap_or(raw);
            children.push(json!({"type":"frontmatter","value":raw.strip_suffix("---").unwrap_or(raw),"position":source.position(fm.span)}));
        }
        let mut after_fence = frontmatter.is_some();
        for n in &root.body {
            if let JSXChild::Text(node) = n
                && after_fence
            {
                let raw = source.slice(node.span);
                let trimmed = raw
                    .strip_prefix("\r\n")
                    .or_else(|| raw.strip_prefix('\n'))
                    .unwrap_or(raw);
                if !trimmed.is_empty() {
                    children.push(source.text_node(Span::new(
                        node.span.end - trimmed.len() as u32,
                        node.span.end,
                    )));
                }
            } else {
                children.push(source.child(n));
            }
            after_fence = matches!(n, JSXChild::AstroDoctype(_));
        }
        if frontmatter.is_some()
            && children.iter().skip(1).all(|n| {
                n["type"] == "text" && n["value"].as_str().is_some_and(|s| s.trim().is_empty())
            })
        {
            children.truncate(1);
        }
    }
    json!({"ast":{"type":"root","children":children},"diagnostics":diagnostics})
}

// The upstream converter maps generated to authored byte offsets. The TypeScript
// checker addresses both strings in UTF-16. Preserve generated-only runs as null.
pub(super) fn tsx(text: &str, filename: &str) -> Value {
    let result = astro2tsx::convert_to_tsx(
        text,
        astro2tsx::ConvertOptions {
            filename: Some(filename.to_owned()),
            ambient_types: false,
        },
    );
    let generated = Source::new(&result.code);
    let original = Source::new(text);
    let mappings: Vec<_> = result
        .mappings
        .iter()
        .map(|m| {
            (
                generated.utf16(m.generated as usize),
                m.original.map(|n| original.utf16(n as usize)),
            )
        })
        .collect();
    json!({"code":result.code,"mappings":mappings})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconsumed_source_is_a_located_parse_error() {
        let result = parse("Hello\n<étiquette>lost</étiquette>");
        assert_eq!(result["ast"]["children"], json!([]));
        assert_eq!(result["diagnostics"][0]["severity"], 1);
        assert_eq!(result["diagnostics"][0]["location"]["line"], 2);
    }

    #[test]
    fn jsx_in_an_expression_keeps_authored_text_and_unicode_positions() {
        let result = parse("😀{items.map(x => <Note>{x}</Note>)}");
        assert_eq!(result["diagnostics"], json!([]));
        let expression = &result["ast"]["children"][1];
        assert_eq!(
            expression["position"]["start"],
            json!({"offset":4,"line":1,"column":3})
        );
        let parts = &expression["children"];
        assert_eq!(parts[0]["value"], "items.map(x => ");
        assert_eq!(parts[1]["name"], "Note");
        assert_eq!(parts[1]["children"][0]["children"][0]["value"], "x");
        assert_eq!(parts[2]["value"], ")");
    }
}
