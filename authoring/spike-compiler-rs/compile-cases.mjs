// SPDX-License-Identifier: Apache-2.0
// Compile and render every case in cases.mjs with atomic's renderer and print
// what atomic reports: the rendered prompt, or the error and its line. Run it
// plainly and under rs-hook.mjs (parity-errors.mjs does both).
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { compile, render } from '../src/renderer.mjs';
import { cases } from './cases.mjs';

const dir = mkdtempSync(path.join(tmpdir(), 'astro-cases-'));
writeFileSync(path.join(dir, 'Card.astro'), '---\nconst { title, strict, count, value, input } = Astro.props;\n---\n<slot />\n');
const out = {};
for (const c of cases) {
  const file = path.join(dir, `${c.name}.atom.astro`);
  writeFileSync(file, c.source);
  try {
    const unit = await compile(file, { root: dir });
    const r = render(unit, { a: 'A', b: 'B', text: 'T', n: 1, ok: true, why: 'W', items: ['x', 'y'] }, { runId: 'parity' });
    out[c.name] = { accepted: true, developer: r.developer, user: r.user };
  } catch (error) {
    out[c.name] = { accepted: false, message: error.message, line: error.line ?? null };
  }
}
rmSync(dir, { recursive: true, force: true });
console.log(JSON.stringify(out));
