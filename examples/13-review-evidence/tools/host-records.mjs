#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// Read the immutable fixture records, and package them as the two atoms'
// inputs. This file is the host side of the example: it never scores anything.
// The join that decides whether a record answers the candidate lives in
// score-evidence.mjs, which is also what a gate would run in host code.
//
// The three files under ../evidence/ are the only source of truth. Both the
// rendered prompt and the scored answer are built from them, so a rendered
// turn and a scored row cannot disagree about a record ID, a revision or an
// origin.
//
// Every read fails closed: a record this reader cannot make sense of stops the
// run with its path and reason, rather than reading as an absent record. A
// missing required execution is a state the scorer reports; a record file that
// cannot be read, parsed, or matched against itself is a broken fixture and is
// refused here.

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const EVIDENCE_DIR = join(dirname(fileURLToPath(import.meta.url)), '..', 'evidence');

export const RECORD_FILES = Object.freeze({
  candidate: 'candidate.json',
  host: 'host-executions.json',
  claims: 'author-claims.json',
});

// What a host record's `access` and `outcome` may say. Anything else is a
// broken fixture: an outcome nobody named must never read as a pass, an
// absence, or an opened artifact.
export const HOST_OUTCOMES = Object.freeze(['pass', 'fail', 'unavailable']);
export const RECORD_ACCESS = Object.freeze(['grant', 'none']);
export const CLAIM_VALUES = Object.freeze(['pass', 'fail']);

// The display dialects the fixtures pair. `plain` writes a section label as
// words, a record as a bare ID and the emphasized phrase with no delimiters;
// `markdown` writes `## `, `[text](id)` and `*…*` / `**…**`. Everything else
// about a rendered turn — the record IDs, the access values, the candidate
// revision and the decision rules — is identical between dialects, which is
// what makes the pair a matched pair.
export const SECTIONS = Object.freeze(['plain', 'markdown']);
export const LINKS = Object.freeze(['plain', 'markdown']);
export const EMPHASES = Object.freeze(['plain', 'italic', 'bold']);

function refuse(path, why) {
  throw new Error(`${path}: ${why}`);
}

function readRecordFile(directory, name, what) {
  const path = join(directory, name);
  let text;
  try {
    text = readFileSync(path, 'utf8');
  } catch (error) {
    // Only a true "it is not there" is a missing fixture; every other failure
    // (a directory in its place, a mode that refuses the read, another I/O
    // error) is reported as the error it is, not as an absent record.
    if (error.code === 'ENOENT') refuse(path, `no ${what} fixture record file`);
    refuse(path, `${what} fixture record file could not be read: ${error.message}`);
  }
  try {
    return JSON.parse(text);
  } catch (error) {
    refuse(path, `${what} fixture record file is not JSON: ${error.message}`);
  }
}

function listOf(value, what, path) {
  if (!Array.isArray(value)) refuse(path, `${what} is not a list`);
  return value;
}

function field(entry, key, kind, path) {
  const value = entry?.[key];
  if (typeof value !== kind || value === null) {
    refuse(path, `entry ${entry?.id ?? '(no id)'} has no \`${key}\` ${kind}`);
  }
  return value;
}

function oneOf(value, allowed, what, path) {
  if (!allowed.includes(value)) {
    refuse(path, `${what} is ${JSON.stringify(value)}, which is not one of ${allowed.join(', ')}`);
  }
  return value;
}

function uniqueIds(entries, path) {
  const seen = new Set();
  for (const entry of entries) {
    const id = field(entry, 'id', 'string', path);
    if (seen.has(id)) refuse(path, `duplicate id ${id}`);
    seen.add(id);
  }
  return seen;
}

