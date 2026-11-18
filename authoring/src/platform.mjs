// SPDX-License-Identifier: Apache-2.0
// The trusted compiler can run on Node or on explicit embedded-host services.
const platform = globalThis.__atomicPlatform ?? {
  ...await import('node:fs/promises'),
  path: (await import('node:path')).default,
  createHash: (await import('node:crypto')).createHash,
  fileURLToPath: (await import('node:url')).fileURLToPath,
};
export const { readFile, realpath, writeFile, mkdir, readdir, rm, open, link, path, createHash, fileURLToPath } = platform;
