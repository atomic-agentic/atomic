// SPDX-License-Identifier: Apache-2.0
// Trusted host resolution and persistence. Atom expressions never read the environment.
import { readFile, mkdir, open, link, rm, realpath, path, createHash, fileURLToPath } from './platform.mjs';

const forbidden = new Set(['__proto__', 'prototype', 'constructor']);
function plainJson(value, budget = { left: 20_000 }, depth = 0) {
  if (--budget.left < 0 || depth > 32) throw new Error('context JSON limit exceeded');
  if (value === null || typeof value === 'boolean' || typeof value === 'string') return;
  if (typeof value === 'number' && Number.isFinite(value)) return;
  if (!value || typeof value !== 'object' || Object.getOwnPropertySymbols(value).length
    || ![Object.prototype, Array.prototype, null].includes(Object.getPrototypeOf(value))) throw new Error('context requires plain JSON');
  for (const [key, item] of Object.entries(Object.getOwnPropertyDescriptors(value))) {
    if (Array.isArray(value) && key === 'length') continue;
    if (forbidden.has(key) || !Object.hasOwn(item, 'value')) throw new Error('context contains forbidden property or accessor');
    plainJson(item.value, budget, depth + 1);
  }
}
function copy(value) {
  plainJson(value);
  const text = JSON.stringify(value);
  if (text.length > 1_048_576) throw new Error('context size limit exceeded');
  return JSON.parse(text);
}
function freeze(value) {
  if (value && typeof value === 'object') { for (const child of Object.values(value)) freeze(child); Object.freeze(value); }
  return value;
}
function canonical(value) { return JSON.stringify(value, (_, item) => item && typeof item === 'object' && !Array.isArray(item)
  ? Object.fromEntries(Object.keys(item).sort().map((key) => [key, item[key]])) : item); }
function hash(value) { return createHash('sha256').update(canonical(value)).digest('hex'); }
function keys(value, allowed, label) {
  if (!value || typeof value !== 'object' || Array.isArray(value) || Object.keys(value).some((key) => !allowed.includes(key))) throw new Error(`invalid ${label}`);
}
function merge(base, patch) {
  const out = { ...base };
  for (const [key, value] of Object.entries(patch)) {
    out[key] = value && typeof value === 'object' && !Array.isArray(value)
      && out[key] && typeof out[key] === 'object' && !Array.isArray(out[key]) ? merge(out[key], value) : copy(value);
  }
  return out;
}

// The shape check the interpreter relies on: types, enums, properties and items.
// Constraint keywords (minimum, pattern, maxLength, format, ...) are left to the
// host, which validates the complete context schema before a snapshot is used.
// Keywords that would change the shape of a value are refused, never ignored.
const shapeKeywords = new Set(['$ref', 'type', 'properties', 'required', 'additionalProperties', 'items', 'enum', 'const']);
const ignoredKeywords = new Set(['$id', '$schema', '$comment', 'title', 'description', 'default', 'examples', 'deprecated', 'readOnly', 'writeOnly',
  'format', 'minimum', 'maximum', 'exclusiveMinimum', 'exclusiveMaximum', 'multipleOf', 'minLength', 'maxLength', 'pattern',
  'minItems', 'maxItems', 'uniqueItems', 'minProperties', 'maxProperties']);
export function validateContext(document, name, value) {
  if (!document || typeof document !== 'object' || !document.$defs || typeof document.$defs !== 'object') throw new Error('context schema document requires $defs');
  plainJson(value);
  if (!Object.hasOwn(document.$defs, name)) throw new Error(`unknown context schema ${name}`);
  let budget = 100_000;
  function check(schema, item, at, depth = 0) {
    if (--budget < 0 || depth > 64) throw new Error('context schema limit exceeded');
    if (schema === true) return;
    if (schema === false) throw new Error(`context schema forbids a value at ${at}`);
    if (!schema || typeof schema !== 'object' || Array.isArray(schema)) throw new Error('invalid context schema');
    for (const key of Object.keys(schema)) {
      if (!shapeKeywords.has(key) && !ignoredKeywords.has(key)) throw new Error(`context schemas do not support ${key}`);
    }
    if (schema.$ref) {
      const target = schema.$ref.startsWith('#/$defs/') ? schema.$ref.slice('#/$defs/'.length) : null;
      if (target === null || !Object.hasOwn(document.$defs, target)) throw new Error(`unknown context schema reference ${schema.$ref}`);
      check(document.$defs[target], item, at, depth + 1);
    }
    if (Object.hasOwn(schema, 'const') && canonical(schema.const) !== canonical(item)) throw new Error(`context schema const mismatch at ${at}`);
    if (schema.enum && !schema.enum.some((option) => canonical(option) === canonical(item))) throw new Error(`context schema enum mismatch at ${at}`);
    if (schema.type !== undefined) {
      const types = Array.isArray(schema.type) ? schema.type : [schema.type];
      const matches = (type) => type === 'integer' ? Number.isSafeInteger(item)
        : type === 'number' ? typeof item === 'number' && Number.isFinite(item)
          : type === 'null' ? item === null
            : type === 'array' ? Array.isArray(item)
              : type === 'object' ? item !== null && typeof item === 'object' && !Array.isArray(item)
                : typeof item === type;
      if (!types.some(matches)) throw new Error(`context schema type mismatch at ${at}`);
    }
    if (item !== null && typeof item === 'object' && !Array.isArray(item)) {
      const properties = schema.properties ?? {};
      for (const key of schema.required ?? []) if (!Object.hasOwn(item, key)) throw new Error(`missing context property ${at}.${key}`);
      for (const [key, child] of Object.entries(item)) {
        if (Object.hasOwn(properties, key)) check(properties[key], child, `${at}.${key}`, depth + 1);
        else if (schema.additionalProperties === false) throw new Error(`unknown context property ${at}.${key}`);
        else if (schema.additionalProperties !== undefined) check(schema.additionalProperties, child, `${at}.${key}`, depth + 1);
      }
    }
    if (Array.isArray(item) && schema.items !== undefined) item.forEach((child, i) => check(schema.items, child, `${at}[${i}]`, depth + 1));
  }
  check(document.$defs[name], value, 'context');
}

