// SPDX-License-Identifier: Apache-2.0
import { originalPosition, convertToTSX } from './native-compiler.mjs';
import { FenceError, literalFences } from './fences.mjs';
import { path } from './platform.mjs';

export async function typecheck(sources, declarations, sourcePaths = {}) {
  const files = Object.create(null),
    originals = new Map(),
    maps = new Map(),
    imports = new Map();
  files['/__lib/atomic.d.ts'] =
    'declare namespace JSX { interface Element {} interface IntrinsicElements { [name: string]: any } interface IntrinsicAttributes { slot?: string } }\ndeclare function Fragment(props: {children?: unknown; slot?: string}): JSX.Element;\ndeclare const Astro: {readonly props: Record<string,unknown>; readonly slots: {has(name:string):boolean} };';
  for (const [name, source] of Object.entries({ ...sources, ...declarations })) {
    let file = name,
      code = source;
    if (name.endsWith('.astro')) {
      let fenced;
      try {
        fenced = literalFences(source);
      } catch (e) {
        if (!(e instanceof FenceError)) throw e;
        fenced = source;
      }
      const result = await convertToTSX(fenced, { filename: name });
      file += '.tsx';
      maps.set(file, originalPosition(result, fenced));
      code = result.code.replace('/* @jsxImportSource astro */', (s) => ' '.repeat(s.length));
      // This ambient is emitted by astro2tsx. Rewrite only its generated
      // occurrence, after the component body, outside authored positions.
      const ambient =
        /declare const Astro: Readonly<import\('astro'\)\.AstroGlobal<([^\n]+?), typeof [^;\n]+>>;/g;
      const match = [...code.matchAll(ambient)].at(-1);
      if (match && !maps.get(file)(null, match.index)) {
        code =
          code.slice(0, match.index) +
          'declare const Astro: {readonly props: ' +
          match[1] +
          '; readonly slots: {has(name:string):boolean} };' +
          code.slice(match.index + match[0].length);
      }
    } else if (name.endsWith('.md')) {
      files[name + '?raw.d.ts'] = 'declare const literal: string; export default literal;';
      continue;
    } else if (!name.endsWith('.ts')) continue;
    if (name.endsWith('/.atomic/types/raw-imports.d.ts')) code = '';
    const prepared = globalThis.__atomicTypeScript('prepare', file, code);
    files[file] = prepared.code;
    originals.set(file, code);
    imports.set(file, prepared.imports);
  }
  const diagnostics = [],
    rewrites = new Map();
  function report(file, offset, message) {
    // Translate tsz offsets across rewritten import literals before applying the Astro map.
    for (const edit of [...(rewrites.get(file) ?? [])].reverse()) {
      if (offset >= edit.at + edit.width) offset -= edit.delta;
      else if (offset >= edit.at) offset = edit.at;
    }
    const original = maps.get(file)?.(null, offset);
    const prefix = (originals.get(file) ?? files[file]).slice(0, offset);
    diagnostics.push({
      file: original ? file.slice(0, -4) : file,
      line: original?.line ?? prefix.split('\n').length,
      column: original ? original.column + 1 : prefix.length - prefix.lastIndexOf('\n'),
      message,
    });
  }
  for (const [file, deps] of imports) {
    const edits = [];
    let delta = 0;
    for (const { name, offset, end } of deps) {
      const resolved = path.resolve(path.dirname(file), name);
      const markdown = resolved.endsWith('.md?raw') && resolved.slice(0, -4);
      const base = markdown
        ? (sourcePaths[markdown] ?? markdown) + '?raw'
        : (sourcePaths[resolved] ?? resolved);
      const ext = ['.tsx', '.d.ts', '.ts', ''].find((ext) => Object.hasOwn(files, base + ext));
      if (!name.startsWith('.') || ext === undefined) {
        report(file, offset, `TS2307: Cannot find module '${name}' or its corresponding type declarations.`);
      } else if (base !== resolved) {
        // Keep declaration imports extensionless so value exports resolve through their declarations.
        let relative = path.relative(path.dirname(file), ext === '.d.ts' ? base : base + ext);
        if (!relative.startsWith('.')) relative = './' + relative;
        const literal = JSON.stringify(relative),
          at = offset + delta;
        files[file] = files[file].slice(0, at) + literal + files[file].slice(end + delta);
        const change = literal.length - (end - offset);
        edits.push({ at, width: literal.length, delta: change });
        delta += change;
      }
    }
    rewrites.set(file, edits);
  }
  if (!diagnostics.length)
    for (const d of globalThis.__atomicTypeScript('check', '', JSON.stringify(files))) {
      report(d.file, d.offset, d.message);
      if (diagnostics.length >= 12) break;
    }
  if (diagnostics.length) {
    const error = new Error(diagnostics[0].message);
    throw Object.assign(error, diagnostics[0], { diagnostics: diagnostics.slice(0, 12) });
  }
}
