#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// The offline scorer's own suite: no `atomic`, no model, no network, no Factory
// system. Each case names the rule it proves, and each rule is also shown
// failing — the negative rows below are the ones that decide between "the
// evidence is there" and "the evidence is missing".
//
//   node --test examples/13-review-evidence/*.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, truncateSync, unlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { EVIDENCE_DIR, RECORD_FILES, loadRecords } from './tools/host-records.mjs';
import { evidenceCases, recordState, scoreEvidence } from './tools/score-evidence.mjs';
import { planCases, scorePlan } from './tools/score-plan.mjs';

const loaded = loadRecords();
const CANDIDATE = loaded.revision;
const evidence = Object.fromEntries(evidenceCases(loaded).map((entry) => [entry.name, entry]));
const plan = Object.fromEntries(planCases(loaded).map((entry) => [entry.name, entry]));

/** A copy of a case's input, so a test can change one thing and nothing else. */
function variant(input, change) {
  const copy = structuredClone(input);
  change(copy);
  return copy;
}

/** A throwaway copy of the record files, for the reader's own failures. */
function recordsCopy(tweak) {
  const directory = mkdtempSync(join(tmpdir(), 'atomic-review-evidence-records-'));
  const evidenceCopy = join(directory, 'evidence');
  cpSync(EVIDENCE_DIR, evidenceCopy, { recursive: true });
  tweak(evidenceCopy);
  return {
    directory,
    load: () => loadRecords(evidenceCopy),
    done: () => rmSync(directory, { recursive: true, force: true }),
  };
}

test('a required execution that never ran scores uncovered and unassessable, never clean', () => {
  // The plan view: an accepted outcome with no task and no record named for it.
  const uncovered = scorePlan(plan['plan-row-without-any-record'].input);
  const [row] = uncovered.rows;
  assert.equal(row.requirement, 'REQ-TEST');
  assert.equal(row.coverage, 'uncovered');
  assert.equal(row.state, 'uncovered_no_record');
  assert.equal(row.task, 'none');
  assert.equal(row.record, 'none');
  assert.deepEqual(row.citations, []);
  assert.equal(row.finding_count, 0, 'nothing ran, so there are no findings to count and no coverage either');
  assert.equal(uncovered.decision, 'escalate');
  assert.equal(uncovered.summary.uncovered, 1);

  // The evidence view: a required check whose named record is simply absent.
  const missing = scoreEvidence(
    variant(evidence['current-pass-omitted-by-author-markdown'].input, (input) => {
      input.required_checks = [{ id: 'docs-links', title: 'Documentation links resolve', record: 'none', required: true }];
      input.host_records = [];
      input.author_claims = [];
    }),
  );
  assert.equal(missing.rows.length, 1);
  assert.equal(missing.rows[0].state, 'record_missing');
  assert.equal(missing.rows[0].outcome, 'missing_evidence');
  assert.deepEqual(missing.rows[0].citations, []);
  assert.equal(missing.rows[0].finding_count, 0);
  assert.equal(missing.decision, 'escalate');
  assert.equal(missing.summary.missing_evidence, 1);

  // A task that names a record no host holds: covered by a task, with nothing
  // to cite, and still not satisfied.
  const noRecord = scorePlan(
    variant(plan['required-scan-unavailable-with-zero-findings'].input, (input) => {
      input.host_records = [];
    }),
  );
  assert.equal(noRecord.rows[0].state, 'covered_no_record');
  assert.equal(noRecord.rows[0].coverage, 'covered');
  assert.ok(
    !Object.hasOwn(noRecord.rows[0], 'outcome'),
    'a plan row states coverage and state; the outcome a required outcome maps to is stated once, in the summary',
  );
  assert.deepEqual(noRecord.rows[0].citations, []);
  assert.equal(noRecord.decision, 'escalate');
  assert.equal(noRecord.summary.missing_evidence, 1);
});

