// SPDX-License-Identifier: Apache-2.0
// The stock Astro editor, run as a test: the declarations `atomic check`
// generates and the ones a person's editor reads are the same files, and the
// per-atom `Config` alias is what tells the editor whether an atom has a public
// context. `npm run probe:config` prints the report this test asserts.
import test from 'node:test';
import { generate } from '../src/generate.mjs';
import { configProbe } from '../src/config-probe.mjs';
const root = new URL('../', import.meta.url);

test('the stock editor reads the per-atom Config: clean atom, one error for a misspelled field', async () => {
  await generate({ root: root.pathname });
  const report = await configProbe({ root: root.pathname });
  const misspelled = JSON.stringify(report.misspelled);
  if (!misspelled.includes('bodyy')) throw new Error(`the misspelled field is not what was reported: ${misspelled}`);
});
