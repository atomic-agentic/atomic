#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// The example at the boundary a person uses it: the built `atomic` checks the
// project and renders its prompts. No model is called and no endpoint answers —
// the harness points every endpoint at a port nothing listens on, so a run that
// tried to reach one would fail here rather than somewhere else.
//
// Each `switch off …` case edits one thing in a private copy of the project and
// shows a named assertion failing, which is how a guard here is known to be a
// guard rather than a sentence.
//
//   node --test examples/13-review-evidence/*.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { bindings, lines, projectFixture, render, scratchRoot } from './tools/cli-harness.mjs';

const EVIDENCE_ATOM = 'atoms/review-evidence.atom.astro';
const PLAN_ATOM = 'atoms/review-plan.atom.astro';
const PLAIN = 'fixtures/review-evidence/current-pass-omitted-by-author-plain.input.json';
const MARKDOWN = 'fixtures/review-evidence/current-pass-omitted-by-author-markdown.input.json';
const SEALED = 'fixtures/review-evidence/inaccessible-pass-for-this-revision.input.json';
const UNAVAILABLE = 'fixtures/review-evidence/unavailable-scan-with-zero-findings.input.json';
const EMPTY_RECORDS = 'fixtures/review-plan/plan-row-without-any-record.input.json';

const PHRASE = 'this candidate revision';

/** Run one test against its own copy of the project, removing it at the end. */
async function withProject(label, body) {
  const project = projectFixture(label);
  try {
    await body(project);
  } finally {
    project.finish();
  }
}

/** The bindings that are not the display dialect, for comparing a pair. */
function dataBindings(turn) {
  return bindings(turn).filter((binding) => !binding.path.startsWith('input.display.'));
}

function asterisks(text) {
  return (text.match(/\*/g) ?? []).length;
}

function names(directory) {
  if (!existsSync(directory)) return [];
  return readdirSync(directory).sort();
}

test('atomic check accepts the example as it is authored', async () => {
  await withProject('check', (project) => {
    const run = project.atomic(['check', '--json']);
    assert.equal(run.status, 0, `atomic check --json: ${run.stderr || run.stdout}`);
    const reported = JSON.parse(run.stdout);
    assert.ok(reported.checked, `atomic check --json reported no counts: ${JSON.stringify(reported)}`);
    const text = JSON.stringify(reported.checked);
    for (const count of ['atoms', 'workflows']) {
      assert.ok(Object.hasOwn(reported.checked, count), `atomic check --json reported ${JSON.stringify(reported)}`);
    }
    assert.ok(reported.checked.atoms >= 2, `atomic checked ${text}, which is fewer than the two atoms`);
  });
});

test('atomic check refuses an answer shape the contracts do not state', async () => {
  await withProject('check-output-shape', (project) => {
    const schema = 'atoms/review-evidence.output.schema.yaml';
    const key = '        state:\n          type: string\n          enum: [record_current_pass';
    // `oneOf` is a keyword the generation profile refuses, so the contract is
    // checked here rather than only read — and the path that fails is named.
    project.replace(
      schema,
      key,
      '        state:\n          type: string\n          oneOf: [{const: record_missing}, {const: nonsense_state}]\n          enum: [record_current_pass',
    );
    const run = project.atomic(['check']);
    const said = `${run.stderr}\n${run.stdout}`;
    assert.notEqual(run.status, 0, `atomic check accepted an output schema the profile cannot lower: ${said}`);
    assert.match(said, /cannot be lowered/, `atomic check refused for an unexpected reason: ${said}`);
    assert.match(said, /`oneOf` is not supported/, `atomic check did not name the keyword it refused: ${said}`);
    assert.match(
      said,
      /\/properties\/rows\/items\/properties\/state/,
      `atomic check did not name the schema path that fails: ${said}`,
    );
  });
});

test('atomic render refuses a host record carrying a pre-decided state', async () => {
  await withProject('render-record-field', (project) => {
    const fixture = UNAVAILABLE;
    const input = JSON.parse(readFileSync(join(project.dir, fixture), 'utf8'));
    input.host_records[0].justified_state = 'satisfied';
    project.write('justified-state.input.json', `${JSON.stringify(input, null, 2)}\n`);
    // The shared execution-record schema allows no such field, and it is the
    // same reference both Atoms take, so this is the record contract biting on
    // the path a run takes — not a rule stated only in a document.
    const run = project.atomic(['render', EVIDENCE_ATOM, '--input', 'justified-state.input.json']);
    const said = `${run.stderr}\n${run.stdout}`;
    assert.notEqual(run.status, 0, `atomic render accepted a record field no schema states: ${said}`);
    assert.match(said, /justified_state/, `atomic render did not name the field it refused: ${said}`);
  });
});

