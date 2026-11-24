#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// Coverage view over the required outcomes, scored from the records a run was
// given. Like score-evidence.mjs this runs no model and reaches no service: it
// answers "which expected checks have host evidence for this candidate?" by
// starting from the required outcomes and joining each to a host record — never
// from the executions that happened to be recorded, which is what hides a
// required check that never ran.
//
// A plan row answers one record: the one its task says ran, or the one the
// requirement names itself when no task covers it. `uncovered` is about tasks;
// `unassessable` is about a record that never reported a state. The two are
// different holes and a reviewer has to see both.
//
// Before any of the states, one join rule for a covered row: the record its task
// names must be a run of the check the outcome names in `expected_criterion`. A
// record that ran for another check answers nothing here, and scoring it is
// refused rather than read as coverage — otherwise one green run of any check
// turns an uncovered or unassessable requirement into `covered_current`.
//
// The decision rule the fixture is built around, in words a model is told:
//
//   required scan unavailable, zero findings -> UNASSESSABLE (never covered)

import { basename } from 'node:path';
import { loadRecords, planInput } from './host-records.mjs';
import { recordState } from './score-evidence.mjs';

export const PLAN_STATES = Object.freeze([
  'covered_current',
  'covered_failed',
  'covered_stale',
  'covered_unassessable',
  'covered_inaccessible',
  'covered_no_record',
  'uncovered_no_task',
  'uncovered_no_record',
]);

const STATE_BY_TASK_RECORD = Object.freeze({
  record_current_pass: 'covered_current',
  record_current_fail: 'covered_failed',
  record_stale: 'covered_stale',
  record_unavailable: 'covered_unassessable',
  record_inaccessible: 'covered_inaccessible',
  // A task whose record is simply not there: covered by a task, with nothing
  // to cite. Refusing to answer would be the wrong failure — the row has to
  // say what is missing.
  record_missing: 'covered_no_record',
});

const OUTCOME_BY_STATE = Object.freeze({
  covered_current: 'satisfied',
  covered_failed: 'failed',
  covered_stale: 'missing_evidence',
  covered_unassessable: 'unassessable',
  covered_inaccessible: 'missing_evidence',
  covered_no_record: 'missing_evidence',
  uncovered_no_task: 'uncovered',
  uncovered_no_record: 'uncovered',
});

function planStateFor(outcome, task, record, revision) {
  if (!task) return record === null ? 'uncovered_no_record' : 'uncovered_no_task';
  const state = recordState(record, revision);
  const mapped = STATE_BY_TASK_RECORD[state];
  if (!mapped) throw new Error(`${outcome.id}: host state ${state} has no plan-row mapping`);
  return mapped;
}

/**
 * Score one `review-plan` input into the coverage view.
 *
 * `rows` has one entry per required outcome, in authored order, whether or not
 * a task, a record or an acceptance criterion exists for it.
 */
