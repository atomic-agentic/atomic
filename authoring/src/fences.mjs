// SPDX-License-Identifier: Apache-2.0
// A ```mermaid fence in an atom's body is prompt text, read literally: its
// braces and angle brackets are diagram syntax, not expressions or tags.
// Before the Astro parser and the TypeScript conversion see a source, each
// such fence's characters are written as entities, which the renderer
// decodes back to the same text. Line breaks are kept, so every line number
// stays the authored one.
const ENTITIES = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '{': '&#123;', '}': '&#125;' };

// A fence left open would turn the rest of the body into literal text,
// expressions and tags included, so it is an error at its opening line.
export class FenceError extends Error {
  constructor(message, line) { super(message); this.line = line; }
}

export function literalFences(source) {
  // Only the body: the frontmatter, between the first two `---` lines, is code.
  const lines = source.split('\n');
  let start = 0;
  if (lines[0]?.trim() === '---') {
    const end = lines.findIndex((line, i) => i > 0 && line.trim() === '---');
    if (end > 0) start = end + 1;
  }
  let open = -1;
  for (let i = start; i < lines.length; i++) {
    const trimmed = lines[i].trim();
    if (open < 0 && /^```mermaid\s*$/.test(trimmed)) { open = i; continue; }
    if (open >= 0 && trimmed === '```') { open = -1; continue; }
    if (open >= 0) lines[i] = lines[i].replace(/[&<>{}]/g, (c) => ENTITIES[c]);
  }
  if (open >= 0) throw new FenceError('this ```mermaid fence is never closed; close it with a ``` line, or everything after it would be read as literal text', open + 1);
  return lines.join('\n');
}
