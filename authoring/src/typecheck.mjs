// SPDX-License-Identifier: Apache-2.0
import ts from 'typescript';
import { originalPosition } from '#astro-source-map';
import { convertToTSX } from '@astrojs/compiler';
import libs from './typescript-libs.json' with { type: 'json' };
import { FenceError, literalFences } from './fences.mjs';

export async function typecheck(sources, declarations, sourcePaths = {}) {
  const files = new Map(Object.entries(sources));
  const maps = new Map();
  for (const [name, text] of [...files]) if (name.endsWith('.astro')) {
    // An unclosed fence is reported by the renderer, at its line.
    let fenced;
    try { fenced = literalFences(text); } catch (cause) { if (!(cause instanceof FenceError)) throw cause; fenced = text; }
    const result = await convertToTSX(fenced, { filename: name });
    maps.set(`${name}.tsx`, originalPosition(result, fenced));
    files.set(`${name}.tsx`, result.code.replace('/* @jsxImportSource astro */', (text) => ' '.repeat(text.length)));
  }
  for (const [name, text] of Object.entries(libs)) files.set(`/__lib/${name}`, text);
  files.set('/__lib/atomic.d.ts', `
declare module 'astro' { export type AstroGlobal<P, C = unknown, R = unknown> = { readonly props: P; readonly slots: { has(name: string): boolean } }; }
declare namespace JSX { interface Element {} interface IntrinsicElements { [name: string]: any } interface IntrinsicAttributes { slot?: string } }
declare function Fragment(props: {children?: unknown; slot?: string}): JSX.Element;
// A component with no Props sees the ambient Astro global, as in Astro.
declare const Astro: Readonly<import('astro').AstroGlobal<Record<string, unknown>>>;
`);
  for (const [name, text] of Object.entries(declarations)) {
    // Editors need a wildcard for Markdown imports. In a frozen compile only
    // individually captured Markdown resolves through rawDeclaration below.
    files.set(name, name.endsWith('/.atomic/types/raw-imports.d.ts') ? '' : text);
  }
  // A literal Markdown import — `import examples from './examples.md?raw'` — is
  // a string, to the author, to the stock editor and here. Each one the sources
  // ask for gets a module declaration of its own, and only when that Markdown
  // is among the captured sources. The Markdown itself is never read as
  // TypeScript: it is not a program file, and its contents are never parsed.
  const rawModules = new Map();
  const rawDeclaration = (requested) => {
    if (!rawModules.has(requested)) {
      const name = `/__raw/${rawModules.size}.d.ts`;
      files.set(name, 'declare const literal: string;\nexport default literal;\n');
      rawModules.set(requested, name);
    }
    return rawModules.get(requested);
  };
  const options = { strict: true, noEmit: true, target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext, moduleResolution: ts.ModuleResolutionKind.Bundler, jsx: ts.JsxEmit.Preserve, skipLibCheck: true, allowImportingTsExtensions: true };
  const host = {
    getSourceFile: (name, language) => files.has(name) ? ts.createSourceFile(name, files.get(name), language, true, name.endsWith('.tsx') ? ts.ScriptKind.TSX : ts.ScriptKind.TS) : undefined,
    getDefaultLibFileName: () => '/__lib/lib.es2022.d.ts',
    writeFile() {}, getCurrentDirectory: () => '/', getDirectories: () => [],
    fileExists: (name) => files.has(name), readFile: (name) => files.get(name),
    getCanonicalFileName: (name) => name, useCaseSensitiveFileNames: () => true, getNewLine: () => '\n',
    resolveModuleNames: (names, containing) => names.map((specifier) => {
      if (!specifier.startsWith('.')) return undefined;
      const parts = `${containing.slice(0, containing.lastIndexOf('/'))}/${specifier}`.split('/'), out = [];
      for (const part of parts) { if (part === '..') out.pop(); else if (part && part !== '.') out.push(part); }
      const requested = '/' + out.join('/');
      // `?raw` is a query, not part of any file's name: the file behind the
      // import is the Markdown beside it, and what the import holds is its text.
      if (requested.endsWith('.md?raw')) {
        const markdown = requested.slice(0, -'?raw'.length);
        const source = sourcePaths[markdown] ?? markdown;
        if (!files.has(source)) return undefined;
        return { resolvedFileName: rawDeclaration(requested), extension: ts.Extension.Dts, isExternalLibraryImport: false };
      }
      const base = sourcePaths[requested] ?? requested;
      const resolvedFileName = [base + '.tsx', base + '.d.ts', base + '.ts', base].find((name) => files.has(name));
      if (!resolvedFileName) return undefined;
      const extension = resolvedFileName.endsWith('.tsx') ? ts.Extension.Tsx : resolvedFileName.endsWith('.d.ts') ? ts.Extension.Dts : resolvedFileName.endsWith('.ts') ? ts.Extension.Ts : undefined;
      return extension ? { resolvedFileName, extension } : undefined;
    }),
  };
  // Helper modules are checked as TypeScript with the atom and its components.
  const program = ts.createProgram([...files.keys()].filter((name) => name.endsWith('.tsx') || name.endsWith('.ts')), options, host);
  const errors = ts.getPreEmitDiagnostics(program).filter((diagnostic) => diagnostic.category === ts.DiagnosticCategory.Error);
  if (!errors.length) return;
  const diagnostics = errors.slice(0, 12).map((diagnostic) => {
    const at = diagnostic.file?.getLineAndCharacterOfPosition(diagnostic.start ?? 0);
    const original = at && maps.get(diagnostic.file.fileName)?.(at, diagnostic.start ?? 0);
    const file = original?.line ? diagnostic.file.fileName.replace(/\.tsx$/, '') : diagnostic.file?.fileName ?? 'types';
    const line = original?.line ?? (at?.line ?? 0) + 1;
    const column = (original?.column ?? at?.character ?? 0) + 1;
    return { file, line, column, message: `TS${diagnostic.code}: ${ts.flattenDiagnosticMessageText(diagnostic.messageText, ' ')}` };
  });
  // Hosts print one line per diagnostic; the first also locates the error.
  const error = new Error(diagnostics[0].message);
  Object.assign(error, { file: diagnostics[0].file, line: diagnostics[0].line, diagnostics });
  throw error;
}
