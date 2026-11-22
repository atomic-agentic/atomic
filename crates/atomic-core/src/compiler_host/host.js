// SPDX-License-Identifier: Apache-2.0
//
// The host the bundled Astro compiler worker runs on: what it needs of Deno,
// Node and the web platform, and nothing else. It runs once, before the
// worker, with deno_core's `Deno.core` still in reach; it takes the host's
// own operations from it and then removes it.
//
// Nothing here reaches the network, the environment, a process or the
// filesystem: the only operations are the protocol's stdin and stdout,
// stderr for the console, the embedded WASM, random bytes, SHA-256, timers
// and the exit status. A file read
// other than the worker's own WASM, any fetch, and every other filesystem
// call is refused with an error.
((wasmUrl) => {
  const core = globalThis.Deno.core;
  const ops = core.ops;
  const read = ops.op_compiler_read, write = ops.op_compiler_write, wasm = ops.op_compiler_wasm;
  const exit = ops.op_compiler_exit, random = ops.op_compiler_random, sha256 = ops.op_compiler_sha256;
  const print = core.print, encode = core.encode, decode = core.decode, createTimer = core.createTimer, cancelTimer = core.cancelTimer;
  const freeze = Object.freeze, define = Object.defineProperty;
  const fixed = (name, value) => define(globalThis, name, { value, writable: false, configurable: false, enumerable: false });
  const refused = (what) => new Error(`the Astro compiler has no ${what}`);

  // deno_core's internals reach its built-in operations; the worker gets
  // none of them. `Deno` is replaced below.
  delete globalThis.__bootstrap;

  const bytes = (input) => {
    if (input === undefined) return new Uint8Array(0);
    if (input instanceof ArrayBuffer) return new Uint8Array(input);
    if (ArrayBuffer.isView(input)) return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
    throw new TypeError('expected an ArrayBuffer or a view of one');
  };

  // Deno: the protocol's stdin and stdout, the exit status, the worker's
  // own WASM. The worker checks the Deno version it was built against;
  // this host implements that version's API for the members it uses.
  fixed('Deno', freeze({
    version: freeze({ deno: '2.9.8' }),
    stdin: freeze({
      readSync(buffer) {
        const n = read(bytes(buffer));
        if (n === -1) return null;
        if (n < 0) throw new Error('cannot read the compiler request');
        return n;
      },
    }),
    stdout: freeze({
      writeSync(buffer) {
        const n = write(bytes(buffer));
        if (n < 0) throw new Error('cannot write the compiler response');
        return n;
      },
    }),
    readFile(file) {
      if (String(file) === wasmUrl) return Promise.resolve(wasm());
      return Promise.reject(refused(`file ${String(file)}: it reads only its own WASM`));
    },
    exit(code) {
      exit(Number.isInteger(code) ? code : 1);
    },
  }));

  // stdout is the protocol, so every console method writes to stderr, which
  // the host reports when the compiler crashes. (deno_core's own console
  // writes errors to stdout.) The worker reassigns console.log, so the
  // object stays writable.
  const show = (value) => {
    if (typeof value === 'string') return value;
    if (value instanceof Error) return value.stack ?? String(value);
    try { return typeof value === 'object' && value !== null ? JSON.stringify(value) : String(value); } catch { return String(value); }
  };
  const log = (...args) => print(`${args.map(show).join(' ')}\n`, true);
  define(globalThis, 'console', {
    value: { log, info: log, warn: log, error: log, debug: log, trace: log },
    writable: true, configurable: true, enumerable: false,
  });

  // UTF-8 only, as the worker and the WASM runtime use them.
  const utf8 = (label) => {
    const name = String(label ?? 'utf-8').trim().toLowerCase();
    if (name !== 'utf-8' && name !== 'utf8' && name !== 'unicode-1-1-utf-8') throw new RangeError(`unsupported encoding ${label}`);
  };
  fixed('TextEncoder', class TextEncoder {
    get encoding() { return 'utf-8'; }
    encode(text = '') { return encode(String(text)); }
  });
  fixed('TextDecoder', class TextDecoder {
    constructor(label, options) {
      utf8(label);
      if (options?.fatal || options?.ignoreBOM) throw new RangeError('unsupported TextDecoder options');
    }
    get encoding() { return 'utf-8'; }
    decode(input, options) {
      if (options?.stream) throw new RangeError('streaming TextDecoder is not supported');
      return decode(bytes(input));
    }
  });

  // URL: absolute URLs with a scheme, and relative references resolved
  // against one (RFC 3986, section 5).
  const shape = /^([a-zA-Z][a-zA-Z0-9+.-]*):(?:\/\/([^/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/;
  const withoutDots = (path) => {
    const out = [];
    const parts = path.split('/');
    for (let i = 0; i < parts.length; i++) {
      const part = parts[i], last = i === parts.length - 1;
      if (part === '.' || part === '..') {
        if (part === '..' && out.length > 1) out.pop();
        if (last) out.push('');
      } else out.push(part);
    }
    return out.join('/');
  };
  const parse = (text) => {
    const match = shape.exec(text);
    if (!match) throw new TypeError(`Invalid URL: ${text}`);
    return { protocol: `${match[1].toLowerCase()}:`, host: match[2], path: match[3], search: match[4] ?? '', hash: match[5] ?? '' };
  };
  const resolve = (reference, base) => {
    if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(reference)) return parse(reference);
    const b = parse(base);
    const r = /^(?:\/\/([^/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/.exec(reference);
    if (r[1] !== undefined) return { protocol: b.protocol, host: r[1], path: withoutDots(r[2]), search: r[3] ?? '', hash: r[4] ?? '' };
    if (r[2] === '') return { ...b, search: r[3] ?? b.search, hash: r[4] ?? '' };
    const merged = r[2].startsWith('/') ? r[2]
      : (b.host !== undefined && b.path === '' ? '/' : b.path.slice(0, b.path.lastIndexOf('/') + 1)) + r[2];
    return { protocol: b.protocol, host: b.host, path: withoutDots(merged), search: r[3] ?? '', hash: r[4] ?? '' };
  };
  fixed('URL', class URL {
    #parts;
    constructor(input, base) {
      this.#parts = base === undefined ? parse(String(input)) : resolve(String(input), String(base));
    }
    get protocol() { return this.#parts.protocol; }
    get host() { return this.#parts.host ?? ''; }
    get hostname() { return this.#parts.host ?? ''; }
    get pathname() { return this.#parts.path; }
    get search() { return this.#parts.search; }
    get hash() { return this.#parts.hash; }
    get href() {
      const p = this.#parts;
      return `${p.protocol}${p.host === undefined ? '' : `//${p.host}`}${p.path}${p.search}${p.hash}`;
    }
    toString() { return this.href; }
    toJSON() { return this.href; }
  });

  // fetch reaches nothing. The worker replaces it with one that answers its
  // own WASM URL, with a Response of the bytes Deno.readFile gave it, and
  // passes every other URL here.
  define(globalThis, 'fetch', { value: (url) => Promise.reject(refused(`network: fetch ${String(url)}`)), writable: true, configurable: false, enumerable: false });
  fixed('Response', class Response {
    #body;
    constructor(body) { this.#body = body === undefined || body === null ? new Uint8Array(0) : bytes(body); }
    arrayBuffer() { return Promise.resolve(this.#body.slice().buffer); }
  });
  // The compiler's loader falls back to instantiate() on fetched bytes; the
  // streaming entry points would need deno_core's fetch machinery.
  delete WebAssembly.instantiateStreaming;
  delete WebAssembly.compileStreaming;

  // The WASM runtime's clock, randomness and timers.
  const origin = Date.now();
  fixed('performance', freeze({ timeOrigin: origin, now: () => Date.now() - origin }));
  fixed('crypto', freeze({
    getRandomValues(view) {
      if (!ArrayBuffer.isView(view) || view instanceof Float32Array || view instanceof Float64Array || view instanceof DataView) {
        throw new TypeError('getRandomValues takes an integer typed array');
      }
      random(bytes(view));
      return view;
    },
  }));
  const timers = new Map();
  let nextTimer = 1;
  fixed('setTimeout', (callback, delay, ...args) => {
    if (typeof callback !== 'function') throw new TypeError('setTimeout takes a function');
    const id = nextTimer++;
    timers.set(id, createTimer(() => { timers.delete(id); callback(...args); }, delay, undefined, false, true));
    return id;
  });
  fixed('clearTimeout', (id) => {
    const timer = timers.get(id);
    if (timer !== undefined) { timers.delete(id); cancelTimer(timer); }
  });

  // The Node modules the worker imports, as `atomic-compiler:node/<name>`
  // modules read from here. No filesystem: the worker replaces readFile and
  // realpath with its own, which read only the sources in its request.
  const fsRefused = (name) => async (file) => { throw refused(`filesystem: ${name} ${String(file)}`); };
  const fs = {};
  for (const name of ['readFile', 'realpath', 'writeFile', 'mkdir', 'readdir', 'rm', 'open', 'link', 'stat', 'lstat', 'access']) fs[name] = fsRefused(name);

  const normalized = (path, aboveRoot) => {
    const out = [];
    for (const part of path.split('/')) {
      if (part === '' || part === '.') continue;
      if (part === '..') {
        if (out.length > 0 && out[out.length - 1] !== '..') out.pop();
        else if (aboveRoot) out.push('..');
      } else out.push(part);
    }
    return out.join('/');
  };
  const text = (value) => {
    if (typeof value !== 'string') throw new TypeError('a path is a string');
    return value;
  };
  const path = {
    sep: '/',
    delimiter: ':',
    isAbsolute: (p) => text(p).startsWith('/'),
    // Only absolute paths: the compiler has no working directory.
    resolve(...args) {
      let joined = '';
      for (let i = args.length - 1; i >= 0; i--) {
        const p = text(args[i]);
        if (p === '') continue;
        joined = `${p}/${joined}`;
        if (p.startsWith('/')) return `/${normalized(joined, false)}`;
      }
      throw refused(`working directory to resolve ${JSON.stringify(args)} against`);
    },
    normalize(p) {
      if (text(p) === '') return '.';
      const absolute = p.startsWith('/'), trailing = p.endsWith('/');
      let out = normalized(p, !absolute);
      if (out === '' && !absolute) out = '.';
      if (out !== '' && trailing) out += '/';
      return absolute ? `/${out}` : out;
    },
    join(...args) {
      const parts = args.map(text).filter((p) => p !== '');
      return parts.length === 0 ? '.' : path.normalize(parts.join('/'));
    },
    relative(from, to) {
      const a = path.resolve(from).split('/').filter(Boolean), b = path.resolve(to).split('/').filter(Boolean);
      let common = 0;
      while (common < a.length && common < b.length && a[common] === b[common]) common++;
      return [...a.slice(common).map(() => '..'), ...b.slice(common)].join('/');
    },
    dirname(p) {
      if (text(p) === '') return '.';
      const root = p.startsWith('/');
      let end = -1, slashes = true;
      for (let i = p.length - 1; i >= 1; i--) {
        if (p[i] === '/') { if (!slashes) { end = i; break; } } else slashes = false;
      }
      if (end === -1) return root ? '/' : '.';
      if (root && end === 1) return '//';
      return p.slice(0, end);
    },
    basename(p, suffix) {
      text(p);
      let start = 0, end = -1, slashes = true;
      for (let i = p.length - 1; i >= 0; i--) {
        if (p[i] === '/') { if (!slashes) { start = i + 1; break; } } else if (end === -1) { slashes = false; end = i + 1; }
      }
      if (end === -1) return '';
      const base = p.slice(start, end);
      return suffix !== undefined && suffix !== base && base.endsWith(text(suffix)) ? base.slice(0, base.length - suffix.length) : base;
    },
    extname(p) {
      const base = path.basename(p);
      const dot = base.lastIndexOf('.');
      return dot <= 0 ? '' : base.slice(dot);
    },
  };
  path.posix = path;
  freeze(path);

  const createHash = (algorithm) => {
    if (algorithm !== 'sha256') throw new Error(`the Astro compiler has only sha256, not ${algorithm}`);
    const chunks = [];
    let done = false;
    const hash = {
      update(data, encoding) {
        if (done) throw new Error('the hash was already digested');
        if (typeof data === 'string') {
          if (encoding !== undefined && encoding !== 'utf8' && encoding !== 'utf-8') throw new Error(`unsupported encoding ${encoding}`);
          chunks.push(encode(data));
        } else chunks.push(bytes(data).slice());
        return hash;
      },
      digest(encoding) {
        if (encoding !== 'hex') throw new Error('the Astro compiler digests only to hex');
        done = true;
        const total = chunks.reduce((n, c) => n + c.length, 0), all = new Uint8Array(total);
        let at = 0;
        for (const c of chunks) { all.set(c, at); at += c.length; }
        return sha256(all);
      },
    };
    return hash;
  };
  const fileURLToPath = (url) => {
    const parsed = typeof url === 'string' ? new globalThis.URL(url) : url;
    if (parsed?.protocol !== 'file:') throw new TypeError('the URL must be of scheme file');
    return decodeURIComponent(parsed.pathname);
  };

  fixed('__atomicCompilerNode', freeze({
    'fs/promises': freeze(fs),
    path,
    crypto: freeze({ createHash }),
    url: freeze({ fileURLToPath }),
  }));
})
