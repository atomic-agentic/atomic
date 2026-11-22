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
  // mulberry32: multiply with Math.imul and read the answer from the high
  // bits. The linear congruence this replaces kept a 31-bit state in a double
  // and multiplied past 2^53, so the low bits it took its modulus of were
  // correlated with the modulus.
  let seed = 0x9e3779b9;
  const next = (n) => {
    seed = (seed + 0x6d2b79f5) | 0;
    let z = seed;
    z = Math.imul(z ^ (z >>> 15), z | 1);
    z ^= z + Math.imul(z ^ (z >>> 7), z | 61);
    return ((z ^ (z >>> 14)) >>> 0) % n;
  };
  const PROSE = ["Don't delay.", 'say "hi"', 'see https://x.y/z', 'glob src/**/*.ts', 'a < b', 'c > d', '`tick`', "it's", 'règle é', '&amp;', ' '];
  const NAMES = ['section', 'notes', 'rubric', 'Note', 'Card', 'règles', 'b', 'em', 'criteria'];
  const generate = (depth) => {
    let out = '';
    for (let i = 0, n = depth === 0 ? 2 + next(6) : 1 + next(3); i < n; i++) {
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
  const seen = new Set();
  for (let i = 0; i < 1000; i++) {
    const body = generate(0);
    seen.add(body);
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
  // The seed and the grammar together owe this run its variety, and templates
  // that repeat one another read fewer sources than the count claims: two to
  // seven top-level tags hold 977 of the 1000 distinct, where one to three
  // held 763. The floor keeps a run that collapses back onto a handful of
  // shapes from passing quietly.
  assert.ok(seen.size >= 900, `${seen.size} of the 1000 generated templates are distinct`);
});

// A start tag with no `>` before the end of the file: the scan holds the whole
// rest as one start tag, which is not what a parse that leaves the value open
// makes of it, so the tag is refused outright rather than read past.
test('a start tag with no end in the file is refused', () => {
  const source = "<a x={'it\\'s'";
  const found = unbalanced(source, {
    type: 'root',
    children: [{
      type: 'element',
      name: 'a',
      attributes: [{ name: 'x', kind: 'expression', value: "'it\\'s'", position: { start: { offset: 3 } } }],
      children: [],
    }],
  });
  assert.deepEqual(found, {
    line: 1,
    message: 'the start tag of <a> cannot be read: its attribute `x` ends where atomic and Astro disagree about the tag',
  });
});

// A report that cannot be placed in the source says nothing about where the
// tag ends, so the cross-check asks nothing of the scan. The same report,
// placed where the source holds it, is a tag that runs past its own `>` and
// is refused.
test('an attribute report the source does not hold asks nothing of the scan', () => {
  const source = "<a x={'it\\'s'}>t</a>";
  // What the parse would report for a value it never closed: the text to the
  // end of the file, named where its name is written.
  const reported = {
    name: 'x',
    kind: 'expression',
    value: source.slice(6),
    position: { start: { offset: 3 } },
  };
  const ast = (offset) => ({
    type: 'root',
    children: [{
      type: 'element',
      name: 'a',
      attributes: [{ ...reported, position: { start: { offset } } }],
      position: { start: { offset: 0 }, end: { offset: source.length } },
      children: [{ type: 'text', value: 't', position: { start: { offset: 15 }, end: { offset: 16 } } }],
    }],
  });
  assert.deepEqual(unbalanced(source, ast(3)), {
    line: 1,
    message: 'the start tag of <a> cannot be read: its attribute `x` ends where atomic and Astro disagree about the tag',
  });
  // The same report, named at a byte the source does not hold it at.
  assert.equal(unbalanced(source, ast(9)), null);
});

// A closing tag's scan is linear: a long run of `</a` with no `>` after
// it finishes at once.
test('closing tags are scanned in linear time', () => {
  const source = '</a'.repeat(262144 / 3);
  const started = performance.now();
  assert.equal(unbalanced(source, { type: 'root', children: [] }), null);
  assert.ok(performance.now() - started < 1000, `took ${performance.now() - started} ms`);
});
