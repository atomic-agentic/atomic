// SPDX-License-Identifier: Apache-2.0
// Astro atom frontend. It interprets an allowlisted AST; it never evaluates JS.
// Calls to imported TypeScript helpers are the one exception: they run in the
// host's embedded runtime, one fresh capability-free isolate per call, and
// each result is labelled with the least trusted origin among its arguments.
import { parse } from '@astrojs/compiler';
import ts from 'typescript';
import { readFile, realpath, path, createHash, fileURLToPath } from './platform.mjs';
import { intrinsics } from './intrinsics.mjs';
import { contextForStep } from './context.mjs';
import { captureHelper } from './capture.mjs';

const denied = new Set(['__proto__', 'prototype', 'constructor']);
const dataBrand = Symbol('data');
const isData = (value) => value?.[dataBrand] === true;
// `fields` gives some first-level fields their own origin, such as a
// workflow's bindings assign; the value as a whole is its least trusted.
const data = (value, at, origin, fields) => ({ [dataBrand]: true, tag: 'data', value, at, origin, fields });
const raw = (value) => isData(value) ? value.value : value;
const fragments = (nodes, scope, slots, unit) => ({ tag: 'fragment', nodes, scope, slots, unit });
const allowedBinary = new Set(['&&', '||', '??', '===', '!==', '>', '>=', '<', '<=', '+', '-', '*', '/']);
const collections = new Set(['map', 'some', 'every', 'filter']);

// A helper call that produced no result: a throw, a limit or a refusal. It is
// an evaluation error, never a value, so a validator that hits one fails the
// operation instead of returning false (which would start a repair round).
export class HelperFailure extends Error {
  constructor(kind, message, file, line) {
    super(message);
    this.kind = kind; this.helperFailure = true;
    if (file) this.file = file;
    if (line) this.line = line;
  }
}
// The host's helper runtime: (capture digest, export name, JSON args) => JSON.
let helperExecutor = null;
export function setHelperExecutor(executor) { helperExecutor = executor; }

function bindingNames(pattern) {
  if (ts.isIdentifier(pattern)) return [pattern.text];
  if (ts.isObjectBindingPattern(pattern)) return pattern.elements.flatMap((element) => {
    if (element.dotDotDotToken || element.initializer || (element.propertyName && !ts.isIdentifier(element.propertyName))) throw new Error('only plain object destructuring is supported');
    if (denied.has(element.propertyName?.text ?? element.name.text)) throw new Error('forbidden destructured property');
    return bindingNames(element.name);
  });
  throw new Error('only identifiers and plain object destructuring are supported');
}
function checkArrow(node) {
  if (node.modifiers?.length || node.parameters.length !== 1 || node.parameters[0].initializer || node.parameters[0].dotDotDotToken || ts.isBlock(node.body)) throw new Error('callback requires one plain parameter and a synchronous expression body');
  const names = bindingNames(node.parameters[0].name);
  if (new Set(names).size !== names.length) throw new Error('duplicate callback binding');
  return names;
}
function fields(node) {
  return node.properties.map((field) => {
    if (!ts.isPropertyAssignment(field) || (!ts.isIdentifier(field.name) && !ts.isStringLiteral(field.name)) || denied.has(field.name.text)) throw new Error('only plain object properties are supported');
    return [field.name.text, field.initializer];
  });
}

