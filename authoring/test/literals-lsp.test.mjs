// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { probe } from '../src/lsp-probe.mjs';

const toolingRoot = fileURLToPath(new URL('../', import.meta.url));
const serverPath = path.join(toolingRoot, 'node_modules/@astrojs/language-server/bin/nodeServer.js');
const tsdk = path.join(toolingRoot, 'node_modules/typescript/lib');

test('stock Astro editor keeps literal fences inert and locates a later typo', { timeout: 60_000 }, async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'atomic-literal-lsp-'));
  try {
    // The stock editor resolves Astro's `?raw` module types from the project's
    // own dependencies, just as it does in a real installed authoring project.
    await symlink(path.join(toolingRoot, 'node_modules'), path.join(root, 'node_modules'), 'dir');
    await mkdir(path.join(root, '.atomic/types'), { recursive: true });
    await writeFile(path.join(root, '.atomic/types/literal.d.ts'),
      'export interface Props { input: { text: string }; }\n');
    await writeFile(path.join(root, 'literal-examples.md'),
      '```json\r\n{ "keep": "<tag>", "items": [1, 2] }\r\n```\r\n\r\n' +
      '```ts\r\ntype Wrap<T> = { value: T };\r\nconst make = <T,>(value: T): Wrap<T> => ({ value });\r\n```\r\n');
    const valid = `---\nimport type { Props } from './.atomic/types/literal';\nimport literalExamples from './literal-examples.md?raw';\nconst { input }: Props = Astro.props;\n---\n{literalExamples}\n{input.text}\n`;
    const invalid = valid.replace('{input.text}', '{input.missing}');
    await writeFile(path.join(root, 'valid.atom.astro'), valid);
    await writeFile(path.join(root, 'invalid.atom.astro'), invalid);
    const options = { root, workspaceRoot: root, serverPath, tsdk, diagnosticsOnly: true };
    const clean = await probe('valid.atom.astro', options);
    assert.deepEqual(clean.diagnostics.filter((item) => item.severity === 1), [],
      'literal JSON and TypeScript fences must give no editor errors');
    const broken = await probe('invalid.atom.astro', options);
    const errors = broken.diagnostics.filter((item) => item.severity === 1);
    assert.equal(errors.length, 1, JSON.stringify(errors));
    assert.equal(errors[0].code, 2339, JSON.stringify(errors));
    const before = invalid.slice(0, invalid.indexOf('missing')).split('\n');
    assert.deepEqual(errors[0].range.start,
      { line: before.length - 1, character: before.at(-1).length }, JSON.stringify(errors));
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
