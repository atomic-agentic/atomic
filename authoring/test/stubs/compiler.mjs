// SPDX-License-Identifier: Apache-2.0
// The Astro compiler, absent: `test/context-boundary-probe.mjs` exercises the
// guards that refuse *before* anything is parsed, so the modules the compiler
// and TypeScript would provide are replaced by these three files and nothing is
// parsed. They are test-only: the bundled worker imports the real packages, and
// no source under `src/` imports these.
export function initialize() {}
export async function parse() { throw new Error('the stub parses nothing'); }
export async function transform() { throw new Error('the stub transforms nothing'); }
export default { initialize, parse, transform };
