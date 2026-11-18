// SPDX-License-Identifier: Apache-2.0
import { build } from 'esbuild';
import { mkdir, copyFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
const root = fileURLToPath(new URL('../', import.meta.url));
await mkdir(path.join(root, '.atomic'), { recursive: true });
await build({ entryPoints: [path.join(root, 'src/embedded-entry.mjs')], outfile: path.join(root, '.atomic/embedded.mjs'), bundle: true, format: 'esm', platform: 'browser', target: 'es2022', conditions: ['browser'], external: ['node:*'], logLevel: 'warning' });
await copyFile(path.join(root, 'node_modules/@astrojs/compiler/dist/astro.wasm'), path.join(root, '.atomic/astro.wasm'));
console.log('Built hidden .atomic/embedded.mjs and compiler WASM.');
