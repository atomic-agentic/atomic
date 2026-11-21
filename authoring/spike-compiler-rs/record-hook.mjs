// SPDX-License-Identifier: Apache-2.0
// `node --import ./spike-compiler-rs/record-hook.mjs --test test/*.test.mjs`
// runs the authoring tests unchanged and appends every source they hand to
// @astrojs/compiler's parse() to $SPIKE_RECORD (JSON lines), so the parity
// harness can replay the test inputs.
import { registerHooks } from 'node:module';

const recorder = new URL('./record-compiler.mjs', import.meta.url).href;
registerHooks({
  resolve(specifier, context, next) {
    if (specifier === '@astrojs/compiler' && context.parentURL !== recorder) return { url: recorder, shortCircuit: true, format: 'module' };
    return next(specifier, context);
  },
});
