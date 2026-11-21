// SPDX-License-Identifier: Apache-2.0
// Parse time per source for each way of running Astro's parsers, and the cost
// of atomic's compiler worker around the parse. Writes results/bench.json.
//
//   node spike-compiler-rs/bench.mjs [--rust path/to/astro-rs-trial] [--deno path/to/deno]
//
// Rows:
//   go-wasm (Node)       @astrojs/compiler 4.0.0 parse(), warm, in Node's V8
//   rs-napi (Node)       @astrojs/compiler-rs 0.5.3 parse(), warm, AST JSON included
//   rs-native            the trial crate (rust/), parse only, no serialisation
//   cold starts          first parse in a fresh process (WASM compile / addon load)
//   worker               atomic's Deno worker per request (process, bundle, WASM, compile)
import { parse as goParse } from '@astrojs/compiler';
import { parse as rsParse } from '@astrojs/compiler-rs';
import { spawnSync } from 'node:child_process';
import { readFileSync, readdirSync, statSync, writeFileSync, mkdirSync, mkdtempSync, copyFileSync, rmSync, existsSync } from 'node:fs';
import { tmpdir, loadavg, cpus } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { literalFences } from '../src/fences.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const authoring = path.dirname(here);
const repo = path.dirname(authoring);
const argv = process.argv.slice(2);
const opt = (name) => { const i = argv.indexOf(name); return i >= 0 ? argv[i + 1] : undefined; };
const rustBin = opt('--rust') ?? path.join(repo, 'target-spike/release/astro-rs-trial');
const deno = opt('--deno') ?? process.env.ATOMIC_DENO ?? 'deno';
const ITER = 200;

function walk(dir, out = []) {
  for (const name of readdirSync(dir)) {
    if (name === 'node_modules' || name.startsWith('target')) continue;
    const file = path.join(dir, name);
    if (statSync(file).isDirectory()) walk(file, out); else if (name.endsWith('.astro')) out.push(file);
  }
  return out;
}
const files = ['examples', 'authoring/fixtures', 'crates'].flatMap((d) => walk(path.join(repo, d))).sort();
const sources = files.map((f) => { const t = readFileSync(f, 'utf8'); try { return literalFences(t); } catch { return t; } });
const median = (xs) => { const s = [...xs].sort((a, b) => a - b); return s[s.length >> 1]; };
const pct = (xs, p) => { const s = [...xs].sort((a, b) => a - b); return s[Math.min(s.length - 1, Math.floor(s.length * p))]; };

async function timeEach(fn) {
  const perFile = [];
  for (const source of sources) {
    for (let i = 0; i < 20; i++) await fn(source);
    const samples = [];
    for (let i = 0; i < ITER; i++) { const t = process.hrtime.bigint(); await fn(source); samples.push(Number(process.hrtime.bigint() - t) / 1e3); }
    perFile.push(median(samples));
  }
  return perFile;
}

const report = { machine: { cpus: cpus().length, loadavg: loadavg() }, files: files.length, bytes: { median: median(sources.map((s) => s.length)), max: Math.max(...sources.map((s) => s.length)) }, rows: {} };
const row = (name, perFileUs, note) => { report.rows[name] = { medianUs: +median(perFileUs).toFixed(1), p90Us: +pct(perFileUs, 0.9).toFixed(1), maxUs: +Math.max(...perFileUs).toFixed(1), note }; };

row('go-wasm (Node)', await timeEach((s) => goParse(s, { position: true })), 'per source, warm');
row('rs-napi (Node)', await timeEach((s) => rsParse(s)), 'per source, warm, includes AST JSON transfer and JSON.parse');

if (existsSync(rustBin)) {
  const r = spawnSync(rustBin, files, { encoding: 'utf8', env: { ...process.env, ITERATIONS: String(ITER) }, maxBuffer: 256 << 20 });
  const lines = r.stdout.trim().split('\n').map((l) => JSON.parse(l));
  row('rs-native', lines.map((l) => l.medianNs / 1e3), 'per source, warm, parse only (no serialisation), trial crate release build');
} else report.rows['rs-native'] = { skipped: `no binary at ${rustBin}` };

// Cold: the first parse in a new process, minus the same process doing nothing.
const coldScript = (mod, call) => `const t0=performance.now();const m=await import('${mod}');const t1=performance.now();await m.${call};const t2=performance.now();console.log(JSON.stringify({importMs:t1-t0,firstParseMs:t2-t1}))`;
function cold(mod, call, runs = 7) {
  const out = [];
  for (let i = 0; i < runs; i++) {
    const r = spawnSync(process.execPath, ['--input-type=module', '-e', coldScript(mod, call)], { cwd: authoring, encoding: 'utf8' });
    out.push(JSON.parse(r.stdout));
  }
  return { importMs: +median(out.map((o) => o.importMs)).toFixed(1), firstParseMs: +median(out.map((o) => o.firstParseMs)).toFixed(1) };
}
report.cold = {
  'go-wasm (Node)': cold('@astrojs/compiler', "parse('---\\nconst a = 1;\\n---\\nhi {a}', {position: true})"),
  'rs-napi (Node)': cold('@astrojs/compiler-rs', "parse('---\\nconst a = 1;\\n---\\nhi {a}')"),
};

