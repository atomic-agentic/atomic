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

A push to a `release-check/*` branch, or a manual run of the workflow, does
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

Pushing a `codex-bump/*` branch runs the Codex bump workflow. It rebuilds and
tests the durare fork against the candidate's sqlx and toolchain, then builds
atomic and runs `scripts/milestone-tests.sh`, whose table in the job summary
shows which milestone suites pass at the candidate. The same workflow runs on
the first of each month against Codex's latest release, and by hand with any
tag. To finish a bump, commit the `Cargo.lock` the workflow uploads, account
for what `scripts/lock-check.sh` reports, and follow the other steps the
script prints.

`scripts/milestone-tests.sh` also runs locally:

```bash
scripts/dev.sh scripts/milestone-tests.sh
```
