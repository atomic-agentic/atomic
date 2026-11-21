// SPDX-License-Identifier: Apache-2.0
// @astrojs/compiler with parse() recording its input (see record-hook.mjs).
import { appendFileSync } from 'node:fs';
import * as real from '@astrojs/compiler';

export * from '@astrojs/compiler';
export async function parse(source, options) {
  if (process.env.SPIKE_RECORD) appendFileSync(process.env.SPIKE_RECORD, JSON.stringify({ source }) + '\n');
  return real.parse(source, options);
}
