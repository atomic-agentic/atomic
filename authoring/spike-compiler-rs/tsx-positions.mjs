// SPDX-License-Identifier: Apache-2.0
// Where each type error is reported, three ways, for the language-server
// probe's own broken variants of the fixtures:
//
//   atomic   atomic's checker today: Go convertToTSX + its source map (src/typecheck.mjs)
//   astro2tsx the same TypeScript host, with the TSX from @astrojs/astro2tsx
//            (compiler-rs's TSX crate, the one the language server uses), mapped back
//   editor   the stock @astrojs/language-server, over LSP
//
// Writes results/tsx-positions.json. Lines and columns are 1-based.
import ts from 'typescript';
import { convertToTsx } from '@astrojs/astro2tsx';
import { mkdtemp, cp, readFile, writeFile, rm, readdir, mkdir, stat } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const authoring = path.dirname(here);
const libsFile = path.join(authoring, 'src/typescript-libs.json');
if (!existsSync(libsFile)) {
  // The same generated (ignored) file `npm run build` writes.
  const libs = {};
  const dir = path.join(authoring, 'node_modules/typescript/lib');
  for (const name of await readdir(dir)) if (/^lib\..*\.d\.ts$/.test(name)) libs[name] = await readFile(path.join(dir, name), 'utf8');
  await writeFile(libsFile, JSON.stringify(libs));
}
const { typecheck } = await import('../src/typecheck.mjs');
const { generate } = await import('../src/generate.mjs');
const { probe } = await import('../src/lsp-probe.mjs');
const libs = JSON.parse(await readFile(libsFile, 'utf8'));

async function walk(dir, out = []) {
  for (const name of await readdir(dir)) {
    const file = path.join(dir, name);
    if ((await stat(file)).isDirectory()) await walk(file, out); else out.push(file);
  }
  return out;
}
const lineCol = (text, offset) => { const before = text.slice(0, offset).split('\n'); return { line: before.length, column: before.at(-1).length + 1 }; };

// atomic's TypeScript host (typecheck.mjs), with astro2tsx producing the TSX.
function typecheckAstro2tsx(sources, declarations) {
  const files = new Map(Object.entries(sources)), maps = new Map();
  for (const [name, text] of [...files]) if (name.endsWith('.astro')) {
    const result = convertToTsx(text, { filename: path.basename(name) });
    maps.set(`${name}.tsx`, { mappings: result.mappings, text });
    // Blank the pragma rather than delete it, so the mappings' offsets hold.
    files.set(`${name}.tsx`, result.code.replace('/* @jsxImportSource astro */', (m) => ' '.repeat(m.length)));
  }
  for (const [name, text] of Object.entries(libs)) files.set(`/__lib/${name}`, text);
  files.set('/__lib/atomic.d.ts', `
declare module 'astro' { export type AstroGlobal<P, C = unknown, R = unknown> = { readonly props: P; readonly slots: { has(name: string): boolean } }; }
declare namespace JSX { interface Element {} interface IntrinsicElements { [name: string]: any } interface IntrinsicAttributes { slot?: string } }
declare function Fragment(props: {children?: unknown; slot?: string}): JSX.Element;
declare const Astro: Readonly<import('astro').AstroGlobal<Record<string, unknown>>>;
`);
  for (const [name, text] of Object.entries(declarations)) files.set(name, text);
  const options = { strict: true, noEmit: true, target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext, moduleResolution: ts.ModuleResolutionKind.Bundler, jsx: ts.JsxEmit.Preserve, skipLibCheck: true, allowImportingTsExtensions: true };
  const host = {
    getSourceFile: (name, language) => files.has(name) ? ts.createSourceFile(name, files.get(name), language, true, name.endsWith('.tsx') ? ts.ScriptKind.TSX : ts.ScriptKind.TS) : undefined,
    getDefaultLibFileName: () => '/__lib/lib.es2022.d.ts', writeFile() {}, getCurrentDirectory: () => '/', getDirectories: () => [],
    fileExists: (name) => files.has(name), readFile: (name) => files.get(name),
    getCanonicalFileName: (name) => name, useCaseSensitiveFileNames: () => true, getNewLine: () => '\n',
    resolveModuleNames: (names, containing) => names.map((specifier) => {
      if (!specifier.startsWith('.')) return undefined;
      const parts = `${containing.slice(0, containing.lastIndexOf('/'))}/${specifier}`.split('/'), out = [];
      for (const part of parts) { if (part === '..') out.pop(); else if (part && part !== '.') out.push(part); }
      const base = '/' + out.join('/');
      const resolvedFileName = [base + '.tsx', base + '.d.ts', base + '.ts', base].find((n) => files.has(n));
      if (!resolvedFileName) return undefined;
      const extension = resolvedFileName.endsWith('.tsx') ? ts.Extension.Tsx : resolvedFileName.endsWith('.d.ts') ? ts.Extension.Dts : resolvedFileName.endsWith('.ts') ? ts.Extension.Ts : undefined;
      return extension ? { resolvedFileName, extension } : undefined;
    }),
  };
  const program = ts.createProgram([...files.keys()].filter((n) => n.endsWith('.tsx') || n.endsWith('.ts')), options, host);
  return ts.getPreEmitDiagnostics(program).filter((d) => d.category === ts.DiagnosticCategory.Error).map((d) => {
    const map = maps.get(d.file?.fileName);
    let where = { file: d.file?.fileName, ...lineCol(d.file?.text ?? '', d.start ?? 0) };
    if (map) {
      const m = map.mappings.find(([vs, vl]) => d.start >= vs && d.start <= vs + vl);
      if (m) where = { file: d.file.fileName.replace(/\.tsx$/, ''), ...lineCol(map.text, m[2] + Math.min(d.start - m[0], m[3])) };
      else where = { file: d.file.fileName.replace(/\.tsx$/, ''), unmapped: true };
    }
    return { ...where, code: d.code, message: ts.flattenDiagnosticMessageText(d.messageText, ' ') };
  });
}