test('the Markdown dialect is what draws the heading, the link and the bold', async () => {
  await withProject('render-pair', (project) => {
    const plain = render(project, EVIDENCE_ATOM, PLAIN);
    const markdown = render(project, EVIDENCE_ATOM, MARKDOWN);

    assert.ok(
      lines(markdown.developer).some((line) => line.startsWith('## Required criteria')),
      'the Markdown render drew no heading for the required criteria',
    );
    assert.ok(
      lines(plain.developer).some((line) => line.trim() === 'Required criteria'),
      'the plain render does not state the same section without a heading',
    );
    assert.ok(!lines(plain.developer).some((line) => line.startsWith('## ')), 'the plain render drew a Markdown heading');

    assert.ok(markdown.developer.includes('[open the record]('), 'the Markdown render drew no record link');
    assert.ok(!plain.developer.includes(']('), 'the plain render drew a Markdown link');

    assert.ok(markdown.developer.includes(`**${PHRASE}**`), 'the Markdown render drew no bold');
    assert.ok(plain.developer.includes(PHRASE), 'the plain render lost the emphasized phrase');
    assert.ok(!plain.developer.includes(`**${PHRASE}**`), 'the plain render bolded the phrase');

    // What matters about the pair: the records each side can reach, and how
    // open each one is.
    const plainData = dataBindings(plain.user);
    const markdownData = dataBindings(markdown.user);
    assert.deepEqual(plainData, markdownData, 'the pair does not hand the reviewer the same records');

    const fixtureIds = JSON.parse(readFileSync(join(project.dir, PLAIN), 'utf8')).host_records.map((record) => record.id);
    const offered = plainData.filter((binding) => fixtureIds.includes(binding.value));
    assert.deepEqual(
      [...new Set(offered.map((binding) => binding.value))].sort(),
      [...fixtureIds].sort(),
      `the record IDs the prompt offers are not the fixture IDs ${JSON.stringify(fixtureIds)}`,
    );
    assert.ok(offered.length > 0, 'no record ID reached the prompt as data');

    const access = plainData.filter((binding) => binding.path.endsWith('.access'));
    assert.ok(access.length > 0, 'no access field reached the prompt as data');
    assert.deepEqual(
      access.map((binding) => binding.value),
      access.map(() => 'grant'),
      'a record in the current-pass pair is not openable, so the pair proves nothing',
    );

    // Provenance travels as data, never as a trust level the CLI hands out:
    // every binding of a passed input is `external`, and the record's own
    // `origin` field is what says a host wrote it.
    assert.deepEqual(
      [...new Set(plainData.map((binding) => binding.origin))],
      ['external'],
      'a value from the input reached the prompt marked as more trusted than external',
    );
    assert.deepEqual(
      [...new Set(plainData.filter((binding) => binding.path.endsWith('.origin')).map((binding) => binding.value))].sort(),
      ['author', 'host'],
      'the record and claim bindings do not carry the origins the fixture gives them',
    );
  });
});

test('the emphasis triple changes only its delimiters', async () => {
  await withProject('render-emphasis', (project) => {
    const base = JSON.parse(readFileSync(join(project.dir, PLAIN), 'utf8'));
    const seen = {};
    for (const emphasis of ['plain', 'italic', 'bold']) {
      const path = `emphasis-${emphasis}.input.json`;
      project.write(path, JSON.stringify({ ...base, display: { ...base.display, emphasis } }, null, 2));
      seen[emphasis] = render(project, EVIDENCE_ATOM, path);
    }
    assert.ok(seen.plain.developer.includes(PHRASE), 'the plain dialect lost the emphasized phrase');
    assert.ok(seen.italic.developer.includes(`*${PHRASE}*`), 'the italic dialect drew no italic');
    assert.ok(seen.bold.developer.includes(`**${PHRASE}**`), 'the bold dialect drew no bold');
    // Every render gets its own data-tag nonces, so the turns are compared
    // with the nonces and the delimiters taken out: what is left must match.
    const skeleton = (turn) => turn.replace(/data-[0-9a-f]{16}/g, 'data-<nonce>').replace(/\*/g, '');
    assert.equal(skeleton(seen.italic.developer), skeleton(seen.plain.developer), 'italic changed more than the delimiters');
    assert.equal(skeleton(seen.bold.developer), skeleton(seen.plain.developer), 'bold changed more than the delimiters');
    // Two places carry the phrase — the candidate line and the credit rule —
    // so italic writes four delimiters and bold eight, and nothing else does.
    assert.equal(asterisks(seen.plain.developer), 0, 'the plain dialect wrote a delimiter');
    assert.equal(asterisks(seen.italic.developer), 4, 'the italic dialect wrote the wrong number of delimiters');
    assert.equal(asterisks(seen.bold.developer), 8, 'the bold dialect wrote the wrong number of delimiters');
    assert.deepEqual(dataBindings(seen.italic.user), dataBindings(seen.plain.user));
    assert.deepEqual(dataBindings(seen.bold.user), dataBindings(seen.plain.user));
  });
});

