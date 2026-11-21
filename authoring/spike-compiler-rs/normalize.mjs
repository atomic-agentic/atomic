// SPDX-License-Identifier: Apache-2.0
// Normalise the two compilers' ASTs to one comparable tree.
//
// @astrojs/compiler (Go, WASM) returns an HTML-shaped tree: frontmatter as raw
// text, expressions as raw text interleaved with the elements nested in them,
// attributes with a `kind`. @astrojs/compiler-rs returns oxc's ESTree/JSX tree
// with every expression fully parsed. The common form is the Go shape, since
// that is what atomic's renderer lowers today:
//
//   {k: 'frontmatter', code}
//   {k: 'text', raw}
//   {k: 'comment', value}
//   {k: 'expression', parts: [string | node]}
//   {k: 'element' | 'component' | 'fragment' | 'custom-element', name, attrs, children}
//   attrs: [{name, kind, value}]
//
// Every node also gets `pos: {line, column, offset}` (1-based line and column,
// 0-based offset in UTF-16 code units for Rust, as the compiler reports it for Go)
// and, for the Go tree, `authoredLine`: the line atomic's renderer reports for it.

const lineStarts = (source) => {
  const starts = [0];
  for (let i = 0; i < source.length; i++) if (source[i] === '\n') starts.push(i + 1);
  return starts;
};

export function locator(source) {
  const starts = lineStarts(source);
  return (offset) => {
    let lo = 0, hi = starts.length - 1;
    while (lo < hi) { const mid = (lo + hi + 1) >> 1; if (starts[mid] <= offset) lo = mid; else hi = mid - 1; }
    return { line: lo + 1, column: offset - starts[lo] + 1, offset };
  };
}

const goPos = (node) => node.position?.start ? { line: node.position.start.line, column: node.position.start.column, offset: node.position.start.offset } : null;

// The line atomic's renderer uses (renderer.mjs `authoredLine`): Astro starts an
// expression's position before its brace, so the renderer takes its first child's.
const goAuthoredLine = (node) => (node.type === 'expression' ? node.children?.find((c) => c.position)?.position.start.line : undefined) ?? node.position?.start?.line;

function mergeParts(parts) {
  const out = [];
  for (const part of parts) {
    if (typeof part === 'string' && typeof out.at(-1) === 'string') out[out.length - 1] += part;
    else if (part !== '') out.push(part);
  }
  return out;
}

export function normalizeGo(ast) {
  const node = (n) => {
    const pos = goPos(n), authoredLine = goAuthoredLine(n);
    switch (n.type) {
      case 'frontmatter': return { k: 'frontmatter', code: n.value, pos, authoredLine };
      case 'text': return { k: 'text', raw: n.value, pos, authoredLine };
      case 'comment': return { k: 'comment', value: n.value, pos, authoredLine };
      case 'doctype': return { k: 'doctype', pos, authoredLine };
      case 'expression': return { k: 'expression', parts: mergeParts((n.children ?? []).map((c) => c.type === 'text' ? c.value : node(c))), pos, authoredLine };
      case 'element': case 'component': case 'fragment': case 'custom-element':
        return {
          k: n.type, name: n.name,
          attrs: (n.attributes ?? []).map((a) => ({ name: a.name, kind: a.kind, value: a.value, pos: goPos(a) })),
          children: (n.children ?? []).map(node), pos, authoredLine,
        };
      default: return { k: `go:${n.type}`, pos, authoredLine };
    }
  };
  return (ast.children ?? []).map(node);
}

const jsxName = (name) => {
  if (!name) return '';
  if (name.type === 'JSXIdentifier') return name.name;
  if (name.type === 'JSXMemberExpression') return `${jsxName(name.object)}.${jsxName(name.property)}`;
  if (name.type === 'JSXNamespacedName') return `${jsxName(name.namespace)}:${jsxName(name.name)}`;
  return `?${name.type}`;
};

// Go's element classification, applied to a JSX name.
const kindOf = (name) => {
  if (name === 'Fragment' || name === '') return 'fragment';
  if (name.includes('.') || /^[A-Z]/.test(name)) return 'component';
  if (name.includes('-')) return 'custom-element';
  return 'element';
};

// The outermost JSX elements or fragments inside an expression subtree, in source order.
function jsxInside(value, found = []) {
  if (Array.isArray(value)) { for (const v of value) jsxInside(v, found); return found; }
  if (!value || typeof value !== 'object') return found;
  if (value.type === 'JSXElement' || value.type === 'JSXFragment') { found.push(value); return found; }
  for (const [key, v] of Object.entries(value)) if (key !== 'start' && key !== 'end') jsxInside(v, found);
  return found;
}

