// SPDX-License-Identifier: Apache-2.0
// Mutate disposable copies only. A passing mutant is a failed control check.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtemp, cp, mkdir, readFile, writeFile, symlink, rm } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import os from 'node:os';
import path from 'node:path';

const root = fileURLToPath(new URL('../', import.meta.url));
const temp = await mkdtemp(path.join(os.tmpdir(), 'atomic-astro-controls-'));
try {
  for (const dir of ['fixtures', 'src', 'test', 'schemas']) await cp(path.join(root, dir), path.join(temp, dir), { recursive: true });
  await cp(path.join(root, 'authoring.d.ts'), path.join(temp, 'authoring.d.ts'));
  await symlink(path.join(root, 'node_modules'), path.join(temp, 'node_modules'));
  const originals = new Map();
  for (const file of ['src/renderer.mjs', 'src/intrinsics.mjs', 'src/generate.mjs', 'src/context.mjs']) originals.set(file, await readFile(path.join(temp, file), 'utf8'));
  const controls = [
    {
      name: 'external data isolation',
      before: "result.developer += `[${value.at}: see <data-${suffix}> in the user turn]`;",
      after: 'result.developer += text;',
      expected: 'hostile input stays opaque through an imported component',
    },
    {
      name: 'frontmatter statement allowlist',
      before: "else error(unit, null, `unsupported script statement: ${ts.SyntaxKind[statement.kind]}; script code is never executed`);",
      after: 'else { /* control: silently allow unsupported statements */ }',
      expected: 'rejects script statements without executing it',
    },
    {
      name: 'contract AST allowlist',
      before: 'validateExpression(node.body, unit, new Set(names));',
      after: '/* control: contract AST validation removed */',
      expected: 'contract rejects inactive executable branches at load',
    },
    {
      name: 'meeting containment', file: 'src/intrinsics.mjs',
      before: 'low <= start && end <= high && low <= high',
      after: 'true',
      expected: 'meeting containment rejects out-of-slot and nonpositive durations',
    },
    {
      name: 'scorer threshold',
      before: 'passed: mean >= threshold', after: 'passed: true',
      expected: 'score aggregation enforces the declared threshold and rejects empty/non-finite inputs',
    },
    {
      name: 'generated schema freshness', file: 'src/generate.mjs',
      before: 'if (check && (changed.length || extra.length))',
      after: 'if (false && check && (changed.length || extra.length))',
      expected: 'schema edits invalidate the manifest and generated declaration content',
    },
    {
      name: 'checkpoint reuse', file: 'src/context.mjs',
      before: 'try { return await load(); } catch (error)',
      after: 'try { await load(); } catch (error)',
      expected: 'checkpoint restore does not reread changed live environment or configuration',
    },
    {
      name: 'context snapshot integrity', file: 'src/context.mjs',
      before: 'if (digest !== hash(payload) || snapshot.schemaDigest !== hash(snapshot.schema))',
      after: 'if (false && (digest !== hash(payload) || snapshot.schemaDigest !== hash(snapshot.schema)))',
      expected: 'snapshot verification rejects changed payload, schema and run identity',
    },
    {
      name: 'prompt-visible secret rejection', file: 'src/context.mjs',
      before: "if (entry.secret !== undefined && entry.secret !== false)",
      after: 'if (false)',
      expected: 'secret-marked environment declarations fail before any environment reads',
    },
  ];
  const evidence = [];
  for (const control of controls) {
    for (const [file, source] of originals) await writeFile(path.join(temp, file), source);
    const file = control.file ?? 'src/renderer.mjs';
    const original = originals.get(file), target = path.join(temp, file);
    assert(original.includes(control.before), `Control target missing: ${control.name}`);
    await writeFile(target, original.replace(control.before, control.after));
    const result = spawnSync(process.execPath, ['--test', '--test-isolation=none', 'test/renderer.test.mjs', 'test/contracts.test.mjs', 'test/generate.test.mjs', 'test/context.test.mjs'], { cwd: temp, encoding: 'utf8', timeout: 30_000 });
    const output = result.stdout + result.stderr;
    if (result.error) throw result.error;
    assert.notEqual(result.status, 0, `Control did not fail: ${control.name}`);
    assert(output.includes(`✖ ${control.expected}`), `Expected assertion did not fail: ${output}`);
    evidence.push({ name: control.name, exit: result.status, expectedFailure: control.expected, summary: output.match(/ℹ (?:tests|pass|fail) \d+/g) });
  }
  await mkdir(path.join(root, 'results'), { recursive: true });
  await writeFile(path.join(root, 'results/controls.json'), JSON.stringify(evidence, null, 2) + '\n');
  console.log(JSON.stringify(evidence, null, 2));
} finally { await rm(temp, { recursive: true, force: true }); }