test('a required scan that could not run, with zero findings, scores unassessable', () => {
  const scan = scoreEvidence(evidence['unavailable-scan-with-zero-findings'].input);
  const [row] = scan.rows;
  assert.equal(row.criterion, 'secret-scan');
  assert.equal(row.state, 'record_unavailable');
  assert.equal(row.outcome, 'unassessable', 'an unavailable scan is missing coverage, not a clean result');
  assert.equal(row.finding_count, 0, 'the empty findings list is why this rule exists');
  assert.equal(row.credit, 'not_credited');
  assert.equal(row.claimed, true, 'the author does claim a pass here');
  assert.deepEqual(row.citations, []);
  assert.equal(scan.decision, 'escalate');
  assert.equal(scan.summary.unassessable, 1);
  assert.equal(scan.summary.satisfied, 0);
  assert.match(row.detail, /missing coverage, not a clean result/);

  const planView = scorePlan(plan['required-scan-unavailable-with-zero-findings'].input);
  assert.equal(planView.rows[0].state, 'covered_unassessable');
  assert.equal(planView.summary.unassessable, 1, 'covered_unassessable maps to unassessable, never to covered-and-satisfied');
  assert.equal(planView.rows[0].finding_count, 0);
  assert.match(planView.rows[0].detail, /UNASSESSABLE, not covered/);
  assert.equal(planView.decision, 'escalate');
});

test('one record is accepted for this revision and refused for an earlier one', () => {
  const stale = scoreEvidence(evidence['stale-pass-for-earlier-revision'].input);
  const [staleRow] = stale.rows;
  assert.equal(staleRow.record, 'fixture:execution:pricing-scan', 'the ID is kept even when it cites nothing');
  assert.equal(staleRow.state, 'record_stale');
  assert.equal(staleRow.outcome, 'missing_evidence');
  assert.deepEqual(staleRow.citations, []);
  assert.equal(stale.decision, 'escalate');

  // The same record, the same input, one field changed: its revision.
  const current = scoreEvidence(
    variant(evidence['stale-pass-for-earlier-revision'].input, (input) => {
      input.host_records = input.host_records.map((record) => ({ ...record, revision: CANDIDATE }));
    }),
  );
  const [currentRow] = current.rows;
  assert.equal(currentRow.record, 'fixture:execution:pricing-scan');
  assert.equal(currentRow.state, 'record_current_pass');
  assert.equal(currentRow.outcome, 'satisfied');
  assert.deepEqual(currentRow.citations, ['fixture:execution:pricing-scan']);
  assert.equal(currentRow.credit, 'host_record');
  assert.equal(currentRow.claimed, true);
  assert.equal(current.summary.uncredited_claims, 0, 'the claim now matches a host record');
  assert.equal(current.decision, 'approve', 'one satisfied required check and nothing else outstanding');
});

test('a record that ran for another check is refused, never borrowed as this check’s evidence', () => {
  const input = evidence['claimed-pass-without-host-record'].input;
  const byId = new Map(input.host_records.map((record) => [record.id, record]));
  for (const check of input.required_checks) {
    if (check.record !== 'none') {
      assert.equal(byId.get(check.record).criterion, check.id, `${check.id}: the fixture does not join each check to its own run`);
    }
  }

  // The swap the rule exists for. docs-build failed for this candidate, so
  // point it at docs-links' passing run and ask what the score says.
  const borrowed = variant(input, (copy) => {
    copy.required_checks.find((check) => check.id === 'docs-build').record = 'fixture:execution:docs-links';
  });
  assert.throws(
    () => scoreEvidence(borrowed),
    /docs-build: fixture:execution:docs-links was recorded for docs-links, not for this check, so it is not evidence here/,
    'a check borrowed another check’s passing run and the score said nothing',
  );

  // The same borrow from the other side: a record re-labelled as docs-build's,
  // so the join by ID still matches while the criterion no longer does. The
  // check that owned that record is the one left with nothing.
  const relabelled = variant(input, (copy) => {
    copy.host_records.find((record) => record.id === 'fixture:execution:docs-links').criterion = 'docs-build';
  });
  assert.throws(
    () => scoreEvidence(relabelled),
    /docs-links: fixture:execution:docs-links was recorded for docs-build, not for this check/,
    'a record re-labelled for another check was still read as evidence here',
  );

  // The plan view had the same hole and is closed by the same rule, in its own
  // test below. What is asserted here is only the shape that makes that rule
  // possible: every plan outcome carries the host's own answer-to criterion.
  const planCase = plan['plan-coverage-with-uncovered-outcome'].input;
  for (const outcome of planCase.required_outcomes) {
    assert.equal(
      typeof outcome.expected_criterion,
      'string',
      `${outcome.id}: a plan outcome with no \`expected_criterion\`, so a borrowed run could not be told from its own`,
    );
  }
});

