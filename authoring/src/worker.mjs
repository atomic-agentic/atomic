// SPDX-License-Identifier: Apache-2.0
// Trusted, bundled compiler. Atom modules are never imported or executed.
import * as fs from 'node:fs/promises';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';

// The request is one line of JSON on stdin. Every outcome is one JSON line on
// stdout and exit code 0: `ok` with a result, or `error` with a message and,
// when known, the authored file and line. A non-zero exit means the worker
// crashed before it could answer. While it works, the worker may ask the host
// to run a captured helper: it writes a `call` line and reads the host's
// one-line reply from stdin.
//
// Stdout carries protocol messages. Diagnostics go to stderr.
console.log = (...args) => console.error(...args);
function respond(body) {
  const bytes = new TextEncoder().encode(`${JSON.stringify(body)}\n`);
  let written = 0;
  while (written < bytes.length) {
    const n = Deno.stdout.writeSync(bytes.subarray(written));
    // A write that takes nothing is a closed pipe rather than a short one,
    // and spinning on it would leave the host waiting for a line that never
    // comes. The exit says what happened.
    if (n <= 0) throw new Error('the protocol line could not be written');
    written += n;
  }
}
function failure(error) {
  const body = { message: String(error?.message ?? error) };
  if (typeof error?.file === 'string') body.file = error.file;
  if (Number.isSafeInteger(error?.line) && error.line > 0) body.line = error.line;
  if (Number.isSafeInteger(error?.column) && error.column > 0) body.column = error.column;
  if (Array.isArray(error?.diagnostics)) body.diagnostics = error.diagnostics;
  return { error: body };
}
// One line from stdin, read synchronously (the interpreter is synchronous and
// waits for each helper result). Bytes after the newline are kept for the next.
let pending = new Uint8Array(0);
function readLine(limit) {
  const chunk = new Uint8Array(65536);
  for (;;) {
    const newline = pending.indexOf(10);
    if (newline >= 0) {
      const line = new TextDecoder().decode(pending.subarray(0, newline));
      pending = pending.slice(newline + 1);
      return line;
    }
    if (pending.length > limit) throw new Error('compiler request limit exceeded');
    const read = Deno.stdin.readSync(chunk);
    if (read === null) {
      if (!pending.length) return null;
      const line = new TextDecoder().decode(pending); pending = new Uint8Array(0); return line;
    }
    const next = new Uint8Array(pending.length + read); next.set(pending); next.set(chunk.subarray(0, read), pending.length); pending = next;
  }
}
let request;
try {
  const line = readLine(8_388_608);
  if (line === null) throw new Error('missing compiler request');
  request = JSON.parse(line);
} catch (error) {
  respond(failure(error));
  Deno.exit(0);
}
if (request.options) for (const key of ['contextSnapshot', 'stepId']) if (request.options[key] === null) delete request.options[key];
const sources = new Map(Object.entries(request.sources ?? {}));
const frozen = request.sources !== undefined;
const sourcePaths = request.sourcePaths ?? {};
// Files the host looked for and found absent: an import of one is an
// authored error, reported where the import is.
const absent = new Set(request.absent ?? []);
function missing(file) {
  if (absent.has(file)) throw Object.assign(new Error(`no such file: ${file}`), { code: 'ENOENT' });
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
    // A source the host captured under its own name is already canonical: its
    // alias was never recorded because nothing asked for it under another name.
    if (sources.has(file) || file === request.root) return file;
    missing(file);
  },
};
try {
  const { compile, render, validateOutput, judgeInput, judgeScore, aggregateScores, needsContext } = await import('./renderer.mjs');
  if (!frozen) throw new Error('the compiler reads atom sources only from the request');
  const unit = await compile(request.file, { root: request.root, authoringFile: `${request.root}/.atomic/types/authoring.d.ts`, production: true });
  // The host's compiled contract says whether this atom has a public context.
  // The compiler's own look at the source sees a fifth `AtomConfig` type
  // argument, a `context` schema in the config, or a `context` prop bound in a
  // template; it cannot see a context that the sidecar beside the atom declares,
  // nor read the per-atom `Config` alias back to its fifth argument. A host that
  // says "declared" is believed; a host that says otherwise clears nothing, so
  // two disagreeing readings stay a refusal rather than a silent pass.
  if (request.contextDeclared === true) unit.requiresContext = true;
  // Helper calls go to the host, which runs each in a fresh isolate from the
  // capture it recorded when the atom loaded.
  const { locateThrow } = await import('./capture.mjs');
  const { setHelperExecutor, HelperFailure } = await import('./renderer.mjs');
  setHelperExecutor((capture, name, args) => {
    respond({ call: { capture, export: name, args } });
    const line = readLine(8_388_608);
    if (line === null) throw new HelperFailure('host', 'the host closed the helper channel');
    const reply = JSON.parse(line);
    if (Object.hasOwn(reply, 'ok')) return reply.ok;
    const failure = reply.error ?? {};
    const text = String(failure.message ?? 'failed');
    const at = failure.kind === 'threw' ? locateThrow(text, unit.captures.values()) : undefined;
    // A thrown error's stack locates it; the message is its first line.
    throw new HelperFailure(failure.kind ?? 'host', `helper ${name}: ${text.split('\n')[0]}`, at?.file, at?.line);
  });
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
  } else if (request.action === 'load') {
    // Type-only imports across the component graph, which the host resolves to declarations.
    const typeImports = new Set(), seen = new Set();
    const visit = (item) => { if (seen.has(item)) return; seen.add(item); item.typeImports.forEach((file) => typeImports.add(file)); item.imports.forEach(visit); };
    visit(unit);
    const helpers = Object.fromEntries([...unit.captures].sort(([a], [b]) => (a < b ? -1 : 1)).map(([digest, captured]) => [digest, captured.artifact]));
    result = { config: unit.config, needsContext: needsContext(unit), sources: Object.fromEntries(sources), sourcePaths, typeImports: [...typeImports].sort(), helpers };
  } else if (request.action === 'check') {
    // The host generates every declaration from the full schemas; the worker only checks.
    const { typecheck } = await import('./typecheck.mjs');
    const declarations = request.declarations ?? {};
    for (const [name, text] of Object.entries(declarations)) {
      if (!name.startsWith(`${request.root}/.atomic/types/`) || !name.endsWith('.d.ts') || typeof text !== 'string') throw new Error('declarations must be .d.ts text under .atomic/types');
    }
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
