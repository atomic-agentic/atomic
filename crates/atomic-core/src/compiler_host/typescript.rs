// SPDX-License-Identifier: Apache-2.0
//! Native TypeScript services for frozen compiler requests.
use super::{string, text};
use boa_engine::{Context, JsNativeError, JsResult, JsValue};
use oxc_allocator::Allocator;
use oxc_ast::ast::{JSXText, Statement, StringLiteral};
use oxc_ast_visit::{Visit, utf8_to_utf16::Utf8ToUtf16};
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{SourceType, Span};
use oxc_transformer::{TransformOptions, Transformer};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc};

pub(super) fn op(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let action = string(args, 0, ctx)?;
    let file = string(args, 1, ctx)?;
    let source = string(args, 2, ctx)?;
    let extra = string(args, 3, ctx)?;
    let out = match action.as_str() {
        "parse" => parse(&source),
        "transpile" => transpile(&source, &file, &extra),
        "prepare" => prepare(&source, &file),
        "check" => check(&source),
        _ => Err("unknown TypeScript operation".into()),
    }
    .map_err(|e| JsNativeError::error().with_message(e))?;
    Ok(text(&out))
}

fn parse(source: &str) -> Result<String, String> {
    let arena = Allocator::default();
    let mut parsed = Parser::new(&arena, source, SourceType::ts()).parse();
    if let Some(e) = parsed.errors.first() {
        let pos = e.labels.iter().flatten().next().map_or(0, |l| l.offset());
        return Ok(json!({"diagnostics":[{"messageText":e.message,"start":source[..pos].encode_utf16().count()}]}).to_string());
    }
    Utf8ToUtf16::new(source).convert_program(&mut parsed.program);
    Ok(format!(
        "{{\"ast\":{},\"diagnostics\":[]}}",
        parsed.program.to_estree_ts_json(false)
    ))
}

fn rewrite<'a>(s: &mut StringLiteral<'a>, arena: &'a Allocator, edges: &BTreeMap<String, String>) {
    if let Some(url) = edges.get(s.value.as_str()) {
        s.value = arena.alloc_str(url).into();
        s.raw = None;
    }
}

fn transpile(source: &str, file: &str, extra: &str) -> Result<String, String> {
    let edges: BTreeMap<String, String> = serde_json::from_str(extra).map_err(|e| e.to_string())?;
    let arena = Allocator::default();
    let mut parsed = Parser::new(&arena, source, SourceType::ts()).parse();
    if let Some(e) = parsed.errors.first() {
        return Err(e.to_string());
    }
    for stmt in &mut parsed.program.body {
        match stmt {
            Statement::ImportDeclaration(d) => rewrite(&mut d.source, &arena, &edges),
            Statement::ExportNamedDeclaration(d) => {
                if let Some(s) = &mut d.source {
                    rewrite(s, &arena, &edges);
                }
            }
            Statement::ExportAllDeclaration(d) => rewrite(&mut d.source, &arena, &edges),
            _ => {}
        }
    }
    let semantics = SemanticBuilder::new().build(&parsed.program);
    if let Some(e) = semantics.errors.first() {
        return Err(e.to_string());
    }
    let options = TransformOptions::default();
    let transformed = Transformer::new(&arena, Path::new(file), &options)
        .build_with_scoping(semantics.semantic.into_scoping(), &mut parsed.program);
    if let Some(e) = transformed.errors.first() {
        return Err(e.to_string());
    }
    let output = Codegen::new()
        .with_options(CodegenOptions {
            source_map_path: Some(file.into()),
            ..Default::default()
        })
        .build(&parsed.program);
    Ok(json!({"outputText":output.code, "sourceMapText":output.map.ok_or("missing helper source map")?.to_json_string()}).to_string())
}

#[derive(Default)]
struct LiteralText {
    spans: Vec<Span>,
    type_imports: Vec<(String, Span)>,
}
impl<'a> Visit<'a> for LiteralText {
    fn visit_ts_import_type(&mut self, node: &oxc_ast::ast::TSImportType<'a>) {
        self.type_imports
            .push((node.source.value.to_string(), node.source.span));
        oxc_ast_visit::walk::walk_ts_import_type(self, node);
    }
    fn visit_jsx_text(&mut self, node: &JSXText<'a>) {
        self.spans.push(node.span);
    }
}

