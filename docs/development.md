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

Stopping `scripts/dev.sh` with Ctrl-C, SIGTERM or SIGHUP removes the
container it started before it exits, with 130, 143 or 129. Killing it outright,
or its whole process group, or the process that started it, removes the
container within a few seconds: a watcher in a session of its own sees to that,
and when the parent died it stops dev.sh first, which then exits 143. Only that
container: each run labels its own with a token of its own
(`org.atomic-agentic.dev-run`), and nothing else on the host is touched. The
test Postgres stays up, since every run shares it. A run started with
`ATOMIC_DEV_DETACHED=1`, as for `nohup scripts/dev.sh … &`, outlives the
process that started it, as does one the system's first process starts.
`scripts/dev-sh-test.sh` runs on the host, with docker or podman, and checks
each of these; it exits 75 at once while `ATOMIC_DEV_WAIT_LOCK` exists.

## The test database

`scripts/dev.sh` starts a Postgres 18 container for tests before it runs your
command, and passes its address in `ATOMIC_TEST_DATABASE_URL`. The database:

- runs on a private container network named `atomic-dev`, with no port
  published on the host;
- keeps its data in memory, so it starts empty each time its container starts;
- uses fixed credentials that exist only for tests.

```bash
scripts/dev-db.sh status   # is it running?
scripts/dev-db.sh usage    # how full its in-memory disk is, and its schemas by prefix
scripts/dev-db.sh down     # stop it and discard its data
scripts/dev.sh bash -c 'psql "$ATOMIC_TEST_DATABASE_URL"'   # a SQL shell
```

Set `ATOMIC_DEV_DB=off` to run a command without starting the database.

A test suite that needs Postgres fails when `ATOMIC_TEST_DATABASE_URL` is not
set, or when the database it names does not answer, with a message naming it.
It never skips silently. `scripts/backends-check.sh postgres` shows this. It
runs every `atomic-agentic-durable` test without the database, and requires
the ones that fail to be exactly its Postgres variants, each naming the
database in its own output, with none of them ignored, and every test binary
to report its result. It then runs the workspace's tests whose names say
postgres, which adds the command-line tests in
`crates/atomic/tests/run_cli.rs`, and requires every one to fail so, with the
database unset and with it unreachable: a test that passes or is ignored
there is a failure of the check. Tests are told apart by binary and name, and
each binary's own count of passed, failed and ignored tests must match the
tests it listed. `scripts/backends-check-audit-test.sh` tests the reading of
cargo's output, plain and coloured.

Every checkout's tests share the one database, so each test reaches it only
through `atomic_durable::testing::schema`, which gives it a schema of its own
and drops that schema when the test ends, passed or failed. A schema's name is
`atomic_test_<seconds>_<tag>_<process>_<n>_<random>`, with a tag of at most 16
bytes so the name fits PostgreSQL's 63; the random part keeps names apart
across containers, whose process IDs repeat. A schema a sweep cannot drop,
because a lock on it is still held, is reported and left for the next sweep. A test process that
was killed leaves its schemas behind; the first schema each test process asks
for also sweeps those more than an hour old, and those older tests named under
a known prefix with the time last. A test in
`crates/atomic-durable/tests/test_database.rs` fails if anything else reads
`ATOMIC_TEST_DATABASE_URL`, or if anything outside `atomic-agentic-durable`
and `run_cli.rs`, the two places the check covers, uses the database: by
naming `atomic_durable::testing`, importing `testing` from `atomic_durable`,
renaming `atomic_durable`, or naming `testing::schema` or `TestSchema`.

The database keeps its data on a 1 GB tmpfs under a 1.5 GB memory limit, with
a small write-ahead log; a container started before that change keeps its old
sizes until `scripts/dev-db.sh down` and `up` restart it.

The tests that need Postgres are behind the `atomic-agentic` package's
`postgres-tests` feature, which turns on the durable store's as well; without
it they are not built.

```bash
scripts/dev.sh cargo test --workspace --locked --features atomic-agentic/postgres-tests
```

