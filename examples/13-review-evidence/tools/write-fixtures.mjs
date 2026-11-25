#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// Rewrite the committed fixture and eval files from the immutable records under
// `evidence/`, the case list in the two scorers, and the scorers themselves:
//
//   fixtures/<atom>/<case>.input.json     one input per case, for `atomic render`
//   fixtures/<atom>/cases.outputs.jsonl   one golden answer per case, in order
//   evals/<atom>/cases.jsonl              the same input, with its assertions
//
//   node examples/13-review-evidence/tools/write-fixtures.mjs
//
// The golden answer of a case is the host's own answer for the records the case
// shows, which is why a golden and a score cannot drift: they are the same
// computation. The CEL assertions are authored by hand beside each case, and a
// case passes only when the hand-written expectation and the computed answer
// agree — `atomic check`, the catalog checker and `fixture-check.test.mjs` all
// re-check that. `fixture-check.test.mjs` never runs this script: the committed
// files are the deliverable, and drift in any of them is a failure there.

import { mkdirSync, writeFileSync } from 'node:fs';
import { basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { evidenceCases, scoreEvidence } from './score-evidence.mjs';
import { planCases, scorePlan } from './score-plan.mjs';

const PROJECT = join(dirname(fileURLToPath(import.meta.url)), '..');

function write(path, text) {
  const destination = join(PROJECT, path);
  mkdirSync(dirname(destination), { recursive: true });
  writeFileSync(destination, text);
  return path;
}

/** One atom's three files, from its cases and its scorer. */
function writeProject(atom, cases, score) {
  const written = [];
  for (const entry of cases) {
    written.push(
      write(`fixtures/${atom}/${entry.name}.input.json`, `${JSON.stringify(entry.input, null, 2)}\n`),
    );
  }
  const rows = cases.map((entry) => score(entry.input));
  written.push(
    write(`fixtures/${atom}/cases.outputs.jsonl`, `${rows.map((row) => JSON.stringify(row)).join('\n')}\n`),
  );
  written.push(
    write(
      `evals/${atom}/cases.jsonl`,
      `${cases.map((entry, index) => JSON.stringify({ input: entry.input, expect: entry.expect })).join('\n')}\n`,
    ),
  );
  return written;
}

const invokedDirectly = process.argv?.[1] && basename(process.argv[1]) === 'write-fixtures.mjs';
if (invokedDirectly) {
  const written = [
    ...writeProject('review-evidence', evidenceCases(), (input) => scoreEvidence(input)),
    ...writeProject('review-plan', planCases(), (input) => scorePlan(input)),
  ];
  for (const path of written) process.stdout.write(`wrote ${path}\n`);
  process.stdout.write(
    'from evidence/candidate.json, evidence/host-executions.json and evidence/author-claims.json\n',
  );
}