// A diagnostic that names the authored file and line. Hosts format it; it is
// never a string the host has to parse.
export class AtomDiagnostic extends Error {
  constructor(message, file, line, diagnostics) {
    super(message);
    this.file = file; this.line = line; this.diagnostics = diagnostics;
  }
}
function lineOf(node) {
  if (!node) return undefined;
  if (node.position?.start?.line) return node.position.start.line;
  const file = typeof node.getSourceFile === 'function' ? node.getSourceFile() : undefined;
  if (!file) return undefined;
  return (file.atomicLine ?? 1) + file.getLineAndCharacterOfPosition(node.getStart(file)).line;
}
function error(unit, node, message) {
  throw new AtomDiagnostic(message, unit?.file, lineOf(node));
}
// Attach the nearest authored location to an error raised by a helper.
function relocate(unit, node, cause) {
  if (cause.sources) return cause;
  // A diagnostic from another file (an imported component) is already located.
  if (cause instanceof AtomDiagnostic && cause.file && (cause.line || cause.file !== unit?.file)) return cause;
  return new AtomDiagnostic(cause.message ?? String(cause), cause.file ?? unit?.file, cause.line ?? lineOf(node), cause.diagnostics);
}
function located(unit, node, work) {
  try { return work(); } catch (cause) { throw relocate(unit, node, cause); }
}
// `line` is the authored line that holds the first character of `text`.
function sourceFile(text, name, line = 1) {
  const file = ts.createSourceFile(name, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
  file.atomicLine = line;
  if (file.parseDiagnostics.length) {
    const first = file.parseDiagnostics[0];
    throw new AtomDiagnostic(ts.flattenDiagnosticMessageText(first.messageText, '\n'), name, line + file.getLineAndCharacterOfPosition(first.start ?? 0).line);
  }
  return file;
}
function expression(text, unit, line) {
  const file = sourceFile(`const __value = (${text});`, unit.file, line);
  if (file.statements.length !== 1) error(unit, null, 'one expression is required');
  const node = file.statements[0].declarationList.declarations[0].initializer;
  validateExpression(node, unit);
  return node;
}
function validateExpression(node, unit, locals = null) {
  const check = (child) => validateExpression(child, unit, locals);
  if (ts.isParenthesizedExpression(node)) return check(node.expression);
  if (ts.isIdentifier(node) && locals && !locals.has(node.text)) error(unit, node, `unknown contract value ${node.text}`);
  if (ts.isNumericLiteral(node)) { literal(node, unit); return; }
  if (ts.isIdentifier(node) || ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)
    || [ts.SyntaxKind.TrueKeyword, ts.SyntaxKind.FalseKeyword, ts.SyntaxKind.NullKeyword].includes(node.kind)) return;
  if (ts.isPropertyAccessExpression(node)) {
    if (ts.isOptionalChain(node) && !node.questionDotToken) error(unit, node, 'continued optional chains are unsupported; use an explicit nullish fallback');
    if (denied.has(node.name.text)) error(unit, node, `forbidden property ${node.name.text}`);
    return check(node.expression);
  }
  if (ts.isPrefixUnaryExpression(node) && [ts.SyntaxKind.ExclamationToken, ts.SyntaxKind.MinusToken].includes(node.operator)) return check(node.operand);
  if (ts.isBinaryExpression(node) && allowedBinary.has(node.operatorToken.getText())) {
    check(node.left); check(node.right); return;
  }
  if (ts.isConditionalExpression(node)) {
    for (const child of [node.condition, node.whenTrue, node.whenFalse]) check(child);
    return;
  }
  if (ts.isObjectLiteralExpression(node)) { for (const [, value] of fields(node)) check(value); return; }
  if (ts.isTemplateExpression(node)) { for (const span of node.templateSpans) check(span.expression); return; }
  if (ts.isCallExpression(node) && ts.isIdentifier(node.expression) && unit.helpers.has(node.expression.text)) {
    if (node.questionDotToken || node.typeArguments?.length) error(unit, node, 'helper calls take plain arguments');
    if (node.arguments.some((arg) => ts.isSpreadElement(arg))) error(unit, node, 'helper calls take plain arguments');
    if (node.arguments.length > 16) error(unit, node, 'helper calls take at most 16 arguments');
    node.arguments.forEach(check); return;
  }
  if (ts.isCallExpression(node) && ts.isIdentifier(node.expression) && unit.intrinsics.has(node.expression.text)) {
    const intrinsic = intrinsics.get(unit.intrinsics.get(node.expression.text));
    if (node.arguments.length !== intrinsic.arity) error(unit, node, 'incorrect intrinsic argument count');
    node.arguments.forEach(check); return;
  }
  if (ts.isCallExpression(node) && ts.isPropertyAccessExpression(node.expression) && node.expression.getText() === 'Astro.slots.has') {
    if (locals) error(unit, node, 'Astro.slots is for templates, not contracts');
    if (node.arguments.length !== 1 || !ts.isStringLiteral(node.arguments[0])) error(unit, node, 'Astro.slots.has takes one slot name as a string literal');
    return;
  }
  if (ts.isCallExpression(node) && ts.isPropertyAccessExpression(node.expression) && collections.has(node.expression.name.text)
    && node.arguments.length === 1 && ts.isArrowFunction(node.arguments[0])) {
    if (ts.isOptionalChain(node) || node.questionDotToken || node.expression.questionDotToken) error(unit, node, 'optional collection calls are unsupported; use an explicit condition');
    const callback = node.arguments[0];
    let names;
    try { names = checkArrow(callback); } catch (cause) { error(unit, callback, `${node.expression.name.text} requires: ${cause.message}`); }
    if (names.some((name) => unit.intrinsics.has(name))) error(unit, callback, 'callback cannot shadow an intrinsic');
    check(node.expression.expression);
    validateExpression(callback.body, unit, locals ? new Set([...locals, ...names]) : null); return;
  }
  error(unit, node, `unsupported expression: ${ts.SyntaxKind[node.kind]}; calls, assignment, functions and runtime globals are not executed`);
}
function literal(node, unit) {
  if (ts.isAsExpression(node) && node.type.getText() === 'const') return literal(node.expression, unit);
  if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) return node.text;
  if (ts.isPrefixUnaryExpression(node) && node.operator === ts.SyntaxKind.MinusToken && ts.isNumericLiteral(node.operand)) {
    const value = -literal(node.operand, unit);
    return Object.is(value, -0) ? 0 : value;
  }
  if (ts.isNumericLiteral(node)) { const value = Number(node.text); if (!Number.isFinite(value) || (Number.isInteger(value) && !Number.isSafeInteger(value))) error(unit, node, 'numeric literal is outside finite safe-number range'); return value; }
  if (node.kind === ts.SyntaxKind.TrueKeyword) return true;
  if (node.kind === ts.SyntaxKind.FalseKeyword) return false;
  if (node.kind === ts.SyntaxKind.NullKeyword) return null;
  if (ts.isArrayLiteralExpression(node)) return node.elements.map((n) => literal(n, unit));
  if (ts.isObjectLiteralExpression(node)) {
    const out = Object.create(null);
    for (const field of node.properties) {
      if (!ts.isPropertyAssignment(field) || (!ts.isIdentifier(field.name) && !ts.isStringLiteral(field.name))) error(unit, field, 'configuration must be a plain literal');
      if (denied.has(field.name.text)) error(unit, field, 'forbidden configuration key');
      out[field.name.text] = literal(field.initializer, unit);
    }
    return out;
  }
  error(unit, node, 'configuration and defaults must be plain literals');
}
function contractConfig(node, unit, at = []) {
  if (ts.isArrowFunction(node)) {
    const legal = (at.length === 2 && at[0] === 'validate' && typeof at[1] === 'number')
      || (at.length === 3 && at[0] === 'scorers' && ['with', 'score'].includes(at[2]));
    if (!legal) error(unit, node, 'callbacks are only allowed in validate, scorer.with and scorer.score');
    const names = located(unit, node, () => checkArrow(node));
    if (names.some((name) => unit.intrinsics.has(name))) error(unit, node, 'callback cannot shadow an intrinsic');
    validateExpression(node.body, unit, new Set(names));
    const id = at.join('.');
    unit.contracts.set(id, node);
    return { kind: 'interpreted', id, source: node.getText() };
  }
  if (ts.isObjectLiteralExpression(node)) {
    const out = Object.create(null);
    for (const [key, value] of fields(node)) {
      if (Object.hasOwn(out, key)) error(unit, value, `duplicate config property ${key}`);
      out[key] = contractConfig(value, unit, [...at, key]);
    }
    return out;
  }
  if (ts.isArrayLiteralExpression(node)) return node.elements.map((value, i) => contractConfig(value, unit, [...at, i]));
  return literal(node, unit);
}
function decode(text, unit) {
  return text.replace(/&([^;\s]+);/g, (whole, name) => {
    const named = { lt: '<', gt: '>', amp: '&', quot: '"', apos: "'" };
    if (Object.hasOwn(named, name)) return named[name];
    if (/^#(?:\d+|x[\da-f]+)$/i.test(name)) {
      const n = name[1].toLowerCase() === 'x' ? parseInt(name.slice(2), 16) : Number(name.slice(1));
      if (n > 0 && n <= 0x10ffff && !(n >= 0xd800 && n <= 0xdfff)) return String.fromCodePoint(n);
    }
    error(unit, null, `unsupported entity ${whole}`);
  });
}

