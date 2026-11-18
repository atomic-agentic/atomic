// SPDX-License-Identifier: Apache-2.0
// Type projections are authoring aids; Rust validates the complete JSON Schema.
export function schemaType(schema, depth = 0) {
  if (depth > 64) throw new Error('schema type depth limit exceeded');
  if (schema === true) return 'unknown';
  if (schema === false) return 'never';
  if (!schema || typeof schema !== 'object') throw new Error('invalid schema');
  const type = (value) => schemaType(value, depth + 1);
  if (Object.hasOwn(schema, 'const')) return JSON.stringify(schema.const);
  if (schema.enum) return schema.enum.map((value) => JSON.stringify(value)).join(' | ') || 'never';
  if (schema.anyOf || schema.oneOf || schema.allOf) {
    const { anyOf, oneOf, allOf, ...base } = schema;
    const branches = `(${(anyOf ?? oneOf ?? allOf).map(type).join(allOf ? ' & ' : ' | ')})`;
    return base.type || base.properties ? `(${type(base)} & ${branches})` : branches;
  }
  if (Array.isArray(schema.type)) return schema.type.map((item) => type({ ...schema, type: item })).join(' | ');
  if (schema.$ref) throw new Error('type generation requires resolved schema references');
  if (schema.type === 'object' || schema.properties) {
    const required = new Set(schema.required ?? []);
    const properties = Object.entries(schema.properties ?? {}).map(([key, value]) => `${JSON.stringify(key)}${required.has(key) ? '' : '?'}: ${type(value)};`);
    if (schema.additionalProperties !== false) properties.push('[key: string]: unknown;');
    return `{ ${properties.join(' ')} }`;
  }
  if (schema.type === 'array') return `Array<${type(schema.items ?? true)}>`;
  return ({ string: 'string', number: 'number', integer: 'number', boolean: 'boolean', null: 'null' })[schema.type] ?? 'unknown';
}

export function atomDeclarations(schemas) {
  const types = Object.entries(schemas).map(([key, value]) => `export type ${key === 'Context' ? 'ContextData' : key} = ${schemaType(value)};`).join('\n');
  return `// Generated from the full runtime contracts. Do not edit.\n${types}\nexport type Context = Immutable<ContextData>;\nexport type Reply = Output;\nexport type EvalCase = Case;\nexport type Immutable<T> = T extends object ? { readonly [K in keyof T]: Immutable<T[K]> } : T;\nexport interface Props { input: Input; readonly context: Immutable<Context>; }\n`;
}