/** Every record this fixture set holds, with its integrity checked. */
export function loadRecords(directory = EVIDENCE_DIR) {
  const candidatePath = join(directory, RECORD_FILES.candidate);
  const candidate = readRecordFile(directory, RECORD_FILES.candidate, 'candidate');
  const hostFile = readRecordFile(directory, RECORD_FILES.host, 'host execution');
  const claimsFile = readRecordFile(directory, RECORD_FILES.claims, 'author claim');

  const revision = field(field(candidate, 'candidate', 'object', candidatePath), 'revision', 'string', candidatePath);
  if (!/^[0-9a-f]{40}$/.test(revision)) {
    refuse(candidatePath, `candidate revision ${JSON.stringify(revision)} is not 40 lowercase hex digits`);
  }

  const checks = listOf(candidate.required_checks, 'candidate `required_checks`', candidatePath);
  uniqueIds(checks, candidatePath);
  for (const check of checks) {
    oneOf(field(check, 'required', 'boolean', candidatePath), [true, false], `check ${check.id} \`required\``, candidatePath);
    field(check, 'title', 'string', candidatePath);
    field(check, 'record', 'string', candidatePath);
  }

  const outcomes = listOf(candidate.required_outcomes, 'candidate `required_outcomes`', candidatePath);
  uniqueIds(outcomes, candidatePath);
  for (const outcome of outcomes) {
    for (const key of ['statement', 'acceptance_criterion', 'depends_on', 'record']) {
      field(outcome, key, 'string', candidatePath);
    }
    // The host owns which check's execution answers an outcome. It travels with
    // the outcome, so a task cannot answer its requirement with whichever run
    // happens to be green.
    field(outcome, 'expected_criterion', 'string', candidatePath);
    oneOf(field(outcome, 'required', 'boolean', candidatePath), [true, false], `outcome ${outcome.id} \`required\``, candidatePath);
  }

  const tasks = listOf(candidate.proposed_tasks, 'candidate `proposed_tasks`', candidatePath);
  uniqueIds(tasks, candidatePath);
  const outcomeIds = new Set(outcomes.map((outcome) => outcome.id));
  for (const task of tasks) {
    field(task, 'title', 'string', candidatePath);
    field(task, 'depends_on', 'string', candidatePath);
    field(task, 'record', 'string', candidatePath);
    for (const covered of listOf(task.covers, `task ${task.id} \`covers\``, candidatePath)) {
      if (typeof covered !== 'string' || !outcomeIds.has(covered)) {
        refuse(candidatePath, `task ${task.id} covers ${JSON.stringify(covered)}, which is not a required outcome`);
      }
    }
  }

  const hostPath = join(directory, RECORD_FILES.host);
  if (hostFile.origin !== 'host') refuse(hostPath, `origin is ${JSON.stringify(hostFile.origin)}, which must be "host"`);
  const records = listOf(hostFile.records, 'host file `records`', hostPath);
  // An empty host set is refused, not read as "nothing was run": that is the
  // difference between a fixture and a quiet pass.
  if (records.length === 0) refuse(hostPath, 'no host execution records; an empty host set would read as "nothing required anything"');
  uniqueIds(records, hostPath);
  for (const record of records) {
    oneOf(field(record, 'outcome', 'string', hostPath), HOST_OUTCOMES, `record ${record.id} \`outcome\``, hostPath);
    oneOf(field(record, 'access', 'string', hostPath), RECORD_ACCESS, `record ${record.id} \`access\``, hostPath);
    field(record, 'criterion', 'string', hostPath);
    field(record, 'revision', 'string', hostPath);
    field(record, 'detail', 'string', hostPath);
    for (const finding of listOf(record.findings, `record ${record.id} \`findings\``, hostPath)) {
      if (typeof finding !== 'string') refuse(hostPath, `record ${record.id} holds a finding that is not a string`);
    }
  }

  const claimsPath = join(directory, RECORD_FILES.claims);
  if (claimsFile.origin !== 'author') refuse(claimsPath, `origin is ${JSON.stringify(claimsFile.origin)}, which must be "author"`);
  const claims = listOf(claimsFile.claims, 'author file `claims`', claimsPath);
  uniqueIds(claims, claimsPath);
  for (const claim of claims) {
    field(claim, 'criterion', 'string', claimsPath);
    field(claim, 'basis', 'string', claimsPath);
    oneOf(field(claim, 'claim', 'string', claimsPath), CLAIM_VALUES, `claim ${claim.id} \`claim\``, claimsPath);
  }
  const assumptions = listOf(claimsFile.assumptions, 'author file `assumptions`', claimsPath);
  uniqueIds(assumptions, claimsPath);
  for (const assumption of assumptions) field(assumption, 'text', 'string', claimsPath);

  // Every record a required check or outcome names must exist. A check that
  // names no record at all says so with the literal `none`, so a typo cannot
  // quietly become "there was no evidence for it".
  const recordById = new Map(records.map((record) => [record.id, record]));
  const recordIds = new Set(recordById.keys());
  for (const [entry, where] of [...checks.map((c) => [c, 'check']), ...outcomes.map((o) => [o, 'outcome'])]) {
    if (entry.record !== 'none' && !recordIds.has(entry.record)) {
      refuse(candidatePath, `${where} ${entry.id} names ${entry.record}, which no host execution record holds (write "none" for a declared absence)`);
    }
  }
  // A named record must be an execution of the check that names it. Without
  // this one passing run answers for every check allowed to point at it, and a
  // missing execution borrows another check's pass and looks satisfied.
  for (const check of checks) {
    if (check.record === 'none') continue;
    const record = recordById.get(check.record);
    if (record.criterion !== check.id) {
      refuse(
        candidatePath,
        `check ${check.id} names ${check.record}, which was recorded for ${record.criterion}; a check may name only a record whose \`criterion\` is itself`,
      );
    }
  }
  for (const task of tasks) {
    if (task.record !== 'none' && !recordIds.has(task.record)) {
      refuse(candidatePath, `task ${task.id} names ${task.record}, which no host execution record holds (write "none" for a declared absence)`);
    }
  }
  const checkIds = new Set(checks.map((check) => check.id));
  for (const claim of claims) {
    if (!checkIds.has(claim.criterion)) {
      refuse(claimsPath, `claim ${claim.id} is about ${claim.criterion}, which is not a required check`);
    }
  }
  // A host record whose `criterion` matches no authored check is a record no
  // row would ever show. Refuse it rather than drop it silently.
  for (const record of records) {
    if (!checkIds.has(record.criterion)) {
      refuse(hostPath, `record ${record.id} answers ${record.criterion}, which no required check names`);
    }
  }
  // A required outcome must expect a check the candidate states, or say plainly
  // that no check answers it.
  for (const outcome of outcomes) {
    if (outcome.expected_criterion !== 'none' && !checkIds.has(outcome.expected_criterion)) {
      refuse(candidatePath, `outcome ${outcome.id} expects ${outcome.expected_criterion}, which no required check states`);
    }
  }
  // A covered outcome is answered only by a run of the check it names in
  // `expected_criterion`. Presence was checked above — every task record names a
  // record that exists, and every `covers` names an outcome that exists — so the
  // only thing left to get wrong is which check that run was for. Without this,
  // pointing a task at another check's passing run reads as coverage delivered
  // for this candidate.
  const outcomeById = new Map(outcomes.map((outcome) => [outcome.id, outcome]));
  for (const task of tasks) {
    for (const covered of task.covers) {
      const outcome = outcomeById.get(covered);
      if (task.record === 'none') continue;
      if (outcome.expected_criterion === 'none') {
        refuse(
          candidatePath,
          `task ${task.id} names ${task.record} for ${outcome.id}, which expects no check; write "none" for that task's record`,
        );
      }
      const record = recordById.get(task.record);
      if (record.criterion !== outcome.expected_criterion) {
        refuse(
          candidatePath,
          `task ${task.id} names ${task.record}, which was recorded for ${record.criterion}; ${outcome.id} is answered only by a run of ${outcome.expected_criterion}`,
        );
      }
    }
  }

  return {
    candidate,
    revision,
    checks: checks.map((check) => ({ ...check })),
    outcomes: outcomes.map((outcome) => ({ ...outcome })),
    tasks: tasks.map((task) => ({ ...task, covers: [...task.covers] })),
    records: records.map((record) => ({ ...record, findings: [...record.findings] })),
    claims: claims.map((claim) => ({ ...claim })),
    assumptions: assumptions.map((assumption) => ({ ...assumption })),
  };
}