export async function compile(file, options = {}) {
  const root = await realpath(options.root ?? path.dirname(file));
  const cache = new Map();
  // Every helper graph the atom's components import, keyed by capture digest.
  const captures = new Map();
  const captureState = { cache: new Map(), modules: 0, bytes: 0 };
  let sourceBytes = 0;
  let componentCount = 0;
  async function load(target, stack = []) {
    const canonical = await realpath(target);
    if (canonical !== root && !canonical.startsWith(root + path.sep)) throw new Error('component escapes project root');
    if (stack.includes(canonical)) throw new Error('component import cycle');
    if (cache.has(canonical)) return cache.get(canonical);
    if (stack.length >= 32 || ++componentCount > 128) throw new Error('component graph limit exceeded');
    const text = await readFile(canonical, 'utf8');
    sourceBytes += text.length;
    if (text.length > 262144 || sourceBytes > 1048576) throw new Error('source size limit exceeded');
    if (options.production && /@ts-(?:ignore|nocheck|expect-error)/.test(text)) throw new Error('TypeScript diagnostic suppression is not allowed in atom sources');
    const parsed = await parse(text, { position: true });
    const parseError = parsed.diagnostics?.find((d) => d.severity === 1);
    if (parseError) throw new AtomDiagnostic(parseError.text ?? 'Astro parse error', canonical, parseError.location?.line);
    const defaults = canonical.endsWith('.atom.astro')
      ? { atomic: 1, name: path.basename(canonical, '.atom.astro'), kind: 'lite' }
      : {};
    const unit = { file: canonical, root, config: { ...defaults }, bindings: [], imports: new Map(), templates: new Map(), nodes: [], intrinsics: new Map(), contracts: new Map(), typeImports: [], helpers: new Map(), locals: [] };
    const missingSources = new Set();
    const contractTypes = new Set();
    const propTypes = new Set();
    const fm = parsed.ast.children.filter((n) => n.type === 'frontmatter');
    if (fm.length > 1) error(unit, fm[1], 'one script fence is supported');
    const script = sourceFile(fm[0]?.value ?? '', canonical, fm[0]?.position?.start?.line ?? 1);
    if (options.production) {
      const checkTypes = (node) => { if (node.kind === ts.SyntaxKind.AnyKeyword) error(unit, node, 'explicit any cannot bypass schema-derived checking'); ts.forEachChild(node, checkTypes); };
      checkTypes(script);
    }
    for (const statement of script.statements) try {
      if (ts.isImportDeclaration(statement)) {
        const specifier = statement.moduleSpecifier.text;
        const authoring = options.authoringFile ?? fileURLToPath(new URL('../authoring.d.ts', import.meta.url));
        const isAuthoring = specifier.startsWith('.') && path.resolve(path.dirname(canonical), `${specifier}.d.ts`) === authoring;
        if (isAuthoring) {
          const clause = statement.importClause;
          if (clause?.name || !clause?.namedBindings || !ts.isNamedImports(clause.namedBindings)) error(unit, null, 'intrinsics require named imports');
          for (const item of clause.namedBindings.elements) {
            const imported = item.propertyName?.text ?? item.name.text;
            if (clause.isTypeOnly || item.isTypeOnly) {
              if (imported === 'AtomConfig') contractTypes.add(item.name.text);
            } else {
              if (!intrinsics.has(imported)) error(unit, null, `unknown intrinsic ${imported}`);
              unit.intrinsics.set(item.name.text, imported);
            }
          }
          continue;
        }
        if (statement.importClause?.isTypeOnly) {
          // The host supplies declarations for these, such as generated schema types.
          if (specifier.startsWith('.')) unit.typeImports.push(path.resolve(path.dirname(canonical), specifier));
          if (path.resolve(path.dirname(canonical), `${specifier}.d.ts`) === `${root}/.atomic/types/${path.basename(canonical, '.atom.astro')}.d.ts`) {
            for (const item of statement.importClause.namedBindings?.elements ?? []) if ((item.propertyName?.text ?? item.name.text) === 'Props') propTypes.add(item.name.text);
          }
          continue;
        }
        const clause = statement.importClause;
        // Named value imports of a local module are helpers: `./helpers/review`
        // (as editors resolve it without a tsconfig) or `./helpers/review.ts`.
        if (specifier.startsWith('.') && !specifier.endsWith('.astro') && clause && !clause.name && clause.namedBindings && ts.isNamedImports(clause.namedBindings)
          && clause.namedBindings.elements.some((item) => !item.isTypeOnly)) {
          let captured = null;
          try {
            captured = await captureHelper(root, path.resolve(path.dirname(canonical), specifier), { file: canonical, line: lineOf(statement), specifier }, captureState);
            captures.set(captured.digest, captured);
            for (const file of captured.typeImports) unit.typeImports.push(file);
          } catch (cause) {
            if (!cause.sources) throw cause;
            // Still being discovered: the names are helpers, so the rest of the
            // script checks; the missing sources are requested below.
            for (const source of cause.sources) missingSources.add(source);
          }
          for (const item of clause.namedBindings.elements) {
            if (item.isTypeOnly) continue;
            if (unit.intrinsics.has(item.name.text) || unit.helpers.has(item.name.text)) error(unit, null, `duplicate helper ${item.name.text}`);
            unit.helpers.set(item.name.text, { capture: captured?.digest ?? null, name: item.propertyName?.text ?? item.name.text, module: specifier });
          }
          continue;
        }
        if (!statement.importClause?.name || statement.importClause.namedBindings || !specifier.startsWith('.') || !specifier.endsWith('.astro')) error(unit, null, 'only default imports of local .astro components and named imports of local .ts helpers are supported');
        try {
          const imported = await load(path.resolve(path.dirname(canonical), specifier), [...stack, canonical]);
          unit.imports.set(statement.importClause.name.text, imported);
        } catch (error) {
          if (!error.sources) throw error;
          for (const source of error.sources) missingSources.add(source);
        }
      } else if (ts.isInterfaceDeclaration(statement) || ts.isTypeAliasDeclaration(statement) || ts.isEmptyStatement(statement)) {
        // Type declarations have no runtime effect. The stock language server checks them.
      } else if (ts.isVariableStatement(statement) && (statement.declarationList.flags & ts.NodeFlags.Const)) {
        for (const declaration of statement.declarationList.declarations) {
          if (ts.isObjectBindingPattern(declaration.name) && declaration.initializer?.getText(script) === 'Astro.props') {
            if (options.production && canonical.endsWith('.atom.astro') && (!declaration.type || !propTypes.has(declaration.type.getText(script)))) error(unit, null, 'atom props require an explicit generated Props annotation and direct declaration import');
            for (const binding of declaration.name.elements) {
              if (binding.dotDotDotToken || (binding.propertyName && !ts.isIdentifier(binding.propertyName))) error(unit, null, 'only plain prop destructuring is supported');
              // A nested pattern, such as `const { input: { change } } = Astro.props`.
              if (ts.isObjectBindingPattern(binding.name)) {
                if (!binding.propertyName) error(unit, null, 'only plain prop destructuring is supported');
                located(unit, binding, () => bindingNames(binding.name));
              } else if (!ts.isIdentifier(binding.name)) error(unit, null, 'only plain prop destructuring is supported');
              const prop = binding.propertyName?.text ?? binding.name.text;
              if (denied.has(prop)) error(unit, null, 'forbidden destructured property');
              unit.bindings.push({ local: ts.isIdentifier(binding.name) ? binding.name.text : null, pattern: ts.isIdentifier(binding.name) ? null : binding.name, prop, fallback: binding.initializer ? literal(binding.initializer, unit) : undefined });
            }
          } else if (ts.isIdentifier(declaration.name) && declaration.name.text === 'config' && declaration.initializer) {
            const typed = options.production || (declaration.type && ts.isTypeReferenceNode(declaration.type) && contractTypes.has(declaration.type.typeName.getText()));
            if (typed && declaration.type?.typeArguments?.length === 5) unit.requiresContext = true;
            const overrides = typed ? contractConfig(declaration.initializer, unit) : literal(declaration.initializer, unit);
            if (overrides === null || Array.isArray(overrides) || typeof overrides !== 'object') error(unit, null, 'config must be a plain object');
            unit.config = Object.assign(Object.create(null), defaults, overrides);
          } else if (declaration.initializer && (ts.isIdentifier(declaration.name) || ts.isObjectBindingPattern(declaration.name))) {
            // An interpreted local, evaluated in order when the component renders.
            if (declaration.initializer.getText(script) === 'Astro.props') error(unit, null, 'destructure Astro.props in the declaration');
            located(unit, declaration, () => bindingNames(declaration.name));
            validateExpression(declaration.initializer, unit);
            unit.locals.push({ pattern: declaration.name, ast: declaration.initializer });
          } else error(unit, null, 'script values must be config, destructured Astro.props or const locals');
        }
      } else error(unit, statement, `unsupported script statement: ${ts.SyntaxKind[statement.kind]}; script code is never executed`);
    } catch (cause) { throw relocate(unit, statement, cause); }
    if (missingSources.size) {
      const error = new Error('missing sources'); error.sources = [...missingSources]; throw error;
    }
    // Astro starts an expression's position at the text before its brace; its
    // first child holds the brace's line.
    const authoredLine = (node) => (node.type === 'expression' ? node.children?.find((child) => child.position)?.position.start.line : undefined) ?? node.position?.start?.line;
    // `slotted` nodes are a component's direct children, which may name the
    // slot they fill with a literal `slot` attribute.
    const lowerNodes = (nodes, slotted = false) => nodes.filter((n) => n.type !== 'frontmatter').map((node) => located(unit, { position: { start: { line: authoredLine(node) } } }, () => {
      const line = authoredLine(node);
      if (node.type === 'text') return { op: 'text', text: decode(node.value, unit) };
      if (node.type === 'comment') return { op: 'comment' };
      if (node.type === 'expression') {
        let source = '';
        for (const child of node.children) {
          if (child.type === 'text') source += child.value;
          else {
            const key = `__atomic_template_${unit.templates.size}`;
            unit.templates.set(key, null);
            unit.templates.set(key, lowerNodes([child])); source += key;
          }
        }
        return { op: 'expression', ast: expression(source, unit, line), source };
      }
      const name = node.name ?? 'Fragment';
      if (name.startsWith('data-') || ['script', 'style'].includes(name)) error(unit, node, `unsupported/reserved element ${name}`);
      if (!['element', 'component', 'fragment'].includes(node.type)) error(unit, node, `unsupported template node ${node.type}`);
      if (node.type === 'component' && name !== 'Fragment' && !unit.imports.has(name)) error(unit, node, `unknown component ${name}`);
      if (node.attributes?.some((a) => a.name.includes(':') || !['quoted', 'expression'].includes(a.kind))) error(unit, node, 'directives, spread and shorthand attributes are unsupported');
      const attributes = node.attributes ?? [];
      const slotAttribute = attributes.find((a) => a.name === 'slot');
      if (slotAttribute && !slotted) error(unit, node, "the slot attribute names a slot of the enclosing component; use it on a component's direct children");
      if (slotAttribute && slotAttribute.kind !== 'quoted') error(unit, node, 'slot names are literal strings');
      const rest = attributes.filter((a) => a !== slotAttribute);
      let slotName;
      if (name === 'slot') {
        // <slot name="x">fallback</slot>; the fallback renders when the slot is absent.
        const named = rest.find((a) => a.name === 'name');
        if (rest.some((a) => a.name !== 'name') || (named && named.kind !== 'quoted')) error(unit, node, 'a slot takes a literal name and nothing else');
        slotName = named ? decode(named.value, unit) : 'default';
      } else if (node.type === 'element' && rest.length) error(unit, node, 'literal section attributes are unsupported');
      else if (name === 'Fragment' && rest.length) error(unit, node, 'Fragment takes only a slot attribute');
      return {
        op: name === 'Fragment' ? 'fragment' : name === 'slot' ? 'slot' : node.type === 'component' ? 'component' : 'section',
        name: name === 'slot' ? slotName : name,
        fills: slotAttribute ? decode(slotAttribute.value, unit) : undefined,
        children: lowerNodes(node.children ?? [], node.type === 'component' && name !== 'Fragment'),
        attrs: (node.type === 'component' ? rest : []).map((a) => ({ name: a.name, literal: a.kind === 'quoted', value: a.kind === 'quoted' ? decode(a.value, unit) : expression(a.value, unit, a.position?.start?.line ?? line) })),
      };
    }));
    unit.nodes = lowerNodes(parsed.ast.children);
    cache.set(canonical, unit);
    return unit;
  }
  const unit = await load(file);
  unit.captures = captures;
  return unit;
}

