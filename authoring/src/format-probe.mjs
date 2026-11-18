// SPDX-License-Identifier: Apache-2.0
import { format } from 'prettier';
import * as astro from 'prettier-plugin-astro';
import { mkdtemp, cp, readFile, writeFile, rm, mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import os from 'node:os';
import path from 'node:path';
import { compile, render } from './renderer.mjs';

const root = fileURLToPath(new URL('../', import.meta.url));
const temp = await mkdtemp(path.join(os.tmpdir(), 'atomic-astro-format-'));
try {
  await cp(path.join(root, 'fixtures'), temp, { recursive: true });
  const input = JSON.parse(await readFile(path.join(temp, 'input.json'), 'utf8'));
  const entry = path.join(temp, 'draft-reply.atom.astro');
  const before = render(await compile(entry, { root: temp }), input);
  for (const name of ['draft-reply.atom.astro', 'triage-email.atom.astro', 'components/Email.astro', 'components/HouseStyle.astro']) {
    const target = path.join(temp, name);
    const formatted = await format(await readFile(target, 'utf8'), { parser: 'astro', plugins: [astro] });
    await writeFile(target, formatted);
  }
  const after = render(await compile(entry, { root: temp }), input);
  const report = {
    prettier: '3.10.1', plugin: '1.1.2',
    developerIdentical: before.developer === after.developer,
    userIdentical: before.user === after.user,
    beforeDeveloper: before.developer, afterDeveloper: after.developer,
  };
  await mkdir(path.join(root, 'results'), { recursive: true });
  await writeFile(path.join(root, 'results/format.json'), JSON.stringify(report, null, 2) + '\n');
  console.log(JSON.stringify(report, null, 2));
} finally { await rm(temp, { recursive: true, force: true }); }
