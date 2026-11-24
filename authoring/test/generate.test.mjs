// SPDX-License-Identifier: Apache-2.0
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, cp, rm, readFile, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { generate, declarations } from '../src/generate.mjs';
const root = fileURLToPath(new URL('../', import.meta.url));
async function project(t) {
  const temp = await mkdtemp(path.join(os.tmpdir(), 'atomic-schema-types-'));
  t.after(() => rm(temp, { recursive: true, force: true }));
  await cp(path.join(root, 'schemas'), path.join(temp, 'schemas'), { recursive: true });
  return temp;
}
test('schema generation is deterministic and clean output passes the stale check', async (t) => {
  const root = await project(t), first = await generate({ root });
  assert.equal(first.changed.length, 5);
  assert.deepEqual((await generate({ root })).changed, []);
  assert.deepEqual((await generate({ root, check: true })).changed, []);
  const types = await readFile(path.join(root, '.atomic/types/schemas.d.ts'), 'utf8');
  assert.match(types, /"feedback"\?: Array<string>/);
  assert.match(types, /"meeting"\?: Meeting/);
  assert.match(types, /"action": "ignore" \| "notify" \| "respond"/);
});
// The per-atom `Config` alias is what the stock editor and `atomic check` read
// the contract from: the fifth argument, and so any read of `context`, is present
// only for an atom the registry declares a context for.
test('the per-atom Config carries a context only for an atom that declares one', async (t) => {
  const root = await project(t);
  await generate({ root });
  const contextual = await readFile(path.join(root, '.atomic/types/callback-context.d.ts'), 'utf8');
  assert.match(contextual, /export type Config = AtomConfig<Input, Reply, EvalCase, Judges, Context>;/);
  assert.match(contextual, /export interface Props \{ input: Input; readonly context: Context \}/);
  const plain = await readFile(path.join(root, '.atomic/types/triage-email.d.ts'), 'utf8');
  assert.match(plain, /export type Config = AtomConfig<Input, Reply, EvalCase, Judges>;/);
  assert.match(plain, /export interface Props \{ input: Input \}/);
  assert(!plain.includes('Context'), plain);
});
test('schema edits invalidate the manifest and generated declaration content', async (t) => {
  const root = await project(t), before = await generate({ root });
  const source = path.join(root, 'schemas/contracts.schema.json');
  const document = JSON.parse(await readFile(source, 'utf8'));
  document.$defs.Meeting.properties.room = { type: 'string' };
  await writeFile(source, JSON.stringify(document));
  await assert.rejects(() => generate({ root, check: true }), /stale/);
  const after = await generate({ root });
  assert.notEqual(before.schemaDigest, after.schemaDigest);
  assert.match(await readFile(path.join(root, '.atomic/types/schemas.d.ts'), 'utf8'), /"room"\?: string/);
});
test('deleted or corrupted generated files fail checks and regeneration repairs them', async (t) => {
  const root = await project(t);
  await generate({ root });
  await rm(path.join(root, '.atomic/types/draft-reply.d.ts'));
  await writeFile(path.join(root, '.atomic/types/schemas.d.ts'), 'export type Email = any;');
  await assert.rejects(() => generate({ root, check: true }), /stale/);
  await generate({ root });
  await generate({ root, check: true });
});
test('removed registry entries cannot leave usable stale atom declarations', async (t) => {
  const root = await project(t);
  await generate({ root });
  const source = path.join(root, 'schemas/atoms.json'), registry = JSON.parse(await readFile(source, 'utf8'));
  delete registry['triage-email'];
  await writeFile(source, JSON.stringify(registry));
  await assert.rejects(() => generate({ root, check: true }), /stale/);
  await generate({ root });
  await assert.rejects(() => readFile(path.join(root, '.atomic/types/triage-email.d.ts')), /ENOENT/);
});
test('unsupported schema constraints, remote refs and recursive aliases fail explicitly', () => {
  const document = (definition) => ({ $schema: 'https://json-schema.org/draft/2020-12/schema', $defs: { Value: definition } });
  assert.throws(() => declarations(document({ type: 'string', minLength: 3 })), /unsupported schema keyword/);
  assert.throws(() => declarations(document({ $ref: 'https://example.test/schema' })), /unknown local schema/);
  assert.throws(() => declarations(document({ $ref: '#/$defs/Value' })), /recursive schema/);
});
test('unknown registry schemas fail before generated output is written', async (t) => {
  const root = await project(t);
  await writeFile(path.join(root, 'schemas/atoms.json'), JSON.stringify({ bad: { input: 'Unknown', output: 'Reply', case: 'EvalCase' } }));
  await assert.rejects(() => generate({ root }), /unknown registry schema/);
  await assert.rejects(() => readFile(path.join(root, '.atomic/types/manifest.json')), /ENOENT/);
});
