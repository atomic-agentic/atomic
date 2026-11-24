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
const es2022 = {};
function addLib(name) {
  const path = '/__lib/' + name;
  if (path in es2022) return;
  es2022[path] = libs[name];
  for (const match of libs[name].matchAll(/<reference lib="([^"]+)"/g)) addLib('lib.' + match[1] + '.d.ts');
}
addLib('lib.es2022.d.ts');
await writeFile(out + '/typescript-es2022.json', JSON.stringify(es2022));
await build({ entryPoints: [`${root}/src/worker.mjs`], outfile: `${out}/worker.mjs`, bundle: true, plugins: [{ name: 'native-typescript', setup(build) { build.onResolve({filter: /^\.\/typecheck\.mjs$/}, () => ({path: root + 'src/native-typecheck.mjs'})); } }], alias: { typescript: root + 'src/native-typescript.mjs', '@astrojs/compiler': `${root}/src/native-compiler.mjs`, '#astro-source-map': `${root}/src/native-compiler.mjs` }, minify: true, format: 'esm', platform: 'browser', target: 'es2022', conditions: ['browser'], external: ['node:*'], legalComments: 'external', logLevel: 'warning' });
await copyFile(`${root}/node_modules/source-map-js/LICENSE`, `${out}/SOURCE-MAP-LICENSE`);
await copyFile(`${root}/node_modules/typescript/LICENSE.txt`, `${out}/TYPESCRIPT-LICENSE.txt`);
console.log('Built the authoring worker for the native Astro compiler.');

const sourceNames = ['src/build-worker.mjs','src/native-typescript.mjs','src/native-typecheck.mjs','src/native-compiler.mjs','src/worker.mjs','src/renderer.mjs','src/typecheck.mjs','src/platform.mjs','src/context.mjs','src/generate.mjs','src/intrinsics.mjs','src/capture.mjs','src/fences.mjs','src/balance.mjs','authoring.d.ts','raw-imports.d.ts','package.json','package-lock.json'];
const hash = async (file) => createHash('sha256').update(await readFile(file)).digest('hex');
const manifest = { version: 1, sources: {}, assets: {}, dependencies: { typescript: '5.9.3', 'source-map-js': '1.2.2' } };
for (const name of sourceNames) manifest.sources[`authoring/${name}`] = await hash(`${root}/${name}`);
for (const name of ['typescript-es2022.json','worker.mjs','worker.mjs.LEGAL.txt','TYPESCRIPT-LICENSE.txt','SOURCE-MAP-LICENSE','ASTRO-RUST-LICENSE.txt']) manifest.assets[name] = await hash(`${out}/${name}`);
await writeFile(`${out}/manifest.json`, JSON.stringify(manifest, null, 2) + '\n');
