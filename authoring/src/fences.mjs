// SPDX-License-Identifier: Apache-2.0
// A ```mermaid fence in an atom's body is prompt text, read literally: its
// braces and angle brackets are diagram syntax, not expressions or tags.
// Before the Astro parser and the TypeScript conversion see a source, each
// such fence's characters are written as entities, which the renderer
// decodes back to the same text. Line breaks are kept, so every line number
// stays the authored one.
const ENTITIES = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '{': '&#123;', '}': '&#125;' };

export function literalFences(source) {
  // Only the body: the frontmatter, between the first two `---` lines, is code.
  const lines = source.split('\n');
  let start = 0;
  if (lines[0]?.trim() === '---') {
    const end = lines.findIndex((line, i) => i > 0 && line.trim() === '---');
    if (end > 0) start = end + 1;
  }
  let open = false;
  for (let i = start; i < lines.length; i++) {
    const trimmed = lines[i].trim();
    if (!open && /^```mermaid\s*$/.test(trimmed)) { open = true; continue; }
    if (open && trimmed === '```') { open = false; continue; }
    if (open) lines[i] = lines[i].replace(/[&<>{}]/g, (c) => ENTITIES[c]);
  }
  return lines.join('\n');
}
