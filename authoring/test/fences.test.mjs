// SPDX-License-Identifier: Apache-2.0
// ```mermaid fences reach the model as text: tags and braces inside them are
// diagram syntax, never expanded.
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { compile, render } from '../src/renderer.mjs';
import { literalFences } from '../src/fences.mjs';

const DIAGRAM = 'flowchart TD\n  A[Ticket] --> B{Urgent?}\n  B -->|yes| C<page>\n  B -->|no| D[&amp; queue]';

async function atom(t, body) {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'atomic-fences-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  await writeFile(path.join(dir, 'test.atom.astro'), `---\nconst { input } = Astro.props;\n---\n${body}`);
  return compile(path.join(dir, 'test.atom.astro'), { root: dir });
}

test('a mermaid fence reaches the model as written', async (t) => {
  const unit = await atom(t, `Route the ticket as this diagram shows.\n\`\`\`mermaid\n${DIAGRAM}\n\`\`\`\nTicket: {input.text}`);
  const out = render(unit, { text: 'printer on fire' });
  assert(out.developer.includes(`\`\`\`mermaid\n${DIAGRAM}\n\`\`\``), out.developer);
  // Outside the fence, expressions still work.
  assert(out.user.includes('printer on fire'));
});

test('a tag or brace inside the fence is left as text', async (t) => {
  const unit = await atom(t, '```mermaid\nA --> B{input.text}\nC <Missing /> D\n```\n');
  const out = render(unit, { text: 'secret' });
  assert(out.developer.includes('B{input.text}') && out.developer.includes('<Missing />'), out.developer);
  assert(!out.user.includes('secret'));
});

test('only mermaid fences in the body are literal, and lines are kept', () => {
  const source = '---\nconst x = `{a}`;\n---\n```js\n{input.text}\n```\n```mermaid\nA{b}\n```\n';
  const out = literalFences(source);
  assert(out.includes('const x = `{a}`;'));
  assert(out.includes('```js\n{input.text}\n```'));
  assert(out.includes('A&#123;b&#125;'));
  assert.equal(out.split('\n').length, source.split('\n').length);
});
