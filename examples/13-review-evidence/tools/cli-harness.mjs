#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
//
// What the CLI tests need: the built `atomic`, a private state directory that
// is outside the example's own source tree, a copy of the project to mutate,
// and the rendered turns read back. Same shape as
// `crates/atomic/tests/astro_cli.rs`: every state, config and cache directory
// is beneath the fixture's home, and none is inherited from the caller.
//
// Nothing here reaches a network or a model. `atomic check` and `atomic render`
// compile and render locally; the endpoint variables are pointed at a port
// nothing listens on, so a run that tried to reach one would fail here rather
// than somewhere else.

import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const PROJECT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
export const REPOSITORY = resolve(PROJECT, '..', '..');

/** Where scratch lives. Defaults to the build target, never the source tree. */
export function scratchRoot() {
  return process.env.ATOMIC_EXAMPLE_SCRATCH || join(REPOSITORY, 'target', 'review-evidence-scratch');
}

/** The `atomic` under test: `ATOMIC_BIN`, else this checkout's debug build. */
export function atomicBinary() {
  const configured = process.env.ATOMIC_BIN;
  if (configured) {
    if (!existsSync(configured)) throw new Error(`ATOMIC_BIN is ${configured}, which is not a file`);
    return configured;
  }
  const built = join(REPOSITORY, 'target', 'debug', 'atomic');
  if (!existsSync(built)) {
    throw new Error(`${built} is not built; run \`cargo build -p atomic-agentic --locked\` or set ATOMIC_BIN`);
  }
  return built;
}

function copyTree(source, destination) {
  mkdirSync(destination, { recursive: true });
  for (const entry of readdirSync(source, { withFileTypes: true })) {
    const from = join(source, entry.name);
    const to = join(destination, entry.name);
    if (entry.isDirectory()) {
      copyTree(from, to);
      continue;
    }
    if (!entry.isFile()) throw new Error(`${from}: not a regular file; the example holds only files`);
    writeFileSync(to, readFileSync(from));
  }
}

/**
 * A private copy of the example, in its own directory, for one test.
 *
 * The directory lives under the scratch root (the build target by default), so
 * `atomic check` can write its generated declarations without putting runtime
 * state in the source tree, and so the state directory is outside the project
 * it is checking. Returns `{ dir, home, atomic, edit, finish }`.
 */
export function projectFixture(label) {
  const root = scratchRoot();
  mkdirSync(root, { recursive: true });
  const dir = mkdtempSync(join(root, `${basename(PROJECT)}-${label}-`));
  const project = join(dir, 'project');
  copyTree(PROJECT, project);
  let checked = 0;
  let finished = false;
  const self = {
    dir: project,
    home: join(dir, 'home'),
    /** Run the built atomic in this copy, with state beneath this fixture's home. */
    atomic(args) {
      const result = spawnSync(atomicBinary(), args, {
        cwd: project,
        encoding: 'utf8',
        timeout: 120_000,
        env: {
          // Nothing inherited: a key, a base URL or a state directory from the
          // caller would make this test depend on the machine it ran on.
          PATH: '/usr/bin:/bin',
          HOME: self.home,
          ATOMIC_HOME: join(self.home, '.local', 'share', 'atomic'),
          ATOMIC_DETECT_OLLAMA_ADDR: '127.0.0.1:9',
          OPENAI_BASE_URL: 'http://127.0.0.1:9/v1',
          RUST_LOG: process.env.RUST_LOG ?? '',
        },
      });
      if (result.error) throw new Error(`atomic ${args.join(' ')} could not run: ${result.error.message}`);
      if (result.status === 0 && args[0] === 'check') {
        // `atomic check` is the command here that compiles the project and
        // writes its generated declarations; `render` prints turns and writes
        // nothing, so only a success of the former has something to prove.
        checked += 1;
      }
      return { status: result.status, stdout: result.stdout ?? '', stderr: result.stderr ?? '' };
    },
    /** Replace one file inside the copy, to switch one thing off. */
    edit(relativePath, text) {
      const path = join(project, relativePath);
      if (!existsSync(path)) throw new Error(`${relativePath}: no such file in the copy`);
      writeFileSync(path, text);
      return relativePath;
    },
    /** Add a file to the copy, such as an input drawn one way on purpose. */
    write(relativePath, text) {
      const path = join(project, relativePath);
      if (existsSync(path)) throw new Error(`${relativePath}: already in the copy; use edit to change it`);
      writeFileSync(path, text);
      return relativePath;
    },
    /** Edit by reading first, so a switch that matched nothing is an error. */
    replace(relativePath, find, replace) {
      const before = readFileSync(join(project, relativePath), 'utf8');
      if (!before.includes(find)) throw new Error(`${relativePath}: nothing matched ${JSON.stringify(find)}`);
      return self.edit(relativePath, before.split(find).join(replace));
    },
    /** Remove one file from the copy. */
    remove(relativePath) {
      const path = join(project, relativePath);
      if (!existsSync(path)) throw new Error(`${relativePath}: no such file in the copy`);
      if (!statSync(path).isFile()) throw new Error(`${relativePath}: not a file`);
      rmSync(path);
      return relativePath;
    },
    /**
     * Prove a copy that was checked really was compiled, then delete it.
     * Always called.
     *
     * `atomic check` answers success only after writing the project's
     * generated declarations, so a check that never reached the compiler
     * cannot pass for one. A copy whose every run was an intentional refusal —
     * the switched-off cases — has no success to prove.
     */
    finish() {
      if (finished) return;
      finished = true;
      if (checked > 0 && !existsSync(join(project, '.atomic', 'types'))) {
        throw new Error(`${join(project, '.atomic', 'types')}: atomic check answered success but wrote no declarations, so nothing was compiled`);
      }
      rmSync(dir, { recursive: true, force: true });
      if (existsSync(dir)) throw new Error(`${dir}: scratch directory still exists after removal`);
    },
  };
  return self;
}

/** `atomic render` on a file in the copy; throws with stderr when it fails. */
export function render(fixture, atomPath, inputPath, extra = []) {
  const run = fixture.atomic(['render', atomPath, '--input', inputPath, ...extra]);
  if (run.status !== 0) throw new Error(`atomic render ${atomPath}: ${run.stderr || run.stdout}`);
  let parsed;
  try {
    parsed = JSON.parse(run.stdout);
  } catch (error) {
    throw new Error(`atomic render ${atomPath} did not print JSON: ${run.stdout.slice(0, 400)} (${error.message})`);
  }
  for (const part of ['developer', 'user']) {
    if (typeof parsed[part] !== 'string') throw new Error(`atomic render ${atomPath}: no \`${part}\` string in its output`);
  }
  return parsed;
}

/** Every `<data-… path="…" origin="…">value</data-…>` binding in a user turn. */
export function bindings(userTurn) {
  const pattern = /<data-[0-9a-f]{16} path="([^"]+)" origin="([^"]+)">([\s\S]*?)<\/data-[0-9a-f]{16}>/g;
  const found = [];
  for (const match of userTurn.matchAll(pattern)) found.push({ path: match[1], origin: match[2], value: match[3] });
  const stripped = userTurn.replace(pattern, '');
  if (stripped.trim() !== '') {
    throw new Error(`the user turn holds text outside its data bindings: ${JSON.stringify(stripped.slice(0, 200))}`);
  }
  return found;
}

/** Every line of a turn, for assertions that name one line at a time. */
export function lines(turn) {
  return turn.split('\n');
}