const trust = { external: 0, agent: 1, team: 2 };
// The least trusted data value anywhere inside `values`. Arrays and objects
// built from data (a mapped list, an object literal) carry data inside them
// without being data themselves, so provenance is searched for, not assumed.
function leastTrusted(values) {
  let found = null, budget = 100_000;
  const visit = (value, depth) => {
    if (--budget < 0 || depth > 64) throw new Error('provenance search limit exceeded');
    if (isData(value)) { if (!found || trust[value.origin] < trust[found.origin]) found = value; return; }
    if (value?.tag === 'fragment') return;
    if (Array.isArray(value)) for (const item of value) visit(item, depth + 1);
    else if (value && typeof value === 'object') for (const item of Object.values(value)) visit(item, depth + 1);
  };
  for (const value of values) visit(value, 0);
  return found;
}
// A value computed from data is data. It is never emitted as instructions.
function derive(result, values, node) {
  const origin = leastTrusted(values);
  if (!origin) return result;
  const identity = JSON.stringify([node.getText(), values.map((v) => isData(v) ? { path: v.at } : { literal: v })]);
  const suffix = createHash('sha256').update(identity).digest('hex').slice(0, 16);
  return data(materialize(result), `${origin.at}.expression_${suffix}`, origin.origin);
}
// A helper sees only its arguments, so its result is no more trusted than its
// least trusted argument. A result computed from authored literals alone is
// authored computation and may supply instructions.
function labelHelperResult(result, args, node, helper) {
  const origin = leastTrusted(args);
  if (!origin) return result;
  const identity = JSON.stringify([helper.module, helper.name, node.getText(), args.map((v) => isData(v) ? { path: v.at } : { literal: materialize(v) })]);
  const suffix = createHash('sha256').update(identity).digest('hex').slice(0, 16);
  return data(result, `helpers.${helper.name}_${suffix}`, origin.origin);
}
// The slots a component call supplies, as Astro defines them. A slot is
// present when the caller supplied it: a child with `slot="x"`, even an empty
// Fragment, is explicit content. Default content of only whitespace and
// comments is absent, and a forwarded slot (`<slot name="y" slot="x" />`, or
// a bare `<slot />` as default content) is present only when the slot it
// forwards is, or its fallback has content. Presence is decided from the
// tree, so `Astro.slots.has` renders nothing and spends no helper budget.
function slotSet(children, scope, callerSlots, unit) {
  const nodes = new Map([['default', []]]);
  for (const child of children) {
    const name = child.fills ?? 'default';
    if (!nodes.has(name)) nodes.set(name, []);
    nodes.get(name).push(child);
  }
  const occupied = (list) => list.some((node) => {
    if (node.op === 'comment' || (node.op === 'text' && !node.text.trim())) return false;
    if (node.op === 'slot') return Boolean(callerSlots?.has(node.name)) || occupied(node.children);
    return true;
  });
  return { nodes, scope, slots: callerSlots, unit, has: (name) => nodes.has(name) && occupied(nodes.get(name)) };
}
function property(parent, key) {
  const value = raw(parent);
  if (denied.has(key)) throw new Error(`forbidden property ${key}`);
  if (value == null) throw new Error(`cannot read property ${key} of missing value`);
  const item = Object.hasOwn(Object(value), key) ? value[key] : undefined;
  if (isData(parent)) return data(item, `${parent.at}.${key}`, (parent.fields && Object.hasOwn(parent.fields, key)) ? parent.fields[key] : parent.origin);
  if (isData(item) || item === undefined || typeof value !== 'object') return item;
  // For example the length of a mapped list of data.
  const origin = leastTrusted([value]);
  return origin ? data(materialize(item), `${origin.at}.${key}`, origin.origin) : item;
}
// A falsy data value that JSX would render as nothing, kept as data so later
// operations still see where it came from.
function quiet(value) {
  return Object.assign(data(value.value, value.at, value.origin), { quiet: true });
}
function bind(pattern, value, scope) {
  if (ts.isIdentifier(pattern)) { scope[pattern.text] = value; return; }
  for (const entry of pattern.elements) bind(entry.name, property(value, entry.propertyName?.text ?? entry.name.text), scope);
}
function materialize(value) {
  value = raw(value);
  if (Array.isArray(value)) return value.map(materialize);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, materialize(item)]));
  return value;
}
function evaluate(node, scope, unit, slots, budget) {
  if (--budget.remaining < 0) throw new Error('render operation limit exceeded');
  if (ts.isParenthesizedExpression(node)) return evaluate(node.expression, scope, unit, slots, budget);
  if (ts.isIdentifier(node)) {
    if (unit.templates.has(node.text)) return fragments(unit.templates.get(node.text), scope, slots, unit);
    if (!Object.hasOwn(scope, node.text)) error(unit, node, `unknown value ${node.text}`);
    return scope[node.text];
  }
  if (ts.isStringLiteral(node) || ts.isNumericLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)
    || [ts.SyntaxKind.TrueKeyword, ts.SyntaxKind.FalseKeyword, ts.SyntaxKind.NullKeyword].includes(node.kind)) return literal(node, unit);
  if (ts.isPropertyAccessExpression(node)) {
    const parent = evaluate(node.expression, scope, unit, slots, budget), value = raw(parent), key = node.name.text;
    if (value == null && node.questionDotToken) return undefined;
    return property(parent, key);
  }
  if (ts.isPrefixUnaryExpression(node)) {
    const value = evaluate(node.operand, scope, unit, slots, budget);
    return derive(node.operator === ts.SyntaxKind.ExclamationToken ? !raw(value) : -raw(value), [value], node);
  }
  if (ts.isBinaryExpression(node)) {
    const a = evaluate(node.left, scope, unit, slots, budget), op = node.operatorToken.getText();
    if (op === '&&') {
      const value = raw(a);
      if (value) return evaluate(node.right, scope, unit, slots, budget);
      if (value == null) return value;
      // As in JSX, `{flag && <X />}` renders nothing for false and ''; 0 still renders.
      return (value === false || value === '') && isData(a) ? quiet(a) : a;
    }
    if (op === '||') return raw(a) ? a : evaluate(node.right, scope, unit, slots, budget);
    if (op === '??') return raw(a) == null ? evaluate(node.right, scope, unit, slots, budget) : a;
    const b = evaluate(node.right, scope, unit, slots, budget);
    const av = raw(a), bv = raw(b);
    const operations = { '===': () => av === bv, '!==': () => av !== bv, '>': () => av > bv, '>=': () => av >= bv, '<': () => av < bv, '<=': () => av <= bv, '+': () => av + bv, '-': () => av - bv, '*': () => av * bv, '/': () => av / bv };
    const result = operations[op]();
    if (typeof result === 'number' && (!Number.isFinite(result) || (Number.isInteger(result) && !Number.isSafeInteger(result)))) error(unit, node, 'non-finite or unsafe arithmetic result');
    return derive(result, [a, b], node);
  }
  if (ts.isConditionalExpression(node)) return evaluate(raw(evaluate(node.condition, scope, unit, slots, budget)) ? node.whenTrue : node.whenFalse, scope, unit, slots, budget);
  if (ts.isTemplateExpression(node)) {
    const values = node.templateSpans.map((span) => evaluate(span.expression, scope, unit, slots, budget));
    let text = node.head.text;
    node.templateSpans.forEach((span, i) => {
      const value = raw(values[i]);
      if (value === undefined) error(unit, span.expression, 'cannot interpolate a missing value into a template literal');
      if (value !== null && typeof value === 'object') error(unit, span.expression, 'template literals interpolate strings, numbers, booleans and null');
      text += String(value) + span.literal.text;
    });
    // As trusted as its least trusted part; authored literals alone are instructions.
    return derive(text, values, node);
  }
  if (ts.isObjectLiteralExpression(node)) {
    const values = fields(node).map(([key, child]) => [key, evaluate(child, scope, unit, slots, budget)]);
    return derive(Object.fromEntries(values.map(([key, value]) => [key, materialize(value)])), values.map(([, value]) => value), node);
  }
  if (ts.isCallExpression(node) && ts.isPropertyAccessExpression(node.expression) && node.expression.getText() === 'Astro.slots.has') {
    // Structural: whether the caller supplied the slot, never its rendering.
    return Boolean(slots?.has(node.arguments[0].text));
  }
  if (ts.isCallExpression(node) && ts.isIdentifier(node.expression) && unit.helpers.has(node.expression.text)) {
    const helper = unit.helpers.get(node.expression.text);
    const args = node.arguments.map((arg) => evaluate(arg, scope, unit, slots, budget));
    if (!helperExecutor) error(unit, node, 'helper calls need the atomic host');
    const result = helperExecutor(helper.capture, helper.name, args.map(materialize));
    return labelHelperResult(result, args, node, helper);
  }
  if (ts.isCallExpression(node) && ts.isIdentifier(node.expression)) {
    const args = node.arguments.map((arg) => evaluate(arg, scope, unit, slots, budget));
    const intrinsic = intrinsics.get(unit.intrinsics.get(node.expression.text));
    return derive(intrinsic.run(...args.map(materialize)), args, node);
  }
  if (ts.isCallExpression(node)) {
    const collection = evaluate(node.expression.expression, scope, unit, slots, budget), values = raw(collection), arrow = node.arguments[0];
    if (!Array.isArray(values) || values.length > 1000) error(unit, node, 'collection operation requires an array of at most 1000 values');
    const method = node.expression.name.text, out = [];
    for (let i = 0; i < values.length; i++) {
      const childScope = Object.assign(Object.create(null), scope);
      const bound = isData(collection) ? data(values[i], `${collection.at}[${i}]`, collection.origin) : values[i];
      bind(arrow.parameters[0].name, bound, childScope);
      const result = evaluate(arrow.body, childScope, unit, slots, budget);
      if (method === 'map') out.push(result);
      else {
        if (typeof raw(result) !== 'boolean') error(unit, arrow.body, 'collection predicate must return boolean');
        if (method === 'some' && raw(result)) return derive(true, [collection], node);
        if (method === 'every' && !raw(result)) return derive(false, [collection], node);
        if (method === 'filter' && raw(result)) out.push(raw(bound));
      }
    }
    return method === 'map' ? out : derive(method === 'filter' ? out : method === 'every', [collection], node);
  }
  error(unit, node, 'unexpected expression AST');
}

