#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// Score the review-evidence atom's contract from the records a run was given.
// No model, no network, no Factory system: this reads the fixture input — the
// same three bindings the prompt shows — and answers, per required check,
// whether the host holds evidence for *this* candidate.
//
// The rules, in the order they are applied. Each is the whole of one decision,
// and the tests name which one they exercise:
//
//   1. A record you cannot open credits nothing            -> record_inaccessible
//   2. A record for another revision credits nothing       -> record_stale
//   3. A scan that could not run has no findings and no state
//                                                          -> record_unavailable
//   4. A pass for this candidate is evidence               -> record_current_pass
//   5. A fail for this candidate is evidence too           -> record_current_fail
//   6. Nothing named, or nothing recorded                  -> record_missing
//
// Before any of them, one join rule: the record a check names must be an
// execution of that check. A record whose `criterion` is another check answers
// nothing, and scoring it is refused rather than read as that check's evidence.
//
// A required check ends `satisfied`, `failed`, `missing_evidence` or
// `unassessable`; a check that is not required ends `not_required` unless it
// passed. An author claim is never evidence: it can only be `claimed`, and a
// claim the host records do not support is an `uncredited_claim`.

import { basename } from 'node:path';
import { evidenceInput, loadRecords } from './host-records.mjs';

export const EVIDENCE_STATES = Object.freeze([
  'record_current_pass',
  'record_current_fail',
  'record_unavailable',
  'record_stale',
  'record_inaccessible',
  'record_missing',
]);

/**
 * Which of the six states one required check is in.
 *
 * `record` is the host record the check names, or null when nothing was
 * recorded for it; `revision` is the candidate the review is about. Access is
 * asked before revision and outcome: a record whose contents cannot be opened
 * says nothing, whatever its fields claim.
 */
export function recordState(record, revision) {
  if (record === null || record === undefined) return 'record_missing';
  if (record.access === 'none') return 'record_inaccessible';
  if (record.access !== 'grant') {
    throw new Error(`${record.id}: access is ${JSON.stringify(record.access)}, which must be "grant" or "none"`);
  }
  if (record.revision !== revision) return 'record_stale';
  switch (record.outcome) {
    case 'unavailable':
      return 'record_unavailable';
    case 'fail':
      return 'record_current_fail';
    case 'pass':
      return 'record_current_pass';
    default:
      // Unknown means refused: a record nobody can classify is never a pass.
      throw new Error(`${record.id}: outcome is ${JSON.stringify(record.outcome)}, which this scorer cannot classify`);
  }
}

function outcomeFor(state, required) {
  switch (state) {
    case 'record_current_pass':
      return 'satisfied';
    case 'record_current_fail':
      return 'failed';
    case 'record_unavailable':
      return required ? 'unassessable' : 'not_required';
    default:
      return required ? 'missing_evidence' : 'not_required';
  }
}

/**
 * Score one `review-evidence` input.
 *
 * Returns `{decision, rows, summary}`. `rows` has exactly one entry per
 * required check, in the order the checks are authored, so a required row
 * survives every kind of missing record.
 */
