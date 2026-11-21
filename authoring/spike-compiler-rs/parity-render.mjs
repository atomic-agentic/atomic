// SPDX-License-Identifier: Apache-2.0
// Render the example fixtures with the Go parse and with compiler-rs's parse
// (through rs-as-go.mjs), with and without Go's dropped fence line break, and
// report which rendered prompts differ. Writes results/render.json.
import { spawnSync } from 'node:child_process';
import { writeFileSync, mkdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const run = (env, hook) => {
  const r = spawnSync(process.execPath, [...(hook ? ['--import', path.join(here, 'rs-hook.mjs')] : []), path.join(here, 'render-examples.mjs')], { cwd: path.dirname(here), encoding: 'utf8', env: { ...process.env, ...env }, maxBuffer: 64 << 20 });
  if (r.status !== 0) throw new Error(r.stderr);
  return JSON.parse(r.stdout);
};
const go = run({}, false);
const variants = { 'compiler-rs, fence line break dropped': run({}, true), 'compiler-rs, fence line break kept': run({ SPIKE_FENCE_NEWLINE: 'keep' }, true) };
const firstDifference = (a, b) => {
  a = JSON.stringify(a); b = JSON.stringify(b);
  let i = 0; while (i < a.length && a[i] === b[i]) i++;
  return { go: a.slice(Math.max(0, i - 50), i + 50), rs: b.slice(Math.max(0, i - 50), i + 50) };
};
const report = { renders: Object.keys(go).length, goErrors: Object.entries(go).filter(([, v]) => v.error).map(([k, v]) => `${k}: ${v.error}`), variants: {} };
for (const [name, rs] of Object.entries(variants)) {
  const differing = Object.keys(go).filter((k) => JSON.stringify(go[k]) !== JSON.stringify(rs[k]));
  report.variants[name] = { identical: report.renders - differing.length, differing: differing.map((k) => ({ render: k, ...firstDifference(go[k], rs[k]) })) };
}
mkdirSync(path.join(here, 'results'), { recursive: true });
writeFileSync(path.join(here, 'results/render.json'), JSON.stringify(report, null, 2) + '\n');
console.log(`${report.renders} example renders; ${report.goErrors.length} fail under the Go parse`);
for (const e of report.goErrors) console.log(`  ${e}`);
for (const [name, v] of Object.entries(report.variants)) {
  console.log(`${name}: ${v.identical}/${report.renders} identical`);
  for (const d of v.differing.slice(0, 6)) console.log(`  ${d.render}\n    go: ${JSON.stringify(d.go)}\n    rs: ${JSON.stringify(d.rs)}`);
}
