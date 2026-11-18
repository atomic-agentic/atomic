// SPDX-License-Identifier: Apache-2.0
// Named failpoint: the parent kills this process after CONTEXT_COMMITTED.
import { readFile } from 'node:fs/promises';
import { checkpointContext, resolveContext } from '../src/context.mjs';
const document = JSON.parse(await readFile(new URL('../schemas/contracts.schema.json', import.meta.url), 'utf8'));
const settings = JSON.parse(await readFile(new URL('../fixtures/context/settings.json', import.meta.url), 'utf8'));
const host = JSON.parse(await readFile(new URL('../fixtures/context/host.json', import.meta.url), 'utf8'));
const runId = 'killed-context-run';
const metadata = { ...host.metadata, run: { id: runId } };
const snapshot = await checkpointContext({ stateDir: process.argv[2], runId,
  resolve: () => resolveContext({ document, settings, profile: 'test', metadata, readEnv: (name) => process.env[name] }) });
console.log(JSON.stringify({ failpoint: 'CONTEXT_COMMITTED', digest: snapshot.digest }));
setInterval(() => {}, 1000);
