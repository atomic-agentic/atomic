// SPDX-License-Identifier: Apache-2.0
// Host entrypoint: only the resolver calls process.env, for declared names.
import { parseArgs } from 'node:util';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
import { checkpointContext, resolveContext, contextForStep } from './context.mjs';
import { compile, render } from './renderer.mjs';
const root = fileURLToPath(new URL('../', import.meta.url));
const { values } = parseArgs({ options: {
  profile: { type: 'string' }, 'run-id': { type: 'string' }, 'step-id': { type: 'string', default: 'draft' },
  'state-dir': { type: 'string' }, 'fixture-env': { type: 'boolean', default: false },
  settings: { type: 'string' }, overrides: { type: 'string' },
} });
if (!values['run-id']) throw new Error('--run-id is required');
const projectKey = createHash('sha256').update(root).digest('hex').slice(0, 16);
const stateDir = values['state-dir'] ?? path.join(process.env.XDG_STATE_HOME ?? path.join(os.homedir(), '.local/state'), 'atomic', 'astro-spike', projectKey);
const environmentReads = [];
const snapshot = await checkpointContext({ stateDir, projectRoot: root, runId: values['run-id'], resolve: async () => {
  // These reads are intentionally inside resolve, so a resume never consults them.
  const document = JSON.parse(await readFile(path.join(root, 'schemas/contracts.schema.json'), 'utf8'));
  const settings = JSON.parse(await readFile(values.settings ?? path.join(root, 'fixtures/context/settings.json'), 'utf8'));
  const host = JSON.parse(await readFile(path.join(root, 'fixtures/context/host.json'), 'utf8'));
  const overrides = values.overrides ? JSON.parse(await readFile(values.overrides, 'utf8')) : {};
  const metadata = { ...host.metadata, run: { id: values['run-id'] }, step: { id: values['step-id'] } };
  return resolveContext({ document, settings, profile: values.profile, metadata, overrides,
    readEnv: (name) => { environmentReads.push(name); return values['fixture-env'] ? host.environment[name] : process.env[name]; } });
} });
const unit = await compile(path.join(root, 'fixtures/context/draft-reply.atom.astro'), { root });
const input = JSON.parse(await readFile(path.join(root, 'fixtures/input.json'), 'utf8'));
console.log(JSON.stringify({ snapshotDigest: snapshot.digest, environmentReads, context: contextForStep(snapshot, values['step-id']),
  ...render(unit, input, { contextSnapshot: snapshot, stepId: values['step-id'] }) }, null, 2));
