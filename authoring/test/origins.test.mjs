// SPDX-License-Identifier: Apache-2.0
// Per-field input origins, as a workflow's bindings assign them, and how they
// combine through expressions and helper calls.
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { compile, render, setHelperExecutor } from '../src/renderer.mjs';

async function atom(t, body) {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'atomic-origins-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  await writeFile(path.join(dir, 'join.ts'), 'export const join = (a: string, b: string): string => `${a} / ${b}`;\n');
  await writeFile(path.join(dir, 'test.atom.astro'), `---\nimport { join } from './join';\nconst { input } = Astro.props;\n---\n${body}`);
  return compile(path.join(dir, 'test.atom.astro'), { root: dir });
}
const origin = (out, at) => out.bindings.find((b) => b.path === at)?.origin;

test('each field keeps the origin its binding gave it', async (t) => {
  const unit = await atom(t, 'Policy {input.policy}\nVerdict {input.verdict}\nEvidence {input.evidence}');
  const out = render(unit, { policy: 'p', verdict: 'v', evidence: 'e' }, { origin: 'external', origins: { policy: 'team', verdict: 'agent', evidence: 'external' } });
  assert.equal(origin(out, 'input.policy'), 'team');
  assert.equal(origin(out, 'input.verdict'), 'agent');
  assert.equal(origin(out, 'input.evidence'), 'external');
  // Team data is still data, never instructions.
  assert(!out.developer.includes('Policy p'));
});

test('a helper over agent and external fields gives external data', async (t) => {
  setHelperExecutor((capture, name, args) => args.join(' / '));
  t.after(() => setHelperExecutor(null));
  const unit = await atom(t, '{join(input.verdict, input.evidence)}|{join(input.verdict, input.verdict)}|{join(input.policy, input.policy)}');
  const out = render(unit, { policy: 'p', verdict: 'v', evidence: 'e' }, { origins: { policy: 'team', verdict: 'agent', evidence: 'external' } });
  const helpers = out.bindings.filter((b) => b.path.startsWith('helpers.join_')).map((b) => b.origin);
  assert.deepEqual(helpers, ['external', 'agent', 'team']);
});

test('the whole input is as trusted as its least trusted field', async (t) => {
  const unit = await atom(t, '{input}');
  const out = render(unit, { policy: 'p', verdict: 'v' }, { origins: { policy: 'team', verdict: 'agent' } });
  assert.equal(origin(out, 'input'), 'agent');
});

test('unknown origins are refused', async (t) => {
  const unit = await atom(t, '{input.policy}');
  assert.throws(() => render(unit, { policy: 'p' }, { origins: { policy: 'trusted' } }), /invalid input origins/);
});
