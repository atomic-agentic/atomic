// SPDX-License-Identifier: Apache-2.0
// Captures a local TypeScript helper module graph for the embedded helper
// runtime: the runtime imports, transpiled to JavaScript, keyed by
// `atomic-helper:///` URLs. No author module is evaluated here. Type-only
// imports belong to the type graph (generated declarations, schema types):
// they are checked with the atom and never captured.
import ts from 'typescript';
import sourceMap from 'source-map-js';
import { readFile, realpath, path, createHash } from './platform.mjs';

export const SCHEME = 'atomic-helper:///';
const MODULE_LIMIT = 64;
const MODULE_BYTES = 262144;
const CAPTURE_BYTES = 1048576;

class CaptureError extends Error {
  constructor(message, file, line) { super(message); this.file = file; this.line = line; }
}

// A statement that imports or re-exports types only.
function typeOnly(statement) {
  if (ts.isImportDeclaration(statement)) {
    const clause = statement.importClause;
    if (!clause) return false;
    if (clause.isTypeOnly) return true;
    const bindings = clause.namedBindings;
    return !clause.name && !!bindings && ts.isNamedImports(bindings) && bindings.elements.length > 0 && bindings.elements.every((item) => item.isTypeOnly);
  }
  if (statement.isTypeOnly) return true;
  const clause = statement.exportClause;
  return !!clause && ts.isNamedExports(clause) && clause.elements.length > 0 && clause.elements.every((item) => item.isTypeOnly);
}

const lineAt = (source, node) => source.getLineAndCharacterOfPosition(node.getStart(source)).line + 1;
const urlOf = (root, file) => SCHEME + path.relative(root, file).split(path.sep).map(encodeURIComponent).join('/');

// Captures the graph rooted at `entry` (an absolute path), imported from
// `from` ({file, line}). `state` is shared across one compilation so the
// size limits and the cache cover every helper in the atom. Resolves to
// { digest, artifact: {entry, modules}, maps, typeImports }.
export async function captureHelper(root, entry, from, state) {
  const first = await resolveRuntime(root, entry, from);
  if (state.cache.has(first)) return state.cache.get(first);
  const files = new Map(), edges = new Map(), typeImports = new Set();
  const missing = new Set();
  async function load(file) {
    if (files.has(file)) return;
    let text;
    try { text = await readFile(file, 'utf8'); } catch (error) {
      if (error.sources) { error.sources.forEach((source) => missing.add(source)); return; }
      throw error;
    }
    state.bytes += text.length;
    if (++state.modules > MODULE_LIMIT || text.length > MODULE_BYTES || state.bytes > CAPTURE_BYTES) {
      throw new CaptureError('helper graph limit exceeded', file, 1);
    }
    const source = ts.createSourceFile(file, text, ts.ScriptTarget.ES2022, true, ts.ScriptKind.TS);
    if (source.parseDiagnostics.length) {
      const first = source.parseDiagnostics[0];
      throw new CaptureError(ts.flattenDiagnosticMessageText(first.messageText, '\n'), file, source.getLineAndCharacterOfPosition(first.start ?? 0).line + 1);
    }
    files.set(file, text);
    const imports = new Map(); edges.set(file, imports);
    const children = [];
    for (const statement of source.statements) {
      if (!(ts.isImportDeclaration(statement) || ts.isExportDeclaration(statement)) || !statement.moduleSpecifier) continue;
      const specifier = statement.moduleSpecifier.text;
      if (typeOnly(statement)) {
        if (specifier.startsWith('.')) typeImports.add(path.resolve(path.dirname(file), specifier));
        continue;
      }
      if (!specifier.startsWith('.')) {
        throw new CaptureError(`runtime import ${specifier} is not a local .ts helper module; helpers import local modules only, and types with \`import type\``, file, lineAt(source, statement));
      }
      const target = await resolveRuntime(root, path.resolve(path.dirname(file), specifier), { file, line: lineAt(source, statement), specifier });
      imports.set(specifier, target);
      children.push(target);
    }
    // Dynamic imports are refused by the runtime; refuse them here too, with a location.
    const dynamic = (node) => {
      if (ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword) throw new CaptureError('helpers cannot import dynamically', file, lineAt(source, node));
      ts.forEachChild(node, dynamic);
    };
    dynamic(source);
    for (const child of children) await load(child);
  }
  await load(first);
  if (missing.size) { const error = new Error('missing sources'); error.sources = [...missing]; throw error; }
  const modules = {}, maps = {};
  for (const [file, text] of [...files].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))) {
    const url = urlOf(root, file);
    const rewrite = (context) => (source) => ts.visitNode(source, function visit(node) {
      if (ts.isStringLiteral(node) && edges.get(file)?.has(node.text)
        && (ts.isImportDeclaration(node.parent) || ts.isExportDeclaration(node.parent))) {
        return context.factory.createStringLiteral(urlOf(root, edges.get(file).get(node.text)));
      }
      return ts.visitEachChild(node, visit, context);
    });
    const output = ts.transpileModule(text, {
      fileName: file,
      compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext, sourceMap: true, inlineSources: false },
      transformers: { before: [rewrite] },
    });
    modules[url] = output.outputText.replace(/\n\/\/# sourceMappingURL=.*$/, '\n');
    maps[url] = { file, map: output.sourceMapText };
  }
  const artifact = { entry: urlOf(root, first), modules };
  const digest = createHash('sha256').update(JSON.stringify(artifact)).digest('hex');
  const captured = { digest, artifact, maps, typeImports: [...typeImports] };
  state.cache.set(first, captured);
  return captured;
}

async function resolveRuntime(root, target, from) {
  const specifier = from.specifier ?? path.relative(path.dirname(from.file), target);
  // An extensionless specifier names a .ts module, as editors resolve it.
  if (path.extname(target) === '') target += '.ts';
  if (!target.endsWith('.ts') || target.endsWith('.d.ts')) {
    throw new CaptureError(`runtime import ${specifier} is not a local .ts helper module; import types with \`import type\``, from.file, from.line);
  }
  const lexical = path.resolve(target);
  if (lexical !== root && !lexical.startsWith(root + path.sep)) throw new CaptureError(`helper module ${specifier} is outside the project`, from.file, from.line);
  let file;
  try { file = await realpath(target); } catch (error) {
    if (error.sources) throw error;
    throw new CaptureError(`helper module ${specifier} does not exist; import declarations and schema types with \`import type\``, from.file, from.line);
  }
  if (file !== root && !file.startsWith(root + path.sep)) throw new CaptureError(`helper module ${specifier} is outside the project`, from.file, from.line);
  return file;
}

// The authored file and line for the first captured frame in a helper's
// stack trace, mapped back through the transpiler's source map.
export function locateThrow(text, captures) {
  const frame = /atomic-helper:\/\/\/([^\s):]+):(\d+):(\d+)/.exec(text);
  if (!frame) return undefined;
  const url = SCHEME + frame[1];
  for (const captured of captures) {
    const entry = captured.maps[url];
    if (!entry) continue;
    const consumer = new sourceMap.SourceMapConsumer(JSON.parse(entry.map));
    const original = consumer.originalPositionFor({ line: Number(frame[2]), column: Number(frame[3]) - 1 });
    return { file: entry.file, line: original.line ?? Number(frame[2]) };
  }
  return undefined;
}
