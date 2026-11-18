// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict';
import { mkdtemp, cp, symlink, readFile, writeFile, rm, mkdir } from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { generate } from './generate.mjs';
import { probe } from './lsp-probe.mjs';
const root = fileURLToPath(new URL('../', import.meta.url));
const temp = await mkdtemp(path.join(os.tmpdir(), 'atomic-schema-editor-'));
try {
  for (const name of ['schemas', 'fixtures', 'tsconfig.json', 'package.json', 'astro.config.mjs', 'authoring.d.ts']) await cp(path.join(root, name), path.join(temp, name), { recursive: true });
  await symlink(path.join(root, 'node_modules'), path.join(temp, 'node_modules'));
  const before = await generate({ root: temp });
  const source = path.join(temp, 'schemas/contracts.schema.json'), document = JSON.parse(await readFile(source, 'utf8'));
  document.$defs.Meeting.properties.room = { type: 'string' };
  await writeFile(source, JSON.stringify(document));
  await assert.rejects(() => generate({ root: temp, check: true }), /stale/);
  const after = await generate({ root: temp });
  assert.notEqual(before.schemaDigest, after.schemaDigest);
  const lsp = await probe('fixtures/typed/draft-reply.atom.astro', { root: temp, meetingMember: 'room' });
  assert(lsp.contracts.meetingCompletion.includes('room'));
  const report = { changedSchemaChangesDigest: true, staleCheckRejects: true, schemaFieldCompletion: 'room', validErrors: lsp.validErrors, negativeContractDiagnostics: lsp.contracts.invalidErrors.map((error) => error.code) };
  await mkdir(path.join(root, 'results'), { recursive: true });
  await writeFile(path.join(root, 'results/generation.json'), JSON.stringify(report, null, 2) + '\n');
  console.log(JSON.stringify(report, null, 2));
} finally { await rm(temp, { recursive: true, force: true }); }
