# Pinned upstreams

atomic builds against exact revisions of the projects it depends on from git.
Each pin lives in one file here, and the build checks the rest of the workspace
against it.

| File | Upstream |
| --- | --- |
| [`codex.toml`](codex.toml) | [openai/codex](https://github.com/openai/codex), the `codex-rs` workspace, built from atomic's fork [atomic-agentic/codex](https://github.com/atomic-agentic/codex) |
| [`lock-exceptions.toml`](lock-exceptions.toml) | Where `Cargo.lock` differs from Codex's lockfile at the pinned revision, each with its reason. `Cargo.lock` starts from Codex's, and `scripts/lock-check.sh` fails on any unlisted difference |
| [`v8.toml`](v8.toml) | The prebuilt V8 archives atomic links, pinned by SHA-256 per target (`scripts/v8-archive.sh` verifies them), and V8 versions `Cargo.lock` holds but never builds, each with its reason. `scripts/lock-check.sh` fails when more than one other V8 version is locked; see [one V8](#one-v8) |
| [`forks.toml`](forks.toml) | Every fork the build uses, with its pinned revision: the Codex fork, the durare fork and the sqlx fork. `scripts/forks-check.sh` checks it against `Cargo.lock` |

## Codex

atomic pins a Codex release tag rather than a commit on `main`, so that a bump
moves between releases upstream has published.

### atomic's Codex fork

atomic builds that tag from its fork,
[atomic-agentic/codex](https://github.com/atomic-agentic/codex), whose `atomic`
branch is the tag plus atomic's own commits. Every Codex git dependency points
at the fork, at the commit `rev` in `codex.toml` names; `tag` and
`upstream_rev` record the release the branch starts from. The checks read
Codex's files from the fork at that commit.

| Commit | Change | Why | Upstream |
| --- | --- | --- | --- |
| `a4b0cac` | `utils/pty`: the parent-death signal is best effort where `prctl` is refused (`EPERM`, `EACCES`, `ENOSYS`) | AWS Lambda's seccomp policy refuses every `prctl` call, so no command could start there in any sandbox mode. Found in atomic-deploy's AWS spikes | To offer |
| `f8e52f3` | `utils/pty`: skip the parent-PID check when no parent-death signal was armed; `EINVAL` is also "unavailable" | On iSH (openai/codex#40454) `prctl` answers `EINVAL` and the check then kills every child; the check only closes the race for an armed signal | To offer, with the commit above |

Keep the branch small and each commit something upstream could take: when a
release includes a commit's change, drop the commit at the next bump, and when
none are left, point `codex.toml` back at openai/codex and remove the fork from
`forks.toml`.

A bump with the fork: rebase the `atomic` branch onto the new tag, push it, then
run `scripts/codex-bump.sh <tag> <fork commit>`, which looks the tag up upstream
and pins the fork commit. The facts below are re-checked against the fork's
commit, which differs from the tag only in the commits above.

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

## One V8

Two V8 builds cannot be linked into one binary, so atomic builds at most one
V8: one version of one V8 binding crate (`v8`, or deno_v8's alternative `v8x`).
`scripts/lock-check.sh` fails when `Cargo.lock` holds a second version that
[`v8.toml`](v8.toml) does not list as locked but unbuilt, and
`scripts/dev.sh scripts/lock-check.sh v8-built` proves with `cargo tree`
that each listed version stays out of the graph for every workspace feature
and target.

Codex's patch block points `v8` at Codex's own fork for its code-mode
runtime. atomic does not build that runtime, so `Cargo.lock` records the
patch as unused and it does not count. The V8 atomic links need not match
that fork while Codex code-mode stays off; before atomic enables Codex
code-mode, the two must converge on one version, either through a deno_core
release on Codex's V8 line or by building deno_core against the same fork.
A Codex bump that brings a second V8 into the graph fails the check.

The V8 that is built comes as a prebuilt static library from rusty_v8's
releases. Each target's archive and generated bindings are pinned in
[`v8.toml`](v8.toml); moving `deno_core` to a release on another V8 means
pinning that version's archives here, for every release target and for the
development container's Linux targets. [Developing atomic](../docs/development.md#the-prebuilt-v8)
describes how builds read them.

## durare

atomic uses a fork of durare, ported to sqlx 0.9 so that it shares Codex's
sqlx and `libsqlite3-sys` versions. On SQLite, the fork also begins a
transactional step that may write `IMMEDIATE`, through the provider and
through a data source, so that another connection's commit cannot make the
step's body run again; fails a step whose body returns a lock conflict ten
times in a row, as a body waiting on its own step's lock does; and logs its
conflict retries. The fork's changes live on its `atomic`
branch and are kept small enough to rebase on upstream. A durare dependency
only resolves inside a workspace whose `[patch.crates-io]` patches
`sqlx-core`, `sqlx-sqlite` and `sqlx-postgres` to the same sqlx fork revision,
which atomic's Codex patch block does.