function assertJson(value, state = { remaining: 10_000 }, depth = 0) {
  if (--state.remaining < 0 || depth > 32) throw new Error('contract input limit exceeded');
  if (value === null || ['string', 'boolean'].includes(typeof value)) return;
  if (typeof value === 'number') { if (!Number.isFinite(value) || (Number.isInteger(value) && !Number.isSafeInteger(value))) throw new Error('numeric values require finite safe integer precision'); return; }
  if (!value || typeof value !== 'object' || Object.getOwnPropertySymbols(value).length
    || ![Object.prototype, Array.prototype, null].includes(Object.getPrototypeOf(value))) throw new Error('contract inputs must be plain JSON');
  for (const [key, descriptor] of Object.entries(Object.getOwnPropertyDescriptors(value))) {
    if (Array.isArray(value) && key === 'length') continue;
    if (!Object.hasOwn(descriptor, 'value')) throw new Error('contract inputs must not have accessors');
    assertJson(descriptor.value, state, depth + 1);
  }
}

export function invokeContract(unit, descriptor, context) {
  const arrow = descriptor?.kind === 'interpreted' && unit.contracts.get(descriptor.id);
  if (!arrow) throw new Error('expected a compiled typed contract');
  // Each value has its own budget, the same one render applies to input, so an
  // input that rendered is never rejected here for the size of the output.
  // Public context was bounded when its snapshot was verified.
  for (const [key, value] of plainEntries(context)) if (key !== 'context') assertJson(value);
  const scope = Object.create(null);
  bind(arrow.parameters[0].name, data(context, 'context', 'external'), scope);
  return materialize(evaluate(arrow.body, scope, unit, null, { remaining: 10_000 }));
}

