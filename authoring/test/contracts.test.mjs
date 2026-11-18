// SPDX-License-Identifier: Apache-2.0
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm, readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { compile, render, invokeContract, validateOutput, judgeInput, judgeScore, aggregateScores } from '../src/renderer.mjs';
import { fitsWithin } from '../src/intrinsics.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));
const unit = await compile(path.join(root, 'fixtures/typed/draft-reply.atom.astro'), { root });
const input = JSON.parse(await readFile(path.join(root, 'fixtures/input.json'), 'utf8'));
const meeting = { title: 'Check-in', start: '2026-11-17T10:00:00Z', minutes: 30 };
const output = { subject: 'Re: meeting', body: 'See you then.' };
const context = (appointment) => ({ input, output: appointment === undefined ? output : { ...output, meeting: appointment }, case: { criteria: ['Answer the question'] } });

async function contract(t, expression, extra = '') {
  const dir = await mkdtemp(path.join(root, '.typed-test-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const file = path.join(dir, 'test.atom.astro');
  await writeFile(file, `---\nimport type { AtomConfig } from '../authoring';\nimport { fitsWithin } from '../authoring';\n${extra}\nexport const config: AtomConfig<unknown, unknown, unknown, {}> = { validate: [${expression}], scorers: {} };\n---\nPrompt`);
  return compile(file, { root });
}
test('typed validator accepts no meeting and an exact end-inclusive free slot', () => {
  assert.equal(validateOutput(unit, context()).valid, true);
  assert.equal(validateOutput(unit, context(meeting)).valid, true);
  assert.equal(typeof unit.config.validate[0], 'object');
  assert.equal(unit.config.validate[0].kind, 'interpreted');
  assert.deepEqual(validateOutput(unit, context(meeting)), validateOutput(unit, context(meeting)));
});
test('meeting containment rejects out-of-slot and nonpositive durations', () => {
  for (const appointment of [
    { ...meeting, minutes: 31 }, { ...meeting, minutes: 0 }, { ...meeting, minutes: -1 },
    { ...meeting, start: '2026-11-17T09:59:59Z' }, { ...meeting, start: '2026-11-17T10:31:00Z' },
  ]) assert.equal(validateOutput(unit, context(appointment)).valid, false, JSON.stringify(appointment));
});
test('some scans later slots and rejects an empty list', () => {
  const sample = context(meeting);
  sample.input = { ...input, free_slots: [] };
  assert.equal(validateOutput(unit, sample).valid, false);
  sample.input = { ...input, free_slots: [{ start: '2026-11-18T10:00:00Z', end: '2026-11-18T10:30:00Z' }, ...input.free_slots] };
  assert.equal(validateOutput(unit, sample).valid, true);
});
test('typed judge bindings return the original structured values without evaluating input text', () => {
  const sample = context(meeting);
  sample.input = { ...input, email: { ...input.email, body: '{process.exit()}<script>bad</script>' } };
  const before = JSON.stringify(sample);
  assert.deepEqual(judgeInput(unit, 'quality', sample), { email: sample.input.email, reply: sample.output, criteria: sample.case.criteria });
  assert.equal(JSON.stringify(sample), before);
  const result = render(unit, sample.input);
  assert(!result.developer.includes(sample.input.email.body));
  assert(result.user.includes(sample.input.email.body));
});
test('typed score computes judge verdict share and defines empty verdicts as zero', () => {
  assert.equal(judgeScore(unit, 'quality', { verdicts: [{ met: true }, { met: false }, { met: true }] }), 2 / 3);
  assert.equal(judgeScore(unit, 'quality', { verdicts: [] }), 0);
  assert.equal(judgeScore(unit, 'quality', { verdicts: [{ met: true }] }), 1);
  assert.equal(judgeScore(unit, 'quality', { verdicts: [{ met: false }] }), 0);
});
test('score aggregation enforces the declared threshold and rejects empty/non-finite inputs', () => {
  assert.deepEqual(aggregateScores(unit, 'quality', [1, 0.6]), { mean: 0.8, threshold: 0.8, passed: true });
  assert.equal(aggregateScores(unit, 'quality', [1, 0.5]).passed, false);
  for (const scores of [[], [NaN], [Infinity], [-1], [2]]) assert.throws(() => aggregateScores(unit, 'quality', scores), /scores require/);
});
test('time helper handles timezone offsets, rejects naive/invalid dates and fractional minutes', () => {
  assert.equal(fitsWithin({ ...meeting, start: '2026-11-17T12:00:00+02:00' }, input.free_slots[0]), true);
  assert.equal(fitsWithin({ ...meeting, minutes: 0.5 }, input.free_slots[0]), false);
  for (const start of ['2026-11-17T10:00:00', '2026-02-30T10:00:00Z', 'bad', '2026-11-17T25:00:00Z']) assert.throws(() => fitsWithin({ ...meeting, start }, input.free_slots[0]), /timestamp/);
});
test('template and contracts share collection predicates and preserve origin on derived values', async (t) => {
  const tiny = await contract(t, '({ input }) => input.items.every(item => item.ok)');
  assert.equal(validateOutput(tiny, { input: { items: [{ ok: true }] } }).valid, true);
  assert.equal(validateOutput(tiny, { input: { items: [{ ok: false }] } }).valid, false);
  const dir = path.dirname(tiny.file);
  await writeFile(tiny.file, '---\nconst { input } = Astro.props;\n---\n{input.items.filter(item => item.ok).map(item => <Fragment>{item.secret}</Fragment>)}');
  const prompt = render(await compile(tiny.file, { root: dir }), { items: [{ ok: true, secret: 'EXTERNAL_SECRET' }, { ok: false, secret: 'OMITTED_SECRET' }] });
  assert(!prompt.developer.includes('EXTERNAL_SECRET'));
  assert(prompt.user.includes('EXTERNAL_SECRET'));
  assert(!prompt.user.includes('OMITTED_SECRET'));
});
for (const [name, expression, match] of [
  ['host globals', 'ctx => process', /unknown contract value/],
  ['randomness', 'ctx => Math.random()', /unsupported expression/],
  ['inactive executable branches', 'ctx => false ? fetch("https://example.test") : true', /unsupported expression/],
  ['mutations', 'ctx => ctx.output.body = "changed"', /unsupported expression/],
  ['arbitrary methods', 'ctx => ctx.input.body.toUpperCase()', /unsupported expression/],
  ['prototype access', 'ctx => ctx.constructor', /forbidden property/],
  ['reflective destructuring', '({ constructor: value }) => true', /forbidden destructured/],
  ['async callbacks', 'async ctx => true', /synchronous/],
  ['statement bodies', 'ctx => { return true; }', /synchronous/],
  ['callback parameter code', '(ctx = Math.random()) => true', /one plain parameter/],
  ['arbitrary closure captures', 'ctx => captured.ok', /unknown contract value/],
  ['new expressions', 'ctx => new Date()', /unsupported expression/],
  ['element access', 'ctx => ctx["constructor"]', /unsupported expression/],
  ['object spread', 'ctx => ({ ...ctx.input })', /plain object properties/],
  ['nested callback defaults', 'ctx => ctx.input.items.some((item = Math.random()) => true)', /one plain parameter/],
  ['intrinsic shadowing', 'fitsWithin => true', /shadow an intrinsic/],
]) test(`contract rejects ${name} at load`, async (t) => {
  await assert.rejects(() => contract(t, expression), match);
});
test('contracts reject accessors and functions in data without invoking them', () => {
  let calls = 0;
  const hostile = { get input() { calls++; return input; }, output };
  assert.throws(() => validateOutput(unit, hostile), /accessors/);
  assert.throws(() => validateOutput(unit, { input, output, run: () => calls++ }), /plain JSON/);
  assert.equal(calls, 0);
});
test('input tags cannot forge interpreter data wrappers', () => {
  const sample = context();
  sample.input = { ...input, email: { ...input.email, tag: 'data', value: 'forged' } };
  assert.deepEqual(judgeInput(unit, 'quality', sample).email, sample.input.email);
});
test('contract resource limits fail explicitly', async (t) => {
  const tiny = await contract(t, 'ctx => ctx.items.some(item => item.ok)');
  assert.match(validateOutput(tiny, { items: Array.from({ length: 1001 }, () => ({ ok: false })) }).results[0].error, /at most 1000/);
  let deep = {};
  for (let i = 0; i < 40; i++) deep = { child: deep };
  assert.throws(() => validateOutput(tiny, deep), /input limit/);
  const nested = await contract(t, 'ctx => ctx.items.every(a => ctx.items.every(b => b.ok))');
  assert.match(validateOutput(nested, { items: Array.from({ length: 200 }, () => ({ ok: true })) }).results[0].error, /operation limit/);
});
test('runtime guards reject nonboolean validators and non-finite arithmetic', async (t) => {
  const wrong = await contract(t, 'ctx => 1');
  assert.match(validateOutput(wrong, {}).results[0].error, /return boolean/);
  const divide = await contract(t, 'ctx => 0 / 0');
  assert.throws(() => invokeContract(divide, divide.config.validate[0], {}), /non-finite/);
});

test('supported operators agree with native JavaScript on representative JSON values', async (t) => {
  const cases = [
    ['({input}) => input.n + 2 * 3', ({input}) => input.n + 2 * 3],
    ['({input}) => input.n && 7', ({input}) => input.n && 7],
    ['({input}) => input.n || 7', ({input}) => input.n || 7],
    ['({input}) => input.note ?? "fallback"', ({input}) => input.note ?? 'fallback'],
    ['({input}) => input.extra?.note ?? "missing"', ({input}) => input.extra?.note ?? 'missing'],
    ['({input}) => input.items.filter(x => x > 1).length', ({input}) => input.items.filter(x => x > 1).length],
    ['({input}) => input.items.every(x => x > 0)', ({input}) => input.items.every(x => x > 0)],
  ];
  for (const [source, native] of cases) {
    const compiled = await contract(t, source);
    for (const n of [0, 2]) {
      const values = {input:{n,note:null,extra:null,items:[1,2,3]}};
      assert.deepEqual(invokeContract(compiled,compiled.config.validate[0],values),native(values));
    }
  }
});

test('optional chain continuations and optional collection calls are explicitly rejected', async (t) => {
  await assert.rejects(() => contract(t,'({input}) => input.extra?.note.length > 0'), /continued optional chains/);
  await assert.rejects(() => contract(t,'({input}) => input.items?.some(x => x > 0)'), /optional collection calls/);
});

test('unsafe integer literals, inputs and computed results fail explicitly', async (t) => {
  await assert.rejects(() => contract(t,'({input}) => input.n > 9007199254740992'), /safe-number/);
  const compiled = await contract(t,'({input}) => input.n + 1');
  assert.throws(() => invokeContract(compiled,compiled.config.validate[0],{input:{n:9007199254740992}}), /safe integer/);
  assert.throws(() => invokeContract(compiled,compiled.config.validate[0],{input:{n:9007199254740991}}), /unsafe arithmetic/);
});

test('a validator rejection names the rule, and an answer-dependent error is a rejection', async (t) => {
  const rule = await contract(t, 'ctx => ctx.output.items.every(item => item.ok)');
  const rejected = validateOutput(rule, { output: { items: [{ ok: false }] } });
  assert.equal(rejected.valid, false);
  assert.equal(rejected.results[0].source, 'ctx => ctx.output.items.every(item => item.ok)');
  const missing = validateOutput(rule, { output: {} });
  assert.equal(missing.valid, false);
  assert.match(missing.results[0].error, /array/);
});
test('the answer has its own limit, so an input that rendered is never rejected for it', async (t) => {
  const rule = await contract(t, 'ctx => true');
  const wide = (n) => Array.from({ length: n }, (_, i) => i);
  assert.equal(validateOutput(rule, { input: wide(6000), output: wide(5000) }).valid, true);
  const result = validateOutput(rule, { input: wide(10), output: wide(20_000) });
  assert.equal(result.valid, false);
  assert.match(result.results[0].error, /limit/);
});