test('a plan row answers only a run of the criterion its outcome expects', () => {
  // The reproduction this rule exists for. REQ-SCAN's row is decided by its
  // task, whose scan never reported a state; point that one field at
  // docs-links' passing run and the row reads `covered_current`, citing a run
  // that says nothing about secrets.
  const base = plan['plan-coverage-with-uncovered-outcome'].input;
  const scanRow = (scored) => scored.rows.find((row) => row.requirement === 'REQ-SCAN');
  assert.equal(scanRow(scorePlan(base)).state, 'covered_unassessable', 'the unmutated case answers the scan, which could not run');
  assert.equal(scanRow(scorePlan(base)).record, 'fixture:execution:secret-scan');

  const swapped = variant(base, (copy) => {
    copy.proposed_tasks.find((task) => task.covers.includes('REQ-SCAN')).record = 'fixture:execution:docs-links';
  });
  assert.throws(
    () => scorePlan(swapped),
    /REQ-SCAN: fixture:execution:docs-links was recorded for docs-links, not for secret-scan, the check this outcome is answered by/,
    'a required scan was answered by another check’s passing run',
  );

  // The same swap arriving as files, which is how a candidate record arrives:
  // refused on the way in, before any case is scored.
  const borrowedFiles = recordsCopy((directory) => {
    const path = join(directory, RECORD_FILES.candidate);
    const candidate = JSON.parse(readFileSync(path, 'utf8'));
    candidate.proposed_tasks.find((task) => task.id === 'fixture-task-scan').record = 'fixture:execution:docs-links';
    writeFileSync(path, JSON.stringify(candidate));
  });
  try {
    assert.throws(
      borrowedFiles.load,
      /task fixture-task-scan names fixture:execution:docs-links, which was recorded for docs-links; REQ-SCAN is answered only by a run of secret-scan/,
      'the reader let a task answer its outcome with another check’s run',
    );
  } finally {
    borrowedFiles.done();
  }

  // What the rule must leave alone. A covered row whose record is simply not
  // there, and a row no task covers, are states a reviewer has to see; turning
  // either into a throw would hide the hole it reports.
  const missing = variant(base, (copy) => {
    copy.host_records = copy.host_records.filter((record) => record.id !== 'fixture:execution:secret-scan');
  });
  assert.equal(scanRow(scorePlan(missing)).state, 'covered_no_record', 'an absent record is a state, not a refusal');
  assert.equal(scorePlan(base).summary.missing_evidence, 1, 'the unmutated case is missing evidence once, for the stale pricing run');
  assert.equal(scorePlan(missing).summary.missing_evidence, 2, 'and the absent scan is counted beside it, not swallowed');
  assert.equal(scorePlan(missing).decision, 'escalate');
  const taskless = variant(base, (copy) => {
    copy.proposed_tasks.find((task) => task.covers.includes('REQ-SCAN')).record = 'none';
  });
  assert.equal(scanRow(scorePlan(taskless)).state, 'covered_no_record', 'a task naming no record is covered with nothing to cite');
  const polish = scorePlan(base).rows.find((row) => row.requirement === 'REQ-POLISH');
  assert.equal(polish.state, 'uncovered_no_task', 'an outcome no task covers stays uncovered');
  assert.equal(polish.record, 'fixture:execution:changelog-check', 'and still names the evidence it looked for');
  assert.ok(
    plan['plan-row-without-any-record'].input.required_outcomes.every((outcome) => outcome.expected_criterion === 'none'),
    'the empty plan case no longer holds only outcomes no check answers',
  );
});

test('a claimed pass with no host record earns no credit and moves no other row', () => {
  const input = evidence['claimed-pass-without-host-record'].input;
  const scored = scoreEvidence(input);
  assert.equal(scored.rows.length, 6, 'one row per required check, whatever the records hold');
  const secret = scored.rows.find((row) => row.criterion === 'secret-scan');
  assert.equal(secret.claimed, true);
  assert.deepEqual(secret.claim_ids, ['fixture:claim:secret-scan'], 'the claim keeps its own ID');
  assert.equal(secret.credit, 'not_credited');
  assert.equal(secret.outcome, 'unassessable');
  const pricing = scored.rows.find((row) => row.criterion === 'pricing-scan');
  assert.equal(pricing.claimed, true);
  assert.equal(pricing.credit, 'not_credited');
  assert.equal(scored.summary.uncredited_claims, 2);
  const docs = scored.rows.find((row) => row.criterion == 'docs-links');
  assert.equal(docs.claimed, true);
  assert.equal(docs.credit, 'host_record', 'this claim happens to match a host record');
  assert.equal(scored.decision, 'reject', 'docs-build failed for this candidate');
  assert.equal(scored.summary.failed, 1);

  // Remove the claims binding: the host rows and the decision do not move.
  // `detail` is excluded beside `claimed` and `claim_ids` because it is the
  // sentence about the claim; state, credit, outcome, citations and counts stay.
  const withoutClaims = scoreEvidence(variant(input, (copy) => (copy.author_claims = [])));
  const hostRows = (rows) => rows.map(({ claimed, claim_ids, detail, ...host }) => host);
  assert.deepEqual(
    hostRows(withoutClaims.rows),
    hostRows(scored.rows),
    'a claim changes what was claimed, never what the host holds',
  );
  assert.ok(withoutClaims.rows.every((row) => !row.claimed && row.claim_ids.length === 0));
  assert.equal(withoutClaims.decision, scored.decision);
});