// tsz parses backticks in JSX text as TypeScript. Mask only literal text,
// retaining line breaks and UTF-16 widths for Astro's source map.
fn prepare(source: &str, file: &str) -> Result<String, String> {
    let arena = Allocator::default();
    let is_tsx = file.ends_with(".tsx");
    let parsed = Parser::new(
        &arena,
        source,
        if is_tsx {
            SourceType::tsx()
        } else {
            SourceType::ts()
        },
    )
    .parse();
    let mut spans = LiteralText::default();
    spans.visit_program(&parsed.program);
    let mut code = String::with_capacity(source.len());
    let mut end = 0;
    for span in spans.spans {
        code.push_str(&source[end..span.start as usize]);
        for c in span.source_text(source).chars() {
            if c == '\n' || c == '\r' {
                code.push(c);
            } else {
                for _ in 0..c.len_utf16() {
                    code.push(' ');
                }
            }
        }
        end = span.end as usize;
    }
    code.push_str(&source[end..]);
    let mut requests = spans.type_imports;
    for (name, items) in &parsed.module_record.requested_modules {
        requests.extend(items.iter().map(|r| (name.to_string(), r.span)));
    }
    requests.sort_by_key(|(_, span)| span.start);
    let imports = requests
        .into_iter()
        .map(|(name, span)| {
            json!({
                "name":name,
                "offset":source[..span.start as usize].encode_utf16().count(),
                "end":source[..span.end as usize].encode_utf16().count()
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({"code":code,"imports":imports}).to_string())
}

// tsz 0.1.75 does not report unresolved imports from an in-memory program.
// Its input is already the frozen, alias-rewritten source graph, so resolve
// every request against that graph before accepting its type diagnostics.
fn frozen_import_diagnostics(files: &BTreeMap<String, String>) -> Vec<Value> {
    let mut errors = Vec::new();
    for (file, source) in files {
        if file.starts_with("/__lib/") || !(file.ends_with(".ts") || file.ends_with(".tsx")) {
            continue;
        }
        let arena = Allocator::default();
        let parsed = Parser::new(
            &arena,
            source,
            if file.ends_with(".tsx") {
                SourceType::tsx()
            } else {
                SourceType::ts()
            },
        )
        .parse();
        let mut visitor = LiteralText::default();
        visitor.visit_program(&parsed.program);
        let mut requests = visitor.type_imports;
        for (name, items) in &parsed.module_record.requested_modules {
            requests.extend(items.iter().map(|request| (name.to_string(), request.span)));
        }
        requests.sort_by_key(|(_, span)| span.start);
        requests.dedup_by(|(left_name, left_span), (right_name, right_span)| {
            left_name == right_name && left_span == right_span
        });
        for (name, span) in requests {
            let resolved = if name.starts_with('.') {
                let mut parts = file.rsplit_once('/').map_or(Vec::new(), |(dir, _)| {
                    dir.split('/')
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                });
                for part in name.split('/') {
                    match part {
                        "" | "." => {}
                        ".." => {
                            parts.pop();
                        }
                        _ => parts.push(part),
                    }
                }
                let base = format!("/{}", parts.join("/"));
                [".tsx", ".d.ts", ".ts", ""]
                    .iter()
                    .any(|extension| files.contains_key(&format!("{base}{extension}")))
            } else {
                false
            };
            if !resolved {
                let byte = source.floor_char_boundary((span.start as usize).min(source.len()));
                errors.push(json!({
                    "file":file,
                    "offset":source[..byte].encode_utf16().count(),
                    "message":format!("TS2307: Cannot find module '{name}' or its corresponding type declarations.")
                }));
            }
        }
    }
    errors
}

fn check(source: &str) -> Result<String, String> {
    use tsz_common::{CheckerOptions, options::checker::JsxMode};
    use tsz_core::{
        ModuleKind, ScriptTarget,
        lib_loader::LibFile,
        parallel::{
            check_files_parallel, merge_bind_results, parse_and_bind_parallel_with_libs_and_target,
        },
    };
    let files: BTreeMap<String, String> =
        serde_json::from_str(source).map_err(|e| e.to_string())?;
    let unresolved = frozen_import_diagnostics(&files);
    if !unresolved.is_empty() {
        return Ok(json!(unresolved).to_string());
    }
    let bundled: BTreeMap<String, String> =
        serde_json::from_str(include_str!("../../assets/typescript-es2022.json"))
            .map_err(|e| e.to_string())?;
    let libs: Vec<_> = bundled
        .into_iter()
        .map(|(name, source)| Arc::new(LibFile::from_source(name, source)))
        .collect();
    let target = ScriptTarget::ES2022;
    let options = CheckerOptions {
        strict: true,
        module: ModuleKind::ESNext,
        target,
        jsx_mode: JsxMode::Preserve,
        allow_importing_ts_extensions: true,
        ..Default::default()
    }
    .apply_strict_defaults();
    // Each child checks one project. Bound stack reservations on high-core hosts.
    rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .stack_size(8 << 20)
        .build_global()
        .map_err(|e| e.to_string())?;
    let bound = parse_and_bind_parallel_with_libs_and_target(
        files.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        &libs,
        target,
    );
    let mut errors = Vec::<Value>::new();
    let mut diagnostic = |file: &str, pos: u32, code: u32, message: &str| {
        let Some(text) = files.get(file) else {
            return;
        };
        let byte = text.floor_char_boundary((pos as usize).min(text.len()));
        errors.push(json!({"file":file,"offset":text[..byte].encode_utf16().count(),"message":format!("TS{code}: {message}")}));
    };
    for f in &bound {
        for d in &f.parse_diagnostics {
            diagnostic(&f.file_name, d.start, d.code, &d.message);
        }
    }
    let program = merge_bind_results(bound);
    let checked = check_files_parallel(&program, &options, &libs);
    for f in checked.file_results {
        if f.file_name.ends_with(".d.ts") {
            continue;
        }
        for d in f
            .diagnostics
            .into_iter()
            .chain(f.function_results.into_iter().flat_map(|r| r.diagnostics))
        {
            diagnostic(&f.file_name, d.start, d.code, &d.message_text);
        }
    }
    Ok(json!(errors).to_string())
}

#[cfg(test)]
mod tests {
    use super::{check, frozen_import_diagnostics};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    #[test]
    fn missing_frozen_imports_are_located_before_tsz_checks_types() {
        let files = json!({
            "/p/atom.ts":"// é\nimport type { A } from './absent';\ntype B = import('uncaptured').B;",
            "/p/other.ts":"export type { C } from './missing';"
        });
        let errors: Vec<Value> =
            serde_json::from_str(&check(&files.to_string()).expect("checker response"))
                .expect("diagnostics JSON");
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert_eq!(errors[0]["file"], "/p/atom.ts");
        assert_eq!(errors[0]["offset"], 28); // UTF-16, at the first import literal.
        assert_eq!(
            errors[0]["message"],
            "TS2307: Cannot find module './absent' or its corresponding type declarations."
        );
        assert_eq!(
            errors[1]["message"],
            "TS2307: Cannot find module 'uncaptured' or its corresponding type declarations."
        );
        assert_eq!(errors[2]["file"], "/p/other.ts");
    }

    #[test]
    fn captured_type_markdown_and_generated_tsx_imports_resolve() {
        let files: BTreeMap<String, String> = serde_json::from_value(json!({
            "/p/atom.astro.tsx":"import type { T } from './pkg/target.ts'; import text from './note.md?raw'; export type { T as U } from './pkg/target';",
            "/p/pkg/target.ts":"export type { T } from './nested';",
            "/p/pkg/nested.ts":"export type T = number;",
            "/p/note.md?raw.d.ts":"declare const literal: string; export default literal;"
        })).expect("frozen files");
        assert!(frozen_import_diagnostics(&files).is_empty());
    }
}
