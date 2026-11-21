// SPDX-License-Identifier: Apache-2.0
// Parse every .astro source atomic ships or tests with both Astro compilers and
// report where their trees, positions and diagnostics differ.
//
//   node spike-compiler-rs/parity.mjs            # catalog + fixtures + cases + test inputs
//   node spike-compiler-rs/parity.mjs --no-tests # skip replaying the authoring tests
//   node spike-compiler-rs/parity.mjs --strict   # exit 1 when any valid source differs
//
// The report is written to spike-compiler-rs/results/parity.json.
import { parse as goParse } from '@astrojs/compiler';
import { parse as rsParse } from '@astrojs/compiler-rs';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, readdirSync, statSync, writeFileSync, mkdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { literalFences } from '../src/fences.mjs';
import { cases } from './cases.mjs';
import { normalizeGo, normalizeRs, structuralDiff, pairs, dropFenceNewline } from './normalize.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const authoring = path.dirname(here);
const repo = path.dirname(authoring);
const args = new Set(process.argv.slice(2));

function walk(dir, out = []) {
  for (const name of readdirSync(dir)) {
    if (name === 'node_modules' || name === 'target' || name.startsWith('target-')) continue;
    const file = path.join(dir, name);
    const stat = statSync(file);
    if (stat.isDirectory()) walk(file, out);
    else if (name.endsWith('.astro')) out.push(file);
  }
  return out;
}

const corpus = [];
for (const root of ['examples', 'authoring/fixtures', 'crates']) {
  for (const file of walk(path.join(repo, root)).sort()) {
    const text = readFileSync(file, 'utf8');
    let source = text;
    try { source = literalFences(text); } catch { /* the renderer reports an unclosed fence; parse as authored */ }
    corpus.push({ origin: 'file', name: path.relative(repo, file), source });
  }
}
for (const c of cases) corpus.push({ origin: 'case', name: `case:${c.name}`, source: c.source, invalid: !!c.invalid });

if (!args.has('--no-tests')) {
  // Replay the authoring test suite with parse() recorded (record-hook.mjs).
  const dir = mkdtempSync(path.join(tmpdir(), 'astro-parity-'));
  const record = path.join(dir, 'recorded.jsonl');
  const hook = path.join(here, 'record-hook.mjs');
  const files = readdirSync(path.join(authoring, 'test')).filter((n) => n.endsWith('.test.mjs')).map((n) => `test/${n}`);
  const run = spawnSync(process.execPath, ['--test', ...files], {
    cwd: authoring, encoding: 'utf8', timeout: 600_000,
    env: { ...process.env, SPIKE_RECORD: record, NODE_OPTIONS: `--import=${hook}` },
  });
  if (run.status !== 0) { console.error(run.stdout.slice(-4000), run.stderr.slice(-4000)); throw new Error('the authoring tests failed while recording'); }
  const seen = new Set(corpus.map((c) => c.source));
  let n = 0;
  for (const line of readFileSync(record, 'utf8').split('\n').filter(Boolean)) {
    const { source } = JSON.parse(line);
    if (seen.has(source)) continue;
    seen.add(source);
    corpus.push({ origin: 'test', name: `test:${++n}`, source });
  }
  rmSync(dir, { recursive: true, force: true });
}

const goErrors = (r) => (r.diagnostics ?? []).filter((d) => d.severity === 1).map((d) => ({ text: d.text, line: d.location?.line, column: d.location?.column }));
const goWarnings = (r) => (r.diagnostics ?? []).filter((d) => d.severity !== 1).map((d) => ({ severity: d.severity, text: d.text, line: d.location?.line }));
const rsErrors = (r) => (r.diagnostics ?? []).filter((d) => d.severity === 'error').map((d) => ({ text: d.text, line: d.labels?.[0]?.line, column: (d.labels?.[0]?.column ?? -1) + 1 }));
const rsWarnings = (r) => (r.diagnostics ?? []).filter((d) => d.severity !== 'error').map((d) => ({ severity: d.severity, text: d.text, line: d.labels?.[0]?.line }));

const category = (d) => {
  const key = d.path.replace(/\[\d+\]/g, '[]');
  const tail = key.split('.').slice(-1)[0];
  if (tail === 'k') return `node kind ${d.go} -> ${d.rs}`;
  if (tail === 'length') return `${key.split('.').slice(-2, -1)[0] || 'root'} count`;
  return `${tail} value`;
};

const results = [];
const positions = {};
const tally = (kind, field, equal) => {
  positions[kind] ??= {};
  positions[kind][field] ??= { equal: 0, differ: 0, examples: [] };
  positions[kind][field][equal ? 'equal' : 'differ']++;
};