/** The dialect an input carries, checked against what the atoms can render. */
export function dialect({ section = 'plain', link = 'plain', emphasis = 'plain' } = {}) {
  return {
    section: oneOf(section, SECTIONS, 'display `section`', 'dialect'),
    link: oneOf(link, LINKS, 'display `link`', 'dialect'),
    emphasis: oneOf(emphasis, EMPHASES, 'display `emphasis`', 'dialect'),
  };
}

function sourceRef(loaded) {
  return {
    revision: loaded.revision,
    repository: loaded.candidate.candidate.source.repository,
    change: loaded.candidate.candidate.source.change,
    base_revision: loaded.candidate.candidate.source.base_revision,
  };
}

function hostRows(loaded) {
  return loaded.records.map((record) => ({
    id: record.id,
    origin: 'host',
    criterion: record.criterion,
    revision: record.revision,
    outcome: record.outcome,
    access: record.access,
    findings: [...record.findings],
    detail: record.detail,
  }));
}

function claimRows(loaded) {
  return loaded.claims.map((claim) => ({
    id: claim.id,
    origin: 'author',
    criterion: claim.criterion,
    claim: claim.claim,
    basis: claim.basis,
  }));
}

/**
 * The `review-evidence` input for one case.
 *
 * `only` limits which authored checks the case asks about, and `claims`
 * selects the claim rows. Both are how the fixture cases differ: the records
 * themselves are never edited per case.
 */
