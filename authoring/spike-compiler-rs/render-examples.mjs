// SPDX-License-Identifier: Apache-2.0
// Render every example atom against each of its fixture inputs and print the
// results as one JSON document. Run it once plainly and once under
// rs-hook.mjs, then compare the two outputs (parity-render.mjs does both).
import { readdirSync, readFileSync, existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { compile, render } from '../src/renderer.mjs';

const examples = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../examples');
const out = {};
for (const example of readdirSync(examples).sort()) {
  const fixtures = path.join(examples, example, 'fixtures');
  if (!existsSync(fixtures)) continue;
  for (const atom of readdirSync(fixtures).sort()) {
    const candidates = [path.join(examples, example, 'atoms', `${atom}.atom.astro`), path.join(examples, example, '.atomic/atoms', `${atom}.atom.astro`)];
    const file = candidates.find(existsSync);
    if (!file) continue;
    for (const name of readdirSync(path.join(fixtures, atom)).filter((n) => n.endsWith('.input.json')).sort()) {
      const key = `${example}/${atom}/${name}`;
      try {
        const root = path.join(examples, example);
        const unit = await compile(file, { root, production: true, authoringFile: path.join(root, '.atomic/types/authoring.d.ts') });
        const input = JSON.parse(readFileSync(path.join(fixtures, atom, name), 'utf8'));
        out[key] = { ok: render(unit, input, { runId: 'parity' }) };
      } catch (error) {
        out[key] = { error: `${error.message}${error.line ? ` (line ${error.line})` : ''}` };
      }
    }
  }
}
console.log(JSON.stringify(out));
