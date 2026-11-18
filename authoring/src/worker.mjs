// SPDX-License-Identifier: Apache-2.0
// Trusted, bundled compiler. Atom modules are never imported or executed.
import { initialize } from '@astrojs/compiler';
import * as fs from 'node:fs/promises';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';

// Every outcome is one JSON response on stdout and exit code 0: `ok` with a
// result, or `error` with a message and, when known, the authored file and
// line. A non-zero exit means the worker crashed before it could answer.
function respond(body) { console.log(JSON.stringify(body)); }
function failure(error) {
  const body = { message: String(error?.message ?? error) };
  if (typeof error?.file === 'string') body.file = error.file;
  if (Number.isSafeInteger(error?.line) && error.line > 0) body.line = error.line;
  if (Array.isArray(error?.diagnostics)) body.diagnostics = error.diagnostics;
  return { error: body };
}
const PINNED_DENO = '2.9.8';
if (Deno.version.deno !== PINNED_DENO) {
  respond(failure(`the Astro compiler requires Deno ${PINNED_DENO}, but found Deno ${Deno.version.deno}; set ATOMIC_DENO to a Deno ${PINNED_DENO} executable`));
  Deno.exit(0);
}
let request;
try {
  let requestText = '';
  const decoder = new TextDecoder();
  for await (const chunk of Deno.stdin.readable) {
    requestText += decoder.decode(chunk, { stream: true });
    if (requestText.length > 8_388_608) throw new Error('compiler request limit exceeded');
  }
  requestText += decoder.decode();
  request = JSON.parse(requestText);
} catch (error) {
  respond(failure(error));
  Deno.exit(0);
}
if (request.options) for (const key of ['contextSnapshot', 'stepId']) if (request.options[key] === null) delete request.options[key];
const sources = new Map(Object.entries(request.sources ?? {}));
const frozen = request.sources !== undefined;
const sourcePaths = request.sourcePaths ?? {};
function missing(file) {
  if (request.action === 'load' && frozen) {
    const error = new Error('missing source'); error.sources = [file]; throw error;
  }
  throw new Error(`source is absent from compiled snapshot: ${file}`);
}
globalThis.__atomicPlatform = { ...fs, path, createHash, fileURLToPath,
  // Atom sources arrive in the request. The worker has no read access to them.
  async readFile(file) {
    if (sources.has(file)) return sources.get(file);
    missing(file);
  },
  async realpath(file) {
    if (Object.hasOwn(sourcePaths, file)) return sourcePaths[file];
    if (sources.has(file) || file === request.root) return file;
    missing(file);
  },
};
const wasm = await Deno.readFile(new URL('./astro.wasm', import.meta.url));
const fetchOriginal = globalThis.fetch;
globalThis.fetch = (url, options) => String(url) === 'atomic:compiler-wasm'
  ? Promise.resolve(new Response(wasm, { headers: { 'Content-Type': 'application/wasm' } })) : fetchOriginal(url, options);
await initialize({ wasmURL: 'atomic:compiler-wasm' });
try {
  const { compile, render, validateOutput, judgeInput, judgeScore, aggregateScores } = await import('./renderer.mjs');
  if (!frozen) throw new Error('the compiler reads atom sources only from the request');
  const unit = await compile(request.file, { root: request.root, authoringFile: `${request.root}/.atomic/types/authoring.d.ts`, production: true });
  if (Object.hasOwn(unit.config, 'validate') && !Array.isArray(unit.config.validate)) throw new Error('validate must be an array of interpreted callbacks');
  for (const descriptor of unit.config.validate ?? []) if (descriptor?.kind !== 'interpreted' || !unit.contracts.has(descriptor.id)) throw new Error('validate requires interpreted typed callbacks, not CEL strings');
  for (const scorer of Object.values(unit.config.scorers ?? {})) {
    for (const key of ['with', 'score']) if (scorer[key]?.kind !== 'interpreted' || !unit.contracts.has(scorer[key].id)) throw new Error('scorer bindings and scores require interpreted callbacks');
  }
  let result;
  if (request.action === 'resolveContext') {
    const { resolveContext, sealSnapshot } = await import('./context.mjs');
    const resolved = resolveContext({ ...request.resolution, readEnv: (name) => request.environment[name] });
    result = sealSnapshot({ ...resolved, programHash: request.programHash });
  } else if (request.action === 'verifyContext') {
    const { verifySnapshot } = await import('./context.mjs');
    result = verifySnapshot(request.snapshot, request.runId);
    if (result.programHash !== request.programHash) throw new Error('context compiled atom version mismatch');
  } else if (request.action === 'load') result = { config: unit.config, needsContext: unit.requiresContext || unit.bindings.some(binding => binding.prop === 'context'), sources: Object.fromEntries(sources), sourcePaths };
  else if (request.action === 'check') {
    const { typecheck } = await import('./typecheck.mjs');
    const { atomDeclarations } = await import('./schema-types.mjs');
    const stem = path.basename(request.file, '.atom.astro');
    const declarations = { [`${request.root}/.atomic/types/${stem}.d.ts`]: atomDeclarations(request.schemas), [`${request.root}/.atomic/types/authoring.d.ts`]: request.authoring };
    await typecheck(Object.fromEntries(sources), declarations, sourcePaths);
    result = declarations;
  } else if (request.action === 'render') result = render(unit, request.input, request.options);
  else if (request.action === 'validate') result = validateOutput(unit, request.values, request.options);
  else if (request.action === 'judgeInput') result = judgeInput(unit, request.scorer, request.values, request.options);
  else if (request.action === 'judgeScore') result = judgeScore(unit, request.scorer, request.values, request.options);
  else if (request.action === 'aggregate') result = aggregateScores(unit, request.scorer, request.scores);
  else throw new Error('unknown compiler action');
  respond({ ok: result });
} catch (error) {
  respond(error.sources ? { ok: { needsSources: error.sources } } : failure(error));
}
