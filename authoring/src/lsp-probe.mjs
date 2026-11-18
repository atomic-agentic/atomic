// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { readFile, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import path from 'node:path';

const toolingRoot = fileURLToPath(new URL('../', import.meta.url));
function position(text, offset) {
  const before = text.slice(0, offset).split('\n');
  return { line: before.length - 1, character: before.at(-1).length };
}
export async function probe(file = 'fixtures/draft-reply.atom.astro', options = {}) {
  const root = options.root ?? toolingRoot;
  const workspaceRoot = options.workspaceRoot ?? root;
  const child = spawn(process.execPath, [options.serverPath ?? path.join(root, 'node_modules/@astrojs/language-server/bin/nodeServer.js'), '--stdio'], { cwd: workspaceRoot, stdio: ['pipe', 'pipe', 'pipe'] });
  let sequence = 0, buffer = Buffer.alloc(0), stderr = '';
  const pending = new Map(), notifications = [];
  const send = (message) => {
    if (process.env.LSP_TRACE) console.error('SEND', message.method ?? message.id);
    const body = Buffer.from(JSON.stringify(message));
    child.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
    child.stdin.write(body);
  };
  child.stderr.on('data', (data) => { stderr += data; });
  child.stdout.on('data', (data) => {
    buffer = Buffer.concat([buffer, data]);
    while (true) {
      const end = buffer.indexOf('\r\n\r\n');
      if (end < 0) break;
      const length = Number(/Content-Length: (\d+)/i.exec(buffer.subarray(0, end).toString())?.[1]);
      if (!Number.isFinite(length)) throw new Error('Invalid LSP framing');
      if (buffer.length < end + 4 + length) break;
      const message = JSON.parse(buffer.subarray(end + 4, end + 4 + length));
      if (process.env.LSP_TRACE) console.error('RECV', JSON.stringify(message).slice(0, 600));
      buffer = buffer.subarray(end + 4 + length);
      if (message.method && message.id !== undefined) {
        const result = message.method === 'workspace/configuration' ? message.params.items.map(() => null) : null;
        send({ jsonrpc: '2.0', id: message.id, result });
      } else if (message.id !== undefined) {
        const item = pending.get(message.id);
        if (item) {
          pending.delete(message.id); clearTimeout(item.timer);
          message.error ? item.reject(new Error(JSON.stringify(message.error))) : item.resolve(message.result);
        }
      } else notifications.push(message);
    }
  });
  child.on('exit', (code) => {
    for (const item of pending.values()) { clearTimeout(item.timer); item.reject(new Error(`language server exited ${code}: ${stderr}`)); }
    pending.clear();
  });
  const request = (method, params) => new Promise((resolve, reject) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`LSP timeout: ${method}: ${stderr}`)); }, method === 'shutdown' ? 2000 : 20_000);
    pending.set(id, { resolve, reject, timer });
    send({ jsonrpc: '2.0', id, method, params });
  });
  const notify = (method, params) => send({ jsonrpc: '2.0', method, params });
  try {
    const uri = pathToFileURL(path.resolve(root, file)).href;
    const text = await readFile(new URL(uri), 'utf8');
    const initialized = await request('initialize', {
      processId: null, rootUri: pathToFileURL(workspaceRoot).href,
      workspaceFolders: [{ uri: pathToFileURL(workspaceRoot).href, name: 'atomic-astro-spike' }],
      capabilities: { workspace: { configuration: true, workspaceFolders: true }, textDocument: { diagnostic: { relatedDocumentSupport: true }, hover: { contentFormat: ['markdown', 'plaintext'] }, completion: { completionItem: { snippetSupport: true } } } },
      initializationOptions: { typescript: { tsdk: options.tsdk ?? path.join(root, 'node_modules/typescript/lib') } },
    });
    notify('initialized', {});
    notify('textDocument/didOpen', { textDocument: { uri, languageId: 'astro', version: 1, text } });
    const diagnostics = await request('textDocument/diagnostic', { textDocument: { uri } });
    if (options.diagnosticsOnly) return { diagnostics: diagnostics.items, warnings: notifications.filter((n) => n.method === 'window/showMessage') };
    assert.equal(diagnostics.items.filter((d) => d.severity === 1).length, 0, JSON.stringify(diagnostics));
    const needle = 'input.email';
    const start = text.lastIndexOf(needle);
    const completion = await request('textDocument/completion', { textDocument: { uri }, position: position(text, start + 'input.'.length) });
    const labels = (completion.items ?? completion).map((item) => item.label.replace(/\?$/, ''));
    for (const expected of ['email', 'free_slots', 'feedback']) assert(labels.includes(expected), `Missing completion ${expected}: ${labels}`);
    const hover = await request('textDocument/hover', { textDocument: { uri }, position: position(text, start + 7) });
    assert(JSON.stringify(hover).includes('Email'), `Missing typed hover: ${JSON.stringify(hover)}`);
    const definition = await request('textDocument/definition', { textDocument: { uri }, position: position(text, text.lastIndexOf('<Email') + 2) });
    assert(JSON.stringify(definition).includes('Email.astro'), `Missing component definition: ${JSON.stringify(definition)}`);
    let contracts, members;
    if (text.includes('AtomConfig<')) {
      members = async function (needle, prefix, expected) {
        const offset = text.indexOf(needle);
        assert(offset >= 0, `Missing probe target ${needle}`);
        const result = await request('textDocument/completion', { textDocument: { uri }, position: position(text, offset + prefix.length) });
        const labels = (result.items ?? result).map((item) => item.label.replace(/\?$/, ''));
        for (const label of expected) assert(labels.includes(label), `Missing contract completion ${label}: ${labels}`);
        return labels;
      };
      const meeting = await members('meeting.minutes', 'meeting.', ['title', 'start', 'minutes', ...(options.meetingMember ? [options.meetingMember] : [])]);
      const judge = await members('judge.verdicts', 'judge.', ['verdicts']);
      const verdict = await members('verdict.met', 'verdict.', ['met']);
      const helperHover = await request('textDocument/hover', { textDocument: { uri }, position: position(text, text.indexOf('fitsWithin(meeting') + 3) });
      assert(JSON.stringify(helperHover).includes('TimeSlot'), JSON.stringify(helperHover));
      contracts = { meetingCompletion: meeting, judgeCompletion: judge, verdictCompletion: verdict, helperHover };
    }
    const invalid = text.replace('input.email}', 'input.emali}').replace('tone="brief"', 'tone="casual"');
    notify('textDocument/didChange', { textDocument: { uri, version: 2 }, contentChanges: [{ text: invalid }] });
    const broken = await request('textDocument/diagnostic', { textDocument: { uri } });
    const errors = broken.items.filter((d) => d.severity === 1);
    assert(errors.some((d) => d.message.includes('emali')), JSON.stringify(broken));
    assert(errors.some((d) => d.message.includes('casual')), JSON.stringify(broken));
    const executable = text + '\n{Math.random()}\n';
    notify('textDocument/didChange', { textDocument: { uri, version: 3 }, contentChanges: [{ text: executable }] });
    const acceptedJavaScript = await request('textDocument/diagnostic', { textDocument: { uri } });
    assert.equal(acceptedJavaScript.items.filter((d) => d.severity === 1).length, 0);
    if (contracts) {
      const invalidContracts = text.replace('meeting.minutes > 0', 'meeting.minuttes > 0')
        .replace('verdict => verdict.met', 'verdict => verdict.mett')
        .replace('fitsWithin(meeting, slot)', 'fitsWithin(meeting, slot.start)')
        .replace('criteria: evalCase.criteria', 'criteria: input.email');
      notify('textDocument/didChange', { textDocument: { uri, version: 4 }, contentChanges: [{ text: invalidContracts }] });
      const result = await request('textDocument/diagnostic', { textDocument: { uri } });
      contracts.invalidErrors = result.items.filter((d) => d.severity === 1);
      for (const typo of ['minuttes', 'mett']) assert(contracts.invalidErrors.some((d) => d.message.includes(typo)), JSON.stringify(result));
      assert(contracts.invalidErrors.some((d) => d.code === 2345 && d.message.includes('TimeSlot')), JSON.stringify(result));
      assert(contracts.invalidErrors.some((d) => d.message.includes('string[]')), JSON.stringify(result));
      const invalidScore = text.replace(/score: [\s\S]*?(?=,\n      min_mean:)/, "score: ({ judge }) => 'bad-score'");
      notify('textDocument/didChange', { textDocument: { uri, version: 5 }, contentChanges: [{ text: invalidScore }] });
      const scoreResult = await request('textDocument/diagnostic', { textDocument: { uri } });
      contracts.invalidScoreErrors = scoreResult.items.filter((d) => d.severity === 1);
      assert(contracts.invalidScoreErrors.some((d) => d.code === 2322 && d.message.includes('number')), JSON.stringify(scoreResult));
      if (text.includes('context.config.maxReplyCharacters')) {
        // Restore the authored document before querying its runtime context types.
        notify('textDocument/didChange', { textDocument: { uri, version: 6 }, contentChanges: [{ text }] });
        contracts.contextCompletion = await members('context.config.maxReplyCharacters', 'context.', ['profile', 'config', 'env', 'run', 'project', 'step']);
        contracts.environmentCompletion = await members('context.env.SUPPORT_REGION', 'context.env.', ['SUPPORT_REGION', 'DOCS_SITE']);
        contracts.configurationCompletion = await members('context.config.organizationName', 'context.config.', ['organizationName', 'replyLocale', 'maxReplyCharacters', 'features']);
        const invalidContext = text.replace('context.env.SUPPORT_REGION', 'context.env.UNDECLARED_SECRET')
          .replace('context.config.organizationName', 'context.config.unknownName')
          + "\n{context.profile === 'staging'}\n{context.config.replyLocale = 'changed'}\n";
        notify('textDocument/didChange', { textDocument: { uri, version: 7 }, contentChanges: [{ text: invalidContext }] });
        const result = await request('textDocument/diagnostic', { textDocument: { uri } });
        contracts.invalidContextErrors = result.items.filter((error) => error.severity === 1);
        for (const field of ['UNDECLARED_SECRET', 'unknownName']) assert(contracts.invalidContextErrors.some((error) => error.message.includes(field)), JSON.stringify(result));
        assert(contracts.invalidContextErrors.some((error) => error.code === 2540), JSON.stringify(result));
        assert(contracts.invalidContextErrors.some((error) => error.code === 2367), JSON.stringify(result));
      }
    }
    // This is the negative control: schema/property and component checks must actually run.
    const report = {
      languageServer: '2.17.3', compiler: '4.0.0', typescript: '5.9.3', node: process.version,
      fixture: file,
      workspaceRoot,
      contracts,
      validErrors: diagnostics.items.filter((d) => d.severity === 1), completion: labels,
      hover, definition, invalidErrors: errors, diagnosticProvider: initialized.capabilities.diagnosticProvider,
      warnings: notifications.filter((n) => n.method === 'window/showMessage'),
      unsupportedJavaScriptErrors: acceptedJavaScript.items.filter((d) => d.severity === 1),
      unsupportedJavaScriptSource: '{Math.random()} (rejected separately by atomic subset checker)',
    };
    await mkdir(path.join(root, 'results'), { recursive: true });
    await writeFile(path.join(root, 'results/lsp.json'), JSON.stringify(report, null, 2) + '\n');
    return report;
  } finally {
    try { await request('shutdown', null).catch(() => {}); notify('exit', null); } finally { child.kill(); }
  }
}
if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const report = await probe(process.argv[2]);
  console.log(JSON.stringify(report, null, 2));
}
