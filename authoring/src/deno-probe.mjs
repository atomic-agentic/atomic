// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFile, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { compile, render } from './renderer.mjs';
import { contractsReport } from './contract-probe.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));
const image = 'denoland/deno@sha256:a0d88a7fcbd44f062cba2748e19df67d30290ad12aa203c7c1e0b12462c483f0';
const base = ['run', '--rm', '--pull=never', '--network=none', '--read-only', '--cap-drop=ALL', '--security-opt=no-new-privileges', '--memory=512m', '--cpus=1', '--pids-limit=128', '--tmpfs', '/tmp:rw,nosuid,size=128m', '--env', 'DENO_DIR=/tmp/deno', '--mount', `type=bind,src=${root},dst=/spike,readonly`, '--workdir', '/spike', '--entrypoint', 'deno', image];
function docker(args) {
  const result = spawnSync('docker', [...base, ...args], { encoding: 'utf8', timeout: 30_000 });
  if (result.error) throw result.error;
  assert.equal(result.status, 0, result.stderr);
  return result.stdout;
}
const version = docker(['--version']).trim();
const permissions = ['run', '--no-config', '--no-lock', '--node-modules-dir=manual', '--cached-only', '--allow-read=/spike', '--allow-env=TSC_*,NODE_INSPECTOR_IPC,VSCODE_INSPECTOR_OPTIONS,NODE_ENV'];
const actual = JSON.parse(docker([...permissions, 'src/cli.mjs']));
const unit = await compile(path.join(root, 'fixtures/draft-reply.atom.astro'), { root });
const input = JSON.parse(await readFile(path.join(root, 'fixtures/input.json'), 'utf8'));
assert.deepEqual(actual, JSON.parse(JSON.stringify({ config: unit.config, ...render(unit, input) })));
const actualContracts = JSON.parse(docker([...permissions, 'src/contract-probe.mjs']));
assert.deepEqual(actualContracts, await contractsReport());
const report = { version, image, identicalToNode: true, contractsIdenticalToNode: true, permissions: ['read project', 'TypeScript configuration environment variables'], network: false, ffi: false, subprocess: false };
await mkdir(path.join(root, 'results'), { recursive: true });
await writeFile(path.join(root, 'results/deno.json'), JSON.stringify(report, null, 2) + '\n');
console.log(JSON.stringify(report, null, 2));
