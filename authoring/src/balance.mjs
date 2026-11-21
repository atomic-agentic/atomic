// SPDX-License-Identifier: Apache-2.0
// Whether an atom's tags balance, read from the Astro compiler's own parse
// rather than a second tokenizer. Astro closes an element left open at the
// end of the file, drops a closing tag that closes nothing, and lets a
// closing tag of another name close the innermost element, all without a
// diagnostic; and it moves some elements to another parent than the one
// they are written in. Tags are a prompt's structure, so each of these is
// refused, naming the line and the tag.
//
// The parse gives each element where it starts and, for most elements the
// source closes, where a closing tag of its own name would end; an element
// it closed itself has no end. Its positions for void and self-closing
// tags, fragments and elements it re-reads are not the source's, so where
// a start tag ends is read from the source: a scan of the start tag alone,
// its quoted values and braced expressions passed over.

const VOID = new Set(['area', 'base', 'br', 'col', 'command', 'embed', 'hr', 'img', 'input', 'keygen', 'link', 'meta', 'param', 'source', 'track', 'wbr']);
const TAGS = new Set(['element', 'component', 'custom-element', 'fragment']);
// HTML's white space, not JavaScript's: the source is read a byte a
// character, and `\s` would take a byte such as 0xA0 for a space.
const SPACE = '\\t\\n\\f\\r ';
const isSpace = (c) => c === ' ' || c === '\t' || c === '\n' || c === '\f' || c === '\r';

