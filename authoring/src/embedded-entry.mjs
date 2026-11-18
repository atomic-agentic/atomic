// SPDX-License-Identifier: Apache-2.0
import { initialize } from '@astrojs/compiler';

const root = globalThis.__atomicRoot;
// Small POSIX path adapter for this Linux-only embedding experiment.
function resolve(...parts) {
  let value = root;
  for (const part of parts) value = part.startsWith('/') ? part : `${value}/${part}`;
  const segments = [];
  for (const part of value.split('/')) {
    if (part === '..') segments.pop(); else if (part && part !== '.') segments.push(part);
  }
  return '/' + segments.join('/');
}
const path = {
  sep: '/', resolve, join: (...parts) => resolve(...parts),
  dirname: (file) => file.slice(0, file.lastIndexOf('/')) || '/',
  basename: (file, suffix = '') => { const name = file.slice(file.lastIndexOf('/') + 1); return suffix && name.endsWith(suffix) ? name.slice(0, -suffix.length) : name; },
};
function io(fn) { return async (...args) => { try { return await fn(...args); } catch (error) { if (error instanceof Deno.errors.NotFound) error.code = 'ENOENT'; if (error instanceof Deno.errors.AlreadyExists) error.code = 'EEXIST'; throw error; } }; }
globalThis.__atomicPlatform = {
  path,
  readFile: io(async (file, encoding) => encoding ? Deno.readTextFile(file) : Deno.readFile(file)),
  realpath: io((file) => Deno.realPath(file)),
  writeFile: io((file, data) => typeof data === 'string' ? Deno.writeTextFile(file, data) : Deno.writeFile(file, data)),
  mkdir: io((file, options) => Deno.mkdir(file, options)),
  link: io((from, to) => Deno.link(from, to)),
  open: io(async (file, flags, mode) => {
    const handle = await Deno.open(file, flags === 'wx' ? { write: true, createNew: true, mode } : { read: true });
    return {
      async writeFile(text) { const bytes = new TextEncoder().encode(text); let written = 0; while (written < bytes.length) written += await handle.write(bytes.subarray(written)); },
      sync: () => handle.sync(), close: () => handle.close(),
    };
  }),
  rm: io(async (file, options) => { try { await Deno.remove(file, { recursive: options?.recursive }); } catch (error) { if (!(options?.force && error instanceof Deno.errors.NotFound)) throw error; } }),
  readdir: io(async (file) => { const names = []; for await (const entry of Deno.readDir(file)) names.push(entry.name); return names; }),
  fileURLToPath: (value) => decodeURIComponent(new URL(value).pathname),
  createHash: (algorithm) => {
    if (algorithm !== 'sha256') throw new Error('unsupported hash');
    let text = '';
    return { update(value) { text += value; return this; }, digest(format) { if (format !== 'hex') throw new Error('unsupported encoding'); return globalThis.__atomicHash(text); } };
  },
};
const originalFetch = globalThis.fetch;
globalThis.fetch = async (url, options) => String(url) === 'atomic:compiler-wasm'
  ? new Response(await Deno.readFile(`${root}/.atomic/astro.wasm`), { headers: { 'Content-Type': 'application/wasm' } })
  : originalFetch(url, options);
await initialize({ wasmURL: 'atomic:compiler-wasm' });
const { pipelineReport } = await import('./pipeline-report.mjs');
const pipeline = await pipelineReport(root, globalThis.__atomicStateRoot);

async function denied(label, action) {
  try { await action(); } catch (error) { if (error.name === 'NotCapable') return label; throw error; }
  throw new Error(`permission control did not deny ${label}`);
}
const denials = [];
denials.push(await denied('read outside project', () => Deno.readTextFile('/etc/hosts')));
denials.push(await denied('write outside generated directory', () => Deno.writeTextFile(`${root}/forbidden-embedded-write`, 'denied')));
denials.push(await denied('environment', () => Deno.env.get('ATOMIC_EMBED_SECRET')));
denials.push(await denied('network', () => fetch('http://127.0.0.1:43199')));
denials.push(await denied('subprocess', () => new Deno.Command('/bin/true').output()));
denials.push(await denied('ffi', () => Deno.dlopen('/not-a-library', {})));
if (Deno.pid !== globalThis.__atomicHostPid) throw new Error('runtime is not in the host process');
console.log(JSON.stringify({ runtime: { api: 'deno_runtime::MainWorker', hostPid: globalThis.__atomicHostPid, denoPid: Deno.pid, sameProcess: true, version: Deno.version }, denials, pipeline }));