// atomic's worker: one Deno process per request, as crates/atomic-core/src/astro.rs runs it.
const version = spawnSync(deno, ['--version'], { encoding: 'utf8' });
if (version.status === 0) {
  const assets = mkdtempSync(path.join(tmpdir(), 'astro-bench-'));
  for (const name of ['worker.mjs', 'astro.wasm']) copyFileSync(path.join(repo, 'crates/atomic-core/assets', name), path.join(assets, name));
  writeFileSync(path.join(assets, 'empty.mjs'), 'console.log("{}")');
  writeFileSync(path.join(assets, 'wasm.mjs'), `const t=performance.now();const b=await Deno.readFile(new URL('./astro.wasm',import.meta.url));const m=await WebAssembly.compile(b);const t1=performance.now();console.log(JSON.stringify({compileMs:t1-t}))`);
  const env = { DENO_NO_UPDATE_CHECK: '1', DENO_DIR: path.join(assets, 'deno-dir'), PATH: process.env.PATH };
  const runDeno = (script, input = '') => {
    const t = process.hrtime.bigint();
    const r = spawnSync(deno, ['run', '--no-config', '--no-lock', '--no-remote', '--cached-only', '--no-npm', '--no-prompt', `--allow-read=${assets}`, path.join(assets, script)], { input, encoding: 'utf8', env, timeout: 60_000 });
    return { ms: Number(process.hrtime.bigint() - t) / 1e6, stdout: r.stdout, stderr: r.stderr };
  };
  const atom = path.join(repo, 'examples/01-classify/atoms/classify-note.atom.astro');
  const request = JSON.stringify({ action: 'load', file: atom, root: path.join(repo, 'examples/01-classify'), sources: { [atom]: readFileSync(atom, 'utf8') }, sourcePaths: { [atom]: atom } });
  runDeno('worker.mjs', request); // first run fills Deno's module cache
  const sample = (fn, runs = 7) => { const xs = []; for (let i = 0; i < runs; i++) xs.push(fn()); return xs; };
  const emptyRuns = sample(() => runDeno('empty.mjs').ms);
  const wasmRuns = sample(() => JSON.parse(runDeno('wasm.mjs').stdout).compileMs);
  const loadRuns = sample(() => runDeno('worker.mjs', request));
  const ok = JSON.parse(loadRuns[0].stdout);
  report.worker = {
    deno: version.stdout.split('\n')[0],
    'empty Deno process (ms)': +median(emptyRuns).toFixed(1),
    'astro.wasm read + WebAssembly.compile in Deno (ms)': +median(wasmRuns).toFixed(1),
    'worker load request, classify-note (ms)': +median(loadRuns.map((r) => r.ms)).toFixed(1),
    loadOk: 'ok' in ok,
  };
  rmSync(assets, { recursive: true, force: true });
} else report.worker = { skipped: `no Deno at ${deno}` };

report.sizes = {
  'astro.wasm (bytes)': statSync(path.join(repo, 'crates/atomic-core/assets/astro.wasm')).size,
  'worker.mjs (bytes)': statSync(path.join(repo, 'crates/atomic-core/assets/worker.mjs')).size,
  'compiler-binding linux-x64-gnu .node (bytes)': (() => { const d = path.join(authoring, 'node_modules/@astrojs/compiler-binding-linux-x64-gnu'); const f = readdirSync(d).find((n) => n.endsWith('.node')); return f ? statSync(path.join(d, f)).size : null; })(),
  'astro2tsx wasm32-wasi (bytes)': statSync(path.join(authoring, 'node_modules/@astrojs/astro2tsx/astro2tsx.wasm32-wasi.wasm')).size,
};
if (existsSync(rustBin)) {
  const baseline = path.join(path.dirname(rustBin), 'baseline');
  report.sizes['trial binary, parser + ESTree serialiser (bytes)'] = statSync(rustBin).size;
  if (existsSync(baseline)) report.sizes['trial binary without parser (bytes)'] = statSync(baseline).size;
}

mkdirSync(path.join(here, 'results'), { recursive: true });
writeFileSync(path.join(here, 'results/bench.json'), JSON.stringify(report, null, 2) + '\n');
console.log(JSON.stringify(report, null, 2));