export function scorePlan(input) {
  const revision = input.plan.revision;
  const byId = new Map();
  for (const record of input.host_records) {
    if (byId.has(record.id)) throw new Error(`${record.id}: two host execution records share one id`);
    byId.set(record.id, record);
  }
  const taskByRequirement = new Map();
  for (const task of input.proposed_tasks) {
    for (const covered of task.covers) {
      if (taskByRequirement.has(covered)) {
        throw new Error(`${covered}: two proposed tasks cover one required outcome (${taskByRequirement.get(covered).id}, ${task.id})`);
      }
      taskByRequirement.set(covered, task);
    }
  }

  const rows = input.required_outcomes.map((outcome) => {
    const task = taskByRequirement.get(outcome.id) ?? null;
    // The row answers its task's record when it has a task, and its own named
    // record when it has none, so an uncovered requirement still shows the
    // evidence it was looking for.
    const named = task ? task.record : outcome.record;
    const record = named === 'none' ? null : (byId.get(named) ?? null);
    if (task && record !== null && record.criterion !== outcome.expected_criterion) {
      // Refused, not downgraded: an input can arrive from a file, an eval case or
      // a workflow binding, and none of those paths re-run the reader. A missing
      // `expected_criterion` lands here too, which is the right direction for a
      // row the host never said how to answer.
      throw new Error(
        `${outcome.id}: ${record.id} was recorded for ${record.criterion}, not for ${outcome.expected_criterion}, the check this outcome is answered by`,
      );
    }
    const state = planStateFor(outcome, task, record, revision);
    const citable = state === 'covered_current' || state === 'covered_failed';
    const dependency = task ? task.depends_on : 'none';
    const note = !task
      ? `${outcome.id}: no proposed task covers this outcome, so it is uncovered however the plan reads.`
      : state === 'covered_unassessable' && record.findings.length === 0
        ? `${outcome.id}: the required scan reported zero findings because it could not run. That is UNASSESSABLE, not covered.`
        : state === 'covered_stale'
          ? `${outcome.id}: ${named} ran against an earlier revision, so it is not evidence for ${revision}.`
          : state === 'covered_inaccessible'
            ? `${outcome.id}: ${named} exists but its contents cannot be opened, so it credits nothing.`
            : state === 'covered_no_record'
              ? named === 'none'
                ? `${outcome.id}: ${task.id} names no record at all, so there is nothing to cite.`
                : `${outcome.id}: ${task.id} names ${named}, which no host record holds, so there is nothing to cite.`
            : citable
              ? `${outcome.id}: covered by ${task.id}, whose record ${named} answers this revision.`
              : `${outcome.id}: covered by ${task.id}.`;
    return {
      requirement: outcome.id,
      required: outcome.required,
      task: task ? task.id : 'none',
      acceptance_criterion: outcome.acceptance_criterion,
      dependency,
      coverage: state.startsWith('covered_') ? 'covered' : 'uncovered',
      state,
      record: named,
      citations: citable ? [named] : [],
      finding_count: record && state !== 'covered_inaccessible' ? record.findings.length : 0,
      detail: note,
    };
  });

  const assumptions = input.assumptions ?? [];
  // `author_claims` is in the prompt so the reviewer can see what the author
  // says, and nowhere in this scoring path: a claim cannot turn an uncovered
  // outcome into a covered one. `claims cannot change coverage` asserts it.
  const requiredRows = rows.filter((row) => row.required);
  const count = (wanted) => requiredRows.filter((row) => OUTCOME_BY_STATE[row.state] === wanted).length;
  const decision = requiredRows.some((row) => OUTCOME_BY_STATE[row.state] === 'failed')
    ? 'reject'
    : requiredRows.some((row) => ['uncovered', 'missing_evidence', 'unassessable'].includes(OUTCOME_BY_STATE[row.state]))
      ? 'escalate'
      : 'approve';

  return {
    decision,
    rows,
    assumptions: assumptions.map((assumption) => assumption.text),
    summary: {
      required: requiredRows.length,
      covered: requiredRows.filter((row) => row.coverage === 'covered').length,
      uncovered: count('uncovered'),
      unassessable: count('unassessable'),
      missing_evidence: count('missing_evidence'),
      failed: count('failed'),
    },
  };
}

/**
 * The fixture cases this scorer is run over: the input each one builds, the
 * hand-written CEL assertions committed in `evals/review-plan/cases.jsonl`, and
 * a note saying what the case is for. As in score-evidence.mjs, the expected
 * answer is this scorer's own answer for those records and the assertions are
 * written by hand.
 */
