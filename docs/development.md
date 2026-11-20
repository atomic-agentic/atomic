# Developing atomic

atomic builds and tests inside a development container, so a contributor needs
only a container engine and git on the host. You do not need Rust, Postgres or
any other tool installed locally.

## The development container

```bash
scripts/dev.sh                 # an interactive shell in the container
scripts/dev.sh cargo build     # or run one command
scripts/dev.sh cargo test
```

The first run builds the image from [`dev/Dockerfile`](../dev/Dockerfile),
which takes a few minutes. The image is tagged with a hash of that file, so any
change to it builds a new image automatically.

The container has:

| Tool | Version | Why |
| --- | --- | --- |
| Rust | 1.95.0, with clippy, rustfmt and rust-src | The toolchain Codex pins at the revision in [`deps/codex.toml`](../deps/codex.toml) |
| C toolchain, pkg-config, libcap, OpenSSL, D-Bus, bubblewrap | Debian 13 packages | Codex's Linux build and sandbox |
| git, psql | Debian 13 packages | Tests that make commits; inspecting the test database |
| cargo-deny | 0.21.0 | Licence and advisory checks |
| cosign | 3.1.4 | Verifying release signatures |
| cargo-cyclonedx | 0.5.10 | The CycloneDX SBOM for a release |
| bd | 1.3.2 | Bead tracking |

Every downloaded binary is checked against a pinned SHA-256, and the base
images are pinned by digest.

The container runs as your own user ID with no Linux capabilities, mounts the
repository at the same path as on the host, and keeps cargo's registry and git
cache in `~/.cache/atomic/dev`. Build output goes to `target/` in the
repository as usual.

### The prebuilt V8

atomic links one V8, through `deno_core`, for the isolates captured
TypeScript helpers run in. rusty_v8's build script normally downloads a
prebuilt static library and checks nothing about it, so atomic pins each
archive and its generated bindings by SHA-256 in
[`deps/v8.toml`](../deps/v8.toml). `scripts/v8-archive.sh [TARGET]` fetches
them once into a local mirror and verifies them on every run;
[`.cargo/config.toml`](../.cargo/config.toml) points `RUSTY_V8_MIRROR` at
`target/v8-mirror`, so a plain `cargo build` reads only that mirror and fails
inside V8's build script, before linking, until the script has filled it.
`scripts/dev.sh` verifies the archive on the host before the container starts
and keeps its mirror in `~/.cache/atomic/dev/v8-mirror`, so builds work offline
once it is cached. A tampered file stops the build; delete it to fetch it again.
`atomic-core`'s build script checks the mirrored archive against its pin as
well, and refuses a build with `RUSTY_V8_ARCHIVE` or `V8_FROM_SOURCE` set or
with `RUSTY_V8_MIRROR` naming a remote mirror, so no environment can link
another V8.

### Limits

The container has a memory limit with no swap, a CPU limit and a time limit,
and runs with low CPU priority so it gives way to other work on the host. To
change them:

| Variable | Default | Meaning |
| --- | --- | --- |
| `ATOMIC_DEV_MEMORY` | `24g` | Memory limit |
| `ATOMIC_DEV_CPUS` | `16` | CPU limit, also used as cargo's job count |
| `ATOMIC_DEV_TIMEOUT` | `60m` | Time limit for the command |
| `ATOMIC_DEV_CACHE` | `~/.cache/atomic/dev` | Host directory for cargo's caches |
| `ATOMIC_DEV_MOUNTS` | unset | Colon-separated host directories to mount at the same path, for checkouts outside this repository |
| `ATOMIC_DEV_MIN_FREE_GB` | `10` | Refuse to start when the disk holding the repository or the cargo cache has less free space than this, in GiB (`0` turns the check off) |
| `ATOMIC_DEV_WAIT_LOCK` | unset | A file; while it exists, `scripts/dev.sh` refuses to start (exit 75) |

A debug build that includes Codex takes several gigabytes even with
dependency debug info turned off, so keep that much space free.
| `ATOMIC_CONTAINER` | `docker` | The engine; `podman` is supported but less tested |

Exit code 124 means the command hit the time limit, and 137 usually means it
hit the memory limit.

## The test database

`scripts/dev.sh` starts a Postgres 18 container for tests before it runs your
command, and passes its address in `ATOMIC_TEST_DATABASE_URL`. The database:

- runs on a private container network named `atomic-dev`, with no port
  published on the host;
- keeps its data in memory, so it starts empty each time its container starts;
- uses fixed credentials that exist only for tests.

```bash
scripts/dev-db.sh status   # is it running?
scripts/dev-db.sh down     # stop it and discard its data
scripts/dev.sh bash -c 'psql "$ATOMIC_TEST_DATABASE_URL"'   # a SQL shell
```

Set `ATOMIC_DEV_DB=off` to run a command without starting the database.

A test suite that needs Postgres fails when `ATOMIC_TEST_DATABASE_URL` is not
set. It never skips silently.