test('a current host pass is credited even when the author never mentions it', () => {
  const scored = scoreEvidence(evidence['current-pass-omitted-by-author-markdown'].input);
  const ownership = scored.rows.find((row) => row.criterion === 'ownership-review');
  assert.equal(ownership.claimed, false, 'the author claims only docs-links');
  assert.equal(ownership.credit, 'host_record', 'the host record is enough on its own');
  assert.equal(ownership.outcome, 'satisfied');
  assert.deepEqual(ownership.citations, ['fixture:execution:ownership-review']);
  assert.equal(scored.summary.uncredited_claims, 0);
  assert.equal(scored.decision, 'approve');
});

test('every required row survives, in order, with its own IDs', () => {
  const input = evidence['claimed-pass-without-host-record'].input;
  const scored = scoreEvidence(input);
  assert.deepEqual(
    scored.rows.map((row) => row.criterion),
    input.required_checks.map((check) => check.id),
    'the rows are the required checks, in the order they were authored',
  );
  for (const [index, row] of scored.rows.entries()) {
    assert.equal(row.record, input.required_checks[index].record, `${row.criterion}: the named record ID is preserved`);
    assert.equal(row.required, input.required_checks[index].required);
    for (const citation of row.citations) {
      assert.ok(
        input.host_records.some((record) => record.id === citation && record.revision === CANDIDATE && record.access === 'grant'),
        `${row.criterion} cites ${citation}, which is not an openable record for this candidate`,
      );
    }
  }
  const sealed = input.host_records.find((record) => record.access === 'none');
  assert.equal(sealed.id, 'fixture:execution:changelog-check');
  assert.ok(
    scored.rows.every((row) => !row.citations.includes(sealed.id)),
    'a record nobody can open is never cited, though its ID stays on the row',
  );
  assert.deepEqual(
    scored.rows.flatMap((row) => row.claim_ids).sort(),
    input.author_claims.map((claim) => claim.id).sort(),
    'every claim ID reaches a row',
  );

  const coverage = scorePlan(plan['plan-coverage-with-uncovered-outcome'].input);
  assert.deepEqual(
    coverage.rows.map((row) => row.requirement),
    plan['plan-coverage-with-uncovered-outcome'].input.required_outcomes.map((outcome) => outcome.id),
  );
  const polish = coverage.rows.find((row) => row.requirement === 'REQ-POLISH');
  assert.equal(polish.task, 'none');
  assert.equal(polish.coverage, 'uncovered');
  assert.equal(polish.record, 'fixture:execution:changelog-check', 'an uncovered row still shows what it looked for');
  assert.deepEqual(polish.citations, [], 'and that record cannot be opened, so it cites nothing');
  assert.equal(polish.acceptance_criterion, 'CHANGELOG.md names the change for this revision');
  assert.equal(polish.dependency, 'none');
  const pricing = coverage.rows.find((row) => row.requirement === 'REQ-PRICING');
  assert.equal(pricing.state, 'covered_stale');
  assert.equal(pricing.dependency, 'fixture-task-docs', 'the task dependency comes from the plan, not a record');
  assert.deepEqual(pricing.citations, []);
  assert.equal(coverage.summary.covered, 3);
  assert.equal(coverage.summary.uncovered, 2);
  assert.equal(coverage.decision, 'escalate');
});