const temp = await mkdtemp(path.join(os.tmpdir(), 'atomic-tsx-positions-'));
const report = { variants: [] };
try {
  for (const name of ['schemas', 'fixtures', 'authoring.d.ts']) await cp(path.join(authoring, name), path.join(temp, name), { recursive: true });
  await generate({ root: temp });
  const declarations = {};
  for (const file of await walk(path.join(temp, '.atomic/types'))) if (file.endsWith('.d.ts')) declarations[file] = await readFile(file, 'utf8');
  declarations[path.join(temp, 'authoring.d.ts')] = await readFile(path.join(temp, 'authoring.d.ts'), 'utf8');
  const variants = [
    { file: 'fixtures/draft-reply.atom.astro', edit: (t) => t.replace('input.email}', 'input.emali}').replace('tone="brief"', 'tone="casual"') },
    { file: 'fixtures/full/draft-reply.atom.astro', edit: (t) => t.replace('meeting.minutes > 0', 'meeting.minuttes > 0').replace('verdict => verdict.met', 'verdict => verdict.mett').replace('fitsWithin(meeting, slot)', 'fitsWithin(meeting, slot.start)').replace('criteria: evalCase.criteria', 'criteria: input.email') },
  ];
  for (const variant of variants) {
    const target = path.join(temp, variant.file);
    const original = await readFile(target, 'utf8');
    const broken = variant.edit(original);
    if (broken === original) throw new Error(`no edit applied to ${variant.file}`);
    await writeFile(target, broken);
    // The atom and the components it can import, as atomic sends them.
    const sources = { [target]: broken };
    for (const file of await walk(path.join(temp, 'fixtures/components'))) sources[file] = await readFile(file, 'utf8');
    const only = (list) => list.filter((d) => d.file === target).map((d) => ({ line: d.line, column: d.column, code: d.code ?? Number(/^TS(\d+)/.exec(d.message)?.[1]), message: d.message.replace(/^TS\d+: /, '').slice(0, 90) }));
    let atomic = [];
    try { await typecheck(sources, declarations); } catch (error) { if (process.env.SPIKE_DEBUG) console.error('atomic raw', error.message, JSON.stringify(error.diagnostics)); atomic = only(error.diagnostics ?? []); }
    const rawAstro2tsx = typecheckAstro2tsx(sources, declarations);
    if (process.env.SPIKE_DEBUG) console.error('astro2tsx raw', JSON.stringify(rawAstro2tsx).slice(0, 3000));
    const viaAstro2tsx = only(rawAstro2tsx);
    const lsp = await probe(variant.file, { root: temp, workspaceRoot: temp, diagnosticsOnly: true, serverPath: path.join(authoring, 'node_modules/@astrojs/language-server/bin/nodeServer.js'), tsdk: path.join(authoring, 'node_modules/typescript/lib') });
    const editor = lsp.diagnostics.filter((d) => d.severity === 1).map((d) => ({ line: d.range.start.line + 1, column: d.range.start.character + 1, code: d.code, message: d.message.slice(0, 90) }));
    // One row per editor diagnostic, matched to the others by code and message.
    const key = (d) => `${d.code}:${d.message.slice(0, 40)}`;
    const rows = [];
    for (const d of [...editor, ...atomic.filter((a) => !editor.some((e) => key(e) === key(a)))]) {
      const a = atomic.find((x) => key(x) === key(d)), r = viaAstro2tsx.find((x) => key(x) === key(d)), e = editor.find((x) => key(x) === key(d));
      const at = (x) => x ? `${x.line}:${x.column}` : '-';
      rows.push({ code: d.code, message: d.message, atomic: at(a), astro2tsx: at(r), editor: at(e) });
    }
    await writeFile(target, original);
    report.variants.push({ file: variant.file, rows, counts: { atomic: atomic.length, astro2tsx: viaAstro2tsx.length, editor: editor.length } });
  }
} finally { await rm(temp, { recursive: true, force: true }); }

await mkdir(path.join(here, 'results'), { recursive: true });
await writeFile(path.join(here, 'results/tsx-positions.json'), JSON.stringify(report, null, 2) + '\n');
for (const v of report.variants) {
  console.log(`${v.file}: errors atomic ${v.counts.atomic}, astro2tsx ${v.counts.astro2tsx}, editor ${v.counts.editor}`);
  for (const r of v.rows) console.log(`  TS${r.code} atomic ${r.atomic.padEnd(7)} astro2tsx ${r.astro2tsx.padEnd(7)} editor ${r.editor.padEnd(7)} ${r.message.slice(0, 60)}`);
}
