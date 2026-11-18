// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict';
import { mkdtemp, cp, readFile, writeFile, rm, mkdir } from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { generate } from './generate.mjs';
import { probe } from './lsp-probe.mjs';
const root = fileURLToPath(new URL('../', import.meta.url));
const temp = await mkdtemp(path.join(os.tmpdir(), 'atomic-inferred-editor-'));
try {
  // Deliberately no tsconfig, package.json, Astro config or node_modules here.
  for (const name of ['schemas', 'fixtures', 'authoring.d.ts']) await cp(path.join(root, name), path.join(temp, name), { recursive: true });
  await generate({ root: temp });
  const serverPath = process.env.ASTRO_PROBE_SERVER ?? path.join(root, 'node_modules/@astrojs/language-server/bin/nodeServer.js');
  const tsdk = process.env.ASTRO_PROBE_TSDK ?? path.join(root, 'node_modules/typescript/lib');
  const options = { root: temp, serverPath, tsdk };
  const files = ['fixtures/draft-reply.atom.astro', 'fixtures/full/draft-reply.atom.astro', 'fixtures/typed/draft-reply.atom.astro', 'fixtures/context/draft-reply.atom.astro'];
  const fixtures = [];
  for (const file of files) {
    const target = path.join(temp, file), original = await readFile(target, 'utf8');
    const relative = path.relative(path.dirname(target), path.join(temp, '.atomic/types/draft-reply')).replaceAll(path.sep, '/');
    assert(original.includes(`from '${relative}'`), `fixture must import its real generated declaration: ${file}`);
    // Reproduce the actual TS2307 class even after authored fixtures are fixed.
    await writeFile(target, original.replaceAll(relative, 'atomic:atoms/draft-reply'));
    const before = await probe(file, { ...options, diagnosticsOnly: true });
    assert(before.diagnostics.some((error) => error.code === 2307 && error.message.includes('atomic:atoms/draft-reply')), JSON.stringify(before));
    await writeFile(target, original);
    const result = await probe(file, options);
    fixtures.push({ file, aliasErrorReproduced: 2307, errors: result.validErrors, negativeCodes: result.invalidErrors.map((error) => error.code), typedContracts: Boolean(result.contracts) });
  }
  const report = { inferredProject: true, tsconfigPresent: false, nodeModulesPresent: false, fixtures };
  await mkdir(path.join(root, 'results'), { recursive: true });
  await writeFile(path.join(root, 'results/inferred.json'), JSON.stringify(report, null, 2) + '\n');
  console.log(JSON.stringify(report, null, 2));
} finally { await rm(temp, { recursive: true, force: true }); }
