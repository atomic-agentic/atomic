# Developing atomic

atomic builds and tests inside a development container, so a contributor needs
a container engine and git. Node is used to rebuild the embedded authoring worker and run the reference authoring tests.

## The development container

```bash
scripts/dev.sh                 # an interactive shell in the container
scripts/dev.sh cargo build     # or run one command
scripts/dev.sh cargo test
```

The first run builds the image from [`dev/Dockerfile`](../dev/Dockerfile),
which takes a few minutes. The image is tagged with a hash of that file, so any
change to it builds a new image automatically.

### Read-only beads readiness parity

Before moving this repository's work tracking from beads to atomic, run the
full-export parity probe against a current binary built from the source being
checked:

```bash
scripts/dev.sh cargo build --locked -p atomic-agentic
python3 tools/own_beads_parity.py \
  --bd-project "$PWD" \
  --atomic-bin "$PWD/target/debug/atomic"
```

The probe calls `bd --readonly` for two export and readiness snapshots. It
refuses a changing snapshot, imports into a private temporary atomic project
twice to check replay, and compares sorted ready IDs with the graph's SQLite
readiness query. Imported `in_progress` issues have no leases and are counted
but are not considered ready. Its JSON result includes the export hash,
counts, complete sorted ready ID sets, and discrepant IDs. A zero exit code
means the two complete ready sets match; exit one means a mismatch; exit two
means the proof could not run. The temporary project and state are removed
after the run. This check does not switch the repository's work tracker.

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
| `ATOMIC_CONTAINER` | `docker` | The engine; `podman` is supported but less tested |
| `ATOMIC_DEV_MEMORY` | `24g` | Memory limit |
| `ATOMIC_DEV_CPUS` | `16` | CPU limit, also used as cargo's job count |
| `ATOMIC_DEV_TIMEOUT` | `60m` | Time limit for the command |
| `ATOMIC_DEV_CACHE` | `~/.cache/atomic/dev` | Host directory for cargo's caches |
| `ATOMIC_DEV_MOUNTS` | unset | Colon-separated host directories to mount at the same path, for checkouts outside this repository |
| `ATOMIC_DEV_MIN_FREE_GB` | `10` | Refuse to start when the disk holding the repository or the cargo cache has less free space than this, in GiB (`0` turns the check off) |
| `ATOMIC_DEV_WAIT_LOCK` | unset | A file; while it exists, `scripts/dev.sh` refuses to start (exit 75) |

A debug build that includes Codex takes several gigabytes even with
dependency debug info turned off, so keep that much space free.

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
process that started it, as does one the system's first process starts. The
watcher knows dev.sh and its parent by when each started as well as by its
process id (on Linux the kernel's start time, which stepping the clock does
not move), and checks dev.sh is still the process it started just before it
signals it, so a process that took its number is not signalled, short of
one ending and its number being taken in the moment between the check and
the signal. A watcher that cannot start stops the run (exit 1) with a
message saying so.
`scripts/dev-sh-test.sh` runs on the host, with docker or podman, and checks
each of these, with another run's container kept through every stop; it exits
75 at once while `ATOMIC_DEV_WAIT_LOCK` exists. Its runs reach the engine
through a shim that labels each container they make with the test's own
token and lists and removes only those, and refuses every other engine
command, so the test touches no container it did not start.

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
commands, and stops before such a step without it. The missing-sandbox audit
discovers marked tests in six files under `crates/atomic/tests/`:
`command_environment.rs`, `fixture_cli.rs`, `graph_work_loop.rs`,
`render_cli.rs`, `run_cli.rs` and `telemetry.rs`. Each sandbox-dependent test
has `// needs_sandbox` above its attributes; some of those files also have
ordinary tests. The one test in `command_environment.rs` combines an ordinary
exec check with a feature-gated Codex turn. The audit runs each marked test and
every test in a file gated by the feature as a whole. It also enables
`dev-failpoints` because the marked `--fail-at` CLI test requires that feature.

### Exec command process limits

