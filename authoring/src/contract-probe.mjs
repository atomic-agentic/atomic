// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { compile, validateOutput, judgeInput, judgeScore, aggregateScores } from './renderer.mjs';

export async function contractsReport() {
  const root = fileURLToPath(new URL('../', import.meta.url));
  const unit = await compile(path.join(root, 'fixtures/typed/draft-reply.atom.astro'), { root });
  const input = JSON.parse(await readFile(path.join(root, 'fixtures/input.json'), 'utf8'));
  const output = { subject: 'Re: meeting', body: 'See you then.', meeting: { title: 'Check-in', start: '2026-11-17T10:00:00Z', minutes: 30 } };
  const context = { input, output, case: { criteria: ['Answer the question'] } };
  const valid = validateOutput(unit, context);
  const outsideSlot = validateOutput(unit, { ...context, output: { ...output, meeting: { ...output.meeting, minutes: 31 } } });
  // Scripted judge output only: no model invocation or provider credentials.
  const judge = { verdicts: [true, true, true, true, false].map((met) => ({ met })) };
  const score = judgeScore(unit, 'quality', judge);
  const aggregate = aggregateScores(unit, 'quality', [score, 1]);
  assert.equal(valid.valid, true);
  assert.equal(outsideSlot.valid, false);
  assert.equal(score, 0.8);
  assert.equal(aggregate.passed, true);
  return { valid, outsideSlot, judgeInput: judgeInput(unit, 'quality', context), score, aggregate };
}
if (process.argv[1] === fileURLToPath(import.meta.url)) console.log(JSON.stringify(await contractsReport(), null, 2));
