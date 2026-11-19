// SPDX-License-Identifier: Apache-2.0
import { createHash } from 'node:crypto';
import { build } from 'esbuild';
import { readFile, readdir, mkdir, copyFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
const root = fileURLToPath(new URL('../', import.meta.url));
const libs = {};
for (const name of await readdir(`${root}/node_modules/typescript/lib`)) if (/^lib\..*\.d\.ts$/.test(name)) libs[name] = await readFile(`${root}/node_modules/typescript/lib/${name}`, 'utf8');
await writeFile(`${root}/src/typescript-libs.json`, JSON.stringify(libs));
const out = `${root}/../crates/atomic-core/assets`;
await mkdir(out, { recursive: true });
await build({ entryPoints: [`${root}/src/worker.mjs`], outfile: `${out}/worker.mjs`, bundle: true, minify: true, format: 'esm', platform: 'browser', target: 'es2022', conditions: ['browser'], external: ['node:*'], legalComments: 'external', logLevel: 'warning' });
await copyFile(`${root}/node_modules/@astrojs/compiler/dist/astro.wasm`, `${out}/astro.wasm`);
await copyFile(`${root}/node_modules/typescript/LICENSE.txt`, `${out}/TYPESCRIPT-LICENSE.txt`);
await copyFile(`${root}/node_modules/@astrojs/compiler/LICENSE`, `${out}/ASTRO-LICENSE`);
await copyFile(`${root}/node_modules/source-map-js/LICENSE`, `${out}/SOURCE-MAP-LICENSE`);
console.log('Built bundled Astro compiler, TypeScript checker and WASM assets.');

const sourceNames = ['src/build-worker.mjs','src/worker.mjs','src/renderer.mjs','src/typecheck.mjs','src/platform.mjs','src/context.mjs','src/generate.mjs','src/intrinsics.mjs','authoring.d.ts','package.json','package-lock.json'];
const hash = async (file) => createHash('sha256').update(await readFile(file)).digest('hex');
const manifest = { version: 1, sources: {}, assets: {}, dependencies: { '@astrojs/compiler': '4.0.0', typescript: '5.9.3', 'source-map-js': '1.2.2' } };
for (const name of sourceNames) manifest.sources[`authoring/${name}`] = await hash(`${root}/${name}`);
for (const name of ['worker.mjs','worker.mjs.LEGAL.txt','astro.wasm','TYPESCRIPT-LICENSE.txt','ASTRO-LICENSE','SOURCE-MAP-LICENSE']) manifest.assets[name] = await hash(`${out}/${name}`);
await writeFile(`${out}/manifest.json`, JSON.stringify(manifest, null, 2) + '\n');
