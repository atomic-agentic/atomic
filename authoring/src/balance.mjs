// SPDX-License-Identifier: Apache-2.0
// Whether an atom's tags balance, read from the Astro compiler's own parse
// rather than a second tokenizer. Astro closes an element left open at the
// end of the file, drops a closing tag that closes nothing, and lets a
// closing tag of another name close the innermost element, all without a
// diagnostic; and it moves some elements to another parent than the one
// they are written in. Tags are a prompt's structure, so each of these is
// refused, naming the line and the tag.
//
// The parse gives each element where it starts and, when the source closes
// it, where its closing tag ends; an element the source never closes has no
// end. A self-closing tag and a void element have an end, but not one in
// the source: what follows them is read from where they start.

const VOID = new Set(['area', 'base', 'br', 'col', 'command', 'embed', 'hr', 'img', 'input', 'keygen', 'link', 'meta', 'param', 'source', 'track', 'wbr']);
const TAGS = new Set(['element', 'component', 'custom-element', 'fragment']);

const shown = (name) => `<${name}>`;

// The first unbalanced tag in `text`, as parsed into `ast`, if any:
// `{ line, message }`. The parse counts in bytes, so the source is read as
// one character a byte.
export function unbalanced(text, ast) {
  const bytes = new TextEncoder().encode(text);
  let source = '';
  for (let i = 0; i < bytes.length; i += 8192) source += String.fromCharCode(...bytes.subarray(i, i + 8192));
  const named = (raw) => new TextDecoder().decode(Uint8Array.from(raw, (c) => c.charCodeAt(0)));
  const found = check(source, ast, named);
  return found && { line: lineOf(source, found.offset), message: found.message };
}

function check(source, ast, named) {
  const tags = [];
  const covered = [];
  // Every tag in the parse, with its parent tag, in order.
  const walk = (node, parent) => {
    for (const child of node.children ?? []) {
      const start = child.position?.start?.offset;
      const end = child.position?.end?.offset;
      if (TAGS.has(child.type)) {
        const tag = { node: child, name: child.name ?? '', raw: '', start, end, parent };
        tags.push(tag);
        walk(child, tag);
      } else {
        if (start !== undefined && end !== undefined && child.type !== 'expression') covered.push([start, end]);
        walk(child, parent);
      }
    }
  };
  walk(ast, null);
  // Each tag's end in the source: its closing tag, or none.
  for (const tag of tags) {
    tag.raw = Array.from(new TextEncoder().encode(tag.name), (b) => String.fromCharCode(b)).join('');
  }
  for (const tag of tags) {
    const { name, raw, end } = tag;
    // The parse can place a tag's start a byte off its `<`.
    const start = [tag.start, tag.start - 1, tag.start + 1].find((at) => source[at] === '<') ?? tag.start;
    tag.start = start;
    const open = openEnd(source, start);
    const selfClosing = source[open - 2] === '/';
    if (VOID.has(name) || selfClosing) {
      // Nothing to close.
      tag.point = true;
      tag.open = open;
      continue;
    }
    tag.open = open;
    if (end === undefined) {
      // Closed by Astro at the end, or by an ancestor's end: unclosed here,
      // or closed where Astro does not read the closing tag as its own.
      const later = source.indexOf(`</${raw}>`, open);
      const message = later < 0
        ? `${shown(name)} is never closed; close it with \`</${name}>\``
        : `${shown(name)} is closed on line ${lineOf(source, later)}, but Astro does not read that \`</${name}>\` as its end; move ${shown(name)} into an element of its own`;
      return { offset: start, message };
    }
    // The parse ends a tag where a closing tag of its own name's length
    // would end, so the closing tag that closed it starts there; a
    // fragment's `</>` one byte before its end.
    const at = tag.node.type === 'fragment' && name === '' ? end - 1 : end - (raw.length + 3);
    const closing = new RegExp(`^</${escapeRegExp(raw)}\\s*>`).exec(source.slice(at, at + raw.length + 256));
    if (closing) {
      tag.close = at;
      tag.to = at + closing[0].length;
      covered.push([at, tag.to]);
      continue;
    }
    // A closing tag of another name closed it.
    const other = /^<\/([^\s/>]*)/.exec(source.slice(at, at + 4096));
    const written = other ? named(other[1]) : '';
    return { offset: at, message: `\`</${written}>\` closes ${shown(name)}, opened on line ${lineOf(source, start)}; close it with \`</${name}>\`` };
  }
  // A closing tag the parse dropped: one that is no element's end and is in
  // no text, comment or frontmatter, nor inside a start tag.
  for (const tag of tags) covered.push([tag.start, tag.open]);
  covered.sort((a, b) => a[0] - b[0]);
  const inside = (offset) => covered.some(([s, e]) => s <= offset && offset < e);
  const closes = new Set(tags.filter((t) => t.close !== undefined).map((t) => t.close));
  for (const match of source.matchAll(/<\/([^\s/>]*)\s*>/g)) {
    if (closes.has(match.index) || inside(match.index)) continue;
    return { offset: match.index, message: `\`</${named(match[1])}>\` closes no open element; remove it` };
  }
  // Moved: a tag whose parent in the parse is not the tag it is written in.
  const ranged = tags
    .map((t) => ({ tag: t, from: t.start, to: t.point ? t.open : t.to }))
    .sort((a, b) => a.from - b.from || b.to - a.to);
  const stack = [];
  for (const r of ranged) {
    while (stack.length && stack.at(-1).to <= r.from) stack.pop();
    const written = stack.at(-1)?.tag ?? null;
    if (written !== r.tag.parent) {
      const where = written ? `inside ${shown(written.name)}, opened on line ${lineOf(source, written.start)}` : 'at the top of the file';
      return { offset: r.from, message: `${shown(r.tag.name)} is written ${where}, but Astro reads it elsewhere; move it, or close what is open before it` };
    }
    stack.push(r);
  }
  return null;
}

// Where the start tag that begins at `start` ends, its quoted values and
// braced expressions passed over.
function openEnd(source, start) {
  let depth = 0, quote = null;
  for (let i = start + 1; i < source.length; i++) {
    const c = source[i];
    if (quote) { if (c === quote) quote = null; continue; }
    if (depth > 0) {
      if (c === '{') depth++;
      else if (c === '}') depth--;
      else if (c === '"' || c === "'" || c === '`') quote = c;
      continue;
    }
    if (c === '"' || c === "'" || c === '`') quote = c;
    else if (c === '{') depth = 1;
    else if (c === '>') return i + 1;
  }
  return source.length;
}

function escapeRegExp(text) {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

export function lineOf(source, offset) {
  let line = 1;
  for (let i = 0; i < offset && i < source.length; i++) if (source.charCodeAt(i) === 10) line++;
  return line;
}
