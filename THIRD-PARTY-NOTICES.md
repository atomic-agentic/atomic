# Third-party notices

atomic is licensed under the Apache License, Version 2.0. It depends on, links
or reimplements the projects below. Each keeps its own licence. Where a project
is used from a fork, both the upstream and the fork are listed, with the
revision atomic pins.

This file covers the projects atomic is built around. The complete list of
crates in a release binary, with their licences, is in that release's software
bill of materials.

## OpenAI Codex

- Upstream: <https://github.com/openai/codex>
- Licence: Apache License 2.0
- Copyright 2025 OpenAI
- Use: atomic depends on Codex's Rust crates (`codex-rs`) at the revision in
  [`deps/codex.toml`](deps/codex.toml), and links them into the `atomic`
  binary. Codex's NOTICE is reproduced in this repository's [NOTICE](NOTICE).
  Codex's NOTICE credits code derived from Ratatui (MIT, Copyright (c)
  2016-2022 Florian Dehau, Copyright (c) 2023-2025 The Ratatui Developers).

## atomic's fork of Codex

- Fork: <https://github.com/atomic-agentic/codex>
- Licence: Apache License 2.0, unchanged from upstream
- Use: the upstream release tag in [`deps/codex.toml`](deps/codex.toml) plus
  two `utils/pty` commits that make the parent-death signal best effort where
  `prctl` is refused or unsupported. Codex core is unmodified. The commits are
  listed in [`deps/README.md`](deps/README.md).

## durare

- Upstream: <https://github.com/SamuelXing/durare>
- Licence: MIT OR Apache-2.0
- Copyright (c) 2026 durare contributors
- Use: atomic's durable-execution engine, linked into the `atomic` binary.

## atomic's fork of durare

- Fork: <https://github.com/atomic-agentic/durare>
- Licence: MIT OR Apache-2.0, unchanged from upstream
- Use: durare ported to sqlx 0.9 so that it links with Codex's SQLite stack.
  The upstream revision the fork starts from is recorded in the fork's README.

## sqlx, and the fork Codex uses

- Upstream: <https://github.com/launchbadge/sqlx>
- Fork: <https://github.com/transact-rs/sqlx>, the source Codex patches in for
  `sqlx-core` and `sqlx-sqlite` (revision in [`deps/README.md`](deps/README.md))
- Licence: MIT OR Apache-2.0
- Use: database access for durare and atomic. atomic uses the same fork, at the
  same revision, for `sqlx-postgres`.

## beads

- Upstream: <https://github.com/gastownhall/beads>
- Licence: MIT
- Copyright (c) 2025 Beads Contributors
- Use: atomic's work graph is a new implementation, in Rust, of the core of
  beads' design: items, dependencies, claims and the ready queue. It contains
  no beads source code. atomic does not ship a program named `bd`.

## rig-core

- Upstream: <https://github.com/0xPlaygrounds/rig> (crate `rig-core`, 0.46)
- Licence: MIT
- Copyright (c) 2024, Playgrounds Analytics Inc.
- Use: part of atomic's redaction module,
  [`crates/atomic-core/src/redact.rs`](crates/atomic-core/src/redact.rs), is
  adapted from rig-core's `src/redact.rs`: the finder for secrets of a known
  format (its table of provider key prefixes, in its order, and its JSON Web
  Token scan) and the check that a value beside a secret name is plausibly
  the secret. atomic does not depend on rig-core.

The MIT License, as it applies to the projects above that use it:

> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in
> all copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.


## Bundled Astro authoring compiler

The binary embeds a trusted compiler worker and Astro WASM, built from pinned npm
packages. Atom source itself is never executed. Runtime npm installation is not
required. The bundled libraries are:

| Package | Version | License | Upstream |
| --- | --- | --- | --- |
| @astrojs/compiler | 4.0.0 | MIT and included Go BSD notices | https://github.com/withastro/compiler |
| TypeScript | 5.9.3 | Apache-2.0 | https://github.com/microsoft/TypeScript |
| source-map-js | 1.2.2 | BSD-3-Clause | https://github.com/7rulnik/source-map-js |

Original licenses and generated bundle notices are retained under
`crates/atomic-core/assets` and included in release archives under `licenses/`.
Compiler dependency versions and payload hashes are included in the SBOM.
Deno 2.9.8 is a separately provisioned host, pinned in `deps/deno.json`; it is not
linked into this binary. Contributor-only npm dependencies are pinned in the
authoring package lockfile.
