// SPDX-License-Identifier: Apache-2.0
// What atomic reports for each syntax case under each parser. Writes results/errors.json.
import { spawnSync } from 'node:child_process';
import { writeFileSync, mkdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { cases } from './cases.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const run = (hook) => {
  const r = spawnSync(process.execPath, [...(hook ? ['--import', path.join(here, 'rs-hook.mjs')] : []), path.join(here, 'compile-cases.mjs')], { cwd: path.dirname(here), encoding: 'utf8', maxBuffer: 64 << 20 });
  if (r.status !== 0) throw new Error(r.stderr);
  return JSON.parse(r.stdout);
};
const go = run(false), rs = run(true);
const show = (v) => v.accepted ? `accepted: ${JSON.stringify(v.developer.slice(0, 70))}` : `line ${v.line}: ${v.message.split('\n')[0].slice(0, 110)}`;
const rows = cases.map((c) => ({ name: c.name, invalid: !!c.invalid, go: go[c.name], rs: rs[c.name], same: JSON.stringify(go[c.name]) === JSON.stringify(rs[c.name]) }));
mkdirSync(path.join(here, 'results'), { recursive: true });
writeFileSync(path.join(here, 'results/errors.json'), JSON.stringify(rows, null, 2) + '\n');
const count = (f) => rows.filter(f).length;
console.log(`${rows.length} cases; atomic's result identical under both parsers: ${count((r) => r.same)}`);
console.log(`invalid cases accepted by atomic: go ${count((r) => r.invalid && r.go.accepted)}, rs ${count((r) => r.invalid && r.rs.accepted)} (of ${count((r) => r.invalid)})`);
console.log(`Astro-valid cases atomic refuses as unsupported syntax: go ${count((r) => !r.invalid && !r.go.accepted)}, rs ${count((r) => !r.invalid && !r.rs.accepted)} (of ${count((r) => !r.invalid)})`);
for (const r of rows.filter((r) => !r.same)) console.log(`${r.invalid ? 'invalid' : 'valid  '} ${r.name}\n    go: ${show(r.go)}\n    rs: ${show(r.rs)}`);