test('scoring does not depend on how the prompt is drawn', () => {
  const plain = scoreEvidence(evidence['current-pass-omitted-by-author-plain'].input);
  const markdown = scoreEvidence(evidence['current-pass-omitted-by-author-markdown'].input);
  assert.notDeepEqual(
    evidence['current-pass-omitted-by-author-plain'].input.display,
    evidence['current-pass-omitted-by-author-markdown'].input.display,
    'the pair really does differ by its delimiters',
  );
  assert.deepEqual(plain, markdown, 'and the answer is identical');

  const base = plan['plan-row-without-any-record'].input;
  const other = variant(base, (input) => {
    input.display = { section: 'markdown', link: 'markdown', emphasis: 'bold' };
  });
  assert.deepEqual(scorePlan(base), scorePlan(other));
});

test('a record no rule recognises refuses to score, rather than passing', () => {
  assert.throws(() => recordState({ id: 'x', access: 'grant', revision: CANDIDATE, outcome: 'green' }, CANDIDATE), /outcome is "green"/);
  assert.throws(() => recordState({ id: 'x', access: 'maybe', revision: CANDIDATE, outcome: 'pass' }, CANDIDATE), /access is "maybe"/);
  assert.throws(() => recordState({ id: 'x', revision: CANDIDATE, outcome: 'pass' }, CANDIDATE), /access is undefined/);
  assert.equal(recordState(null, CANDIDATE), 'record_missing', 'a true absence is a state, not an error');
  assert.equal(recordState({ id: 'x', access: 'none', revision: '0'.repeat(40), outcome: 'pass' }, CANDIDATE), 'record_inaccessible');
});

