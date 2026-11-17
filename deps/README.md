# Pinned upstreams

atomic builds against exact revisions of the projects it depends on from git.
Each pin lives in one file here, and the build checks the rest of the workspace
against it.

| File | Upstream |
| --- | --- |
| [`codex.toml`](codex.toml) | [openai/codex](https://github.com/openai/codex), the `codex-rs` workspace |
| [`durare.toml`](durare.toml) | [atomic-agentic/durare](https://github.com/atomic-agentic/durare), atomic's fork of [durare](https://github.com/SamuelXing/durare), branch `atomic` |

## Codex

atomic pins a Codex release tag rather than a commit on `main`, so that a bump
moves between releases upstream has published.

These are the facts atomic depends on at the pinned revision, with the file
each one is read from (paths are relative to `codex-rs/`). Re-check every one
when the pin moves.

| Fact | Value at `rust-v0.169.0` | Read from |
| --- | --- | --- |
| Rust toolchain | `1.95.0`, with clippy, rustfmt and rust-src | `rust-toolchain.toml` |
| Edition | `2024` | `Cargo.toml` `[workspace.package]` |
| sqlx | `=0.9.0`, with `sqlx-core` and `sqlx-sqlite` patched to `transact-rs/sqlx` rev `10462fa4` | `Cargo.toml` |
| libsqlite3-sys | `0.39.1`, so no other crate in the graph may link a different `sqlite3` | `Cargo.toml` |
| schemars | `0.8.22` | `Cargo.toml` |
| rmcp | `=3.5.1` from `modelcontextprotocol/rust-sdk` rev `98a54789` | `Cargo.toml` |
| Patch sections | `[patch.crates-io]` (crossterm, sqlx-core, sqlx-sqlite, tokio-tungstenite, tungstenite, v8) and `[patch."ssh://git@github.com/openai-oss-forks/tungstenite-rs.git"]`, which redirects an SSH source to HTTPS. atomic copies both verbatim | `Cargo.toml` |
| `.git` is read-only under `workspace-write` | Added as a default read-only subpath of each project root, with `.agents`, `.codex` and `.aws` | `protocol/src/permissions.rs` |
| V8 stays out of the core path | Only `code-mode-runtime` depends on `v8`, and only `code-mode-host` depends on it. `core`, `app-server` and `tools` depend on `codex-code-mode`, which does not | the crates' `Cargo.toml` files |
| Crates atomic uses exist | `core-api`, `ext/extension-api`, `app-server`, `arg0`, `thread-manager-sample` | directory listing |
| The sample uses one Codex crate | `thread-manager-sample` depends only on `codex-core-api` | `thread-manager-sample/Cargo.toml` |

## durare

atomic uses a fork of durare, ported to sqlx 0.9 so that it shares Codex's
sqlx and `libsqlite3-sys` versions. The fork's changes live on its `atomic`
branch and are kept small enough to rebase on upstream. A durare dependency
only resolves inside a workspace whose `[patch.crates-io]` patches
`sqlx-core`, `sqlx-sqlite` and `sqlx-postgres` to the same sqlx fork revision,
which atomic's Codex patch block does.