// The top-level contract values, read without invoking accessors.
function plainEntries(values) {
  if (!values || typeof values !== 'object' || Array.isArray(values) || Object.getOwnPropertySymbols(values).length
    || ![Object.prototype, null].includes(Object.getPrototypeOf(values))) throw new Error('contract inputs must be plain JSON');
  return Object.entries(Object.getOwnPropertyDescriptors(values)).map(([key, descriptor]) => {
    if (!Object.hasOwn(descriptor, 'value')) throw new Error('contract inputs must not have accessors');
    return [key, descriptor.value];
  });
}
function executionContext(unit, values, { contextSnapshot, stepId } = {}) {
  if (unit.requiresContext && !contextSnapshot) throw new Error('context snapshot required');
  return contextSnapshot ? { ...values, context: contextForStep(contextSnapshot, stepId) } : values;
}
// A rejection that depends on the model's answer is a failed validation, which
// the host may repair, never a host error. Each result names the rule's source.
export function validateOutput(unit, values, options) {
  values = executionContext(unit, values, options);
  const entries = plainEntries(values);
  // Everything but the answer came from the host and was already bounded.
  for (const [key, value] of entries) if (key !== 'output' && key !== 'context') assertJson(value);
  try { assertJson(Object.fromEntries(entries).output ?? null); } catch (cause) {
    return { valid: false, results: [{ id: 'output', passed: false, source: null, error: cause.message }] };
  }
  const results = (unit.config.validate ?? []).map((contract) => {
    let passed, failure;
    try {
      passed = invokeContract(unit, contract, values);
      if (typeof passed !== 'boolean') failure = 'validator must return boolean';
    } catch (cause) {
      if (cause?.helperFailure) throw cause;
      failure = cause.message ?? String(cause);
    }
    return failure === undefined ? { id: contract.id, passed, source: contract.source }
      : { id: contract.id, passed: false, source: contract.source, error: failure };
  });
  return { valid: results.every((result) => result.passed), results };
}
export function judgeInput(unit, name, context, options) {
  const scorer = unit.config.scorers?.[name];
  if (!scorer) throw new Error('unknown scorer');
  return invokeContract(unit, scorer.with, executionContext(unit, context, options));
}
export function judgeScore(unit, name, judge, options) {
  const scorer = unit.config.scorers?.[name];
  if (!scorer) throw new Error('unknown scorer');
  const value = invokeContract(unit, scorer.score, executionContext(unit, { judge }, options));
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0 || value > 1) throw new Error('score must be finite and between 0 and 1');
  return value;
}
export function aggregateScores(unit, name, scores) {
  if (!scores.length || scores.some((score) => typeof score !== 'number' || !Number.isFinite(score) || score < 0 || score > 1)) throw new Error('scores require non-empty finite values between 0 and 1');
  const threshold = unit.config.scorers?.[name]?.min_mean;
  if (typeof threshold !== 'number' || !Number.isFinite(threshold) || threshold < 0 || threshold > 1) throw new Error('invalid scorer threshold');
  const mean = scores.reduce((a, b) => a + b, 0) / scores.length;
  return { mean, threshold, passed: mean >= threshold };
}

