// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtemp, cp, mkdir, writeFile, rm, access } from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { pipelineReport } from './pipeline-report.mjs';
const root = fileURLToPath(new URL('../', import.meta.url));
const repository = path.resolve(root, '..');
const binary = path.join(repository, 'target/astro-embedded/debug/atomic-astro-embedded-spike');
await access(binary); // An unavailable backend is a failed probe, never a skip.
const temp = await mkdtemp(path.join(os.tmpdir(), 'atomic-inprocess-deno-'));
const state = await mkdtemp(path.join(os.tmpdir(), 'atomic-inprocess-state-'));
try {
  for (const name of ['fixtures', 'schemas', 'authoring.d.ts']) await cp(path.join(root, name), path.join(temp, name), { recursive: true });
  await mkdir(path.join(temp, '.atomic'));
  for (const name of ['embedded.mjs', 'astro.wasm']) await cp(path.join(root, '.atomic', name), path.join(temp, '.atomic', name));
  await assert.rejects(() => access(path.join(temp, 'node_modules')));
  const expected = await pipelineReport(temp, path.join(state, 'reference'));
  await mkdir(path.join(state, 'worker'));
  // Force the embedded worker, rather than Node, to generate its declarations.
  await rm(path.join(temp, '.atomic/types'), { recursive: true });
  const mounts = [process.env.ATOMIC_DEV_MOUNTS, temp, state].filter(Boolean).join(':');
  const result = spawnSync(path.join(repository, 'scripts/dev.sh'), [binary, temp, path.join(state, 'worker')], {
    cwd: repository, encoding: 'utf8', timeout: 60_000, maxBuffer: 10_000_000,
    env: { ...process.env, ATOMIC_DEV_DB: 'off', ATOMIC_DEV_CPUS: '2', ATOMIC_DEV_MEMORY: '2g', ATOMIC_DEV_MOUNTS: mounts },
  });
  if (result.error) throw result.error;
  assert.equal(result.status, 0, result.stderr);
  const report = JSON.parse(result.stdout);
  assert.equal(report.runtime.hostPid, report.runtime.denoPid);
  assert.equal(report.runtime.sameProcess, true);
  assert.equal(report.denials.length, 6);
  assert.deepEqual(report.pipeline, expected);
  await access(path.join(temp, '.atomic/types/draft-reply.d.ts'));
  // Use fresh state: both comparison runs must demonstrate declared environment capture.
  await mkdir(path.join(state, 'control'));
  const control = spawnSync(path.join(repository, 'scripts/dev.sh'), [binary, temp, path.join(state, 'control'), '--control-allow-read'], {
    cwd: repository, encoding: 'utf8', timeout: 60_000, maxBuffer: 10_000_000,
    env: { ...process.env, ATOMIC_DEV_DB: 'off', ATOMIC_DEV_CPUS: '2', ATOMIC_DEV_MEMORY: '2g', ATOMIC_DEV_MOUNTS: mounts },
  });
  if (control.error) throw control.error;
  assert.notEqual(control.status, 0, 'read permission control did not fail');
  assert(control.stderr.includes('permission control did not deny read outside project'), control.stderr);
  const summary = { runtime: report.runtime, generatedInsideWorker: true, equivalentToNode: true, nodeModulesAtRuntime: false, denials: report.denials, readPermissionControlFails: true,
    contextCheckpointedOutsideProject: true, contextRestoredWithoutResolver: report.pipeline.runtimeContext.resumeResolverCalls === 0 };
  await mkdir(path.join(root, 'results'), { recursive: true });
  await writeFile(path.join(root, 'results/embedded.json'), JSON.stringify(summary, null, 2) + '\n');
  console.log(JSON.stringify(summary, null, 2));
} finally { await rm(temp, { recursive: true, force: true }); await rm(state, { recursive: true, force: true }); }
