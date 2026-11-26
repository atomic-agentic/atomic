# atomic: instructions for coding agents

atomic is a Rust framework for small, typed, single-purpose agents ("atoms").
It builds on OpenAI's open-source Codex crates and gets durable execution from
a fork of durare. A workflow of atoms survives a crash without redoing finished
work, and a person signs off at durable gates.

**Status: early development.** The current phase is v0.2. After the M5
cutover, this repository tracks its work in atomic's local work graph.

## Where things are

| What | Where |
| --- | --- |
| The work to do | Atomic work graph (`atomic --json graph ready`) |
| User documentation, as designed | `docs/` (`docs/reference/availability.md` says what is built) |
| The example catalog, the v0.1 authoring contract | `examples/`, checked by `tools/check_examples.py` in CI |
| Design notes and private references | `AGENTS.local.md`, if present (not in git) |

Read `AGENTS.local.md` when it exists: it holds maintainer-local pointers to
design material that is not part of this repository. If an item cites a document
you cannot find, say so instead of working from memory.

## Work tracking

After the owner imports the Beads export and verifies ready-set parity, atomic's
local SQLite graph is the source of truth for new work. Imported items keep
their Beads IDs and acceptance criteria. Run these read-only commands from the
repository root; `--json` is a global option and precedes `graph`:

```bash
atomic --json graph ready --limit 1000  # committed items ready to claim
atomic --json graph show ITEM_ID         # body, criteria, blockers and edges
atomic --json graph search '<phrase>'   # find committed items and handoffs
```

The imported work keeps this structure:

- **Three levels.** A phase epic (`v0.1`, `v0.2`, `v0.5`, `v0.7`, `v1.0`) holds
  milestone epics (`M0` to `M56`, some split into `a` and `b` parts), which hold
  tasks.
- **Labels.** The phase and the milestone, plus `spike`, `decision`, `test`,
  `security`, `docs` and `benchmark`.
- **Every milestone ends in an acceptance item** whose criteria state the
  milestone's acceptance test. A milestone is done when that item closes, not
  when its other tasks do.
- **Later phases are blocked by the phase gate before them.** Do not work
  around a blocker; if a dependency is wrong, fix the dependency and say why.

Rules for working an item:

1. Read its acceptance criteria and the design section it cites before
   writing anything.
