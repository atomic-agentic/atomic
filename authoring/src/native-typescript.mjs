// SPDX-License-Identifier: Apache-2.0
// The bounded renderer consumes a small subset of TypeScript's syntax API.
// Keep unknown ESTree kinds opaque so its existing allowlist rejects them.
const SyntaxKind = new Proxy({}, { get: (_, name) => name });
function createSourceFile(fileName, text) {
  const { ast, diagnostics } = globalThis.__atomicTypeScript('parse', fileName, text);
  const lines = [0];
  for (let i = 0; i < text.length; i++) if (text[i] === '\n') lines.push(i + 1);
  const file = {
    fileName,
    parseDiagnostics: diagnostics,
    getLineAndCharacterOfPosition(offset) {
      let lo = 0,
        hi = lines.length;
      while (lo + 1 < hi) {
        const mid = Math.floor((lo + hi) / 2);
        if (lines[mid] <= offset) lo = mid;
        else hi = mid;
      }
      return { line: lo, character: offset - lines[lo] };
    },
  };
  const cache = new WeakMap();
  function node(n) {
    if (!n) return undefined;
    if (cache.has(n)) return cache.get(n);
    const v = {
      kind: n.type,
      getStart: () => n.start,
      getText: () => text.slice(n.start, n.end),
      getSourceFile: () => file,
    };
    cache.set(n, v);
    const list = (items) => (items ?? []).map(node);
    // Walk the original tree, including type positions that have no runtime
    // field in the interpreter, for the any and dynamic-import checks.
    v.forEachChild = (visit) => {
      for (const value of Object.values(n)) {
        if (Array.isArray(value)) {
          for (const item of value) if (item?.type) visit(node(item));
        } else if (value?.type) visit(node(value));
      }
    };
    const binding = (p) => ({
      name: node(p.type === 'AssignmentPattern' ? p.left : p.type === 'RestElement' ? p.argument : p),
      initializer: node(p.right),
      dotDotDotToken: p.type === 'RestElement',
    });
    switch (n.type) {
      case 'Program':
        v.statements = list(n.body);
        break;
      case 'TSAnyKeyword':
        v.kind = 'AnyKeyword';
        break;
      case 'Identifier':
        v.text = n.name;
        break;
      case 'Literal':
        v.kind =
          n.regex || n.bigint
            ? 'UnsupportedLiteral'
            : typeof n.value === 'string'
              ? 'StringLiteral'
              : typeof n.value === 'number'
                ? 'NumericLiteral'
                : n.value === null
                  ? 'NullKeyword'
                  : n.value
                    ? 'TrueKeyword'
                    : 'FalseKeyword';
        v.text = String(n.value);
        break;
      case 'ImportDeclaration': {
        v.moduleSpecifier = node(n.source);
        if (!n.specifiers.length && !/^import\s*\{/.test(v.getText())) break;
        const named = n.specifiers.filter((s) => s.type === 'ImportSpecifier');
        const space = n.specifiers.find((s) => s.type === 'ImportNamespaceSpecifier');
        const def = n.specifiers.find((s) => s.type === 'ImportDefaultSpecifier');
        v.importClause = {
          isTypeOnly: n.importKind === 'type',
          name: node(def?.local),
          namedBindings: space
            ? { kind: 'NamespaceImport' }
            : named.length || !def
              ? {
                  kind: 'NamedImports',
                  elements: named.map((s) => ({
                    name: node(s.local),
                    propertyName: node(s.imported),
                    isTypeOnly: s.importKind === 'type',
                  })),
                }
              : undefined,
        };
        break;
      }
      case 'ExportNamedDeclaration':
        if (n.declaration) return node(n.declaration);
        v.kind = 'ExportDeclaration';
        v.moduleSpecifier = node(n.source);
        v.isTypeOnly = n.exportKind === 'type';
        v.exportClause = {
          kind: 'NamedExports',
          elements: n.specifiers.map((s) => ({ isTypeOnly: s.exportKind === 'type' })),
        };
        break;
      case 'ExportAllDeclaration':
        v.kind = 'ExportDeclaration';
        v.moduleSpecifier = node(n.source);
        v.isTypeOnly = n.exportKind === 'type';
        break;
      case 'TSInterfaceDeclaration':
        v.kind = 'InterfaceDeclaration';
        break;
      case 'TSTypeAliasDeclaration':
        v.kind = 'TypeAliasDeclaration';
        break;
      case 'VariableDeclaration':
        v.kind = 'VariableStatement';
        v.declarationList = { flags: n.kind === 'const' ? 1 : 0, declarations: list(n.declarations) };
        break;
      case 'VariableDeclarator':
        v.name = node(n.id);
        v.type = node(n.id.typeAnnotation?.typeAnnotation);
        v.initializer = node(n.init);
        break;
      case 'ObjectPattern':
        v.kind = 'ObjectBindingPattern';
        v.elements = n.properties.map((p) => ({
          ...binding(p.value ?? p),
          propertyName: p.computed ? { kind: 'ComputedPropertyName' } : node(p.key),
        }));
        break;
      case 'TSTypeReference':
        v.kind = 'TypeReferenceNode';
        v.typeName = node(n.typeName);
        v.typeArguments = list(n.typeArguments?.params);
        break;
      case 'TSAsExpression':
        v.kind = 'AsExpression';
        v.expression = node(n.expression);
        v.type = node(n.typeAnnotation);
        break;
      case 'ImportExpression':
        v.kind = 'CallExpression';
        v.expression = { kind: 'ImportKeyword' };
        break;
      case 'ObjectExpression':
        v.kind = 'ObjectLiteralExpression';
        v.properties = list(n.properties);
        break;
      case 'ArrayExpression':
        v.kind = 'ArrayLiteralExpression';
        v.elements = list(n.elements);
        break;
      case 'Property':
        if (!n.computed && !n.shorthand && !n.method && n.kind === 'init') {
          v.kind = 'PropertyAssignment';
          v.name = node(n.key);
          v.initializer = node(n.value);
        }
        break;
      case 'ParenthesizedExpression':
        v.expression = node(n.expression);
        break;
      case 'ChainExpression': {
        const inner = node(n.expression);
        return Object.assign({}, inner, { optionalChain: true });
      }
      case 'MemberExpression':
        if (!n.computed && n.property.type === 'Identifier') {
          v.kind = 'PropertyAccessExpression';
          v.expression = node(n.object);
          v.name = node(n.property);
          v.questionDotToken = n.optional;
        }
        break;
      case 'UnaryExpression':
        v.kind = 'PrefixUnaryExpression';
        v.operator = { '!': 'ExclamationToken', '-': 'MinusToken' }[n.operator];
        v.operand = node(n.argument);
        break;
      case 'BinaryExpression':
      case 'LogicalExpression':
        v.kind = 'BinaryExpression';
        v.left = node(n.left);
        v.right = node(n.right);
        v.operatorToken = { getText: () => n.operator };
        break;
      case 'ConditionalExpression':
        v.condition = node(n.test);
        v.whenTrue = node(n.consequent);
        v.whenFalse = node(n.alternate);
        break;
      case 'ArrowFunctionExpression':
        v.kind = 'ArrowFunction';
        v.body = node(n.body);
        v.parameters = n.params.map(binding);
        v.modifiers = n.async ? ['async'] : [];
        break;
      case 'BlockStatement':
        v.kind = 'Block';
        break;
      case 'CallExpression':
        v.expression = node(n.callee);
        v.arguments = list(n.arguments);
        v.typeArguments = list(n.typeArguments?.params);
        v.questionDotToken = n.optional;
        break;
      case 'TemplateLiteral':
        if (!n.expressions.length) {
          v.kind = 'NoSubstitutionTemplateLiteral';
          v.text = n.quasis[0].value.cooked;
        } else {
          v.kind = 'TemplateExpression';
          v.head = { text: n.quasis[0].value.cooked };
          v.templateSpans = n.expressions.map((e, i) => ({
            expression: node(e),
            literal: { text: n.quasis[i + 1].value.cooked },
          }));
        }
        break;
    }
    return v;
  }
  if (ast) Object.assign(file, node(ast));
  return file;
}
const api = {
  createSourceFile,
  SyntaxKind,
  NodeFlags: { Const: 1 },
  ScriptTarget: {},
  ScriptKind: {},
  flattenDiagnosticMessageText: String,
  forEachChild: (node, visit) => node.forEachChild?.(visit),
  isOptionalChain: (node) => node?.optionalChain || node?.questionDotToken,
};
export default new Proxy(api, {
  get(target, name) {
    if (name in target) return target[name];
    if (typeof name === 'string' && name.startsWith('is')) return (node) => node?.kind === name.slice(2);
  },
});
