// SPDX-License-Identifier: Apache-2.0
import { readFile, path } from './platform.mjs';
import { checkpointContext, resolveContext, contextForStep } from './context.mjs';
import { compile, render, validateOutput, judgeInput, judgeScore } from './renderer.mjs';

export async function contextReport(root, stateDir) {
  const host = JSON.parse(await readFile(path.join(root, 'fixtures/context/host.json'), 'utf8'));
  const document = JSON.parse(await readFile(path.join(root, 'schemas/contracts.schema.json'), 'utf8'));
  const settings = JSON.parse(await readFile(path.join(root, 'fixtures/context/settings.json'), 'utf8'));
  const reads = [];
  const snapshot = await checkpointContext({ stateDir, projectRoot: root, runId: host.metadata.run.id,
    resolve: () => resolveContext({ document, settings, profile: host.profile, metadata: host.metadata,
      readEnv: (name) => { reads.push(name); return host.environment[name]; } }) });
  let resumedCalls = 0;
  const restored = await checkpointContext({ stateDir, projectRoot: root, runId: host.metadata.run.id, resolve: () => { resumedCalls++; throw new Error('resume must not read live settings'); } });
  if (restored.digest !== snapshot.digest || resumedCalls) throw new Error('context snapshot was not reused');
  const input = JSON.parse(await readFile(path.join(root, 'fixtures/input.json'), 'utf8'));
  const unit = await compile(path.join(root, 'fixtures/context/draft-reply.atom.astro'), { root, authoringFile: path.join(root, 'authoring.d.ts') });
  const invocation = { input, output: { subject: 'Re: meeting', body: 'See you then.' }, case: { criteria: ['Answer the question'] } };
  const options = { contextSnapshot: snapshot, stepId: 'draft' };
  const result = render(unit, input, options);
  if (result.developer.includes(host.environment.SUPPORT_REGION)) throw new Error('environment value entered developer instructions');
  return {
    snapshotDigest: snapshot.digest, context: contextForStep(snapshot, 'draft'), reads, resumeResolverCalls: resumedCalls,
    nextStep: contextForStep(snapshot, 'review'), prompt: result,
    valid: validateOutput(unit, invocation, options),
    tooLong: validateOutput(unit, { ...invocation, output: { ...invocation.output, body: 'x'.repeat(201) } }, options),
    judgeInput: judgeInput(unit, 'quality', invocation, options), score: judgeScore(unit, 'quality', { verdicts: [{ met: true }] }, options),
  };
}
