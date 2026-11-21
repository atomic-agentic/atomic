// SPDX-License-Identifier: Apache-2.0
// Whether the trial crate (rust/, the oxc fork as a pinned git dependency)
// produces the same AST as the npm binding atomic's lockfile already holds.
//
//   node spike-compiler-rs/native-vs-napi.mjs [path/to/astro-rs-trial]
import { parse } from '@astrojs/compiler-rs';
import { spawnSync } from 'node:child_process';
import { readdirSync, readFileSync, statSync, writeFileSync, mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { cases } from './cases.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '../..');
const bin = process.argv[2] ?? path.join(repo, 'target-spike/release/astro-rs-trial');
const walk = (dir, out = []) => { for (const n of readdirSync(dir)) { if (n === 'node_modules' || n.startsWith('target')) continue; const f = path.join(dir, n); if (statSync(f).isDirectory()) walk(f, out); else if (n.endsWith('.astro')) out.push(f); } return out; };
const tmp = mkdtempSync(path.join(tmpdir(), 'astro-native-'));
const files = ['examples', 'authoring/fixtures', 'crates'].flatMap((d) => walk(path.join(repo, d)));
for (const c of cases) { const f = path.join(tmp, `${c.name}.astro`); writeFileSync(f, c.source); files.push(f); }
const run = spawnSync(bin, files, { encoding: 'utf8', env: { ...process.env, ITERATIONS: '1' }, maxBuffer: 256 << 20 });
if (run.status !== 0) throw new Error(run.stderr);
const native = run.stdout.trim().split('\n').map((l) => JSON.parse(l));
let same = 0; const differ = [];
for (const n of native) {
  const napi = parse(readFileSync(n.file, 'utf8'));
  const { comments, ...ast } = napi.ast;
  const equal = JSON.stringify(ast) === JSON.stringify(n.ast) && napi.diagnostics.map((d) => d.text).join('|') === n.diagnostics.map((d) => d.message).join('|');
  if (equal) same++; else differ.push(path.relative(repo, n.file));
}
rmSync(tmp, { recursive: true, force: true });
const report = { sources: native.length, identical: same, differing: differ };
mkdirSync(path.join(here, 'results'), { recursive: true });
writeFileSync(path.join(here, 'results/native-vs-napi.json'), JSON.stringify(report, null, 2) + '\n');
console.log(`native crate vs npm binding: ${same}/${native.length} identical ASTs and diagnostics${differ.length ? `; differing: ${differ.join(', ')}` : ''}`);
