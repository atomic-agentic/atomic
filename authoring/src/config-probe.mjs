// SPDX-License-Identifier: Apache-2.0
// The stock Astro editor, asked about the per-atom `Config` alias.
//
// `atomic check` and a person's editor read the same generated declarations,
// but only the editor proves that someone sees a misspelled output field *in
// the file they are writing*, and that an atom whose only read of the public
// context is inside a validator is typed with that context. Both halves are
// asserted here: a valid callback-only `Config` reports no error, one
// misspelled output field reports exactly one, at the authored source, and a
// context read on an atom that declares none is an error its own atom did not
// have before the read was written.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { readFile, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import path from 'node:path';

const toolingRoot = fileURLToPath(new URL('../', import.meta.url));
const contextual = 'fixtures/context/callback-context.atom.astro';
const plain = 'fixtures/triage-email.atom.astro';

// The smallest client that gets a diagnostic report out of the language server:
// enough of LSP to initialize once, then open a document, ask for its
// diagnostics and change it.
async function session(root, options = {}) {
  const child = spawn(process.execPath, [path.join(root, 'node_modules/@astrojs/language-server/bin/nodeServer.js'), '--stdio'], { cwd: root, stdio: ['pipe', 'pipe', 'pipe'] });
  let sequence = 0, buffer = Buffer.alloc(0), stderr = '';
  const pending = new Map();
  const send = (message) => {
    if (process.env.LSP_TRACE) console.error('SEND', message.method ?? message.id);
    const body = Buffer.from(JSON.stringify(message));
    child.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
    child.stdin.write(body);
  };
  child.stderr.on('data', (data) => { stderr += data; });
  child.stdout.on('data', (data) => {
    buffer = Buffer.concat([buffer, data]);
    for (;;) {
      const end = buffer.indexOf('\r\n\r\n');
      if (end < 0) break;
      const length = Number(/Content-Length: (\d+)/i.exec(buffer.subarray(0, end).toString())?.[1]);
      if (!Number.isFinite(length)) throw new Error('invalid LSP framing');
      if (buffer.length < end + 4 + length) break;
      const message = JSON.parse(buffer.subarray(end + 4, end + 4 + length));
      if (process.env.LSP_TRACE) console.error('RECV', JSON.stringify(message).slice(0, 600));
      buffer = buffer.subarray(end + 4 + length);
      if (message.method && message.id !== undefined) send({ jsonrpc: '2.0', id: message.id, result: message.method === 'workspace/configuration' ? message.params.items.map(() => null) : null });
      else if (message.id !== undefined) {
        const waiter = pending.get(message.id);
        if (waiter) {
          pending.delete(message.id); clearTimeout(waiter.timer);
          message.error ? waiter.reject(new Error(JSON.stringify(message.error))) : waiter.resolve(message.result);
        }
      }
    }
  });
  child.on('exit', (code) => {
    for (const waiter of pending.values()) { clearTimeout(waiter.timer); waiter.reject(new Error(`language server exited ${code}: ${stderr}`)); }
    pending.clear();
  });
  const request = (method, params) => new Promise((resolve, reject) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`LSP timeout: ${method}: ${stderr}`)); }, method === 'shutdown' ? 2_000 : 30_000);
    pending.set(id, { resolve, reject, timer });
    send({ jsonrpc: '2.0', id, method, params });
  });
  const notify = (method, params) => send({ jsonrpc: '2.0', method, params });
  // One `initialize` per server process, as LSP requires; each document is
  // opened into the session it starts.
  await request('initialize', {
    processId: null, rootUri: pathToFileURL(root).href, workspaceFolders: [{ uri: pathToFileURL(root).href, name: 'atomic-config-probe' }],
    capabilities: { workspace: { configuration: true, workspaceFolders: true }, textDocument: { diagnostic: { relatedDocumentSupport: true } } },
    initializationOptions: { typescript: { tsdk: options.tsdk ?? path.join(root, 'node_modules/typescript/lib') } },
  });
  notify('initialized', {});
  let version = 0;
  return {
    /** Open one of the repository's own fixtures, as an editor would. */
    async open(file) {
      const uri = pathToFileURL(path.resolve(root, file)).href;
      const text = await readFile(new URL(uri), 'utf8');
      notify('textDocument/didOpen', { textDocument: { uri, languageId: 'astro', version: ++version, text } });
      return { uri, text };
    },
    /** The document's errors only: severity 1, what an editor squiggles red. */
    async errors() {
      const report = await request('textDocument/diagnostic', { textDocument: { uri: this.uri } });
      return (report.items ?? []).filter((item) => item.severity === 1);
    },
    /** Replace the whole document, as a buffer-wide edit does. */
    async edit(replace) {
      this.text = replace(this.text);
      notify('textDocument/didChange', { textDocument: { uri: this.uri, version: ++version }, contentChanges: [{ text: this.text }] });
    },
    async close() { try { await request('shutdown', null).catch(() => {}); notify('exit', null); } finally { child.kill(); } },
  };
}