export function evidenceInput(loaded, { display = {}, only = null, claims = null } = {}) {
  const checks = (only ? loaded.checks.filter((check) => only.includes(check.id)) : loaded.checks).map(
    (check) => ({ id: check.id, title: check.title, record: check.record, required: check.required }),
  );
  const asked = new Set(checks.map((check) => check.id));
  const claimRows = (claims === null ? loaded.claims : loaded.claims.filter((claim) => claims.includes(claim.id))).map(
    (claim) => ({ id: claim.id, origin: 'author', criterion: claim.criterion, claim: claim.claim, basis: claim.basis }),
  );
  for (const claim of claimRows) {
    if (!asked.has(claim.criterion)) {
      refuse(EVIDENCE_DIR, `case asks about no check ${claim.criterion}, which claim ${claim.id} is about`);
    }
  }
  const shown = new Set(checks.map((check) => check.record).filter((id) => id !== 'none'));
  const rows = hostRows(loaded).filter((record) => shown.has(record.id));
  return {
    candidate: sourceRef(loaded),
    required_checks: checks,
    host_records: rows,
    author_claims: claimRows,
    display: dialect(display),
  };
}

/**
 * The `review-plan` input for one case.
 *
 * A plan row's record is the one its task says ran; a requirement with no task
 * keeps the record it names itself, so an uncovered requirement still shows
 * what evidence it looked for. That choice is made here, on the host side, so
 * the model and the scorer see exactly one record per row.
 */
export function planInput(loaded, { display = {}, only = null, tasks = null, claims = null } = {}) {
  const outcomes = (only ? loaded.outcomes.filter((outcome) => only.includes(outcome.id)) : loaded.outcomes).map(
    (outcome) => ({
      id: outcome.id,
      statement: outcome.statement,
      acceptance_criterion: outcome.acceptance_criterion,
      depends_on: outcome.depends_on,
      record: outcome.record,
      expected_criterion: outcome.expected_criterion,
      required: outcome.required,
    }),
  );
  const asked = new Set(outcomes.map((outcome) => outcome.id));
  const taskRows = (tasks === null ? loaded.tasks : loaded.tasks.filter((task) => tasks.includes(task.id))).map(
    (task) => ({ id: task.id, title: task.title, covers: [...task.covers], depends_on: task.depends_on, record: task.record }),
  );
  const claimRows = (claims === null ? loaded.claims : loaded.claims.filter((claim) => claims.includes(claim.id))).map(
    (claim) => ({ id: claim.id, origin: 'author', criterion: claim.criterion, claim: claim.claim, basis: claim.basis }),
  );
  const assumptionRows = loaded.assumptions.map((assumption) => ({
    id: assumption.id,
    origin: 'author',
    text: assumption.text,
  }));
  // Every task must cover a requirement this case asks about, or its row would
  // vanish and leave a requirement looking uncovered by accident.
  for (const task of taskRows) {
    for (const covered of task.covers) {
      if (!asked.has(covered)) {
        refuse(EVIDENCE_DIR, `case drops ${covered}, which task ${task.id} covers; drop the task too`);
      }
    }
  }
  const recordsByName = new Map(loaded.records.map((record) => [record.id, record]));
  const taskByRequirement = new Map();
  for (const task of taskRows) {
    for (const covered of task.covers) {
      if (taskByRequirement.has(covered)) {
        refuse(EVIDENCE_DIR, `${covered} is covered by both ${taskByRequirement.get(covered).id} and ${task.id}; the plan must name one task per requirement`);
      }
      taskByRequirement.set(covered, task);
    }
  }
  const shown = new Set();
  for (const outcome of outcomes) {
    const task = taskByRequirement.get(outcome.id);
    const named = task ? task.record : outcome.record;
    if (named !== 'none') shown.add(named);
  }
  const rows = hostRows(loaded).filter((record) => shown.has(record.id));
  return {
    plan: {
      record_id: loaded.candidate.plan.record_id,
      revision: loaded.candidate.plan.revision,
      repository: loaded.candidate.candidate.source.repository,
      change: loaded.candidate.candidate.source.change,
    },
    required_outcomes: outcomes,
    proposed_tasks: taskRows,
    host_records: rows,
    author_claims: claimRows,
    assumptions: assumptionRows,
    display: dialect(display),
  };
}

/** The record a plan row answers: its task's, or its own when it has no task. */
export function planRowRecord(outcome, taskByRequirement) {
  const task = taskByRequirement.get(outcome.id);
  return task ? task.record : outcome.record;
}
