// SPDX-License-Identifier: Apache-2.0
import { readFile, writeFile, mkdir, readdir, rm, path, createHash, fileURLToPath } from './platform.mjs';

const identifier = /^[A-Za-z_][A-Za-z_0-9]*$/;
const atomName = /^[a-z][a-z0-9-]*$/;
const canonical = (value) => JSON.stringify(value, (_, item) => item && !Array.isArray(item) && typeof item === 'object'
  ? Object.fromEntries(Object.keys(item).sort().map((key) => [key, item[key]])) : item);
const digest = (value) => createHash('sha256').update(canonical(value)).digest('hex');
async function existing(file) { try { return await readFile(file, 'utf8'); } catch (error) { if (error.code === 'ENOENT') return null; throw error; } }

export function declarations(document) {
  if (document.$schema !== 'https://json-schema.org/draft/2020-12/schema' || !document.$defs
    || Object.keys(document).some((key) => !['$schema', '$defs'].includes(key))) throw new Error('expected a 2020-12 schema with $defs');
  const definitions = document.$defs, edges = new Map();
  for (const name of Object.keys(definitions)) if (!identifier.test(name)) throw new Error(`invalid schema type name ${name}`);
  function type(schema, owner) {
    if (schema && typeof schema === 'object' && !Array.isArray(schema)) schema = Object.fromEntries(Object.entries(schema).filter(([key]) => !['$id', '$schema', 'title', '$comment', 'default', 'examples'].includes(key)));
    if (!schema || typeof schema !== 'object' || Array.isArray(schema)) throw new Error('only object schemas are supported');
    const allowed = ['$ref', 'type', 'properties', 'required', 'additionalProperties', 'items', 'enum', 'description', 'format'];
    for (const key of Object.keys(schema)) if (!allowed.includes(key)) throw new Error(`unsupported schema keyword ${key}`);
    if (schema.$ref) {
      const name = schema.$ref.replace(/^#\/\$defs\//, '');
      if (schema.$ref !== `#/$defs/${name}` || !Object.hasOwn(definitions, name)) throw new Error(`unknown local schema reference ${schema.$ref}`);
      if (Object.keys(schema).some((key) => !['$ref', 'description'].includes(key))) throw new Error('schema reference siblings are unsupported');
      edges.get(owner).add(name); return name;
    }
    if (schema.enum) {
      if (!['string', 'number', 'boolean'].includes(schema.type) || !Array.isArray(schema.enum) || !schema.enum.length || schema.enum.some((value) => typeof value !== schema.type)
        || Object.keys(schema).some((key) => !['type', 'enum', 'description'].includes(key))) throw new Error('enum values must match their declared primitive type');
      return schema.enum.map((value) => JSON.stringify(value)).join(' | ');
    }
    if (schema.type === 'object') {
      if (Object.keys(schema).some((key) => !['type', 'properties', 'required', 'additionalProperties', 'description'].includes(key))) throw new Error('schema keyword does not match object type');
      if (schema.additionalProperties !== false || !schema.properties) throw new Error('object schemas require properties and additionalProperties=false');
      const required = schema.required ?? [];
      if (!Array.isArray(required) || required.some((key) => !Object.hasOwn(schema.properties, key))) throw new Error('required property has no schema');
      return `{ ${Object.entries(schema.properties).map(([key, value]) => `${JSON.stringify(key)}${required.includes(key) ? '' : '?'}: ${type(value, owner)};`).join(' ')} }`;
    }
    if (schema.type === 'array') {
      if (Object.keys(schema).some((key) => !['type', 'items', 'description'].includes(key))) throw new Error('schema keyword does not match array type');
      return `Array<${type(schema.items, owner)}>`;
    }
    if (schema.format && (schema.format !== 'date-time' || schema.type !== 'string')) throw new Error('unsupported schema format');
    const primitive = { string: 'string', integer: 'number', number: 'number', boolean: 'boolean', null: 'null' }[schema.type];
    if (!primitive) throw new Error(`unsupported schema type ${schema.type}`);
    if (Object.keys(schema).some((key) => !['type', 'description', 'format'].includes(key))) throw new Error('schema keyword does not match primitive type');
    return primitive;
  }
  for (const name of Object.keys(definitions)) edges.set(name, new Set());
  const output = Object.keys(definitions).sort().map((name) => `export type ${name} = ${type(definitions[name], name)};`);
  function visit(name, stack = []) {
    if (stack.includes(name)) throw new Error('recursive schema references are outside this spike');
    for (const child of edges.get(name)) visit(child, [...stack, name]);
  }
  for (const name of edges.keys()) visit(name);
  return output.join('\n') + '\n';
}

export async function generate({ root = fileURLToPath(new URL('../', import.meta.url)), check = false } = {}) {
  const document = JSON.parse(await readFile(path.join(root, 'schemas/contracts.schema.json'), 'utf8'));
  const registry = JSON.parse(await readFile(path.join(root, 'schemas/atoms.json'), 'utf8'));
  const header = '// Generated from JSON Schema. Do not edit.\n';
  const files = new Map([['schemas.d.ts', header + declarations(document)]]);
  const metadata = {};
  const reference = (name) => { if (!identifier.test(name) || !Object.hasOwn(document.$defs, name)) throw new Error(`unknown registry schema ${name}`); return `S.${name}`; };
  for (const [name, atom] of Object.entries(registry)) {
    if (!atomName.test(name) || Object.keys(atom).some((key) => !['input', 'output', 'case', 'judges', 'context'].includes(key))) throw new Error('invalid atom registry entry');
    const input = reference(atom.input), output = reference(atom.output), evalCase = reference(atom.case);
    const judges = Object.entries(atom.judges ?? {}).map(([key, judge]) => {
      if (!atomName.test(key) || Object.keys(judge).some((key) => !['input', 'output'].includes(key))) throw new Error('invalid judge registry entry');
      return `${JSON.stringify(key)}: { input: ${reference(judge.input)}; output: ${reference(judge.output)} };`;
    }).join(' ');
    const runtimeContext = atom.context ? `type Immutable<T> = T extends object ? { readonly [K in keyof T]: Immutable<T[K]> } : T;\nexport type Context = Immutable<${reference(atom.context)}>;\n` : '';
    // The per-atom `Config` alias, spelled as the Rust declaration path spells
    // it: the fifth argument, and with it any read of `context` in a validator,
    // scorer or template, is present only when the registry declares a context
    // for this atom. An atom with none gets `Props { input: Input }` and a
    // four-argument `Config`, so a callback that reads `context` is a type error
    // in the editor and in `atomic check`, not a callback that sees nothing.
    const configType = `export type Config = AtomConfig<Input, Reply, EvalCase, Judges${atom.context ? ', Context' : ''}>;\n`;
    files.set(`${name}.d.ts`, `${header}import type * as S from './schemas';\nimport type { AtomConfig } from '../../authoring';\nexport type Input = ${input};\nexport type Reply = ${output};\nexport type EvalCase = ${evalCase};\n${runtimeContext}export interface Props { input: Input${atom.context ? '; readonly context: Context' : ''} }\nexport interface Judges { ${judges} }\n${configType}`);
    metadata[name] = { atomic: 1, name, kind: 'lite', input: atom.input, output: atom.output, case: atom.case, judges: atom.judges ?? {}, ...(atom.context ? { context: atom.context } : {}) };
  }
  const manifest = { version: 1, schemaDigest: digest(document), registryDigest: digest(registry), atoms: metadata };
  files.set('manifest.json', JSON.stringify(manifest, null, 2) + '\n');
  const out = path.join(root, '.atomic/types');
  const changed = [];
  for (const [name, content] of files) if (await existing(path.join(out, name)) !== content) changed.push(name);
  let extra = [];
  try { extra = (await readdir(out)).filter((name) => !files.has(name)); } catch (error) { if (error.code !== 'ENOENT') throw error; }
  if (check && (changed.length || extra.length)) throw new Error(`generated schema types are stale: ${[...changed, ...extra].join(', ')}; run npm run generate`);
  if (!check) {
    await mkdir(out, { recursive: true });
    for (const name of changed) await writeFile(path.join(out, name), files.get(name));
    for (const name of extra) await rm(path.join(out, name), { recursive: true, force: true });
  }
  return { ...manifest, changed: check ? [] : changed };
}
if (typeof process !== 'undefined' && process.argv?.[1] === fileURLToPath(import.meta.url)) {
  console.log(JSON.stringify(await generate({ check: process.argv.includes('--check') }), null, 2));
}