export function render(unit, input, { runId, origin = 'external', origins, contextSnapshot, stepId } = {}) {
  assertJson(input);
  if (!['external', 'team', 'agent'].includes(origin)) throw new Error('invalid input origin');
  if (origins != null && (typeof origins !== 'object' || Array.isArray(origins) || Object.values(origins).some((o) => !Object.hasOwn(trust, o)))) throw new Error('invalid input origins');
  if (contextSnapshot && runId !== undefined && runId !== contextSnapshot.runId) throw new Error('context snapshot run mismatch');
  runId ??= contextSnapshot?.runId ?? 'spike-run';
  const result = { developer: '', user: '', bindings: [] };
  const budget = { remaining: 10_000, depth: 0 };
  const seen = new Set();
  let nonce;
  function emit(value) {
    if (Array.isArray(value)) { for (const item of value) emit(item); return; }
    if (value?.tag === 'fragment') { walk(value.nodes, value.scope, value.unit, value.slots); return; }
    if (isData(value)) {
      if (value.quiet) return;
      if (value.value === undefined) throw new Error(`cannot interpolate missing value ${value.at}`);
      const text = typeof value.value === 'string' ? value.value : JSON.stringify(value.value);
      // The suffix depends on every bound value, so a payload cannot contain the
      // wrapper of another binding: it would have to contain its own hash.
      let suffix, counter = 0;
      do { suffix = createHash('sha256').update(`${nonce}\0${value.at}\0${counter++}`).digest('hex').slice(0, 16); } while (text.includes(`data-${suffix}`));
      result.developer += `[${value.at}: see <data-${suffix}> in the user turn]`;
      if (!seen.has(value.at)) {
        seen.add(value.at);
        result.user += `<data-${suffix} path="${value.at}" origin="${value.origin}">${text}</data-${suffix}>\n`;
        result.bindings.push({ path: value.at, origin: value.origin });
      }
    } else if (value !== false && value != null) result.developer += String(value);
    if (result.developer.length + result.user.length > 1_048_576) throw new Error('prompt size limit exceeded');
  }
  function run(component, props, slots) {
    if (++budget.depth > 32) throw new Error('component depth limit exceeded');
    const scope = Object.create(null);
    for (const binding of component.bindings) {
      const value = raw(props[binding.prop]) === undefined ? binding.fallback : props[binding.prop];
      if (binding.pattern) bind(binding.pattern, value, scope); else scope[binding.local] = value;
    }
    for (const local of component.locals) bind(local.pattern, located(component, local.ast, () => evaluate(local.ast, scope, component, slots, budget)), scope);
    walk(component.nodes, scope, component, slots);
    budget.depth--;
  }
  function walk(nodes, scope, component, slots) {
    for (const node of nodes) {
      if (--budget.remaining < 0) throw new Error('render operation limit exceeded');
      if (node.op === 'text') emit(node.text);
      else if (node.op === 'expression') emit(located(component, node.ast, () => evaluate(node.ast, scope, component, slots, budget)));
      else if (node.op === 'fragment') walk(node.children, scope, component, slots);
      else if (node.op === 'slot') {
        if (slots?.has(node.name)) walk(slots.nodes.get(node.name), slots.scope, slots.unit, slots.slots);
        else walk(node.children, scope, component, slots);
      }
      else if (node.op === 'section') {
        emit(`<${node.name}>`); walk(node.children, scope, component, slots); emit(`</${node.name}>`);
      } else if (node.op === 'component') {
        const props = Object.create(null);
        for (const a of node.attrs) props[a.name] = a.literal ? a.value : located(component, a.value, () => evaluate(a.value, scope, component, slots, budget));
        run(component.imports.get(node.name), props, slotSet(node.children, scope, slots, component));
      }
    }
  }
  if ((unit.requiresContext || unit.bindings.some((binding) => binding.prop === 'context')) && !contextSnapshot) throw new Error('context snapshot required');
  // Per-field origins, as a workflow's bindings assign them; the input as a
  // whole is as trusted as its least trusted field.
  let fields;
  if (origins && input && typeof input === 'object' && !Array.isArray(input)) {
    fields = Object.create(null);
    for (const key of Object.keys(input)) fields[key] = Object.hasOwn(origins, key) ? origins[key] : origin;
  }
  const whole = fields ? Object.values(fields).reduce((least, o) => (trust[o] < trust[least] ? o : least), 'team') : origin;
  const props = { input: data(input, 'input', fields && Object.keys(fields).length ? whole : origin, fields) };
  if (contextSnapshot) props.context = data(contextForStep(contextSnapshot, stepId), 'context', 'team');
  nonce = createHash('sha256').update(JSON.stringify([runId, input, props.context?.value ?? null, unit.config.examples ?? []])).digest('hex');
  run(unit, props, null);
  const examples = unit.config.examples ?? [];
  if (!Array.isArray(examples) || examples.length > 1000) throw new Error('examples require an array of at most 1000 values');
  for (const [index, example] of examples.entries()) {
    if (--budget.remaining < 0) throw new Error('render operation limit exceeded');
    emit(`\n\nExample ${JSON.stringify(example.name)}\nInput: `);
    emit(data(example.input, `examples[${index}].input`, 'team'));
    emit('\nOutput: ');
    emit(data(example.output, `examples[${index}].output`, 'team'));
  }
  return result;
}