export function scoreEvidence(input) {
  const revision = input.candidate.revision;
  const byId = new Map();
  for (const record of input.host_records) {
    if (byId.has(record.id)) throw new Error(`${record.id}: two host execution records share one id`);
    byId.set(record.id, record);
  }
  const claims = input.author_claims ?? [];
  const rows = input.required_checks.map((check) => {
    // `byId` is keyed by record ID, so this lookup cannot return a record other
    // than the one the check names — no guard is needed to say so, and one here
    // could never fail. What can be wrong, and what the reader refuses too, is
    // the record being about a different check.
    const record = check.record === 'none' ? null : (byId.get(check.record) ?? null);
    if (record !== null && record.criterion !== check.id) {
      // Refused here as well as in the reader: an input can arrive from a file,
      // an eval case or a workflow binding, and none of those paths re-runs the
      // reader. A check borrowing another check's passing run is the one swap
      // that would make a missing execution look satisfied.
      throw new Error(
        `${check.id}: ${record.id} was recorded for ${record.criterion}, not for this check, so it is not evidence here`,
      );
    }
    const state = recordState(record, revision);
    const credit = state === 'record_current_pass' ? 'host_record' : 'not_credited';
    const supporting = claims.filter((claim) => claim.criterion === check.id && claim.claim === 'pass');
    const credited = credit === 'host_record';
    // Only a record that is about this candidate and that can be opened is a
    // citation. A stale, unavailable or sealed record keeps its id in `record`
    // — preserved for provenance — and cites nothing.
    const citable = state === 'record_current_pass' || state === 'record_current_fail';
    return {
      criterion: check.id,
      required: check.required,
      state,
      record: check.record,
      citations: citable ? [check.record] : [],
      finding_count: record && state !== 'record_inaccessible' ? record.findings.length : 0,
      claimed: supporting.length > 0,
      claim_ids: supporting.map((claim) => claim.id),
      credit,
      outcome: outcomeFor(state, check.required),
      detail: state === 'record_unavailable' && record.findings.length === 0
        ? `${check.id}: the required scan reported no findings because it could not run. That is missing coverage, not a clean result.`
        : !credited && supporting.length > 0
          ? `${check.id}: claimed by the author, with no passing host record for this candidate.`
          : credited
            ? `${check.id}: satisfied by ${check.record} on this revision.`
            : `${check.id}: no host evidence for this candidate.`,
    };
  });

  const requiredRows = rows.filter((row) => row.required);
  const uncreditedClaims = rows.filter((row) => row.required && row.claimed && row.credit !== 'host_record');
  const decision = requiredRows.some((row) => row.outcome === 'failed')
    ? 'reject'
    : requiredRows.some((row) => row.outcome === 'missing_evidence' || row.outcome === 'unassessable') ||
        uncreditedClaims.length > 0
      ? 'escalate'
      : 'approve';

  const count = (wanted) => requiredRows.filter((row) => row.outcome === wanted).length;
  return {
    decision,
    rows,
    summary: {
      required: requiredRows.length,
      satisfied: count('satisfied'),
      failed: count('failed'),
      missing_evidence: count('missing_evidence'),
      unassessable: count('unassessable'),
      uncredited_claims: uncreditedClaims.length,
    },
  };
}

/**
 * The fixture cases this scorer is run over: the input each one builds, the
 * hand-written CEL assertions committed in `evals/review-evidence/cases.jsonl`,
 * and a note saying what the case is for. A case's expected answer is this
 * scorer's own answer for those records; the assertions here are written by
 * hand, so a case only holds when the two agree.
 */
