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
  On SQLite a transactional step that may write begins `IMMEDIATE`, so its
  body runs once; a body's lock conflicts are capped at ten in a row; and
  conflict retries are logged.
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
  format, that is its table of provider key prefixes (in its order, which
  atomic extends), its JSON Web Token scan, its AWS access key ID check, its
  private key block finder and its rule for where a token may start. Some of
  the fixtures in atomic's redaction tests come from rig-core's tests.
  atomic does not depend on rig-core.

## deno_core and deno_v8

- Upstream: <https://github.com/denoland/deno> (`libs/core` and
  `libs/deno_v8`, published as the crates `deno_core` 0.413.0 and `deno_v8`
  0.5.0)
- Licence: MIT
- Copyright 2018-2026 the Deno authors
- Use: atomic embeds V8 through `deno_core`, linked into the `atomic` binary.
  Both crates are vendored in [`third_party/deno`](third_party/deno/README.md)
  with the `libs/core` and `libs/deno_v8` changes of denoland/deno pull
  request 36665, which move them to the V8 Codex pins; that README lists the
  crates.io checksums and the change. `serde_v8`, `deno_ops` and the other
  Deno crates they use come unmodified from crates.io.

## rusty_v8, and the fork Codex uses

- Upstream: <https://github.com/denoland/rusty_v8> (crate `v8`, 152.2.0)
- Fork: <https://github.com/openai-oss-forks/rusty_v8>, the source Codex
  patches in (revision in [`deps/forks.toml`](deps/forks.toml))
- Licence: MIT; V8 itself is BSD-3-Clause
- Use: the V8 JavaScript engine, linked into the `atomic` binary as the
  prebuilt static library [`deps/v8.toml`](deps/v8.toml) pins.

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

The binary links the native Astro parser and TSX converter. The tsz checker checks the generated TypeScript against the TypeScript ES2022 library declarations. A trusted worker processes captured helper source maps.

| Package | Version | License | Upstream |
| --- | --- | --- | --- |
| TypeScript | 5.9.3 | Apache-2.0 | https://github.com/microsoft/TypeScript |
| source-map-js | 1.2.2 | BSD-3-Clause | https://github.com/7rulnik/source-map-js |

Original licenses and generated bundle notices are retained under
`crates/atomic-core/assets` and included in release archives under `licenses/`.
Compiler dependency versions and payload hashes are included in the SBOM.
The authoring worker and captured helper modules run on V8 through `deno_core` (see above). The native semantic checker is tsz 0.1.75 (Apache-2.0). Contributor-only npm dependencies are pinned in the
authoring package lockfile.

## Astro’s fork of Oxc

atomic links the Astro parser from [withastro/oxc](https://github.com/withastro/oxc)
at `e5e100abf1afb9a79ae22f97de32befe4b925d5d`, the revision used by
Astro compiler-rs v0.5.3. Oxc and the Astro fork are MIT licensed.

The native TSX converter comes from [withastro/compiler-rs](https://github.com/withastro/compiler-rs)
at `c9bf179f77439e427a98751569d2af42ff85d78c` (MIT). Its HTML and JavaScript
parsers come from [Biome](https://github.com/biomejs/biome) at
`4eadd575d6890a785998279193ceeff9d3098850` (MIT or Apache-2.0).