test('the reader refuses a record file it cannot read, parse, or match', () => {
  const cleanups = [];
  const copy = (tweak) => {
    const fixture = recordsCopy(tweak);
    cleanups.push(fixture.done);
    return fixture;
  };
  const read = (directory, file) => JSON.parse(readFileSync(join(directory, file), 'utf8'));
  try {
    const missing = copy((directory) => unlinkSync(join(directory, RECORD_FILES.candidate)));
    assert.throws(missing.load, /candidate\.json: no candidate fixture record file/);

    // A directory in the file's place is an error, not an absence.
    const unreadable = copy((directory) => {
      unlinkSync(join(directory, RECORD_FILES.candidate));
      mkdirSync(join(directory, RECORD_FILES.candidate));
    });
    assert.throws(unreadable.load, /candidate\.json: candidate fixture record file could not be read/);

    const truncated = copy((directory) => truncateSync(join(directory, RECORD_FILES.host), 12));
    assert.throws(truncated.load, /host-executions\.json: host execution fixture record file is not JSON/);

    // An empty host set is not "nothing was required".
    const emptyHost = copy((directory) => {
      const path = join(directory, RECORD_FILES.host);
      writeFileSync(path, JSON.stringify({ origin: 'host', records: [] }));
    });
    assert.throws(emptyHost.load, /no host execution records; an empty host set would read as/);

    // Two records with one ID would let two rows answer one execution.
    const duplicate = copy((directory) => {
      const path = join(directory, RECORD_FILES.host);
      const records = read(directory, RECORD_FILES.host);
      records.records[1].id = records.records[0].id;
      writeFileSync(path, JSON.stringify(records));
    });
    assert.throws(duplicate.load, /host-executions\.json: duplicate id fixture:execution:docs-links/);

    const unknownOutcome = copy((directory) => {
      const path = join(directory, RECORD_FILES.host);
      const records = read(directory, RECORD_FILES.host);
      records.records[0].outcome = 'green';
      writeFileSync(path, JSON.stringify(records));
    });
    assert.throws(
      unknownOutcome.load,
      /record fixture:execution:docs-links `outcome` is "green", which is not one of pass, fail, unavailable/,
    );

    // A field that is not a list is refused by name, not read as empty.
    const notAList = copy((directory) => {
      const path = join(directory, RECORD_FILES.candidate);
      const candidate = read(directory, RECORD_FILES.candidate);
      candidate.proposed_tasks[0].covers = 'REQ-DOCS';
      writeFileSync(path, JSON.stringify(candidate));
    });
    assert.throws(notAList.load, /task fixture-task-docs `covers` is not a list/);

    // A record the host holds that no required check answers. It is added, not
    // re-labelled: re-labelling a record a check names would trip the join
    // guard above first, and each refusal needs its own test.
    const orphan = copy((directory) => {
      const path = join(directory, RECORD_FILES.host);
      const records = read(directory, RECORD_FILES.host);
      records.records.push({ ...records.records[0], id: 'fixture:execution:unasked-for', criterion: 'nothing-asked-for' });
      writeFileSync(path, JSON.stringify(records));
    });
    assert.throws(orphan.load, /record fixture:execution:unasked-for answers nothing-asked-for, which no required check names/);

    const dangling = copy((directory) => {
      const path = join(directory, RECORD_FILES.candidate);
      const candidate = read(directory, RECORD_FILES.candidate);
      candidate.required_checks[0].record = 'fixture:execution:typo-here';
      writeFileSync(path, JSON.stringify(candidate));
    });
    assert.throws(dangling.load, /names fixture:execution:typo-here, which no host execution record holds/);

    // The record exists, so the ID check above passes; only the criterion shows
    // that this run was not a run of docs-build.
    const borrowed = copy((directory) => {
      const path = join(directory, RECORD_FILES.candidate);
      const candidate = read(directory, RECORD_FILES.candidate);
      candidate.required_checks.find((check) => check.id === 'docs-build').record = 'fixture:execution:docs-links';
      writeFileSync(path, JSON.stringify(candidate));
    });
    assert.throws(
      borrowed.load,
      /check docs-build names fixture:execution:docs-links, which was recorded for docs-links; a check may name only a record whose `criterion` is itself/,
    );

    // The plan side: the criterion an outcome is answered by is the host's to
    // state, so a candidate that omits it, invents one, or answers an outcome
    // that expects none is a broken fixture, not a looser join.
    const noCriterion = copy((directory) => {
      const path = join(directory, RECORD_FILES.candidate);
      const candidate = read(directory, RECORD_FILES.candidate);
      delete candidate.required_outcomes.find((outcome) => outcome.id === 'REQ-DOCS').expected_criterion;
      writeFileSync(path, JSON.stringify(candidate));
    });
    assert.throws(noCriterion.load, /entry REQ-DOCS has no `expected_criterion` string/);

    const inventedCriterion = copy((directory) => {
      const path = join(directory, RECORD_FILES.candidate);
      const candidate = read(directory, RECORD_FILES.candidate);
      candidate.required_outcomes.find((outcome) => outcome.id === 'REQ-DOCS').expected_criterion = 'nothing-asked-for';
      writeFileSync(path, JSON.stringify(candidate));
    });
    assert.throws(inventedCriterion.load, /outcome REQ-DOCS expects nothing-asked-for, which no required check states/);

    const answersNothing = copy((directory) => {
      const path = join(directory, RECORD_FILES.candidate);
      const candidate = read(directory, RECORD_FILES.candidate);
      candidate.proposed_tasks.find((task) => task.id === 'fixture-task-scan').covers = ['REQ-TEST'];
      writeFileSync(path, JSON.stringify(candidate));
    });
    assert.throws(
      answersNothing.load,
      /task fixture-task-scan names fixture:execution:secret-scan for REQ-TEST, which expects no check; write "none" for that task's record/,
    );

    const claimOnNothing = copy((directory) => {
      const path = join(directory, RECORD_FILES.claims);
      const claims = read(directory, RECORD_FILES.claims);
      claims.claims[0].criterion = 'nothing-asked-for';
      writeFileSync(path, JSON.stringify(claims));
    });
    assert.throws(claimOnNothing.load, /claim fixture:claim:docs-links is about nothing-asked-for, which is not a required check/);

    // And a copy nobody tampered with still loads, so none of the above is a
    // reader that refuses everything.
    const untouched = copy(() => {});
    assert.equal(untouched.load().records.length, 6);
  } finally {
    for (const done of cleanups) done();
  }
});

test('claims and assumptions never change coverage', () => {
  const base = plan['plan-coverage-with-uncovered-outcome'].input;
  assert.ok(base.author_claims.length > 0, 'the case carries author claims');
  const withoutClaims = variant(base, (input) => (input.author_claims = []));
  const withoutAssumptions = variant(base, (input) => {
    input.assumptions = [];
  });
  const scored = scorePlan(base);
  assert.deepEqual(scorePlan(withoutClaims).rows, scored.rows);
  assert.equal(scorePlan(withoutClaims).decision, scored.decision);
  assert.equal(scorePlan(withoutAssumptions).assumptions.length, 0);
  assert.deepEqual(scorePlan(withoutAssumptions).rows, scored.rows, 'an assumption is not coverage');
  assert.equal(scorePlan(withoutAssumptions).decision, scored.decision);
});
