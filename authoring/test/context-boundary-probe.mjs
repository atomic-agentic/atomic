// SPDX-License-Identifier: Apache-2.0
// The context guards, run as they ship — under the pinned Deno, with the
// compiler and TypeScript stubbed out (see `stubs/`).
//
// A public context that only a validator or a scorer reads has nothing in the
// template to reveal it, so a missing snapshot must not reach the callback as a
// missing value: that comes back as a *failed validation*, which the host could
// repair, of an output whose rule never ran. These checks call the same
// boundaries production calls — `render`, `validateOutput`, `judgeInput`,
// `judgeScore` — and assert the refusal is the operation error
// `context snapshot required`. The contracts map is deliberately empty: if a
// guard were gone, the contract lookup would fail first and say so.
//
//   cd authoring && deno run --no-config --no-lock --no-remote --allow-read=. \
//     --import-map=test/stubs/import-map.json test/context-boundary-probe.mjs
import assert from 'node:assert/strict';
const target = new URL(Deno.args[0] ?? '../src/renderer.mjs', import.meta.url);
const { needsContext, render, validateOutput, judgeInput, judgeScore } = await import(target.href);

const contract = (id) => ({ kind: 'interpreted', id, source: 'x' });
const contextual = {
  bindings: [{ local: 'input', prop: 'input' }], locals: [], nodes: [],
  config: { validate: [contract('validate.0')], scorers: { quality: { with: contract('scorers.quality.with'), score: contract('scorers.quality.score') } } },
  contracts: new Map(), requiresContext: true,
};
const boundInTemplate = { ...contextual, requiresContext: undefined, bindings: [{ local: 'context', prop: 'context' }] };
const plain = { bindings: [{ local: 'input', prop: 'input' }], locals: [], nodes: [], config: {}, contracts: new Map(), requiresContext: undefined };
const values = { input: { text: 'x' }, output: { body: 'x' } };

const failures = [];
function check(label, run) {
  try { run(); console.log(`ok   ${label}`); }
  catch (error) { failures.push(label); console.log(`FAIL ${label}: ${error.message.split('\n')[0]}`); }
}
check('needsContext: a declared context', () => assert.equal(needsContext(contextual), true));
check('needsContext: a template-bound context', () => assert.equal(needsContext(boundInTemplate), true));
check('needsContext: no context', () => assert.equal(needsContext(plain), false));
for (const [label, unit] of [['declared context', contextual], ['template-bound context', boundInTemplate]]) for (const [name, call] of [
  ['render', () => render(unit, values.input)],
  ['validate, no options', () => validateOutput(unit, values)],
  ['validate, empty options', () => validateOutput(unit, values, {})],
  ['judgeInput', () => judgeInput(unit, 'quality', values)],
  ['judgeScore', () => judgeScore(unit, 'quality', 1)],
]) check(`${label} without a snapshot is refused by ${name}`, () => assert.throws(call, (error) => error.message === 'context snapshot required'));
check('an atom with no context validates without a snapshot', () => assert.deepEqual(validateOutput(plain, values), { valid: true, results: [] }));
check('an atom with no context renders without a snapshot', () => assert.equal(render(plain, values.input).developer, ''));

if (failures.length) { console.log(`${failures.length} checks failed: ${failures.join('; ')}`); Deno.exit(1); }
console.log('PASS context guards refuse at render, validate, judgeInput and judgeScore, and stay silent without a context');
