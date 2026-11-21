// SPDX-License-Identifier: Apache-2.0
// `node --import ./spike-compiler-rs/rs-hook.mjs …` makes every import of
// @astrojs/compiler outside this directory resolve to rs-as-go.mjs, so the
// renderer and its tests run unchanged on compiler-rs's parse.
import { registerHooks } from 'node:module';

const here = new URL('./', import.meta.url).href;
const adapter = new URL('./rs-as-go.mjs', import.meta.url).href;
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === '@astrojs/compiler' && !context.parentURL?.startsWith(here)) return { url: adapter, shortCircuit: true, format: 'module' };
    return next(specifier, context);
  },
});