const shown = (name) => `<${name}>`;
const voidMessage = (name) => `\`</${name}>\` closes nothing: <${name}> is a void HTML element, which has no closing tag and cannot be a section's name`;

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
        const name = child.name ?? '';
        const raw = Array.from(new TextEncoder().encode(name), (b) => String.fromCharCode(b)).join('');
        const tag = { node: child, name, raw, start, end, parent };
        tags.push(tag);
        walk(child, tag);
      } else {
        if (start !== undefined && end !== undefined && child.type !== 'expression') covered.push([start, end]);
        walk(child, parent);
      }
    }
  };
  walk(ast, null);
  // Where each tag starts and its start tag ends, and the closing tag that
  // ends it: its own, another's, or none the parse reads.
  for (const tag of tags) {
    // The parse can place a tag's start a byte off its `<`.
    tag.start = [tag.start, tag.start - 1, tag.start + 1].find((at) => source[at] === '<') ?? tag.start;
    const { open, selfClosing } = startTag(source, tag.start);
    tag.open = open;
    if (VOID.has(tag.name) || selfClosing) {
      tag.point = true;
      continue;
    }
    if (tag.end === undefined) continue;
    // The parse ends a tag where a closing tag of its own name's length
    // would end, so the closing tag that ended it starts there; a
    // fragment's `</>` one byte before its end.
    const at = tag.node.type === 'fragment' && tag.name === '' ? tag.end - 1 : tag.end - (tag.raw.length + 3);
    if (at < tag.open) continue;
    const closer = closerAt(source, at);
    if (!closer) continue;
    if (closer.raw === tag.raw) {
      tag.close = at;
      tag.to = at + closer.length;
      covered.push([at, tag.to]);
    } else {
      tag.closer = { at, name: named(closer.raw) };
    }
  }
  // A closing tag of another name that ended a tag: the tag it closed is
  // the one to close, at the closing tag's line.
  const mismatched = tags.filter((t) => t.closer).sort((a, b) => a.closer.at - b.closer.at)[0];
  if (mismatched) {
    const { at, name } = mismatched.closer;
    return { offset: at, message: `\`</${name}>\` closes ${shown(mismatched.name)}, opened on line ${lineOf(source, mismatched.start)}; close it with \`</${mismatched.name}>\`` };
  }
  // What a closing tag cannot be read from: text, comments, frontmatter,
  // start tags, and the closing tags that are other elements' ends.
  for (const tag of tags) covered.push([tag.start, tag.open]);
  const inside = (offset) => covered.some(([s, e]) => s <= offset && offset < e);
  const closes = new Set(tags.filter((t) => t.close !== undefined).map((t) => t.close));
  const free = (offset) => !closes.has(offset) && !inside(offset);
  // A tag with no closing tag the parse reads as its own: never closed, or
  // closed by a closing tag Astro does not read as its end, one that is no
  // other element's and in nothing else.
  const endless = tags.find((t) => !t.point && t.close === undefined);
  if (endless) {
    const later = findCloser(source, endless.raw, endless.open, free);
    const message = later < 0
      ? `${shown(endless.name)} is never closed; close it with \`</${endless.name}>\``
      : `${shown(endless.name)} is closed on line ${lineOf(source, later)}, but Astro does not read that \`</${endless.name}>\` as its end; close it in the same expression it opens in, and put no text or expression before an element Astro moves, such as <slot>, <head> or <template>`;
    return { offset: endless.start, message };
  }
  // A closing tag the parse dropped: one that is no element's end and is in
  // no text, comment or frontmatter, nor inside a start tag.
  for (const closer of closers(source)) {
    if (!free(closer.at)) continue;
    const name = named(closer.raw);
    return { offset: closer.at, message: VOID.has(name) ? voidMessage(name) : `\`</${name}>\` closes no open element; remove it` };
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

// The closing tag at `at`, if one starts there: its name, a byte a
// character, and its length.
function closerAt(source, at) {
  const match = new RegExp(`^</([^${SPACE}/>]*)[^>]*>`).exec(source.slice(at, at + 4096));
  return match && { raw: match[1], length: match[0].length };
}

// Where the next closing tag for `raw` from `from` that `free` allows
// starts, or -1.
function findCloser(source, raw, from, free) {
  for (const closer of closers(source, from)) {
    if (closer.raw === raw && free(closer.at)) return closer.at;
  }
  return -1;
}

// Every closing tag from `from` on, `</name …>` or `</>`, with where it
// starts and its name, a byte a character: one pass, each `>` found once,
// so no source makes the scan quadratic.
function* closers(source, from = 0) {
  let gt = -1;
  for (let at = source.indexOf('</', from); at >= 0; at = source.indexOf('</', at + 2)) {
    if (gt < at) {
      gt = source.indexOf('>', at);
      if (gt < 0) return;
    }
    let end = at + 2;
    while (end < gt && !isSpace(source[end]) && source[end] !== '/') end++;
    const raw = source.slice(at + 2, end);
    if (raw === '' && gt !== at + 2) continue;
    yield { at, raw };
  }
}

// Where the start tag that begins at `start` ends, and whether it closes
// itself. Attribute values are quoted text, read to the matching quote, or
// braced expressions, read as TypeScript to the matching brace: quotes,
// escapes, template literals and comments inside passed over. A quote
// outside a value is part of an attribute's name.
function startTag(source, start) {
  let i = start + 1;
  const ends = (at) => source[at] === '>' || (source[at] === '/' && source[at + 1] === '>');
  // The tag's name, to white space, `/` or `>`.
  while (i < source.length && !isSpace(source[i]) && source[i] !== '/' && source[i] !== '>') i++;
  while (i < source.length) {
    const c = source[i];
    if (isSpace(c)) { i++; continue; }
    if (c === '>') return { open: i + 1, selfClosing: false };
    if (c === '/' && source[i + 1] === '>') return { open: i + 2, selfClosing: true };
    if (c === '{') { i = pastExpression(source, i); continue; }
    if (c === '=') {
      i++;
      while (i < source.length && isSpace(source[i])) i++;
      const v = source[i];
      if (v === '"' || v === "'") {
        const close = source.indexOf(v, i + 1);
        i = close < 0 ? source.length : close + 1;
      } else if (v === '{') {
        i = pastExpression(source, i);
      } else if (v === '`') {
        i = pastTemplate(source, i + 1);
      } else {
        while (i < source.length && !isSpace(source[i]) && source[i] !== '>') i++;
      }
      continue;
    }
    // An attribute's name, to white space, `=`, `>` or `/>`.
    i++;
    while (i < source.length && !isSpace(source[i]) && source[i] !== '=' && !ends(i)) i++;
  }
  return { open: source.length, selfClosing: false };
}

// Past the braced expression that starts at `i`.
function pastExpression(source, i) {
  return pastScript(source, i, []);
}

// Past a template literal from just after its opening backtick.
function pastTemplate(source, i) {
  return pastScript(source, i, ['`']);
}

// Past TypeScript from `i` until what `open` holds, and whatever opens on
// the way, is closed: braces, and template literals with their `${…}`
// parts, kept on a stack of their own rather than by recursion, so no
// source is deep enough to exhaust the call stack. Strings, with their
// escapes, and comments are passed over.
function pastScript(source, i, open) {
  const stack = [...open];
  while (i < source.length) {
    const c = source[i];
    if (stack.at(-1) === '`') {
      if (c === '\\') i += 2;
      else if (c === '`') { stack.pop(); i++; if (!stack.length) return i; }
      else if (c === '$' && source[i + 1] === '{') { stack.push('{'); i += 2; }
      else i++;
      continue;
    }
    if (c === '{') { stack.push('{'); i++; }
    else if (c === '}') { stack.pop(); i++; if (!stack.length) return i; }
    else if (c === '"' || c === "'") i = pastString(source, i);
    else if (c === '`') { stack.push('`'); i++; }
    else if (c === '/' && source[i + 1] === '/') { const e = source.indexOf('\n', i); i = e < 0 ? source.length : e + 1; }
    else if (c === '/' && source[i + 1] === '*') { const e = source.indexOf('*/', i + 2); i = e < 0 ? source.length : e + 2; }
    else i++;
  }
  return source.length;
}

// Past the quoted string that starts at `i`, its escapes passed over.
function pastString(source, i) {
  const quote = source[i];
  for (i++; i < source.length; i++) {
    if (source[i] === '\\') i++;
    else if (source[i] === quote || source[i] === '\n') return i + 1;
  }
  return source.length;
}

export function lineOf(source, offset) {
  let line = 1;
  for (let i = 0; i < offset && i < source.length; i++) if (source.charCodeAt(i) === 10) line++;
  return line;
}
