// SPDX-License-Identifier: Apache-2.0
import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, writeFile, rm, readdir, stat } from 'node:fs/promises';
import { spawnSync, spawn } from 'node:child_process';
import { once } from 'node:events';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { resolveContext, sealSnapshot, verifySnapshot, checkpointContext, contextForStep, validateContext } from '../src/context.mjs';
import { compile, render, validateOutput, judgeInput, judgeScore, needsContext } from '../src/renderer.mjs';
const root = fileURLToPath(new URL('../', import.meta.url));
const document = JSON.parse(await readFile(path.join(root, 'schemas/contracts.schema.json'), 'utf8'));
const settings = JSON.parse(await readFile(path.join(root, 'fixtures/context/settings.json'), 'utf8'));
const host = JSON.parse(await readFile(path.join(root, 'fixtures/context/host.json'), 'utf8'));
const input = JSON.parse(await readFile(path.join(root, 'fixtures/input.json'), 'utf8'));
const unit = await compile(path.join(root, 'fixtures/context/draft-reply.atom.astro'), { root });
function resolve(options = {}) { return resolveContext({ document, settings, metadata: host.metadata, readEnv: (name) => host.environment[name], ...options }); }
function snapshot(options = {}) { return sealSnapshot(resolve(options)); }
async function state(t) { const dir = await mkdtemp(path.join(os.tmpdir(), 'atomic-context-test-')); t.after(() => rm(dir, { recursive: true, force: true })); return dir; }

