// SPDX-License-Identifier: Apache-2.0
import { compile, render } from './renderer.mjs';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
const file = path.resolve(process.argv[2] ?? 'fixtures/draft-reply.atom.astro');
const input = JSON.parse(await readFile(process.argv[3] ?? 'fixtures/input.json', 'utf8'));
const unit = await compile(file, { root: process.cwd() });
console.log(JSON.stringify({ config: unit.config, ...render(unit, input) }, null, 2));