export function planCases(loaded = loadRecords()) {
  const allOutcomes = loaded.outcomes.map((outcome) => outcome.id);
  const allTasks = loaded.tasks.map((task) => task.id);
  const claimsAll = loaded.claims.map((claim) => claim.id);
  const taskFor = (requirement) =>
    loaded.tasks.filter((task) => task.covers.includes(requirement)).map((task) => task.id);
  return [
    {
      name: 'plan-coverage-with-uncovered-outcome',
      note: 'Every required outcome and every proposed task, including the two outcomes no task covers.',
      expect: [
        "size(output.rows) == 5",
        "output.rows.exists(r, r.requirement == 'REQ-POLISH' && r.task == 'none' && r.coverage == 'uncovered' && r.record == 'fixture:execution:changelog-check')",
        "output.rows.exists(r, r.requirement == 'REQ-TEST' && r.state == 'uncovered_no_record' && r.coverage == 'uncovered')",
        "output.rows.exists(r, r.requirement == 'REQ-SCAN' && r.state == 'covered_unassessable' && r.finding_count == 0 && r.detail.contains('UNASSESSABLE'))",
        "output.rows.exists(r, r.requirement == 'REQ-PRICING' && r.state == 'covered_stale' && size(r.citations) == 0 && r.dependency == 'fixture-task-docs')",
        "output.rows.exists(r, r.requirement == 'REQ-DOCS' && r.state == 'covered_current' && size(r.citations) == 1)",
        "output.summary.uncovered == 2 && output.summary.covered == 3",
        "output.decision == 'escalate'",
      ],
      input: planInput(loaded, {
        only: allOutcomes,
        tasks: allTasks,
        claims: claimsAll,
        display: { section: 'markdown', link: 'markdown', emphasis: 'bold' },
      }),
    },
    {
      name: 'required-scan-unavailable-with-zero-findings',
      note: 'The one outcome whose task names the scan that never reported a state.',
      expect: [
        "size(output.rows) == 1",
        "output.rows.exists(r, r.requirement == 'REQ-SCAN' && r.state == 'covered_unassessable' && r.coverage == 'covered')",
        "output.rows.exists(r, r.requirement == 'REQ-SCAN' && r.finding_count == 0 && size(r.citations) == 0)",
        "output.rows.exists(r, r.requirement == 'REQ-SCAN' && r.record == 'fixture:execution:secret-scan')",
        "output.summary.uncovered == 0 && output.summary.unassessable == 1",
        "output.decision == 'escalate'",
      ],
      input: planInput(loaded, {
        only: ['REQ-SCAN'],
        tasks: taskFor('REQ-SCAN'),
        claims: [],
        display: { section: 'markdown', link: 'markdown', emphasis: 'italic' },
      }),
    },
    {
      name: 'stale-task-record-for-earlier-revision',
      note: 'The pricing outcome whose task names a scan that ran on an earlier revision.',
      expect: [
        "size(output.rows) == 1",
        "output.rows.exists(r, r.requirement == 'REQ-PRICING' && r.state == 'covered_stale' && r.coverage == 'covered')",
        "output.rows.exists(r, r.requirement == 'REQ-PRICING' && size(r.citations) == 0 && r.record == 'fixture:execution:pricing-scan')",
        "output.summary.covered == 1 && output.summary.missing_evidence == 1",
        "output.decision == 'escalate'",
      ],
      input: planInput(loaded, {
        only: ['REQ-PRICING'],
        tasks: taskFor('REQ-PRICING'),
        claims: ['fixture:claim:pricing-scan'],
        display: { section: 'markdown', link: 'markdown', emphasis: 'bold' },
      }),
    },
    {
      name: 'plan-row-without-any-record',
      note: 'A required outcome with no task and no record named for it, shown beside an empty host record list.',
      expect: [
        "size(output.rows) == 1",
        "output.rows.exists(r, r.requirement == 'REQ-TEST' && r.task == 'none' && r.state == 'uncovered_no_record' && r.record == 'none')",
        "output.rows.exists(r, r.requirement == 'REQ-TEST' && r.coverage == 'uncovered' && size(r.citations) == 0)",
        "output.rows.exists(r, r.requirement == 'REQ-TEST' && r.acceptance_criterion == 'A test asserts quantity 2 prices at 250 cents')",
        "output.summary.uncovered == 1 && output.summary.covered == 0",
        "output.decision == 'escalate'",
      ],
      input: planInput(loaded, {
        only: ['REQ-TEST'],
        tasks: [],
        claims: [],
        display: { section: 'plain', link: 'plain', emphasis: 'plain' },
      }),
    },
  ];
}

const invokedDirectly = process.argv?.[1] && basename(process.argv[1]) === 'score-plan.mjs';
if (invokedDirectly) {
  const loaded = loadRecords();
  const scored = planCases(loaded).map((entry) => ({
    case: entry.name,
    note: entry.note,
    ...scorePlan(entry.input),
  }));
  process.stdout.write(`${JSON.stringify({ atom: 'review-plan', cases: scored }, null, 2)}\n`);
}
