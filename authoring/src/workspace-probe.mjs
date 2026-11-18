// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict';
import { readFile, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import ts from 'typescript';
import { generate } from './generate.mjs';
import { probe } from './lsp-probe.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));
const repository = path.resolve(root, '..');
await generate({ root });
const source = path.join(root, 'fixtures/full/draft-reply.atom.astro');
const resolutions = [];
for (const file of [null, path.join(root, 'tsconfig.json')]) {
  let options = {};
  if (file) {
    const config = ts.readConfigFile(file, ts.sys.readFile);
    assert(!config.error, ts.flattenDiagnosticMessageText(config.error?.messageText ?? '', '\n'));
    const parsed = ts.parseJsonConfigFileContent(config.config, ts.sys, path.dirname(file));
    assert.equal(parsed.errors.length, 0, JSON.stringify(parsed.errors));
    assert(parsed.fileNames.includes(path.join(root, '.atomic/types/draft-reply.d.ts')));
    options = parsed.options;
  }
  for (const specifier of ['../../.atomic/types/draft-reply', '../../.atomic/types/schemas']) {
    const resolution = ts.resolveModuleName(specifier, source, options, ts.sys).resolvedModule;
    assert(resolution, `${file}: failed to resolve ${specifier}`);
    const control = ts.resolveModuleName(specifier + '-missing', source, options, ts.sys).resolvedModule;
    assert.equal(control, undefined, 'missing-module control unexpectedly resolved');
    resolutions.push({ config: file ? path.relative(repository, file) : 'inferred project', specifier, target: path.relative(root, resolution.resolvedFileName) });
  }
}
const files = ['fixtures/draft-reply.atom.astro', 'fixtures/full/draft-reply.atom.astro', 'fixtures/typed/draft-reply.atom.astro', 'fixtures/context/draft-reply.atom.astro'];
const fixtures = [];
for (const file of files) {
  const report = await probe(file, {
    workspaceRoot: repository,
    serverPath: process.env.ASTRO_PROBE_SERVER,
    tsdk: process.env.ASTRO_PROBE_TSDK,
  });
  assert.deepEqual(report.validErrors, []);
  fixtures.push({ file, errors: report.validErrors, negativeCodes: report.invalidErrors.map((error) => error.code), typedContracts: Boolean(report.contracts) });
}
const settings = JSON.parse(await readFile(path.join(repository, '.vscode/settings.json'), 'utf8'));
assert.equal(settings['files.exclude']['**/.atomic/types'], true);
assert.equal(settings['files.exclude']['**/node_modules'], true);
const report = { workspace: 'repository root', resolutions, missingModuleControlFails: true, fixtures, generatedAndDependenciesHidden: true };
await mkdir(path.join(root, 'results'), { recursive: true });
await writeFile(path.join(root, 'results/workspace.json'), JSON.stringify(report, null, 2) + '\n');
console.log(JSON.stringify(report, null, 2));
