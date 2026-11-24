// SPDX-License-Identifier: Apache-2.0
// TypeScript, absent. Every `ts.*` use in `src/renderer.mjs` is inside a
// function the boundary probe never reaches, so any member will do.
export default new Proxy({}, { get: () => () => undefined });
export const SyntaxKind = new Proxy({}, { get: () => -1 });
