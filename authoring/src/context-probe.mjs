// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { contextReport } from './context-report.mjs';
const root = fileURLToPath(new URL('../', import.meta.url));
const stateDir = await mkdtemp(path.join(os.tmpdir(), 'atomic-context-probe-'));
try {
  const report = await contextReport(root, stateDir);
  assert(report.valid.valid && !report.tooLong.valid);
  assert.equal(report.resumeResolverCalls, 0);
  assert.deepEqual(report.reads, ['SUPPORT_REGION', 'SUPPORT_DOCS_SITE']);
  console.log(JSON.stringify(report, null, 2));
} finally { await rm(stateDir, { recursive: true, force: true }); }
