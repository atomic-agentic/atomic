// SPDX-License-Identifier: Apache-2.0
// @astrojs/compiler with parse() answered by @astrojs/compiler-rs, its tree
// converted to the Go compiler's shape (normalize.mjs). Everything else,
// convertToTSX included, stays the Go compiler's. rs-hook.mjs installs it.
//
// SPIKE_FENCE_NEWLINE=keep leaves compiler-rs's line break after the closing
// frontmatter fence in place; by default it is dropped, as Go does.
import { parse as rsParse } from '@astrojs/compiler-rs';
import { normalizeRs, dropFenceNewline } from './normalize.mjs';

export * from '@astrojs/compiler';

const position = (pos) => pos ? { start: { line: pos.line, column: pos.column, offset: pos.offset } } : undefined;
function toGo(n) {
  switch (n.k) {
    case 'frontmatter': return { type: 'frontmatter', value: n.code, position: position(n.pos) };
    case 'text': return { type: 'text', value: n.raw, position: position(n.pos) };
    case 'comment': return { type: 'comment', value: n.value, position: position(n.pos) };
    case 'doctype': return { type: 'doctype', position: position(n.pos) };
    case 'expression': return {
      type: 'expression', position: position(n.pos),
      children: n.parts.map((p) => typeof p === 'string' ? { type: 'text', value: p, position: position(n.pos) } : toGo(p)),
    };
    default:
      if (['element', 'component', 'fragment', 'custom-element'].includes(n.k)) return {
        type: n.k, name: n.name, position: position(n.pos),
        attributes: n.attrs.map((a) => ({ type: 'attribute', kind: a.kind, name: a.name, value: a.value, position: position(a.pos) })),
        children: n.children.map(toGo),
      };
      return { type: n.k.replace(/^rs:/, 'unknown-'), position: position(n.pos) };
  }
}

export async function parse(source) {
  const result = rsParse(source);
  let tree = normalizeRs(result.ast, source);
  if (process.env.SPIKE_FENCE_NEWLINE !== 'keep') tree = dropFenceNewline(tree);
  return {
    ast: { type: 'root', children: tree.map(toGo) },
    diagnostics: (result.diagnostics ?? []).map((d) => ({
      severity: d.severity === 'error' ? 1 : 2, text: d.text, hint: d.hint,
      location: d.labels?.[0] ? { line: d.labels[0].line, column: d.labels[0].column + 1 } : undefined,
    })),
  };
}
