// SPDX-License-Identifier: Apache-2.0
// Tags must balance, as Astro's own parse reads them: a table of templates,
// balanced and not, with the frontmatter an atom has unless a case starts
// with `!`, when it is the whole file.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { parse } from '@astrojs/compiler';
import { unbalanced } from '../src/balance.mjs';
import { literalFences } from '../src/fences.mjs';

const FRONT = "---\nconst { input } = Astro.props;\nconst x = '<section>';\n---\n";
const cases = JSON.parse(await readFile(new URL('./fixtures/balance-cases.json', import.meta.url), 'utf8'));

async function checked(body) {
  const source = literalFences(body.startsWith('!') ? body.slice(1) : FRONT + body);
  const parsed = await parse(source, { position: true });
  return unbalanced(source, parsed.ast);
}

for (const [body, expected] of cases) {
  test(`${expected === null ? 'balanced' : 'refused'}: ${JSON.stringify(body)}`, async () => {
    const found = await checked(body);
    if (expected === null) assert.equal(found, null);
    else assert.ok(found?.message.includes(expected), JSON.stringify(found));
  });
}

test('a refusal names the line of the tag', async () => {
  assert.deepEqual(await checked('<section>\nx\n</notes>'), {
    line: 7,
    message: '`</notes>` closes <section>, opened on line 5; close it with `</section>`',
  });
  assert.equal((await checked('a\n<section>\nx\n')).line, 6);
  assert.equal((await checked('x\n</section>')).line, 6);
});

// Sources built balanced, prose with quotes, slashes and comparisons in
// them, are never refused; one closing tag dropped, renamed or added always
// is.
test('generated templates: balanced pass, and each mutation is refused', async () => {
  let seed = 12345;
  const next = (n) => { seed = (seed * 1103515245 + 12345) % 2147483648; return seed % n; };
  const PROSE = ["Don't delay.", 'say "hi"', 'see https://x.y/z', 'glob src/**/*.ts', 'a < b', 'c > d', '`tick`', "it's", 'règle é', '&amp;', ' '];
  const NAMES = ['section', 'notes', 'rubric', 'Note', 'Card', 'règles', 'b', 'em', 'criteria'];
  const generate = (depth) => {
    let out = '';
    for (let i = 0, n = 1 + next(3); i < n; i++) {
      const k = next(depth > 3 ? 3 : 7);
      if (k === 0) out += PROSE[next(PROSE.length)];
      else if (k === 1) out += '\n';
      else if (k === 2) out += '<br>';
      else if (k < 5) { const t = NAMES[next(NAMES.length)]; out += `<${t}>${generate(depth + 1)}</${t}>`; }
      else if (k === 5) out += `{input.text && <note>${generate(depth + 1)}</note>}`;
      else if (k === 6 && next(2)) out += `<Note title={'it\\'s'} x={"5\\" disk"} y={/* don't */ 1} />`;
      else out += '<Note title="a>b" x={1 > 2} />';
    }
    return out;
  };
  for (let i = 0; i < 1000; i++) {
    const body = generate(0);
    assert.equal(await checked(body), null, body);
    const closes = [...body.matchAll(/<\/[^\t\n\f\r />]+>/g)];
    if (!closes.length) continue;
    const m = closes[next(closes.length)];
    for (const mutated of [
      body.slice(0, m.index) + body.slice(m.index + m[0].length),
      body.slice(0, m.index) + '</zzz>' + body.slice(m.index + m[0].length),
      `${body}\n</section>`,
    ]) assert.notEqual(await checked(mutated), null, mutated);
  }
});