export function resolveContext({ document, settings, profile, readEnv, metadata, overrides = {} }) {
  settings = copy(settings); metadata = copy(metadata); overrides = copy(overrides);
  keys(settings, ['schema', 'defaultProfile', 'base', 'profiles', 'environment'], 'context settings');
  keys(metadata, ['run', 'project', 'step'], 'host metadata');
  const selected = profile ?? settings.defaultProfile;
  if (typeof selected !== 'string' || !Object.hasOwn(settings.profiles, selected)) throw new Error('unknown configuration profile');
  const environment = {};
  // Validate every declaration before the first environment read.
  for (const [key, entry] of Object.entries(settings.environment)) {
    keys(entry, ['name', 'required', 'secret'], 'environment declaration');
    if (!/^[A-Z_][A-Z_0-9]*$/.test(key) || !/^[A-Z_][A-Z_0-9]*$/.test(entry.name)
      || typeof entry.required !== 'boolean') throw new Error('invalid environment declaration');
    if (entry.secret !== undefined && entry.secret !== false) throw new Error('secret values cannot enter prompt-visible context');
  }
  for (const [key, entry] of Object.entries(settings.environment)) {
    const value = readEnv(entry.name);
    if (value === undefined) { if (entry.required) throw new Error(`missing declared environment variable ${entry.name}`); }
    else { if (typeof value !== 'string') throw new Error(`environment variable must be a string: ${entry.name}`); environment[key] = value; }
  }
  const context = { profile: selected, config: merge(merge(settings.base, settings.profiles[selected]), overrides), env: environment, ...metadata };
  validateContext(document, settings.schema, context);
  return freeze(copy({ version: 1, runId: context.run.id, schema: document, contextSchema: settings.schema, schemaDigest: hash(document), settingsDigest: hash(settings), context, origins: { profile: 'team', config: 'team', env: 'team', run: 'team', project: 'team', step: 'team' } }));
}
export function sealSnapshot(resolved) {
  const payload = copy(resolved);
  return freeze({ ...payload, digest: hash(payload) });
}
export function verifySnapshot(snapshot, runId) {
  snapshot = copy(snapshot);
  const { digest, ...payload } = snapshot;
  if (snapshot.version !== 1 || snapshot.runId !== snapshot.context?.run?.id || (runId !== undefined && snapshot.runId !== runId)) throw new Error('context snapshot identity mismatch');
  if (digest !== hash(payload) || snapshot.schemaDigest !== hash(snapshot.schema)) throw new Error('context snapshot integrity mismatch');
  validateContext(snapshot.schema, snapshot.contextSchema, snapshot.context);
  if (Object.values(snapshot.origins ?? {}).some((origin) => origin !== 'team') || Object.keys(snapshot.origins ?? {}).length !== 6) throw new Error('invalid context origins');
  return freeze(snapshot);
}
export function contextForStep(snapshot, stepId) {
  snapshot = verifySnapshot(snapshot);
  const context = copy(snapshot.context);
  if (stepId !== undefined) context.step = { id: stepId };
  validateContext(snapshot.schema, snapshot.contextSchema, context);
  return freeze(context);
}

export async function checkpointContext({ stateDir, runId, resolve, projectRoot = fileURLToPath(new URL('../', import.meta.url)) }) {
  if (typeof runId !== 'string' || !runId.length || runId.length > 256) throw new Error('invalid run id');
  const source = await realpath(projectRoot);
  const insideSource = (dir) => dir === source || dir.startsWith(source + path.sep);
  if (insideSource(path.resolve(stateDir))) throw new Error('context state must be outside the source project');
  await mkdir(stateDir, { recursive: true, mode: 0o700 });
  if (insideSource(await realpath(stateDir))) throw new Error('context state must be outside the source project');
  const file = path.join(stateDir, `context-${hash(runId)}.json`);
  async function load() {
    const text = await readFile(file, 'utf8');
    if (text.length > 1_048_576) throw new Error('context snapshot size limit exceeded');
    const snapshot = verifySnapshot(JSON.parse(text), runId);
    // Readers also commit a just-published directory entry before using it.
    const directory = await open(stateDir, 'r');
    try { await directory.sync(); } finally { await directory.close(); }
    return snapshot;
  }
  try { return await load(); } catch (error) { if (error.code !== 'ENOENT') throw error; }
  const candidate = verifySnapshot(sealSnapshot(await resolve()), runId);
  const temporary = `${file}.${globalThis.crypto.randomUUID()}.tmp`;
  try {
    const handle = await open(temporary, 'wx', 0o600);
    try { await handle.writeFile(JSON.stringify(candidate)); await handle.sync(); } finally { await handle.close(); }
    try { await link(temporary, file); } catch (error) { if (error.code !== 'EEXIST') throw error; }
    return await load();
  } finally { await rm(temporary, { force: true }); }
}