## The sandbox tests

Tests that run Codex's Linux sandbox are behind the `atomic-agentic` package's
`sandbox-tests` feature. The sandbox uses bubblewrap, which needs unprivileged
user namespaces and a fresh `/proc`; the container engine's default seccomp
profile and masked `/proc` paths forbid both. `ATOMIC_DEV_SANDBOX=on` lifts
those two restrictions for one command, and leaves capabilities dropped and
`no-new-privileges` on:

```bash
ATOMIC_DEV_SANDBOX=on scripts/dev.sh cargo test --workspace --locked \
  --features atomic-agentic/postgres-tests,atomic-agentic/sandbox-tests
```

With the feature on, a sandbox that cannot start fails the tests, each with a
message saying what the sandbox needs. Run without `ATOMIC_DEV_SANDBOX=on`,
`scripts/dev.sh scripts/backends-check.sh sandbox` requires exactly that.
atomic itself needs the sandbox on Linux to run exec atoms and codex atoms'
commands, and stops before such a step without it, so the tests in
`crates/atomic/tests/run_cli.rs` and `crates/atomic/tests/telemetry.rs` that
run commands are behind the same feature there, each marked
`// needs_sandbox`; the rest of those files runs in a plain `cargo test`. The
sandbox check runs each marked test, as it runs every test of a file that is
behind the feature as a whole.

## Controls are tested by removing them