2. Start only after its claim is recorded by an authorized graph workflow or
   the enrolled operator. Close it only when the acceptance criteria hold and
   you have run the check that shows it. Before reporting a change as ready,
   go through [Before you report a change](#before-you-report-a-change).
3. File discovered work under the same parent with a `discovered-from` link,
   and keep the current item to its scope. Inside a graph run, typed graph
   tools add proposals to the current private candidate; publication commits
   accepted proposals. For manual work, give the proposed item and link to the
   enrolled operator to record.
4. Items of kind `decision` are the owner's to decide. Gather evidence, report
   it to the owner and stop there. The same goes for phase gates.
5. Committed graph writes require the enrolled human operator credential.
   Coding agents must not request or use that credential or invoke privileged
   graph commands from an atom's shell. Graph workflows claim and close their
   own items through durable steps. Beads is retained only for the historical
   export and parity checks, not for filing new work.

For manual work, the enrolled operator can use these committed write commands
after reviewing the item, dependency and evidence. They are **not** commands
for a coding agent to run:

```bash
atomic --json graph create 'Task title' --body 'Description and criteria' --kind task --request-id REQUEST_KEY
atomic --json graph link NEW_ID parent-child PARENT_ID
atomic --json graph link NEW_ID discovered-from SOURCE_ID
atomic --json graph claim ITEM_ID
atomic --json graph close ITEM_ID
```

## Architecture rules

These are the rules that are easiest to break:

- **One Cargo workspace, one binary.** Crates: `atomic-core`, `atomic-durable`,
  `atomic-atoms`, `atomic-graph`, `atomic-schema`, `atomic-exec` and the
  `atomic` binary. Their packages are named `atomic-agentic-*` (the binary's is
  `atomic-agentic`); the workspace aliases them to the short names used in
  code, so pass the package name to `cargo -p`.
- **Codex only behind `atomic-core`.** Depend on the Codex crates at one pinned
  git rev and never fork Codex core. The workspace copies Codex's
  `[patch.crates-io]` block verbatim.
- **durare only behind the `DurableRuntime` trait** in `atomic-durable`. No
  other crate names durare.
- **Runners only behind `ExecutionBackend`** in `atomic-exec`, even when the
  control plane and the runner share a process.
- **Steps never nest.** An atom's turn, its reviewers and each repair round
  are sibling steps composed in workflow code.
- **No database transaction stays open across a model turn.** A turn
  checkpoints a private candidate; a short transaction publishes it.
- **Steps run at least once.** Anything outside the step's own checkpoint
  must be idempotent, deduplicated by a stable key, reconcilable, or parked as
  `outcome_unknown`. A gate authorizes an effect; it does not deduplicate it.
- **atomic makes git commits, outside the sandbox.** A worktree an atom wrote
  to is untrusted input: run git there with hooks, drivers, signing and pagers
  off.
- **Declarative atoms interpret frontmatter and templates; only captured
  helper modules execute, one fresh capability-free isolate per call.**
  `.atom.astro` files use the bundled Astro parser and a bounded
  TypeScript-shaped AST interpreter. Helper calls run in a capability-free Boa
  isolate behind `atomic_core::helper_runtime`, with no host operations,
  Rust-enforced limits,
  the step's recorded clock and a result as trusted as its least trusted
  argument. Workflow mappings remain CEL. JSON Schema stays the runtime
  contract.
- **JSON Schema is the contract.** atomic validates every model output and
  tool argument against the full schema itself.
- **Runtime state never lives in the source tree.** It goes in the per-project
  state directory.
- **Gate decisions and admin actions come only from a human credential,**
  never from a model tool call or an atom's shell.

## Build and test

Build and test inside the development container, through `scripts/dev.sh`. It
runs any command in a pinned image with the toolchain Codex pins (Rust 1.95.0),
under memory, CPU and time limits, and starts a throwaway Postgres for tests.
[docs/development.md](docs/development.md) covers its settings.

```bash
scripts/dev.sh cargo build --workspace --locked
scripts/dev.sh cargo test --workspace --locked --no-fail-fast
scripts/dev.sh cargo test --workspace --locked --features atomic-agentic/postgres-tests
scripts/dev.sh cargo fmt --all --check
scripts/dev.sh cargo clippy --workspace --all-targets --locked -- -D warnings
scripts/dev.sh cargo clippy --workspace --all-targets --locked --features atomic-agentic/postgres-tests,atomic-agentic/sandbox-tests -- -D warnings
scripts/dev.sh cargo test -p atomic-agentic-core --lib --features fake-model   # one crate
```

CI runs the same build, fmt, clippy and tests, plus these checks, on Linux
for every push to main, pull request and merge group. macOS runs on pushes to
main and on manual main runs with the `macos` input. Run everything locally
first. These checks also run locally:

```bash
scripts/codex-pin.sh check     # the [patch] block and toolchain match deps/codex.toml
scripts/forks-check.sh         # each fork in deps/forks.toml resolves at its pinned rev
scripts/lock-check.sh          # crates shared with Codex keep Codex's locked versions
scripts/lock-check-test.sh     # exercise the V8 dependency guard on small lockfiles
scripts/dev.sh scripts/lock-check.sh v8-built   # verify the binary's dependency graph is V8-free
scripts/dev.sh scripts/lock-resolves.sh   # a fresh resolution keeps Codex's libsqlite3-sys
scripts/dev.sh cargo deny --locked check bans licenses sources   # deny.toml
scripts/dev.sh scripts/sbom.sh # the SBOM lists every fork at its pin
```

The Codex pin, the forks and the lockfile exceptions are recorded under
`deps/`; change them there and let the checks tell you what else must move.
Release builds and packaging are described in docs/development.md.

## Testing rules

- **A suite that cannot reach its backend fails. It never skips.**
- **Durability and repair tests use the scripted fake model** and count model
  calls. Tests need no provider and no network.
- **A control is tested by removing it:** switch the control off once and see
  its test fail.
- **Port a behavioural test before the code it describes,** and confirm it
  passes against the implementation it came from first.
- **Crash tests kill the process** at named failpoints and assert that no
  completed step repeats.

## Before you report a change

Reviews of this repository keep finding the same things. Go through every
item before you say a change is ready, fix what fails, and in your report give
one line per item saying how you checked it. A reviewer reads those lines
first.

1. **Controls bite on production's own path.** Every new guard has a test
   that fails when the guard is removed, and a row in
   `scripts/controls/<name>.row` (the grammar is in
   [docs/development.md](docs/development.md#controls-are-tested-by-removing-them):
   the name is the file's base name, the switch text appears exactly once in
   its file, and the arguments are what follows `cargo test --locked
   --no-fail-fast`, space-separated and unquoted, with each guarding test
   named after `--`). The test exercises the call production makes, not a
   test-only twin, a hand-built helper or a constructor the real path never
   uses.
2. **Tests fail for the right reason.** Assert the specific error, reason or
   message. Never assert through a fallback that hides a missing value
   (`unwrap_or_default`, an empty-string default, `contains` on text that
   could be absent). A refusal test shows the refusal, not a "not found".
3. **Claims match the code.** Docs, comments, error messages, commit messages
   and the PR description say only what the code does. "Every", "never",
   "any" and "always" are literally true, or the exceptions are named. A
   change covers every path it claims: lite, Codex and exec atoms, plain and
   `--json` output, every call site. When something becomes built, update
   `docs/reference/availability.md`; when an example changes, run the
   example checker as CI's examples job does (its virtualenv and a built atomic
   included).
4. **Fail closed.** No error path, empty set, unknown value or missing
   component ends up allowing, showing or passing what it should refuse.
   Unknown means refused. When narrowing or redacting, compare with main on
   edge inputs: the result is never looser than main's. Redact before you
   escape or transform text. Item 9 is how you show it.
5. **Test-only code stays out of release.** Hooks, seams and fakes sit behind
   `cfg(test)` or a dev-only feature, and in a build without that feature no
   environment variable, configuration file or atom can turn them on.
6. **It passes the checks on current main.** Run the fmt, clippy and test
   commands from [Build and test](#build-and-test) for what you touched, on
   your change merged with current `origin/main`. Merge in a scratch worktree
   so your own tree is untouched, and keep that worktree between checks so
   its `target/` stays warm: `git worktree add --detach <scratch> HEAD` once;
   then each time `git -C <scratch> checkout --detach <your HEAD>` and
   `git -C <scratch> merge --no-commit origin/main`. If it conflicts, report
   the files, run `git -C <scratch> merge --abort` and stop. Build and test
   there through `scripts/dev.sh` (a linked worktree needs the main
   checkout's `.git` mounted, as docs/development.md describes), then run
   `git -C <scratch> merge --abort` (ignore "no merge to abort"). This checks
   committed work only; if your change is not committed, say so. A clean
   textual merge can still break a call whose signature changed.
7. **Resources and races.** A lock, hold, reservation or temporary directory
   is released on every path, including errors, panics and drops. Anything
   checked before a lock is re-read under it. A step that runs twice is safe.
8. **Tests use std and the development image only.** Signal, inspect and
   reap processes with libc or std, not `kill`, `ps` or `pgrep`. Use only
   commands the development image provides (git, sh, coreutils, bwrap). Do
   not depend on the host's paths or path lengths.
9. **Prove every error path fails closed.** List each place on a path that
   decides between allowing and refusing where an error or an absence is
   handled: a `?`, a `match` on an error, `.ok()`, `unwrap_or*`, `is_ok()`,
   `is_file()`, `is_dir()`, `exists()`, a `break` or `continue` on a check.
   For each, say what happens on NotFound, permission denied, a link loop or
   "not a directory", another I/O error, a parse error, a timeout and an
   empty result. Only a true "it is not there" counts as absent; everything
   else refuses. A rule that passes something over because it is "someone
   else's", "out of reach" or "not ours" must hold when the user can make it
   look that way: a uid they can map, a hard link, a link they own, a
   directory they can chmod, a `PATH` entry they can write. Add at least one
   test with an error other than NotFound, such as a mode-000 directory or a
   link to itself, and assert the error took effect, so that under root the
   test fails rather than passing vacuously. Put the list in your report.
10. **Prove every row bites, by running it.** CI's Controls workflow runs
    each row whose row file, switched file or guarding test your pull
    request changes; quote each row's result line from its shard's summary
    ("ok <row>: …"). Check the "Touched controls" summary's "deleted" and
    "not run in CI" lines too, and run a row listed in
    `scripts/controls-local.txt` yourself. Run a row locally only while you
    debug it:
    `ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/controls-check.sh <name>`
    (in a linked worktree, mount the main checkout's `.git` as
    docs/development.md describes). Rows run only on committed work; if you
    have not been asked to commit, say which rows are unrun and why. For
    each row, say what the switched-off code then does and which assertion
    fails. A guard with two halves (kill and reap, the terms of a count, a
    check and its message) is switched off one half at a time; a half whose
    removal fails no test gets its own test and row. With any control off, the test
    still needs no network and no real provider: point every endpoint it
    could fall through to at loopback.

## Git

- Commit only when asked. Never push unless asked.
- Commits carry the repository owner's git identity and nothing else: no AI
  author, no `Co-Authored-By` trailer, no "generated with" line.
- The work graph's runtime database is not in git. The legacy `.beads/`
  configuration and hooks remain tracked for export and parity checks.

## Licensing

- atomic is Apache-2.0, matching Codex.
- beads is MIT. atomic's work graph is a new implementation of its core,
  credited in the third-party notices. Do not ship a binary named `bd`.
- **Never read or copy code from `beads_rust`.** Its licence rider excludes
  OpenAI, Anthropic and anyone acting on their behalf.
- Third-party code that is vendored or adapted keeps its licence and is
  credited in the third-party notices.
- **Repository content is about atomic only.** No business plans, roadmaps
  with dates, competitor analysis or personal details in tracked files or
  commit messages.
