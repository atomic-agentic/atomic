// SPDX-License-Identifier: Apache-2.0
import { generate } from './generate.mjs';
import { compile, render, validateOutput, judgeInput, judgeScore, aggregateScores } from './renderer.mjs';
import { readFile, path } from './platform.mjs';
import { contextReport } from './context-report.mjs';

export async function pipelineReport(root, stateDir) {
  await generate({ root });
  const { changed, ...manifest } = await generate({ root, check: true });
  const unit = await compile(path.join(root, 'fixtures/typed/draft-reply.atom.astro'), { root, authoringFile: path.join(root, 'authoring.d.ts') });
  const input = JSON.parse(await readFile(path.join(root, 'fixtures/input.json'), 'utf8'));
  const output = { subject: 'Re: meeting', body: 'See you then.', meeting: { title: 'Check-in', start: '2026-11-17T10:00:00Z', minutes: 30 } };
  const context = { input, output, case: { criteria: ['Answer the question'] } };
  const valid = validateOutput(unit, context);
  const outsideSlot = validateOutput(unit, { ...context, output: { ...output, meeting: { ...output.meeting, minutes: 31 } } });
  const score = judgeScore(unit, 'quality', { verdicts: [true, true, true, true, false].map((met) => ({ met })) });
  return { manifest, prompt: render(unit, input), valid, outsideSlot, judgeInput: judgeInput(unit, 'quality', context), score, aggregate: aggregateScores(unit, 'quality', [score, 1]),
    ...(stateDir ? { runtimeContext: await contextReport(root, stateDir) } : {}) };
}