test('the prompt states the decision rules, and never a verdict of its own', async () => {
  await withProject('render-rules', (project) => {
    const evidenceTurn = render(project, EVIDENCE_ATOM, UNAVAILABLE);
    assert.match(evidenceTurn.developer, /A scan that could not run lists no findings/);
    assert.match(evidenceTurn.developer, /Required and empty is\s*`unassessable`, never `satisfied`/);
    assert.match(evidenceTurn.developer, /Credit a passing check only when it ran against/);
    assert.match(evidenceTurn.developer, /If the only pass belongs to an earlier revision, report missing evidence/);
    assert.match(evidenceTurn.developer, /A record you cannot open credits nothing/);
    assert.match(evidenceTurn.developer, /You recommend\. You never approve a gate/);

    const planTurn = render(project, PLAN_ATOM, EMPTY_RECORDS);
    assert.match(planTurn.developer, /A scan that could not run lists no findings/);
    assert.match(planTurn.developer, /never `covered` and satisfied/);
    assert.match(planTurn.developer, /An outcome no task covers is `uncovered`/);
    assert.match(planTurn.developer, /You never approve a gate/);

    // The revision the rules are read against arrives as data, not prose.
    const bound = dataBindings(evidenceTurn.user).find((binding) => binding.path === 'input.candidate.revision');
    assert.ok(bound, 'the candidate revision never reached the prompt as data');
    assert.match(bound.value, /^[0-9a-f]{40}$/);
  });
});

test('a record that cannot be opened is shown as sealed, and its contents are not', async () => {
  await withProject('render-sealed', (project) => {
    const sealedTurn = render(project, EVIDENCE_ATOM, SEALED);
    const sealed = dataBindings(sealedTurn.user);
    const access = sealed.find((binding) => binding.path.endsWith('.access'));
    assert.ok(access, 'the access field never reached the prompt');
    assert.equal(access.value, 'none');
    // `atomic render` never marks a passed input as more trusted than
    // `external`; the record saying a host wrote it is a value of its own.
    assert.equal(access.origin, 'external');
    assert.deepEqual(
      [...new Set(sealed.filter((binding) => binding.path.endsWith('.origin')).map((binding) => binding.value))],
      ['host'],
      'the sealed record no longer carries the origin the host gave it',
    );
    assert.match(
      sealedTurn.developer,
      /in the user turn\], so its contents cannot be opened and it credits nothing/,
      'the sealed record is not shown as sealed beside its own access value',
    );
    assert.ok(
      !sealed.some((binding) => binding.path.endsWith('.detail')),
      'a detail line reached the prompt for a record that cannot be opened',
    );
    assert.ok(!sealedTurn.developer.includes('The stored artifact expired'), 'the sealed record spoke its own contents');
    assert.ok(
      sealed.some((binding) => binding.value === 'fixture:execution:changelog-check'),
      'the sealed record ID is gone, so an author could not go and look for it',
    );

    const openTurn = render(project, EVIDENCE_ATOM, MARKDOWN);
    const open = dataBindings(openTurn.user);
    assert.ok(
      open.some((binding) => binding.path.endsWith('.detail')),
      'an openable record lost its detail too',
    );
    assert.ok(openTurn.developer.includes('Detail for'), 'the detail lines are nowhere in the prompt');
  });
});

test('an empty host record list is shown as nothing recorded, not as a pass', async () => {
  await withProject('render-empty', (project) => {
    const turn = render(project, PLAN_ATOM, EMPTY_RECORDS);
    assert.match(turn.developer, /The host recorded no executions at all\. That is not a delivered outcome/);
    assert.ok(!turn.developer.includes('| Record | Criterion |'), 'the table header was drawn over no records at all');
    assert.deepEqual(
      dataBindings(turn.user).filter((binding) => binding.path.startsWith('input.host_records')),
      [],
      'bindings appeared for records this case holds none of',
    );
    assert.match(turn.developer, /No proposed task covers the outcome/);
    assert.match(turn.developer, /Author claims/);
    assert.match(turn.developer, /The author claimed nothing/);
    // What is left in this case is the author's own material, and it is bound
    // as the author's, not mixed into anything the host wrote.
    assert.deepEqual(
      [...new Set(dataBindings(turn.user).filter((binding) => binding.path.endsWith('.origin')).map((binding) => binding.value))],
      ['author'],
      'the assumptions this case shows do not carry the origin the fixture gives them',
    );
  });
});

