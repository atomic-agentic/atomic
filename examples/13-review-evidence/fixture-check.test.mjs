#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// The committed files against each other: the eval rows, the inputs beside
// them, the golden answers, and the immutable records they were built from.
// These tests run no scorer authoritatively over the future — they re-check
// what is committed, so a hand-edit on one side of a pair fails here.
//
//   node --test examples/13-review-evidence/*.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { cpSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { RECORD_FILES, loadRecords } from './tools/host-records.mjs';
import { evidenceCases, scoreEvidence } from './tools/score-evidence.mjs';
import { planCases, scorePlan } from './tools/score-plan.mjs';

const PROJECT = dirname(fileURLToPath(import.meta.url));
const HOST_FILE = join(PROJECT, 'evidence', 'host-executions.json');
const loaded = loadRecords();

const ATOMS = [
  { atom: 'review-evidence', cases: evidenceCases(loaded), score: scoreEvidence },
  { atom: 'review-plan', cases: planCases(loaded), score: scorePlan },
];

function readJson(path) {
  assert.ok(existsSync(path), `${path}: missing`);
  return JSON.parse(readFileSync(path, 'utf8'));
}

function readJsonl(path) {
  assert.ok(existsSync(path), `${path}: missing`);
  const rows = readFileSync(path, 'utf8')
    .split('\n')
    .filter((line) => line.trim() !== '');
  assert.ok(rows.length > 0, `${path}: no cases`);
  return rows.map((line) => JSON.parse(line));
}

test('every eval row is the fixture input beside it, in the same order', () => {
  for (const { atom, cases } of ATOMS) {
    const rows = readJsonl(join(PROJECT, 'evals', atom, 'cases.jsonl'));
    assert.equal(rows.length, cases.length, `${atom}: one eval row per case`);
    for (const [index, entry] of cases.entries()) {
      const fixture = readJson(join(PROJECT, 'fixtures', atom, `${entry.name}.input.json`));
      assert.deepEqual(
        rows[index].input,
        fixture,
        `${atom}: eval row ${index + 1} and fixtures/${atom}/${entry.name}.input.json disagree`,
      );
      assert.deepEqual(
        rows[index].input,
        entry.input,
        `${atom}: eval row ${index + 1} and the case ${entry.name} disagree`,
      );
      assert.ok(Array.isArray(rows[index].expect) && rows[index].expect.length > 0, `${atom}: eval row ${index + 1} asserts nothing`);
      assert.ok(
        rows[index].expect.every((line) => typeof line === 'string' && line.length > 0),
        `${atom}: eval row ${index + 1} holds an assertion that is not a line of CEL`,
      );
    }
  }
});

test("each golden answer is the host's own answer for the records that case shows", () => {
  for (const { atom, cases, score } of ATOMS) {
    const goldens = readJsonl(join(PROJECT, 'fixtures', atom, 'cases.outputs.jsonl'));
    assert.equal(goldens.length, cases.length, `${atom}: one golden answer per case`);
    for (const [index, entry] of cases.entries()) {
      assert.deepEqual(
        goldens[index],
        score(entry.input),
        `${atom}/${entry.name}: the committed golden and the host answer disagree`,
      );
    }
  }
});

test('no golden cites a record the case cannot open for this candidate', () => {
  const openable = new Set(
    JSON.parse(readFileSync(HOST_FILE, 'utf8')).records
      .filter((record) => record.access === 'grant' && record.revision === loaded.revision)
      .map((record) => record.id),
  );
  assert.ok(openable.size > 0, 'the fixture would prove nothing if no record were openable');
  const sealed = JSON.parse(readFileSync(HOST_FILE, 'utf8')).records.filter((record) => record.access === 'none');
  assert.equal(sealed.length, 1, 'the case with inaccessible evidence is still in the fixture');
  for (const { atom } of ATOMS) {
    for (const golden of readJsonl(join(PROJECT, 'fixtures', atom, 'cases.outputs.jsonl'))) {
      for (const row of golden.rows) {
        for (const citation of row.citations) {
          assert.ok(openable.has(citation), `${atom}: ${row.criterion ?? row.requirement} cites ${citation}, which is not openable for this candidate`);
          assert.notEqual(citation, sealed[0].id, `${atom}: ${sealed[0].id} cannot be opened, so nothing may cite it`);
        }
        assert.ok(
          row.citations.every((id) => id === row.record),
          `${atom}: ${row.criterion ?? row.requirement} cites something other than the record its row answers`,
        );
        if (row.record !== 'none') {
          assert.ok(
            loaded.records.some((record) => record.id === row.record),
            `${atom}: ${row.criterion ?? row.requirement} names ${row.record}, which the fixture holds no such record for`,
          );
        }
      }
    }
  }
});