test('context profile merge preserves nested base values and applies explicit config overrides', () => {
  const value = resolve({ profile: 'test', overrides: { replyLocale: 'fr', features: { drafts: false } } }).context;
  assert.equal(value.profile, 'test');
  assert.equal(value.config.replyLocale, 'fr');
  assert.equal(value.config.maxReplyCharacters, 200);
  assert.deepEqual(value.config.features, { calendar: false, drafts: false });
  assert.equal(resolve().context.profile, 'dev');
  assert.equal(resolve({ profile: 'prod' }).context.config.maxReplyCharacters, 600);
});
test('only declared environment variables are read and captured', () => {
  const reads = [];
  const value = resolve({ readEnv: (name) => { reads.push(name); return host.environment[name]; } });
  assert.deepEqual(reads, ['SUPPORT_REGION', 'SUPPORT_DOCS_SITE']);
  assert(!JSON.stringify(value).includes('not-for-the-prompt'));
  assert.deepEqual(Object.keys(value.context.env), ['SUPPORT_REGION', 'DOCS_SITE']);
});
test('secret-marked environment declarations fail before any environment reads', () => {
  let reads = 0;
  const changed = structuredClone(settings);
  changed.environment.DOCS_SITE.secret = true;
  assert.throws(() => resolve({ settings: changed, readEnv: () => { reads++; return 'sensitive'; } }), /secret values/);
  assert.equal(reads, 0);
});
test('missing required environment variables fail while absent optional variables are omitted', () => {
  assert.throws(() => resolve({ readEnv: () => undefined }), /missing declared environment variable SUPPORT_REGION/);
  assert.deepEqual(resolve({ readEnv: (name) => name === 'SUPPORT_REGION' ? 'eu' : undefined }).context.env, { SUPPORT_REGION: 'eu' });
});
test('unknown profiles and schema-invalid configuration or metadata fail', () => {
  assert.throws(() => resolve({ profile: 'staging' }), /unknown configuration profile/);
  assert.throws(() => resolve({ overrides: { maxReplyCharacters: 'many' } }), /schema type mismatch/);
  assert.throws(() => resolve({ overrides: { unrecognized: true } }), /unknown context property/);
  assert.throws(() => resolve({ metadata: { ...host.metadata, admin: true } }), /invalid host metadata/);
  assert.throws(() => resolve({ metadata: { ...host.metadata, run: { id: 1 } } }), /schema type mismatch/);
});
test('context values are deeply immutable and isolated from source configuration', () => {
  const config = structuredClone(settings), value = resolve({ settings: config });
  config.base.organizationName = 'changed';
  assert.equal(value.context.config.organizationName, settings.base.organizationName);
  assert.throws(() => { value.context.config.features.calendar = false; }, TypeError);
  assert.throws(() => { value.context.env.SUPPORT_REGION = 'changed'; }, TypeError);
});
test('context JSON refuses accessors, non-finite values and shape keywords it cannot check', () => {
  let reads = 0;
  assert.throws(() => resolve({ overrides: { get replyLocale() { reads++; return 'bad'; } } }), /accessor/);
  assert.equal(reads, 0);
  assert.throws(() => resolve({ overrides: { maxReplyCharacters: Infinity } }), /plain JSON/);
  const doc = structuredClone(document);
  // Constraints are the host's to enforce against the complete schema.
  doc.$defs.ProjectConfig.properties.replyLocale.minLength = 1;
  validateContext(doc, 'RuntimeContext', resolve().context);
  doc.$defs.ProjectConfig.properties.replyLocale.anyOf = [{ type: 'string' }];
  assert.throws(() => validateContext(doc, 'RuntimeContext', resolve().context), /do not support anyOf/);
});
test('snapshot verification rejects changed payload, schema and run identity', () => {
  const original = snapshot();
  for (const mutate of [
    (value) => { value.context.env.SUPPORT_REGION = 'changed'; },
    (value) => { value.schema.$defs.ProjectConfig.description = 'changed'; },
    (value) => { value.runId = 'other'; },
  ]) { const changed = structuredClone(original); mutate(changed); assert.throws(() => verifySnapshot(changed), /mismatch/); }
  assert.throws(() => verifySnapshot(original, 'different-run'), /identity mismatch/);
});
test('checkpoint restore does not reread changed live environment or configuration', async (t) => {
  const stateDir = await state(t), first = await checkpointContext({ stateDir, runId: host.metadata.run.id, resolve: () => resolve({ profile: 'test' }) });
  let calls = 0;
  const second = await checkpointContext({ stateDir, runId: host.metadata.run.id, resolve: () => { calls++; throw new Error('live configuration changed'); } });
  assert.equal(calls, 0);
  assert.deepEqual(second, first);
  const files = await readdir(stateDir);
  assert.equal(files.length, 1);
  assert.equal((await stat(path.join(stateDir, files[0]))).mode & 0o777, 0o600);
});
test('concurrent first writers converge on one published context snapshot', async (t) => {
  const stateDir = await state(t);
  const results = await Promise.all(['dev', 'test', 'prod'].map((profile) => checkpointContext({ stateDir, runId: host.metadata.run.id, resolve: () => resolve({ profile }) })));
  assert.equal(new Set(results.map((value) => value.digest)).size, 1);
  assert.equal((await readdir(stateDir)).length, 1);
});
test('corrupt checkpoints fail without resolving a replacement context', async (t) => {
  const stateDir = await state(t);
  await checkpointContext({ stateDir, runId: host.metadata.run.id, resolve });
  const [file] = await readdir(stateDir);
  await writeFile(path.join(stateDir, file), '{broken');
  let calls = 0;
  await assert.rejects(() => checkpointContext({ stateDir, runId: host.metadata.run.id, resolve: () => { calls++; return resolve(); } }));
  assert.equal(calls, 0);
});
test('wrong run identity is rejected before a candidate can be published', async (t) => {
  const stateDir = await state(t);
  await assert.rejects(() => checkpointContext({ stateDir, runId: 'another-run', resolve }), /identity mismatch/);
  assert.deepEqual(await readdir(stateDir), []);
  await assert.rejects(() => checkpointContext({ stateDir: path.join(stateDir, 'state'), projectRoot: stateDir, runId: host.metadata.run.id, resolve }), /outside the source/);
  assert.deepEqual(await readdir(stateDir), []);
});
test('dispatch binds current step metadata while preserving frozen run context', () => {
  const value = snapshot({ profile: 'test' });
  const draft = contextForStep(value, 'draft'), review = contextForStep(value, 'review');
  assert.equal(draft.step.id, 'draft'); assert.equal(review.step.id, 'review');
  assert.deepEqual(draft.config, review.config); assert.deepEqual(draft.env, review.env);
  assert.deepEqual(draft.run, review.run);
  assert.equal(value.context.step.id, 'draft');
});
test('context data preserves team provenance and stays outside developer instructions', () => {
  const value = snapshot({ overrides: { organizationName: 'ORG_{process.exit()}<script>payload</script>' } });
  const result = render(unit, input, { contextSnapshot: value, stepId: 'review' });
  for (const text of [value.context.config.organizationName, value.context.env.SUPPORT_REGION, value.context.run.id]) {
    assert(!result.developer.includes(text)); assert(result.user.includes(text));
  }
  for (const at of ['context.config.organizationName', 'context.env.SUPPORT_REGION', 'context.run.id', 'context.step.id']) assert(result.bindings.some((binding) => binding.path === at && binding.origin === 'team'));
  assert(result.bindings.some((binding) => binding.path === 'input.email.body' && binding.origin === 'external'));
});
test('contextual atoms require a verified snapshot for rendering and contract execution', () => {
  const invocation = { input, output: { subject: 'reply', body: 'short' }, case: { criteria: [] } };
  assert.throws(() => render(unit, input), /snapshot required/);
  assert.throws(() => render(unit, input, { contextSnapshot: snapshot(), runId: 'different-run' }), /run mismatch/);
  assert.throws(() => validateOutput(unit, invocation), /snapshot required/);
  assert.throws(() => judgeInput(unit, 'quality', invocation), /snapshot required/);
  assert.throws(() => judgeScore(unit, 'quality', { verdicts: [] }), /snapshot required/);
  const options = { contextSnapshot: snapshot({ profile: 'test' }) };
  assert.equal(validateOutput(unit, invocation, options).valid, true);
  assert.equal(validateOutput(unit, { ...invocation, output: { ...invocation.output, body: 'x'.repeat(201) } }, options).valid, false);
  assert.equal(judgeInput(unit, 'quality', invocation, options).reply.body, 'short');
  assert.equal(judgeScore(unit, 'quality', { verdicts: [{ met: true }] }, options), 1);
});
test('fresh host processes resume the same context despite new profile, environment and missing settings', async (t) => {
  const stateDir = await state(t), script = path.join(root, 'src/context-cli.mjs');
  function run(args, region) {
    const result = spawnSync(process.execPath, [script, '--state-dir', stateDir, '--run-id', 'restart-context', ...args], { cwd: root, encoding: 'utf8', timeout: 20_000, env: { ...process.env, SUPPORT_REGION: region } });
    if (result.error) throw result.error;
    assert.equal(result.status, 0, result.stderr);
    return JSON.parse(result.stdout);
  }
  const first = run(['--profile', 'test'], 'original-region');
  const second = run(['--profile', 'prod', '--settings', path.join(stateDir, 'missing-settings.json'), '--step-id', 'review'], 'changed-region');
  assert.equal(first.snapshotDigest, second.snapshotDigest);
  assert.equal(second.context.profile, 'test'); assert.equal(second.context.env.SUPPORT_REGION, 'original-region');
  assert.equal(second.context.step.id, 'review'); assert.deepEqual(second.environmentReads, []);
});
test('SIGKILL after CONTEXT_COMMITTED preserves the original context on restart', async (t) => {
  const stateDir = await state(t);
  const child = spawn(process.execPath, [path.join(root, 'test/context-crash-worker.mjs'), stateDir], { cwd: root,
    env: { ...process.env, SUPPORT_REGION: 'before-kill' }, stdio: ['ignore', 'pipe', 'pipe'] });
  t.after(() => child.kill('SIGKILL'));
  const committed = await new Promise((resolve, reject) => {
    let buffer = '', stderr = '';
    const timer = setTimeout(() => { child.kill('SIGKILL'); reject(new Error(`checkpoint failpoint timeout: ${stderr}`)); }, 10_000);
    child.stderr.on('data', (data) => { stderr += data; });
    child.once('error', (error) => { clearTimeout(timer); reject(error); });
    child.once('exit', (code) => { clearTimeout(timer); reject(new Error(`worker exited before failpoint: ${code}: ${stderr}`)); });
    child.stdout.on('data', (data) => { buffer += data; if (buffer.includes('\n')) { clearTimeout(timer); resolve(JSON.parse(buffer.slice(0, buffer.indexOf('\n')))); } });
  });
  assert.equal(committed.failpoint, 'CONTEXT_COMMITTED');
  const exited = once(child, 'exit');
  child.kill('SIGKILL');
  const [, signal] = await exited;
  assert.equal(signal, 'SIGKILL');
  const result = spawnSync(process.execPath, [path.join(root, 'src/context-cli.mjs'), '--state-dir', stateDir, '--run-id', 'killed-context-run', '--profile', 'prod'],
    { cwd: root, encoding: 'utf8', timeout: 20_000, env: { ...process.env, SUPPORT_REGION: 'after-kill' } });
  assert.equal(result.status, 0, result.stderr);
  const restored = JSON.parse(result.stdout);
  assert.equal(restored.snapshotDigest, committed.digest);
  assert.equal(restored.context.profile, 'test');
  assert.equal(restored.context.env.SUPPORT_REGION, 'before-kill');
  assert.deepEqual(restored.environmentReads, []);
});
// The per-atom `Config` spelling, read through the generated declarations the
// checker and the stock editor use. Nothing in this atom's template binds
// `context`, so a missing snapshot must not reach a callback as a missing value:
// that would be a repairable rejection of an output whose rule could not run.
test('a config that declares its own context needs a snapshot to validate or judge', async () => {
  const declared = await compile(path.join(root, 'fixtures/context/callback-context.atom.astro'), { root, production: true });
  assert(!declared.bindings.some((binding) => binding.prop === 'context'), JSON.stringify(declared.bindings.map((binding) => binding.prop)));
  assert.equal(declared.requiresContext, true);
  assert.equal(needsContext(declared), true);
  const answer = { input, output: { subject: 'reply', body: 'short' } };
  for (const [label, call] of [
    ['render', () => render(declared, input)],
    ['validate, no options', () => validateOutput(declared, answer)],
    ['validate, empty options', () => validateOutput(declared, answer, {})],
  ]) assert.throws(call, /context snapshot required/, label);
  const options = { contextSnapshot: snapshot({ profile: 'test' }) };
  assert.equal(validateOutput(declared, answer, options).valid, true);
  assert.equal(validateOutput(declared, { ...answer, output: { subject: 'reply', body: 'x'.repeat(201) } }, options).valid, false);
  assert.equal(validateOutput(declared, { ...answer, output: { subject: 'reply', body: 'x'.repeat(201) } }, options).results[0].id, 'validate.0');
});
test('an atom that declares no context validates with no snapshot in hand', async () => {
  const plain = await compile(path.join(root, 'fixtures/triage-email.atom.astro'), { root, production: true });
  assert.equal(needsContext(plain), false);
  assert.doesNotThrow(() => validateOutput(plain, { input: {}, output: {} }));
});