`atomic_exec::Command` may set `cpu_seconds` and `data_bytes`. Both the local
backend and the process runner apply them to the command before `exec` on Unix:
`RLIMIT_CPU` caps CPU seconds and `RLIMIT_DATA` caps writable data bytes.
The limits are inherited by children, but each process has its own counter and
address space. They are not aggregate quotas for the command's process tree.
macOS refuses an `RLIMIT_DATA` below the size of a process's whole address
map, so there the process that waits on the command holds `data_bytes`
instead (`crates/atomic-exec/src/process.rs`, `mod memory`). About every ten
milliseconds it reads the physical footprint of each process in the
command's group, and it kills the group when one process is over the
limit, or when a process's memory cannot be read. The kill is reported on
the command's stderr. As on Linux, the limit applies to each process on its
own. A process can exceed it briefly between two readings, and a process
that left the command's group is not watched.
The fields are optional and default to no OS limit; a caller that requires
these bounds must set them. Non-Unix backends refuse a command that sets either
field. The command's existing wall time and output limits still apply.

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
when the atom loads and again before the command runs, and loopback reachable
only in a build with the test-only `exec-loopback-tests` feature; the runner
process, `atomic runner`, hardened against same-user reads of its memory
whether it is named first or after global flags, and given only `PATH` from
the control plane's environment; the sandbox helper's outer stage, which
holds a confined command's proxy token, made non-dumpable; and the artifact store's
holds, which keep the files an exec step hands over from every sweep until its
turn is recorded, and what a run's input names from every sweep until the input
is recorded, refusing an input whose typed artifact field names content the
store does not hold, and waiting for the store's lock within one bounded wait
that a stop signal ends; and
that an exec atom's files move only through its workspace's backend), it
switches the control off in the source, requires the control's test to fail,
puts the source back and requires the test to pass. Each control costs a
rebuild, so CI runs only the rows a pull request touches (see
[The Controls workflow](#the-controls-workflow)). To run a row by hand, or all
of them:

```bash
ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/controls-check.sh <name>
ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/controls-check.sh
```

In a linked worktree, also mount the main checkout's `.git` with
`ATOMIC_DEV_MOUNTS`, since the script hashes files with git. It puts every
file it edits back, even when interrupted, and `scripts/controls-check-test.sh`
(in CI) tests that. It refuses to start while a file a selected control edits
differs from HEAD, including a file git does not track or is told to assume
unchanged, so commit or put back changes to those files first; changes to
other files do not matter. It also refuses to start while another run holds the
tree's lock at `target/controls-check.lock`, taken with `flock`: two runs switch
the same files and each saves its own originals, so the one that finishes last
would put back the other's switched-off source. That lock is closed before the
run's cargo and its git are started, so neither a test that leaves a process
behind nor a git that starts a fsmonitor daemon holds a finished run's lock.
The lock frees itself when the run that took it ends, however it ends, so its
file is never something to remove: removing it frees nothing and would let a
third run lock a fresh file beside the one still held.

Each control is one file under `scripts/controls/`, named `<name>.row` and
holding one line:

```
name|file|text to replace|replacement|cargo test arguments
```

`name` is what you pass to the script and must equal the file's name; `file` is
where the control lives; the script switches the control off by replacing the
first `text to replace` there with `replacement`; the rest are the arguments
given to `cargo test`, with every test the control has named after `--`, each of
which must fail with the control off. The arguments are taken as written, with
no variable expanded, so a control that needs the sandbox and Postgres suites
names `--features atomic-agentic/postgres-tests,atomic-agentic/sandbox-tests`
itself. Only the first `text to replace` is switched, so a control with two
sites has a row for each, and the text of each is unlike the other's. Add a
control by adding a file: the script
reads the directory in file-name order and refuses a file whose row names a
different control, a file holding anything but one row, a second file naming a
control again, and anything there that is not a `*.row` regular file. That is
why a control is a file rather than a line in the script: two branches adding a
control add two files and never edit the same line. A row whose code has moved
fails until it is updated. `scripts/controls-on.py` and
`scripts/controls-touched.py` read row files by these same rules, and refuse a
row that names no control; the second reads them from git's tree, so a row
file that is a link must lead to a regular file inside the repository.

`python3 scripts/controls-on.py` asks of every row whether its file holds the
control rather than what switching the row off writes, and CI runs it on the
commit it checks out, for every row at once: a rebuild per control is why
`controls-check.sh` runs only the rows a pull request touches, and this needs
no build. A row whose replacement does not hold its own switch text takes that
text out of the file when it switches the control off — whatever the
replacement is, even a value the file says elsewhere — so for those rows the check requires the switch text to be in the file exactly
once. A row whose replacement does hold its own switch text leaves that text
where the row points, so for those the check requires the replacement not to be
in the file at all. A row that fails either is named. The script reads the row
files by the same rules as `controls-check.sh`, so a row that cannot be read is
refused there too rather than left unchecked, and
`scripts/controls-check-test.sh` tests both with fixtures it commits.

### The rows as a workflow step

`scripts/controls-exec.py` is the same proof driven from inside a workflow: a
step hands it the commit a coding step made and the row names that step
declares, and it reports one typed result per row. It runs the real
`scripts/controls-check.sh`, once per row, inside a copy, so nothing here is
duplicated: the switch, the saved originals, the restore and the reasons are
that script's, and the verdict is the script's last word about the row.
Anything else it ends with — no result line, a reason this driver does not
know, an `ok` under a nonzero exit, a `FAIL` that still exited 0 — is reported
as `errored` and fails the step.

What it may prove is settled before anything is read out of git. The input
names a commit, the ref atomic keeps that commit under, and the base recorded
beside it; the ids and the ref are checked against their shapes first, so an
input can name a commit id or a `refs/atomic/` ref and never an option or a
revision that means something else. The driver then requires that ref to name
exactly that commit in exactly that repository, and that commit to have exactly
that base as its one parent. A ref naming nothing, another commit, a second
parent — no step's attempt makes a merge — and a row the candidate does not
carry are each refused. Every row named is run; choosing which rows a change
touches is a step in its own right, and this one proves the set it is handed.
For coding reviews, the host also checks that each selected row file has the
same Git blob and mode in the bound base and candidate. A candidate cannot
replace the selected control's definition while asking it to approve that
candidate.

What it may run is settled the same way, out of the step's declaration rather
than out of the change. `CONTROL_RUNNER` names the proof runner and
`CONTROL_RUNNER_SHA256` its exact bytes; `CONTROL_DRIVER_SHA256` says what this
file's own bytes must be, and the driver checks that before it reads anything.
A relative `CONTROL_RUNNER` is taken against this file's own directory — the
entry point the atom's `exec.command` already names and whose bytes the run
pins — so an atom can name the harness that sits beside it without writing down
a path that only exists in one checkout; the pinned hash is checked over the
bytes either way. A candidate that carries a `scripts/controls-check.sh` which
prints the passing line for every row is therefore never started, and a driver
planted in the tree being checked refuses to answer. The runner is reached
through a fresh link the driver makes inside the copy for each row, its bytes
are hashed again through that link when the row ends, and a row that replaced
them is reported `runner-changed` — as is every row after it, because nothing
those bytes report is the declared runner's. The runner may not live inside the
candidate or inside the step's own scratch, and a row that switches
`scripts/controls-check.sh` off is refused: the runner cannot prove itself.

The candidate comes out of git's object store and never out of a working tree.
A coding worktree with uncommitted files is untrusted input, and the script
refuses to start on a file that differs from HEAD, so the step asks the commit
for its tree (`rev-parse --verify`), lists it (`ls-tree -r -z`) and reads its
blobs (`cat-file --batch`), writes them into the scratch directory of its own
attempt, and binds the copy to the candidate without making a commit: the copy's
own repository is given one object alternate — the one git names for the source
repository, checked to be a directory — and through it `read-tree` fills its
index with the candidate's tree and `update-ref` gives its HEAD the candidate's
commit. atomic makes git commits outside the sandbox, and a scratch copy needs
no commit of its own: the candidate's commit and tree are already in the store
the alternate names, so the copy needs a HEAD and an index. The run is refused
unless the copy's HEAD names the candidate's tree and `git status` finds the
copy's files exactly as that tree holds them: a file skipped, a mode changed or
a link resolved is a copy that is not the change asked about, and nothing is
switched off in it. Two rows naming one file are refused in one step, because
one run saves what the other switches off — the script's own rule for a run.

The step keeps to the five minutes an exec command has. `setup_seconds` (40 s
by default) is reserved for reading the candidate out of git and copying it, and
`report_seconds` (20 s by default) is held back at the end for what has to
happen after the last row: stopping a process group that will not die, removing
the copy, and writing the one JSON value the step answers with. Nothing a row or
a git command is ever given runs into that reserve — each is cut off
`report_seconds` before the step's own end — and a row gets `row_seconds`
(240 s by default) or what is left before the reserve, whichever is less. A
declared set whose setup, rows and reserve cannot all fit `step_seconds` (300 s
at most) is refused where the reason can still be said rather than started and
left half-proved. Once the step's own clock has less than five seconds of work
left nothing further starts, and every row still unrun is reported `no-time` and
fails the step, so an unproved control is never simply absent from the answer;
and if less than five seconds are left for the answer itself once the rows have
ended, the step refuses with that said, rather than being cut off by the backend
mid-report. A row that overruns is ended by killing its whole process group, and
a `cat-file` stream this driver rejects is killed, reaped and joined before its
refusal is raised, so neither a hung build nor a hung git outlives the step.

Nothing is read further than its cap allows. A row's output is drained as it
arrives, on a thread that ends the row's command the moment the limit (2 MB) is
passed — and what is kept for the answer is cut at the limit, not merely
counted — so a row that floods is reported instead of buffered; git's blobs are
streamed to disk through `cat-file --batch` with a bound per object and one on
the whole copy; and the hashes are taken over a bounded number of bytes. A
candidate that holds a very large file or writes hundreds of megabytes is a
refusal or a verdict, never the step's memory.

The verdicts are `passed`, `blind` (its test still passes with the control off,
which is what fails the step), `switch-stale`, `build-failed`, `no-test`,
`not-seen-failing`, `fails-when-on`, `timed-out`, `left-switched` (its saved
originals are still in the copy, so a control may be off), `runner-changed`,
`no-time` and `errored`. A refusal exits 2, a failed row 1, a passed set 0; the
input is one JSON value on stdin and the answer one JSON value on stdout, with
one short line per row on stderr, where atomic keeps the end of a failed
command. The answer is written before cleanup. The driver then attempts to remove its
scratch copy; an outer kill or a deletion error can leave it behind. A second
attempt reads the same recorded commit and reruns the declared rows.

The step is confined as any exec atom is (see
[exec atoms](reference/atom-files.md#exec-atoms)): no network, no write outside
its scratch directory, and none of what the backend denies it.
`scripts/controls-exec-test.sh` (CI's Pins job) tests the driver against
throwaway repositories with a stand-in for cargo, and
`crates/atomic/tests/controls_exec.rs` runs the same driver through
`runtime::run_exec` under the real sandbox, where a row whose command reaches
the network, writes outside its scratch, reads a denied directory or writes
through the link its runner was started through fails the step. Neither needs a
toolchain, a model or a provider. Six rows under `scripts/controls/` switch this
step's own guards off — that the candidate is the recorded commit, that the
runner and the driver are the declared ones, that a relative runner name is
resolved against the driver, that the copy is bound to the candidate's commit
and tree, and that the declared rows and the answer reserve fit the step — and
name that sandbox test, so CI's Controls workflow runs them as it runs every
other row. The guard that refuses an answer the step has no room left to write
is not among them: a row runs a `cargo test`, and that guard is reached only by
spending real time, so `scripts/controls-exec-test.sh` is what proves it bites.
What memory and CPU a row uses is what the container running the runner allows,
as for `controls-check.sh` itself: atomic bounds the command's wall time (300 s)
and its output (8 MiB), and the driver bounds each row's time and output.

### The Controls workflow

On every pull request, `.github/workflows/controls.yml` runs the rows the pull
request touches, which `scripts/controls-touched.py <base> <head>` lists: each
row whose row file it adds or changes; each row whose `file` it changes, since
a code change can move a switch's text or stop it biting; each row naming a
test whose name is in a file it changes, before or after the change, since a
change to the guarding test can stop it failing; each row whose `--test <t>`
target, `crates/*/tests/<t>.rs` or `crates/*/tests/<t>/`, it changes; and,
when it changes test code a crate's targets share (`tests/support/`,
`tests/fixtures/` or any other directory under `crates/<c>/tests/` that is
no one target's, or a file there that is not Rust), every row whose
`--test` target is in that crate; and, when it changes a test helper outside
`tests/` that `scripts/controls-shared.txt` lists (the fake model in
`crates/atomic-core/src/testing.rs`, the Postgres test schema in
`crates/atomic-durable/src/testing.rs`), every row the list says it serves:
each with a `--test` target, each whose features turn on the one named, or
each that builds a package named, since a crate's own unit tests and those of
the crates that turn its test features on compile the helper too. The script fails on a row file it cannot
read, and on a row file deleted while its switch's text is still in some
file, rows aside, that no other row switches it in: a control whose code
remains, renamed or moved included, keeps its row.

Before applying those rules, the selector follows one level of literal Rust
`include_bytes!`, `include_str!`, `include!` and top-level `#[path]` references,
at both the base and the head. Paths resolve from the source file that names
them, following committed symlinks before parent components and retaining both
the link and its final input. Legal Rust comments between tokens do not hide
references. Equal basenames elsewhere do not claim the same input. Literal
crate-relative `tests` paths named by integration targets retain every reader,
including equivalent spellings such as `tests`, `./tests/data`, and Rust
escapes. Runtime file and directory symlinks also resolve against committed
trees, retaining their link names and final inputs. A concrete `tests/...`
directory named by an integration target belongs to every target that names
it. An unowned directory, a compiled shared Rust module, or an includer whose
further users were not followed keeps the crate-wide shared-test fallback.
Root-wide test scans and individual runtime filenames do not narrow that
fallback.

This is a literal, one-level dependency scan, not the Cargo dependency graph.
Computed include arguments, nested inline-module `#[path]` bases, changes to
Cargo features, `build.rs`, lockfiles and dependency pins, and runtime-opened
data not covered by a literal reference or the shared-helper list still need
the full sweep. Cross-module test helpers not named by those rules need it too.
The ordinary Rust test jobs still run for those changes; the sweep additionally
checks the guard-removed twins that selective Controls may not select.

A few rows switch text that also appears in another file, such as
`held: true,` or a `NotFound` arm, so deleting one of them can be refused for
a copy that has nothing to do with it; the refusal names the files. Then take
it in two pull requests: first change the row's switch to a longer stretch of
its line that only its own file holds (`git grep -F` shows where text
appears), and let that pull request's Controls run show the row still bites;
then delete the row. Where no line of the control is unique, the owner
decides whether to merge past the check.

The "Touched controls" job cuts the rows into shards, one for every six rows
and at most twelve; each shard, "Controls (i/n)", is set up as the test jobs
are, sandbox and Postgres included, builds once and runs its rows in turn with
`controls-check.sh`. A shard's summary holds one result line per row, the
script's own:

```
ok run-keeps-input: its test fails with the control off and passes with it on
FAIL serve-line-names: its test passed with the control off
```

A row that ends any other way, or runs past 45 minutes, gets a `FAIL` line
too, as does each row a stopped shard did not reach, and a failing row's cargo
output is kept as the shard's `controls-logs-<i>` artifact. "Controls" passes
only when every touched row passed, and in seconds when none is touched. The
"Touched controls" summary also lists each deleted row and why its deletion
was allowed, and each touched row CI does not run, each with a warning on the
pull request: check both.

A merge group runs no rows: once "Controls" is a required check, a pull
request joins the queue only after its rows passed. Every Sunday, and when
started by hand (Actions > Controls > Run workflow), the workflow sweeps every
row instead, in up to sixteen shards, so that what touches no row, such as
changes that only break a row together or a lockfile or pin bump, is checked
too.

A row CI does not run, one a GitHub runner cannot run or one known to fail
until a fix lands, is listed with its reason in `scripts/controls-local.txt`.
A pull request touching it, and every sweep, reports it as "not run in CI",
with a warning, and leaves it to a local run; a pull request that changes the
list gets a warning too, and should say why, and a row it takes off the list
is run.

## Milestone acceptance

A milestone's acceptance clauses, from [the milestones](design/milestones.md),
are listed in `scripts/acceptance/<milestone>.txt`, each with the tests that show
it, and `scripts/acceptance.py` runs them as one check. CI runs it in the
"Acceptance and backends" job, after building the tests with the same
features, so nothing rebuilds:

```bash
scripts/dev.sh scripts/acceptance.py m42a # lists no Postgres or sandbox tests
ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/acceptance.py m12
ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/acceptance.py m39a
ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/acceptance.py m45
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
a colon, however that word is spelt, and however it is wrapped in markdown
punctuation (`-`, `_`, `*`, `~`, `>` and `#`, with spaces), which may carry one
bullet's mark in front of the word (`- `, `* `, `+ `, or a number, then `.` or
`)`, then a space), as in `Acceptance:`, `__Acceptance:__`, `**Acceptence:**`,
`- **Acceptance, files and data:**` and `1. **Acceptance, files and data:**`;
within a block, every
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

### The v0.1 scenario

`scripts/v01-gate.py` runs the v0.1 release's own scenario on a built binary
and prints a transcript of it. It runs the order-report example's workflow and
kills it twice with SIGKILL: once in the middle of a step, while the lite atom's
model call is in flight, and once while the run waits at its gate. It recovers
the run with `atomic runs resume` and approves the gate from a second
invocation, then runs `atomic eval`. From what the store recorded, it checks
that the run finished under the id it started with, that no completed step ran
again, that the run stayed within the budget caps it started with and that the
gate kept its deadline. A fake model on a loopback port answers every call and
counts the calls, so it needs no provider and no network; exec atoms run in the
sandbox, hence `ATOMIC_DEV_SANDBOX=on`:

```bash
scripts/dev.sh cargo build --release --locked -p atomic-agentic
ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/v01-gate.py --atomic target/release/atomic
```

It works in a new directory under `target/` and removes it at the end;
`--keep <dir>` works in a new `<dir>`, which must not exist yet, and keeps it.
Neither works under `/tmp` or `$TMPDIR`, where atomic refuses an atom's
scratch — including a checkout that lives there, whose `target/` is under it
too.

## The crash suite

atomic's crash tests kill the process at named failpoints. A failpoint is
compiled in only with the `failpoints` features of `atomic-agentic-durable` and
`atomic-agentic-core`, which the `atomic-agentic` package's tests turn on; a
release build has none. Build the CLI with `--features
atomic-agentic/dev-failpoints` to enable `atomic run <workflow> --fail-at
<step>`: it kills the process just after that step's accepted result is
checkpointed, and `atomic runs resume <run-id>` continues the same run. The
flag is absent from release builds. Three variables also drive failpoints:

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
| Acceptance and backends | Builds every test, checks that the shards run each test exactly once, runs the M42a, M12, M35a, M39a and M45 acceptance checks, and runs the Postgres and sandbox suites without their backends |
| Build, lockfile and SBOM | `cargo build`; the doctests without the test features, which must include the compile-fail doctest showing that `ExecSandbox`'s loopback method is absent without its feature; `scripts/lock-resolves.sh`, `scripts/lock-check.sh v8-built` and `scripts/sbom.sh` |
| Rust (ubuntu-24.04) | Passes only when every job above passes |

Pushes to main also run two macOS jobs in parallel. "Clippy (macos-15)" runs
formatting and Clippy while "Rust (macos-15)" builds and tests the workspace.
A manual run, on main or on a branch, can add both jobs with the `macos`
input, which is how a change for macOS is checked before it merges:

```bash
gh workflow run ci.yml --ref <branch> -f macos=true
```

Pull requests and merge groups run the Linux checks. CodeQL runs in its own
workflow on main only
(each push, weekly and by hand), so an alert shows up after its change merges;
fix it on main. Nothing in CI waits for it. The Controls workflow runs the
controls rows a pull request touches
([The Controls workflow](#the-controls-workflow)).

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

The cold full-repository controls proof in `tests/pipeline.rs` runs separately
from the broad pipeline target so its bounded Cargo row has the runner to
itself. The broad suite reports that one test as ignored; CI runs it in a
required shard-1 step and checks that exactly one named test passed. Run the
same gate locally in addition to the workspace suite:

```bash
ATOMIC_DEV_SANDBOX=on scripts/dev.sh cargo test -p atomic-agentic \
  --test pipeline --locked \
  --features atomic-agentic/postgres-tests,atomic-agentic/sandbox-tests \
  a_full_repository_control_row_uses_host_cargo_in_normal_review \
  -- --ignored --exact
```

The cold full-repository trial-merge proof runs in the same dedicated shard.
It checks the current workspace in a fresh private Cargo target with the pinned
offline registry and Git cache. Trial-merge Cargo checks have a 900-second
command limit for cold workspace builds; other trial commands keep their
300-second limit. Run it locally with:

```bash
ATOMIC_DEV_SANDBOX=on scripts/dev.sh cargo test -p atomic-agentic \
  --test pipeline --locked \
  --features atomic-agentic/postgres-tests,atomic-agentic/sandbox-tests \
  fixture_bound_trial_merge_checks_full_atomic_with_rust_runtime \
  -- --ignored
```

Confirm its output names the test and reports `1 passed; 0 failed; 0 ignored`.
A zero-test result does not satisfy this gate.

`scripts/test-shards.py run` first checks the table against `cargo metadata`,
so a test file in no shard, or in two, fails every shard until the table
places it. `scripts/test-shards.py check`, in the "Acceptance and backends"
job, lists the tests (`cargo test -- --list`) of one unsplit
`cargo test --workspace` and of each shard, and fails unless each test is in
exactly one shard. `scripts/test-shards-test.sh` (in CI) tests both checks,
and that the workflow runs every shard of the table: a shard added to the
table needs a matrix entry in the test job as well.

### Build caches

The Linux Rust jobs restore two caches, through `.github/actions/rust-setup`:
cargo's downloads (the registry and git sources), keyed on the toolchain and `Cargo.lock`, and
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

The macOS build and test job can restore Linux's Cargo download cache for the
same toolchain and lockfile. Cargo fetches any missing macOS dependencies and
builds for macOS on its own runner.

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

1. builds `atomic` in release mode for `x86_64-unknown-linux-gnu`,
   `aarch64-apple-darwin` and `x86_64-pc-windows-msvc`, with the checkout and cargo paths remapped out of
   the binary;
2. packages each binary with `LICENSE`, `NOTICE` and
   `THIRD-PARTY-NOTICES.md` into a deterministic archive. Windows uses a ZIP
   containing `atomic.exe` and the three sandbox helpers built from the Codex pin.
   Linux and macOS use `.tar.gz`;
3. writes each binary's CycloneDX SBOM, which records the binary's SHA-256 and
   lists every fork at its pinned revision;
4. signs each archive and each SBOM with Sigstore, keyless, under the
   workflow's own GitHub identity, writing a `<file>.sigstore.json` bundle
   beside it, then verifies every bundle with `cosign verify-blob` against
   that identity, so a signing step that fails or signs as anyone else fails
   the run;
5. attaches the archives, the SBOMs, their bundles and a `SHA256SUMS` file to
   a draft GitHub release, which a maintainer reviews and publishes.

Only the signing job can request the OIDC token that signs; the build jobs
cannot. A manual run of the workflow on any branch does steps 1 to 4 and keeps
the results as workflow artifacts; its signatures name that branch, not a tag,
so they never pass the check a user runs on a release download
([verifying a download](getting-started/installation.md#verifying-a-download)).
Selecting `windows_only` on a manual run builds, tests and signs just the Windows
ZIP and its SBOM. The Windows test extracts the ZIP and runs a coding atom
twice against a local scripted Responses endpoint. Each time, the sandboxed
`cmd.exe` must copy a fixture file in its worktree, hash the copy and receive
access denied when reading private Atomic state.

To package a build locally:

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

atomic knows Codex's Linux sandbox helper by how Codex starts it, and a bump
that changes that changes what atomic recognises as the helper. Check, in the
candidate's `sandboxing` and `linux-sandbox` crates, that the helper's first
arguments are still `--sandbox-policy-cwd`, alone or after
`--inherit-pid-namespace`, and that the helper is still started under the
name `codex-linux-sandbox`. Then update `LINUX_SANDBOX_ARG1` and
`INHERIT_PID_NAMESPACE` in `crates/atomic-core/src/codex.rs` to match.

`scripts/milestone-tests.sh` also runs locally:

```bash
scripts/dev.sh scripts/milestone-tests.sh
```

## Astro compiler

Declarative atoms use `.atom.astro`. The production compiler assets are bundled
under `crates/atomic-core/assets` and included in the binary. Running atoms needs
no Deno, npm install or project `node_modules`: atomic starts itself as the
compiler's child process (`__atomic-compiler`) and runs the bundled worker on its
embedded Boa engine. Oxc parses TypeScript and transpiles captured helpers.
The tsz checker uses the captured sources, generated declarations and bundled
ES2022 library. The compiler child has a cleared environment, 1 GiB allocation
budgets for the heap and buffers, a 2.5 GiB writable-memory ceiling, a 32-second
CPU limit, no core files and a 30-second parent deadline. On macOS the parent
enforces the memory ceiling as described below. Its
host (`compiler_host/host.js`) gives the worker the protocol's stdin and stdout,
stderr for its console, its exit status, native Astro parsing and TSX conversion,
random bytes, SHA-256 and timers, and nothing else: no network, environment, filesystem or subprocess.
atomic reads atom sources itself and sends them in the request, for loading as
for rendering. Direct-run context checkpoints live outside the source tree.

The compiler child and the child that runs a captured helper call
(`helper_runtime/process.rs`) each have a memory limit, and the two platforms
hold it differently:

- **Linux.** The child sets `RLIMIT_DATA` on itself before compilation starts, and the
  kernel refuses writable memory past it, written to or not.
- **macOS.** There `RLIMIT_DATA` and `RLIMIT_AS` both bound the size of the
  whole address map, the kernel refuses a limit below the map's present size,
  and an arm64 process starts with hundreds of gigabytes mapped that nothing can
  use, so a child cannot set a limit near the memory it may hold. The parent
  holds the limit instead (`crates/atomic-core/src/child_memory.rs`): every two
  milliseconds while it waits, including during a captured helper call, it
  asks for the child's physical footprint. A reading over budget kills the
  child. The interval is a requested wait and the operating system schedules
  each reading. The footprint counts committed memory, including compressed
  memory. A reservation or brief peak can go unobserved. A failed reading
  kills the child, with a one-second grace period when the OS reports that
  the child is absent and the parent has yet to collect it. Each child also
  watches a private channel and exits if the supervisor disappears.

The host allocation budgets (the heap and, for helpers, the buffer budget)
are the same on both. The tests of the parent's limit run on Linux too,
where they put a small limit in place of the child's own
(`child_memory::with_limit`).

A test binary cannot dispatch `__atomic-compiler` from `main`, since libtest owns
it, so atomic-core's tests and the `compiler-in-tests` feature (atomic's
dev-dependencies turn it on) serve the child from a constructor that runs before
`main`. Release builds never have it, and `scripts/release-package.sh` refuses a
binary that does.

`tools/check_examples.py` runs its Astro checks on that same compiler: build atomic
first, or set `ATOMIC_BIN` to an atomic binary. It is a Python program of its own,
needing `PyYAML`, `jsonschema` and `referencing`, which the development image does
not carry: CI's Examples job installs them in a virtualenv with
`pip install --no-deps --require-hashes -r tools/requirements.txt`, and runs the
checker from there. The development image still carries
Deno, for developer tooling only: `authoring/src/production-controls.mjs`, a probe
of the worker under Deno's own permissions, and the example code's own `deno test`.

Contributors changing the interpreter or type checker rebuild the shipped assets.
The development image carries Deno but no Node or npm, so these run where Node is
installed, not through `scripts/dev.sh`; CI's Astro compiler job installs, tests and
builds in its pinned `node:24.21.0` container, and then fails if the rebuilt assets
differ from the committed ones:

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