test('both Atoms are handed one record shape, by the same reference', () => {
  const reference = '$ref: https://atomic.example/schemas/execution-record-v1';
  const record = readFileSync(join(PROJECT, 'schemas', 'execution-record.schema.yaml'), 'utf8');
  assert.match(record, new RegExp(`^\\$id: https://atomic\\.example/schemas/execution-record-v1$`, 'm'));
  for (const atom of ['review-evidence', 'review-plan']) {
    const input = readFileSync(join(PROJECT, 'atoms', `${atom}.input.schema.yaml`), 'utf8');
    assert.ok(input.includes(reference), `${atom}: its host records do not reference the shared execution-record schema`);
    assert.ok(
      input.includes('$ref: https://atomic.example/schemas/author-claim-v1'),
      `${atom}: its author claims do not reference the shared author-claim schema`,
    );
  }
});

test('every case is scored against the candidate the records name', () => {
  for (const { atom, cases } of ATOMS) {
    for (const entry of cases) {
      const revision = atom === 'review-evidence' ? entry.input.candidate.revision : entry.input.plan.revision;
      assert.equal(revision, loaded.revision, `${atom}/${entry.name}: a revision the records do not name`);
      const recordsById = new Map(loaded.records.map((record) => [record.id, record]));
      for (const shown of entry.input.host_records) {
        // The binding adds one field the record file does not carry per record:
        // the origin, which travels with the row into the prompt. Everything
        // else is the fixture's record, verbatim.
        assert.deepEqual(
          shown,
          { ...recordsById.get(shown.id), origin: 'host' },
          `${atom}/${entry.name}: ${shown.id} is not the record the fixture holds`,
        );
      }
      for (const claim of entry.input.author_claims) {
        assert.equal(claim.origin, 'author', `${atom}/${entry.name}: a claim arrived from somewhere else`);
      }
      for (const record of entry.input.host_records) {
        assert.equal(record.origin, 'host', `${atom}/${entry.name}: a host record arrived from somewhere else`);
      }
    }
  }
});

test('the committed borrow fixture is refused for its join, not for anything else', () => {
  // `evidence-bad-join/` is the one shape the reader must never let through: a
  // required check answered by a run recorded for a different check. It is
  // committed as files, so the refusal is about the fixture and not only about
  // a value built inside a test.
  const borrowed = join(PROJECT, 'evidence-bad-join');
  assert.throws(
    () => loadRecords(borrowed),
    /check docs-build names fixture:execution:docs-links, which was recorded for docs-links; a check may name only a record whose `criterion` is itself/,
    'docs-build was accepted on the strength of docs-links’ passing run',
  );

  // The same files with the borrow undone load, which is what makes the refusal
  // above about the join rather than about a file this fixture left out. docs-build
  // then has no record at all, and is shown as missing evidence.
  const directory = mkdtempSync(join(tmpdir(), 'atomic-review-evidence-bad-join-'));
  try {
    const fixed = join(directory, 'evidence');
    cpSync(borrowed, fixed, { recursive: true });
    const path = join(fixed, RECORD_FILES.candidate);
    const candidate = JSON.parse(readFileSync(path, 'utf8'));
    candidate.required_checks.find((check) => check.id === 'docs-build').record = 'none';
    writeFileSync(path, JSON.stringify(candidate));

    const records = loadRecords(fixed);
    assert.equal(records.checks.length, 2);
    const scored = scoreEvidence({
      candidate: { revision: records.revision },
      required_checks: records.checks,
      host_records: records.records.map((record) => ({ ...record, origin: 'host' })),
      author_claims: [],
    });
    const build = scored.rows.find((row) => row.criterion === 'docs-build');
    assert.equal(build.state, 'record_missing');
    assert.equal(build.outcome, 'missing_evidence');
    assert.equal(scored.decision, 'escalate');
    assert.equal(
      scored.rows.find((row) => row.criterion === 'docs-links').state,
      'record_current_pass',
      'the run that did happen still answers its own check',
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('the matched pair differs only in how it is drawn', () => {
  const plain = readJson(join(PROJECT, 'fixtures', 'review-evidence', 'current-pass-omitted-by-author-plain.input.json'));
  const markdown = readJson(join(PROJECT, 'fixtures', 'review-evidence', 'current-pass-omitted-by-author-markdown.input.json'));
  assert.deepEqual(plain.display, { section: 'plain', link: 'plain', emphasis: 'plain' });
  assert.deepEqual(markdown.display, { section: 'markdown', link: 'markdown', emphasis: 'bold' });
  const withoutDisplay = ({ display, ...rest }) => rest;
  assert.deepEqual(withoutDisplay(plain), withoutDisplay(markdown), 'everything but the delimiters is identical');
  assert.ok(markdown.host_records.length > 0 && plain.host_records.length === markdown.host_records.length);
  assert.equal(
    markdown.author_claims.length,
    1,
    'one claim, so the other current pass in the pair is one the author never mentioned',
  );
});