// The 0-based line an authored document holds `needle` on, so a diagnostic can
// be pinned to the source a person is typing in.
function lineOf(text, needle) {
  const offset = text.indexOf(needle);
  assert(offset >= 0, `probe target missing from the fixture: ${needle}`);
  return text.slice(0, offset).split('\n').length - 1;
}

export async function configProbe(options = {}) {
  const root = options.root ?? toolingRoot;
  const server = await session(root, options);
  try {
    // A context read that lives only in a validator: the generated `Config`
    // carries the atom's context, so this document is clean.
    Object.assign(server, await server.open(contextual));
    const valid = await server.errors();
    assert.deepEqual(valid, [], `errors on a valid callback-only Config: ${JSON.stringify(valid)}`);
    assert(server.text.includes('context.config.maxReplyCharacters'), 'the fixture must read the public context in a validator');
    assert(!server.text.includes('{context.'), 'the fixture must not read the context in its template');
    // One misspelled field of the validator's output is one error, on the line
    // that spells it — not a second error, and not one whose only location is
    // the generated declaration.
    const typo = 'output.bodyy';
    await server.edit((text) => text.replace('output.body.length', `${typo}.length`));
    const broken = await server.errors();
    assert.equal(broken.length, 1, `expected one error for ${typo}: ${JSON.stringify(broken)}`);
    const [first] = broken;
    assert(first.message.includes('bodyy') && /body/.test(first.message), `${typo}: ${JSON.stringify(first)}`);
    assert([2551, 2339].includes(first.code), `${typo}: expected a missing-property diagnostic: ${JSON.stringify(first)}`);
    assert.equal(first.range.start.line, lineOf(server.text, typo), `reported away from the authored line: ${JSON.stringify(first)}`);
    const misspelled = { code: first.code, message: first.message, line: first.range.start.line, character: first.range.start.character };
    // An atom that declares no context has no `context` to read: the
    // four-argument `Config` leaves it out of the callback, so writing a read
    // of it is an error the document did not have before the read was written.
    Object.assign(server, await server.open(plain));
    const name = path.basename(plain, '.atom.astro');
    const before = await server.errors();
    const reading = before.filter((item) => item.message.includes('context'));
    assert.deepEqual(reading, [], `the context-free fixture already reads a context: ${JSON.stringify(reading)}`);
    await server.edit((text) => text
      .replace(`import type { Props } from '../.atomic/types/${name}';`, `import type { Props, Config } from '../.atomic/types/${name}';`)
      .replace('const { input }: Props = Astro.props;', "const { input }: Props = Astro.props;\nexport const config: Config = { validate: [({ context }) => context.profile === 'prod'] };"));
    assert(server.text.includes('export const config: Config'), `the probe's edit did not apply: ${server.text}`);
    const added = (await server.errors()).filter((item) => !before.some((seen) => seen.message === item.message && seen.code === item.code));
    const undeclared = added.filter((item) => item.message.includes('context'));
    assert(undeclared.length >= 1, `a context read on an atom declaring no context must be an error: ${JSON.stringify(await server.errors())}`);
    return {
      languageServer: '2.17.3', typescript: '5.9.3', node: process.version,
      contextual, plain, misspelled,
      undeclaredContext: undeclared.map(({ code, message, range }) => ({ code, message, line: range.start.line })),
    };
  } finally {
    await server.close();
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const report = await configProbe();
  await mkdir(path.join(toolingRoot, 'results'), { recursive: true });
  await writeFile(path.join(toolingRoot, 'results/config.json'), `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify(report, null, 2));
}
