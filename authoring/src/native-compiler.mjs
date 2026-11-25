// SPDX-License-Identifier: Apache-2.0
// These operations are installed by the compiler child before it loads the worker.
export const parse = globalThis.__atomicParseAstro;
export const convertToTSX = (text, { filename }) =>
  globalThis.__atomicAstroTsx(text, filename.split('/').at(-1));

// Last entry starting at or before the sought UTF-16 offset.
function preceding(entries, offset, start) {
  let lo = 0, hi = entries.length;
  while (lo < hi) {
    const mid = lo + Math.floor((hi - lo) / 2);
    if (start(entries[mid]) <= offset) lo = mid + 1;
    else hi = mid;
  }
  return lo - 1;
}

export function originalPosition({ mappings }, text) {
  const lines = [0];
  for (let i = 0; i < text.length; i++) if (text[i] === '\n') lines.push(i + 1);
  return (_, offset) => {
    const run = mappings[preceding(mappings, offset, entry => entry[0])];
    if (!run || run[1] === null) return undefined;
    const authored = Math.min(text.length, run[1] + offset - run[0]);
    const line = preceding(lines, authored, entry => entry);
    return { line: line + 1, column: authored - lines[line] };
  };
}