for (const entry of corpus) {
  const go = await goParse(entry.source, { position: true });
  const rs = rsParse(entry.source);
  const goTree = normalizeGo(go.ast);
  const rsTree = normalizeRs(rs.ast, entry.source);
  const diffs = structuralDiff(goTree, rsTree);
  const result = {
    name: entry.name, origin: entry.origin, invalid: !!entry.invalid,
    identical: diffs.length === 0,
    identicalWithoutFenceNewline: structuralDiff(goTree, dropFenceNewline(rsTree)).length === 0,
    differences: diffs.slice(0, 12).map((d) => ({ ...d, category: category(d) })),
    differenceCount: diffs.length,
    categories: [...new Set(diffs.map(category))],
    // compiler-rs can stop early without a diagnostic; compare how much of
    // the source its tree covers (the rest may only be whitespace).
    rsTruncated: !rsErrors(rs).length && entry.source.slice(Math.max(rs.ast.frontmatter?.end ?? 0, ...(rs.ast.body ?? []).map((n) => n.end))).trim() !== '',
    go: { errors: goErrors(go), warnings: goWarnings(go) },
    rs: { errors: rsErrors(rs), warnings: rsWarnings(rs) },
  };
  // Positions, where the two trees agree on a node.
  const nonAscii = /[^\x00-\x7f]/.test(entry.source);
  for (const p of pairs(goTree, dropFenceNewline(rsTree))) {
    if (!p.go.pos || !p.rs.pos) { tally(p.kind, 'missing', false); continue; }
    tally(p.kind, 'line', p.go.pos.line === p.rs.pos.line);
    tally(p.kind, 'column', p.go.pos.column === p.rs.pos.column);
    tally(p.kind, nonAscii ? 'offset (non-ASCII source)' : 'offset', p.go.pos.offset === p.rs.pos.offset);
    if (p.go.authoredLine !== undefined) tally(p.kind, 'atomic-reported line', p.go.authoredLine === p.rs.pos.line);
    const ex = positions[p.kind].column.examples;
    if (p.go.pos.column !== p.rs.pos.column && ex.length < 3) ex.push({ source: entry.name, go: p.go.pos, rs: p.rs.pos });
  }
  results.push(result);
}

const valid = results.filter((r) => !r.invalid);
const byCategory = {};
for (const r of valid) for (const c of r.categories) (byCategory[c] ??= []).push(r.name);
const summary = {
  compilers: {
    go: JSON.parse(readFileSync(path.join(authoring, 'node_modules/@astrojs/compiler/package.json'), 'utf8')).version,
    rs: JSON.parse(readFileSync(path.join(authoring, 'node_modules/@astrojs/compiler-rs/package.json'), 'utf8')).version,
  },
  sources: results.length,
  byOrigin: Object.fromEntries(['file', 'case', 'test'].map((o) => [o, results.filter((r) => r.origin === o).length])),
  validIdentical: valid.filter((r) => r.identical).length,
  validDiffering: valid.filter((r) => !r.identical).length,
  validIdenticalWithoutFenceNewline: valid.filter((r) => r.identicalWithoutFenceNewline).length,
  validDifferingBeyondFenceNewline: valid.filter((r) => !r.identicalWithoutFenceNewline).map((r) => r.name),
  validWithRustErrors: valid.filter((r) => r.rs.errors.length).map((r) => ({ name: r.name, errors: r.rs.errors })),
  validWithGoErrors: valid.filter((r) => r.go.errors.length).map((r) => ({ name: r.name, errors: r.go.errors })),
  differenceCategories: Object.fromEntries(Object.entries(byCategory).map(([c, names]) => [c, { sources: names.length, examples: names.slice(0, 4) }])),
  positions,
  rsSilentlyTruncated: results.filter((r) => r.rsTruncated).map((r) => r.name),
  invalid: results.filter((r) => r.invalid).map((r) => ({ name: r.name, go: r.go.errors, rs: r.rs.errors, goWarnings: r.go.warnings, rsWarnings: r.rs.warnings })),
};

mkdirSync(path.join(here, 'results'), { recursive: true });
writeFileSync(path.join(here, 'results/parity.json'), JSON.stringify({ summary, results }, null, 2) + '\n');

console.log(`compilers: @astrojs/compiler ${summary.compilers.go}, @astrojs/compiler-rs ${summary.compilers.rs}`);
console.log(`sources: ${summary.sources} (${Object.entries(summary.byOrigin).map(([k, v]) => `${v} ${k}`).join(', ')})`);
console.log(`valid sources with identical normalised trees: ${summary.validIdentical}/${valid.length}`);
console.log(`... identical once Rust's line break after the closing fence is dropped: ${summary.validIdenticalWithoutFenceNewline}/${valid.length}`);
if (summary.validDifferingBeyondFenceNewline.length) console.log(`  still differing: ${summary.validDifferingBeyondFenceNewline.join(', ')}`);
console.log('difference categories (valid sources):');
for (const [c, v] of Object.entries(summary.differenceCategories).sort((a, b) => b[1].sources - a[1].sources)) console.log(`  ${String(v.sources).padStart(4)}  ${c}  e.g. ${v.examples.slice(0, 2).join(', ')}`);
console.log(`valid sources Rust rejects: ${summary.validWithRustErrors.length}; Go rejects: ${summary.validWithGoErrors.length}`);
for (const r of summary.validWithRustErrors.slice(0, 8)) console.log(`  rs ${r.name}: ${r.errors.map((e) => `${e.line}:${e.column} ${e.text}`).join(' | ')}`);
console.log(`sources compiler-rs stops parsing early with no diagnostic: ${summary.rsSilentlyTruncated.length ? summary.rsSilentlyTruncated.join(', ') : 'none'}`);
console.log('positions where both trees have the node, after the fence-newline adjustment (equal/total):');
for (const [kind, fields] of Object.entries(positions)) console.log(`  ${kind.padEnd(22)} ${Object.entries(fields).map(([f, v]) => `${f} ${v.equal}/${v.equal + v.differ}`).join(', ')}`);
console.log('invalid sources (first error each):');
for (const r of summary.invalid) {
  const f = (e) => e.length ? `${e[0].line}:${e[0].column} ${e[0].text}` : '(no error)';
  console.log(`  ${r.name}\n    go: ${f(r.go)}\n    rs: ${f(r.rs)}`);
}
if (args.has('--strict') && summary.validDiffering) process.exitCode = 1;
