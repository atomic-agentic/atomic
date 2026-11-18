// SPDX-License-Identifier: Apache-2.0
// A disposable mutation of the actual bundled worker's mandatory type gate.
import assert from 'node:assert/strict';
import { mkdtemp, cp, symlink, readFile, writeFile, rm } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import os from 'node:os';
const root = fileURLToPath(new URL('../', import.meta.url));
const deno = process.env.ATOMIC_DENO ?? 'deno';
const request = {
  action:'check',root:'/project',file:'/project/bad.atom.astro',
  sources:{'/project/bad.atom.astro':"---\nimport type {Props} from './.atomic/types/bad';\nconst {input}:Props = Astro.props;\n---\n{input.txt}\n"},
  schemas:{Input:{type:'object',additionalProperties:false,required:['text'],properties:{text:{type:'string'}}},Output:{type:'object',additionalProperties:false,properties:{}},Context:{type:'object',additionalProperties:false,properties:{}},Case:true},
  authoring:await readFile(path.join(root,'authoring.d.ts'),'utf8'),
};
function invoke(assets) {
  const result = spawnSync(deno,['run','--no-config','--no-lock','--no-remote','--cached-only','--no-npm','--no-prompt',`--allow-read=${assets}`,path.join(assets,'worker.mjs')],{input:JSON.stringify(request),encoding:'utf8',timeout:35000});
  if (result.error) throw result.error;
  return {status:result.status,result:JSON.parse(result.stdout),stderr:result.stderr};
}
const baseline = invoke(path.resolve(root,'../crates/atomic-core/assets'));
assert.equal(baseline.status,1);
assert.match(baseline.result.error,/TS(?:2339|2551)/);
assert.match(baseline.result.error,/bad\.atom\.astro:5:/,'type error maps to the authored line');
const temp = await mkdtemp(path.join(os.tmpdir(),'atomic-type-gate-control-'));
try {
  const authoring = path.join(temp,'authoring');
  await cp(root,authoring,{recursive:true,filter:(source) => !['node_modules','.atomic','.astro','results','embedded','.git'].includes(path.basename(source))});
  await symlink(path.join(root,'node_modules'),path.join(authoring,'node_modules'));
  const file = path.join(authoring,'src/typecheck.mjs');
  const source = await readFile(file,'utf8');
  assert(source.includes('if (errors.length)'));
  await writeFile(file,source.replace('if (errors.length)','if (false)'));
  const build = spawnSync(process.execPath,[path.join(authoring,'src/build-worker.mjs')],{encoding:'utf8',timeout:30000});
  assert.equal(build.status,0,build.stderr);
  const removed = invoke(path.join(temp,'crates/atomic-core/assets'));
  assert.equal(removed.status,0,JSON.stringify(removed));
  assert(removed.result.ok);
  console.log('PASS mandatory production type gate: wrong schema field rejected at authored line; removing the gate accepts it');
} finally { await rm(temp,{recursive:true,force:true}); }