## The sandbox tests

Tests that run Codex's Linux sandbox are behind the `atomic-agentic` package's
`sandbox-tests` feature. The sandbox uses bubblewrap, which needs unprivileged
user namespaces and a fresh `/proc`; the container engine's default seccomp
profile and masked `/proc` paths forbid both. `ATOMIC_DEV_SANDBOX=on` lifts
those two restrictions for one command, and leaves capabilities dropped and
`no-new-privileges` on:

```bash
ATOMIC_DEV_SANDBOX=on scripts/dev.sh cargo test --workspace --locked \
  --features atomic-agentic-durable/postgres-tests,atomic-agentic/sandbox-tests
```

With the feature on, a sandbox that cannot start fails the tests. atomic itself
needs the sandbox on Linux to run exec atoms and codex atoms' commands, and
stops before such a step without it, so the tests in
`crates/atomic/tests/run_cli.rs` that run commands are behind the same feature
there; the rest of that file runs in a plain `cargo test`.

## Releases

A release starts from a tag `v<version>` whose version equals the
`atomic-agentic` package's. The Release workflow then:

1. builds `atomic` in release mode for `x86_64-unknown-linux-gnu` and
   `aarch64-apple-darwin`, with the checkout and cargo paths remapped out of
   the binary;
2. packages each binary with `LICENSE`, `NOTICE` and
   `THIRD-PARTY-NOTICES.md` into a deterministic `.tar.gz`;
3. writes each binary's CycloneDX SBOM, which records the binary's SHA-256 and
   lists every fork at its pinned revision;
4. attaches the archives, the SBOMs and a `SHA256SUMS` file to a draft GitHub
   release, which a maintainer reviews and publishes.

A manual run of the workflow on any branch does
steps 1 to 3 and keeps the results as workflow artifacts. To package a build
locally:

```bash
scripts/dev.sh cargo build --release --locked -p atomic-agentic --target x86_64-unknown-linux-gnu
scripts/dev.sh scripts/release-package.sh x86_64-unknown-linux-gnu   # writes target/dist/
```

To check that an SBOM describes the binary you have, compare the hash on its
`atomic` component with the binary's own:

```bash
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print([c["hashes"][0]["content"] for c in d["metadata"]["component"]["components"] if c["name"]=="atomic"][0])' atomic-<version>-<target>.cdx.json
sha256sum atomic-<version>-<target>/atomic
```

## Bumping Codex

atomic depends on Codex's internal crates, which have no stability promise,
so the pin in `deps/codex.toml` moves on a schedule, one release at a time:

```bash
git switch -c codex-bump/rust-v0.170.0
scripts/codex-bump.sh rust-v0.170.0   # rewrites the pins and reseeds Cargo.lock
git commit -am "Bump Codex to rust-v0.170.0" && git push -u origin HEAD
```

Then start the Codex bump workflow by hand on that branch (Actions > Codex
bump > Run workflow, with no tag). It rebuilds and tests the durare fork
against the candidate's sqlx and toolchain, then builds atomic and runs
`scripts/milestone-tests.sh`, whose table in the job summary shows which
milestone suites pass at the candidate. Given a tag, or `latest` for Codex's
latest release, it applies that candidate itself on any branch. To finish a
bump, commit the `Cargo.lock` the workflow uploads, account for what
`scripts/lock-check.sh` reports, and follow the other steps the script prints.

`scripts/milestone-tests.sh` also runs locally:

```bash
scripts/dev.sh scripts/milestone-tests.sh
```

## Astro compiler

Declarative atoms use `.atom.astro`. The production compiler assets are bundled
under `crates/atomic-core/assets` and included in the binary. Running atoms needs
no npm install or project `node_modules`. The development image supplies pinned
Deno 2.9.8; other builds can provision that host outside the source tree:

```sh
python3 scripts/setup-deno.py /tmp/atomic-deno
export ATOMIC_DENO=/tmp/atomic-deno/deno
```

The setup script verifies the platform archive against `deps/deno.json`. The worker
checks its runtime version, clears inherited environment, and has no network, npm,
write, FFI or subprocess grant, and reads only the compiler assets: atomic reads
atom sources itself and sends them in the request, for loading as for rendering.
Compiler cache and direct-run context checkpoints live outside the source tree.

Contributors changing the interpreter or type checker rebuild the shipped assets:

```sh
cd authoring
npm ci --ignore-scripts
npm run build
npm test
npm run probe:lsp
npm run probe:controls
```

Use the Node version in `authoring/.node-version`. npm dependencies are contributor
tooling only. Parser/compiler licenses accompany the assets and release notices.
The `authoring/embedded` experiment is kept out of the repository, because it is a
second Cargo workspace; the combined
Atomic/Codex/Deno native dependency and release-packaging decision is still open.
Workflow-level durable execution context integration remains separate from the
direct-invocation file checkpoint; do not substitute it for a durable transaction.
