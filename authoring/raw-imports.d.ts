// SPDX-License-Identifier: Apache-2.0
// Stock Astro/TypeScript tooling resolves atomic's captured literal imports.
declare module '*.md?raw' {
  const text: string;
  export default text;
}