Each security control has a test that fails when the control is switched off.
`scripts/controls-check.sh` proves it: for each v0.1 control (the hardened git
command's settings, its neutralized filter drivers and its explicit git
directory; the sandbox's confinement of writes, which keeps the state
directory and a worktree's git directory out of a turn's reach, its exclusion
of `/tmp`, and its denial of atomic's private state, to turns and to exec
atoms' commands; the confinement of exec atoms' commands itself; and their
network: none unless declared, and through the proxy only the declared hosts,
on ports 80 and 443, never through another proxy, with the declaration checked
when the atom loads and again before the command runs; and the artifact store's
holds, which keep the files an exec step hands over from every sweep until its
turn is recorded; and that an exec atom's files move only through its
workspace's backend), it
switches the control off in the source, requires the control's test to fail,
puts the source back and requires the test to pass. Each control costs a rebuild, so it
runs by hand rather than in CI:

```bash
ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/controls-check.sh
```

In a linked worktree, also mount the main checkout's `.git` with
`ATOMIC_DEV_MOUNTS`, since the script hashes files with git. It puts every
file it edits back, even when interrupted, and `scripts/controls-check-test.sh`
(in CI) tests that. It refuses to start while a file a selected control edits
differs from HEAD, including a file git does not track or is told to assume
unchanged, so commit or put back changes to those files first; changes to
other files do not matter.

A control added later gets a row in the script. A row whose code has moved
fails until it is updated.

## Milestone acceptance

A milestone's acceptance clauses, from [the milestones](design/milestones.md),
are listed in `scripts/acceptance/<milestone>.txt`, each with the tests that show
it, and `scripts/acceptance.py` runs them as one check. CI runs it in the
"Acceptance and backends" job, after building the tests with the same
features, so nothing rebuilds:

```bash
ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/acceptance.py m12
```

Each clause is written as the milestones state it, and a clause of several
sub-clauses, separated by semicolons, can list each sub-clause on a `### ` line
with tests of its own. Dropping those `### ` lines drops that requirement with
them: the clause's tests, taken together, then need only show the clause, and no
sub-clause needs one of its own. Sub-clauses are split on `;` alone, so a clause
whose sub-clauses are joined with `, and` cannot be listed one by one. A
sub-clause is not a part: a milestone's parts are the `a` and `b` sections under
it, which are milestones of their own.

A milestone's clauses are those of every Acceptance block under its heading, a
titled block such as `**Acceptance, the managed Deno:**` included. A split
milestone is asked only for the clauses of its own text, which ends where its
first part's section starts, so none of a part's clauses is counted as its
parent's. A "b" part finishes its parent's work, so its list states its parent's
clauses as well as its own; an "a" part states only its own. Only a line that
states a marker opens a block, and only a block holds clauses, so what is
written under a line that opens none, or under a deeper heading that states
none, is read as nothing. Each such line is refused with its line number:
outside a block, every line that leads with a spelling of `accept` and ends in
a colon, however it is wrapped or spelt (`Acceptance:`, `__Acceptance:__`,
`**Acceptence:**`, `- **Acceptance, files and data:**`); within a block, every
line that begins `**Acceptance` and is not a marker; and under a deeper
heading, every bullet, of whatever shape. A clause carried onto its next line
is read as one clause only when that line is indented by at least two spaces
and begins with neither a bullet nor a number, so a hard-wrapped clause whose
continuation begins with a number, as `  100. The run then waits.`, is refused
too.

The check fails when the list's clauses and the milestone's differ, when
anything in a milestone's Acceptance block cannot be read as a clause, when a
listed test fails, is ignored or does not run (renamed, removed, or built out
by a feature), when a clause or sub-clause lists no test, or when cargo fails. A
clause its tests do not yet show in full is marked `pending:` with the reason,
and the verdict names it. The Postgres and sandbox tests it lists need their
backends, as in the test jobs, hence `ATOMIC_DEV_SANDBOX=on`.
`scripts/acceptance-test.sh` (in CI) tests the check itself.

## The crash suite

atomic's crash tests kill the process at named failpoints. A failpoint is
compiled in only with the `failpoints` features of `atomic-agentic-durable` and
`atomic-agentic-core`, which the `atomic-agentic` package's tests turn on; a
release build has none. Three variables drive them:

- `ATOMIC_FAILPOINT=<name>` kills the process (SIGKILL, so no core is dumped)
  at the first failpoint of that name;
- `ATOMIC_FAILPOINT_PANIC=<name>` makes the code that reaches the first
  failpoint of that name panic, as a bug in atomic would;
- `ATOMIC_FAILPOINT_LOG=<file>` appends the name of every failpoint the process
  reaches to `<file>`.

`atomic_durable::failpoints::SITES` lists every site: before and after every
durable checkpoint, at each message wait's record, between a model call's
admission, its answer and its settlement, between the steps of a commit and of
a finished run's cleanup, and around gates and parks. A site reached more than
once in a run names each occurrence, such as
`usage:before-settle:<request>`. A test reads the source to check that every
failpoint named there is in `SITES`, and every site is named somewhere.

The crash suite, `crates/atomic/tests/crash.rs`, runs each v0.1 workflow once
to record the failpoints it passes, then, for each of them, kills a fresh run
there and recovers it in a new process. Every recovered run must finish with
the result an uninterrupted run gives, repeat no step whose checkpoint had
committed, and leave one accepted commit for each code-changing step, changing
what an uninterrupted run's commit changes. Across the workflows, every site
must be reached, and no name twice in one run. Its code-changing atom runs a
command under Codex's sandbox, so the suite is behind `sandbox-tests`. It
takes a few minutes; while you work on one failpoint, `ATOMIC_TEST_CRASH_ONLY`
narrows it:

```bash
ATOMIC_DEV_SANDBOX=on scripts/dev.sh cargo test -p atomic-agentic --locked \
  --features sandbox-tests --test crash
ATOMIC_DEV_SANDBOX=on ATOMIC_TEST_CRASH_ONLY=pipeline:after-commit scripts/dev.sh \
  cargo test -p atomic-agentic --locked --features sandbox-tests --test crash
```

CI runs it on Linux, in a job of its own ("Crash suite").

## Continuous integration

CI (`.github/workflows/ci.yml`) runs on every push to main, every pull request
and every merge group of the merge queue, on Linux. The checks that need no
Rust build run in small jobs of their own. The Rust checks run in parallel
jobs, so a pull request waits for the slowest one rather than for all of them
in turn:

| Job | What it runs |
| --- | --- |
| Clippy | `cargo fmt --check`, and clippy with and without the test features |
| Test (1/3), Test (2/3), Test (3/3) | The workspace's tests, with Postgres and the sandbox, one shard each |
| Crash suite | `crates/atomic/tests/crash.rs`, the shard `crash` |
| Acceptance and backends | Builds every test, checks that the shards run each test exactly once, runs the M12 and M35a acceptance checks, and runs the Postgres and sandbox suites without their backends |
| Build, lockfile and SBOM | `cargo build`, `scripts/lock-resolves.sh`, `scripts/lock-check.sh v8-built` and `scripts/sbom.sh` |
| Rust (ubuntu-24.04) | Passes only when every job above passes |

A manual run with the `macos` input adds "Rust (macos-15)", which builds and
tests on macOS in one job. CodeQL runs in a workflow of its own, on pull
requests too, and nothing in CI waits for it.

Main is merged through a merge queue. Once a pull request's checks pass, it can
be added to the queue, which runs CI again on the pull request merged onto main
and the pull requests queued ahead of it (the `merge_group` event), and merges
it only if those checks pass. So what reaches main has passed CI as it is
merged, and a pull request is not held back by changes elsewhere on main.
CodeQL is not one of the queue's checks.

### Test shards

`scripts/test-shards.txt` assigns each test target to a shard: each
integration test (`tests/<name>.rs`) by name, and the libraries' unit tests,
the binaries', the doctests and the examples' build as one unit each. The
shards are balanced by how long their tests take on a CI runner. Every shard
runs over the whole workspace with the test features, so all of them build the
same dependencies:

```bash
ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/test-shards.py run 2
```

`scripts/test-shards.py run` first checks the table against `cargo metadata`,
so a test file in no shard, or in two, fails every shard until the table
places it. `scripts/test-shards.py check`, in the "Acceptance and backends"
job, lists the tests (`cargo test -- --list`) of one unsplit
`cargo test --workspace` and of each shard, and fails unless each test is in
exactly one shard. `scripts/test-shards-test.sh` (in CI) tests both checks,
and that the workflow runs every shard of the table: a shard added to the
table needs a matrix entry in the test job as well.

### Build caches

The Rust jobs restore two caches, through `.github/actions/rust-setup`:
cargo's downloads (the registry and the git sources, rusty_v8's Chromium
submodules among them), keyed on the toolchain and `Cargo.lock`, and
`target/`, keyed on those and the manifests, one for the test features'
build and one for clippy's. A checkout gives every source file a new time, so
the workspace's own crates rebuild in every job; the cached `target/` holds
only the dependencies, trimmed by `scripts/cache-trim.py`, which keeps it near
1.5 GB compressed beside about 3 GB of downloads, inside the repository's
10 GB of cache. The trim deletes everything else in `target/`, so it runs only
in CI, or with `--force`.

Only pushes to main save the caches: Clippy saves the downloads and clippy's
build once both clippy passes have passed, and "Acceptance and backends" saves
the test build. `scripts/test-shards-test.sh` fails if any other step saves a
cache. Pull requests restore main's, so they never push main's caches out. A
miss falls back to the newest cache of the same kind, or builds everything,
which takes a few minutes longer.

## Coding tasks

`crates/atomic/tests/coding-tasks/` holds fourteen coding tasks, eval fixtures
for atoms that change code. Each is a seed project, a request, a checker,
hidden tests and a reference solution; [its README](../crates/atomic/tests/coding-tasks/README.md)
gives the contract and what each task covers. `crates/atomic/tests/coding_tasks.rs`
holds that contract: every seed fails its checker with exit 1, every reference
solution passes, every wrong fix kept beside a task fails for its reason, a
checker changes nothing it grades, and a workspace holding a link, a Python
startup hook, a hidden test's name or a standard-library module's name fails. A
checker runs the attempt's code: run it only inside a sandbox. The checkers need
Linux, `python3` and GNU `timeout`, which the development container has; the
test runs on Linux only, and fails there without them.

```bash
scripts/dev.sh cargo test -p atomic-agentic --locked --test coding_tasks
```

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