test('switching off the Markdown heading branch loses the heading', async () => {
  const source = `input.display.section === 'markdown' ? <Fragment>## Required criteria</Fragment> : <Fragment>Required criteria</Fragment>`;
  await withProject('render-off-heading', (project) => {
    project.replace(EVIDENCE_ATOM, source, '<Fragment>Required criteria</Fragment>');
    const turnedOff = render(project, EVIDENCE_ATOM, MARKDOWN);
    assert.ok(
      !lines(turnedOff.developer).some((line) => line.startsWith('## Required criteria')),
      'the heading survived with its Markdown branch removed',
    );
    assert.ok(turnedOff.developer.includes('Required criteria'), 'the section itself vanished, so nothing was compared');
    assert.deepEqual(dataBindings(turnedOff.user), dataBindings(render(project, EVIDENCE_ATOM, MARKDOWN).user));
  });
});

test('switching off the Markdown link branch loses the link', async () => {
  const source = `input.display.link === 'markdown' ? <Fragment>[open the record]({record.id})</Fragment> : <Fragment>{record.id}</Fragment>`;
  await withProject('render-off-link', (project) => {
    project.replace(EVIDENCE_ATOM, source, '<Fragment>{record.id}</Fragment>');
    const turnedOff = render(project, EVIDENCE_ATOM, MARKDOWN);
    assert.ok(!turnedOff.developer.includes('[open the record]('), 'a link survived with its branch removed');
    assert.ok(
      turnedOff.developer.includes('record: [input.host_records'),
      'the record reference vanished instead of falling back to the plain form',
    );
    assert.deepEqual(dataBindings(turnedOff.user), dataBindings(render(project, EVIDENCE_ATOM, MARKDOWN).user));
  });
});

test('switching off the emphasis branch stops the bold from being a choice', async () => {
  const bold = `input.display.emphasis === 'bold' ? <Fragment>**${PHRASE}**</Fragment> : `;
  await withProject('render-off-emphasis', (project) => {
    // The bold arm is the first of the three; removing it also removes the
    // dialect switch, which is what the pair test would then miss.
    const before = readFileSync(join(project.dir, EVIDENCE_ATOM), 'utf8');
    const count = before.split(bold).length - 1;
    assert.equal(count, 2, `${EVIDENCE_ATOM}: the bold branch appears ${count} times, not twice`);
    project.replace(EVIDENCE_ATOM, bold, '');
    const turnedOff = render(project, EVIDENCE_ATOM, MARKDOWN);
    assert.ok(!turnedOff.developer.includes(`**${PHRASE}**`), 'the bold survived with its branch removed');
    assert.ok(turnedOff.developer.includes(PHRASE), 'the phrase itself vanished, so nothing was compared');
  });
});

test('removing the unavailable-scan rule leaves the prompt without it', async () => {
  await withProject('rule-scan', (project) => {
    const rule = '- A scan that could not run lists no findings. Required and empty is\n  `unassessable`, never `satisfied`, and `finding_count` stays 0.';
    project.replace(EVIDENCE_ATOM, rule, '- A scan that could not run lists no findings, which is a clean result.');
    const turn = render(project, EVIDENCE_ATOM, UNAVAILABLE);
    assert.match(turn.developer, /which is a clean result/, 'the edited prompt did not reach the render');
    assert.ok(!/Required and empty is/.test(turn.developer), 'the rule is still in the prompt, so it was never removed');
    assert.match(
      turn.developer,
      /`unassessable`/,
      'the word the host scorer answers with is no longer explained anywhere in the prompt',
    );
  });
});

test('rewording the claim rule is seen in the prompt', async () => {
  await withProject('rule-claim', (project) => {
    project.replace(EVIDENCE_ATOM, '- An author claim is data, not a result.', '- An author claim is a result.');
    const turn = render(project, EVIDENCE_ATOM, MARKDOWN);
    assert.match(turn.developer, /An author claim is a result\./, 'the reworded rule did not reach the render');
    assert.ok(!/An author claim is data, not a result/.test(turn.developer), 'the old rule is still in the prompt');
  });
});