export function normalizeRs(ast, source) {
  const at = locator(source);
  const pos = (n) => at(n.start);
  const node = (n) => {
    switch (n.type) {
      case 'JSXText': return { k: 'text', raw: source.slice(n.start, n.end), pos: pos(n) };
      case 'AstroDoctype': return { k: 'doctype', pos: pos(n) };
      case 'AstroComment': return { k: 'comment', value: n.value, pos: pos(n) };
      case 'AstroScript': return { k: 'text', raw: source.slice(n.program?.start ?? n.start, n.program?.end ?? n.end), pos: pos(n), note: 'AstroScript' };
      case 'JSXExpressionContainer': {
        const inner = [n.start + 1, n.end - 1];
        const nested = jsxInside(n.expression).sort((a, b) => a.start - b.start);
        const parts = [];
        let cursor = inner[0];
        for (const child of nested) { parts.push(source.slice(cursor, child.start)); parts.push(node(child)); cursor = child.end; }
        parts.push(source.slice(cursor, inner[1]));
        return { k: 'expression', parts: mergeParts(parts), pos: pos(n) };
      }
      case 'JSXFragment':
        return { k: 'fragment', name: '', attrs: [], children: n.children.map(node), pos: pos(n) };
      case 'JSXElement': {
        const name = jsxName(n.openingElement.name);
        const attrs = n.openingElement.attributes.map((a) => {
          if (a.type === 'JSXSpreadAttribute') return { name: source.slice(a.argument.start, a.argument.end), kind: 'spread', value: '', pos: pos(a) };
          const attr = { name: jsxName(a.name), pos: pos(a) };
          const v = a.value;
          if (v === null || v === undefined) return { ...attr, kind: 'empty', value: '' };
          if (v.type === 'Literal') return { ...attr, kind: 'quoted', value: source.slice(v.start + 1, v.end - 1) };
          if (v.type === 'JSXExpressionContainer') {
            if (v.start === a.start) return { ...attr, kind: 'shorthand', value: '' };
            if (v.expression?.type === 'TemplateLiteral' && v.start === v.expression.start) return { ...attr, kind: 'template-literal', value: source.slice(v.start + 1, v.end - 1) };
            return { ...attr, kind: 'expression', value: source.slice(v.start + 1, v.end - 1) };
          }
          return { ...attr, kind: `rs:${v.type}`, value: source.slice(v.start, v.end) };
        });
        return { k: kindOf(name), name, attrs, children: n.children.map(node), pos: pos(n) };
      }
      default: return { k: `rs:${n.type}`, pos: pos(n) };
    }
  };
  const out = [];
  // compiler-rs reports an empty frontmatter node when a source has none.
  if (ast.frontmatter && ast.frontmatter.end > ast.frontmatter.start) {
    const text = source.slice(ast.frontmatter.start, ast.frontmatter.end);
    out.push({ k: 'frontmatter', code: text.replace(/^---/, '').replace(/---$/, ''), pos: pos(ast.frontmatter) });
  }
  for (const n of ast.body ?? []) out.push(node(n));
  return out;
}

// Structural differences, ignoring positions. Returns [{path, go, rs}].
export function structuralDiff(a, b, path = '', out = []) {
  const strip = (v) => (v && typeof v === 'object' && !Array.isArray(v)) ? Object.fromEntries(Object.entries(v).filter(([k]) => !['pos', 'authoredLine', 'note'].includes(k))) : v;
  a = strip(a); b = strip(b);
  if (Array.isArray(a) && Array.isArray(b)) {
    const n = Math.max(a.length, b.length);
    if (a.length !== b.length) out.push({ path: `${path}.length`, go: a.length, rs: b.length });
    for (let i = 0; i < Math.min(a.length, b.length); i++) structuralDiff(a[i], b[i], `${path}[${i}]`, out);
    return out;
  }
  if (a && b && typeof a === 'object' && typeof b === 'object') {
    for (const key of new Set([...Object.keys(a), ...Object.keys(b)])) structuralDiff(a[key], b[key], path ? `${path}.${key}` : key, out);
    return out;
  }
  if (a !== b) out.push({ path, go: a, rs: b });
  return out;
}

// Pairs of nodes (and attributes) at the same structural path, for position comparison.
export function pairs(a, b, path = '', out = []) {
  if (Array.isArray(a) && Array.isArray(b)) {
    for (let i = 0; i < Math.min(a.length, b.length); i++) pairs(a[i], b[i], `${path}[${i}]`, out);
    return out;
  }
  if (a && b && typeof a === 'object' && typeof b === 'object') {
    if ('k' in a && 'k' in b && a.k === b.k) out.push({ path, kind: a.k, go: a, rs: b });
    else if ('kind' in a && 'name' in a && 'kind' in b) out.push({ path, kind: `attr:${a.kind}`, go: a, rs: b });
    for (const key of ['children', 'parts', 'attrs']) if (key in a && key in b) pairs(a[key], b[key], `${path}.${key}`, out);
  }
  return out;
}

// The one systematic difference: Go drops the line break straight after the
// closing frontmatter fence (and after a doctype); compiler-rs keeps it as text.
// This removes it from a normalised Rust tree, to show what else differs.
export function dropFenceNewline(tree) {
  const out = [...tree];
  for (let i = 0; i < out.length; i++) {
    if (!['frontmatter', 'doctype'].includes(out[i].k)) continue;
    const next = out[i + 1];
    if (next?.k !== 'text' || !/^\r?\n/.test(next.raw)) continue;
    const raw = next.raw.replace(/^\r?\n/, '');
    const shift = next.raw.length - raw.length;
    if (raw === '') out.splice(i + 1, 1);
    else out[i + 1] = { ...next, raw, pos: next.pos && { line: next.pos.line + 1, column: 1, offset: next.pos.offset + shift } };
  }
  return out;
}