export function evidenceCases(loaded = loadRecords()) {
  const all = loaded.checks.map((check) => check.id);
  const claimsAll = loaded.claims.map((claim) => claim.id);
  // `current` is the matched pair: one candidate, one set of checks, one claim,
  // one output, and two display dialects. Everything the pair differs by sits
  // in `display`, which is why scoring it twice must give the same answer.
  const current = { only: ['docs-links', 'ownership-review'], claims: ['fixture:claim:docs-links'] };
  return [
    {
      name: 'claimed-pass-without-host-record',
      note: 'The author claims a clean secret scan; the required scan never reported a state.',
      expect: [
        "size(output.rows) == 6",
        "output.rows.exists(r, r.criterion == 'secret-scan' && r.claimed && r.state == 'record_unavailable' && r.finding_count == 0)",
        "output.rows.exists(r, r.criterion == 'secret-scan' && r.outcome == 'unassessable' && r.credit == 'not_credited')",
        "output.rows.exists(r, r.criterion == 'docs-links' && r.state == 'record_current_pass' && r.credit == 'host_record')",
        "output.rows.exists(r, r.criterion == 'docs-build' && r.outcome == 'failed' && size(r.citations) == 1)",
        "output.summary.uncredited_claims == 2",
        "output.decision == 'reject'",
      ],
      input: evidenceInput(loaded, { only: all, claims: claimsAll, display: { section: 'markdown', link: 'markdown', emphasis: 'bold' } }),
    },
    {
      name: 'stale-pass-for-earlier-revision',
      note: 'The only pricing-scan pass belongs to an earlier revision.',
      expect: [
        "size(output.rows) == 1",
        "output.rows.exists(r, r.criterion == 'pricing-scan' && r.state == 'record_stale' && r.outcome == 'missing_evidence')",
        "output.rows.exists(r, r.criterion == 'pricing-scan' && r.record == 'fixture:execution:pricing-scan' && size(r.citations) == 0)",
        "output.rows.exists(r, r.criterion == 'pricing-scan' && r.claimed && r.credit == 'not_credited')",
        "output.summary.missing_evidence == 1",
        "output.decision == 'escalate'",
      ],
      input: evidenceInput(loaded, { only: ['pricing-scan'], claims: ['fixture:claim:pricing-scan'], display: { section: 'markdown', link: 'markdown', emphasis: 'bold' } }),
    },
    {
      name: 'unavailable-scan-with-zero-findings',
      note: 'The required scan reported zero findings because it could not run.',
      expect: [
        "size(output.rows) == 1",
        "output.rows.exists(r, r.criterion == 'secret-scan' && r.state == 'record_unavailable' && r.finding_count == 0 && r.outcome == 'unassessable')",
        "output.rows.exists(r, r.criterion == 'secret-scan' && size(r.citations) == 0 && r.credit == 'not_credited')",
        "output.summary.unassessable == 1 && output.summary.satisfied == 0",
        "output.decision == 'escalate'",
      ],
      input: evidenceInput(loaded, { only: ['secret-scan'], claims: ['fixture:claim:secret-scan'], display: { section: 'markdown', link: 'markdown', emphasis: 'bold' } }),
    },
    {
      name: 'inaccessible-pass-for-this-revision',
      note: 'A passing changelog-check record exists whose contents can no longer be opened.',
      expect: [
        "size(output.rows) == 1",
        "output.rows.exists(r, r.criterion == 'changelog-check' && r.state == 'record_inaccessible' && r.outcome == 'not_required')",
        "output.rows.exists(r, r.criterion == 'changelog-check' && r.record == 'fixture:execution:changelog-check' && size(r.citations) == 0)",
        "output.rows.exists(r, r.criterion == 'changelog-check' && !r.claimed)",
        // Nothing required in this case, so nothing is satisfied: the sealed
        // record credits nothing and cites nothing.
        "output.summary.required == 0 && output.summary.satisfied == 0 && output.summary.missing_evidence == 0",
      ],
      input: evidenceInput(loaded, { only: ['changelog-check'], claims: [], display: { section: 'markdown', link: 'markdown', emphasis: 'italic' } }),
    },
    {
      name: 'current-pass-omitted-by-author-plain',
      note: 'ownership-review passes on this revision and the author never claims it; every delimiter is off.',
      expect: [
        "size(output.rows) == 2",
        "output.rows.exists(r, r.criterion == 'ownership-review' && !r.claimed && r.credit == 'host_record' && r.outcome == 'satisfied')",
        "output.rows.exists(r, r.criterion == 'docs-links' && r.claimed && r.credit == 'host_record')",
        "output.summary.uncredited_claims == 0",
        "output.decision == 'approve'",
      ],
      input: evidenceInput(loaded, { ...current, display: { section: 'plain', link: 'plain', emphasis: 'plain' } }),
    },
    {
      name: 'current-pass-omitted-by-author-markdown',
      note: 'The same candidate, records and claim as the plain case, with a Markdown heading, a Markdown link and bold.',
      expect: [
        "size(output.rows) == 2",
        "output.rows.exists(r, r.criterion == 'ownership-review' && !r.claimed && r.credit == 'host_record' && r.outcome == 'satisfied')",
        "output.rows.exists(r, r.criterion == 'docs-links' && r.claimed && r.credit == 'host_record')",
        "output.summary.uncredited_claims == 0",
        "output.decision == 'approve'",
      ],
      input: evidenceInput(loaded, { ...current, display: { section: 'markdown', link: 'markdown', emphasis: 'bold' } }),
    },
  ];
}

const invokedDirectly = process.argv?.[1] && basename(process.argv[1]) === 'score-evidence.mjs';
if (invokedDirectly) {
  const loaded = loadRecords();
  const scored = evidenceCases(loaded).map((entry) => ({
    case: entry.name,
    note: entry.note,
    ...scoreEvidence(entry.input),
  }));
  process.stdout.write(`${JSON.stringify({ atom: 'review-evidence', cases: scored }, null, 2)}\n`);
}