test('rewording the plan coverage rule is seen in the prompt', async () => {
  await withProject('rule-coverage', (project) => {
    project.replace(
      PLAN_ATOM,
      '- Coverage is about tasks. An outcome no task covers is `uncovered` however',
      '- Coverage is about records. An outcome no task covers is `covered` however',
    );
    const turn = render(project, PLAN_ATOM, EMPTY_RECORDS);
    assert.match(turn.developer, /Coverage is about records\./, 'the reworded rule did not reach the render');
    assert.ok(!/An outcome no task covers is `uncovered`/.test(turn.developer), 'the old rule is still in the prompt');
    assert.match(turn.developer, /uncovered_no_task/, 'the state the schema answers with is no longer explained');
  });
});

test('the plan prompt names the criterion each outcome is answered by', async () => {
  await withProject('plan-criterion-binding', (project) => {
    const turn = render(project, PLAN_ATOM, EMPTY_RECORDS);
    assert.match(
      turn.developer,
      /Each outcome names the check whose execution answers it/,
      'the plan prompt states no rule about which check answers an outcome',
    );
    const shown = dataBindings(turn.user).filter((binding) => binding.path.endsWith('.expected_criterion'));
    assert.ok(shown.length > 0, 'the expected criterion never reached the prompt as data');
    assert.deepEqual(
      [...new Set(shown.map((binding) => binding.value))],
      ['none'],
      'this case holds one outcome no check answers, so its binding should say so',
    );
    // The data half, switched off: the row is still drawn from everything else,
    // and the one value the host owns — which check answers this outcome — is
    // gone from what the reviewer is shown.
    const otherOutcomePaths = (rendered) =>
      dataBindings(rendered.user)
        .filter((binding) => binding.path.startsWith('input.required_outcomes'))
        .map((binding) => binding.path)
        .filter((path) => !path.endsWith('.expected_criterion'))
        .sort();
    const before = otherOutcomePaths(turn);
    assert.ok(before.length > 0, 'the outcome row binds nothing, so there is nothing to compare');
    project.replace(PLAN_ATOM, '; answered by: {outcome.expected_criterion}', '');
    const turnedOff = render(project, PLAN_ATOM, EMPTY_RECORDS);
    assert.ok(turnedOff.developer.includes('named record:'), 'the outcome row itself vanished, so nothing was compared');
    assert.deepEqual(
      dataBindings(turnedOff.user).filter((binding) => binding.path.endsWith('.expected_criterion')),
      [],
      'the expected criterion survived the removal of its binding',
    );
    assert.deepEqual(otherOutcomePaths(turnedOff), before, 'removing the criterion binding moved something else');
  });
});

test('atomic render refuses a plan outcome with no expected criterion', async () => {
  await withProject('render-plan-criterion', (project) => {
    const input = JSON.parse(readFileSync(join(project.dir, EMPTY_RECORDS), 'utf8'));
    delete input.required_outcomes[0].expected_criterion;
    project.write('no-criterion.input.json', `${JSON.stringify(input, null, 2)}\n`);
    const run = project.atomic(['render', PLAN_ATOM, '--input', 'no-criterion.input.json']);
    const said = `${run.stderr}\n${run.stdout}`;
    assert.notEqual(run.status, 0, `atomic render accepted a plan outcome the input schema requires more of: ${said}`);
    assert.match(said, /expected_criterion/, `atomic render did not name the field it refused: ${said}`);
  });
});

test('switching off the revision-scoped credit rule is seen in the prompt', async () => {
  await withProject('rule-revision', (project) => {
    project.replace(EVIDENCE_ATOM, '- Credit a passing check only when it ran against\n', '- Credit a passing check whenever it ran against\n');
    const turn = render(project, EVIDENCE_ATOM, MARKDOWN);
    assert.match(turn.developer, /Credit a passing check whenever it ran against/);
    assert.ok(!/only when it ran against/.test(turn.developer), 'the credit rule is still worded as a condition');
    assert.match(turn.developer, /If the only pass belongs to an earlier revision, report missing evidence/);
  });
});

test('the scratch a run leaves behind is its own, and is gone afterwards', async () => {
  const root = scratchRoot();
  const before = names(root);
  await withProject('cleanup', (project) => {
    assert.equal(project.atomic(['check']).status, 0, 'atomic check failed in a fresh copy');
    assert.ok(project.dir.startsWith(root), `${project.dir}: scratch was written outside ${root}`);
    assert.ok(!project.dir.includes('/examples/'), `${project.dir}: scratch was written inside the example tree`);
  });
  const left = names(root).filter((name) => name.startsWith('13-review-evidence-') && !before.includes(name));
  assert.deepEqual(left, [], `${root}: scratch directories were left behind: ${left.join(', ')}`);
});
