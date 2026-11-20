# Operating and evolving atomic

This page is about change and upkeep: how a new atomic release or a new file
format reaches a project without breaking the runs already under way, what an
operator has to look after, how atomic's own test suite is organised, and the
licence and platforms. The user's side of local and team operation is in
[run locally or with a team](../operations/running-atomic.md); the development
container and the repository checks are in
[developing atomic](../development.md).

## Upgrades and format stability

- **Format versioning.** Workflow and manifest files must state their format
  version, `atomic: 1`, and `atomic check` fails a file that leaves it out. An
  Astro atom gets `atomic: 1` by default. Within a format version, releases only
  add fields that are optional; anything breaking moves to a new number, and
  `atomic migrate` will rewrite older files (M36b).
- **Store migrations.** Every store records its own schema version and migrates
  itself forward when atomic starts. A binary that finds a database newer than
  itself refuses to start (M36a).
- **Runs are pinned.** A run keeps the content hashes of the atoms, workflow and
  code it began with and replays against those; an edited file changes only the
  runs started after the edit.
- **The binary is part of a run's identity.** durare will only recover a run
  stamped with the application version it is running, and unless told
  otherwise it uses a hash of its own executable as that version. Installing a
  new binary would then quietly orphan every unfinished run. atomic will set
  the version itself, derived from its release number and the format of its
  workflow pipeline, so an upgrade that changes neither keeps recovering those
  runs.
- **Incompatible changes drain or refuse.** If a new binary would replay a run
  in a different way, that run is either migrated on purpose or left to finish
  on a worker still running the old version. A run that allows neither is
  refused with a message telling the operator what to do; it is never dropped
  silently.
- **Rolling upgrades.** During a rollout, control-plane replicas of both
  versions run at once. Each worker announces its version, and a workflow is
  recovered only by a worker that can replay it.

## Operations

- **Backup and recovery.** Postgres keeps point-in-time recovery, worktree
  commits live on the git remote, and artifact and snapshot buckets are
  versioned. For the team profile the goal is to lose no more than five minutes
  of data and to be running again within an hour; a restore drill checks both,
  including that workflows interrupted by the restore carry on (M37).
- **Garbage collection.** Finished runs, durable checkpoints, Codex threads,
  worktrees, artifacts and snapshots each have a retention rule and a cleaner,
  so a system running continuously does not keep growing on disk.
- **Service levels.** Objectives cover how quickly steps are scheduled, how
  quickly gate notifications go out and what share of workflows succeed. They
  are built on the signals in [observability](observability.md), with
  dashboards, alerts and runbooks for a stuck workflow, an exhausted quota and a
  recovery that failed.
- **Admin endpoints.** Besides health and readiness, `atomic serve` reports its
  resolved configuration, the conditions it detected, its runs and its metrics.

## Testing atomic itself

A durability promise means little unless a test kills the process and checks
what happens next, so the testing approach is part of the design:

- **A crash harness with named failpoints.** There are failpoints on both sides
  of every checkpoint, inside an atom's turn, inside a commit, a merge and a
  sync, and during a suspend. The crash suite fires them one at a time. Each
  time the workflow must either finish without re-running a step that had
  completed, or, when an external effect's outcome cannot be known, leave that
  step parked as `outcome_unknown` (M38).
- **Property tests for graph merge.** Applying the same set of operations in any
  sync order, across many random orders, must reach the same state (M8, M38b).
- **Missing backends are failures.** When a suite's database or service is
  unreachable, the suite reports an error instead of passing quietly. A test
  that returns early because its database is absent makes a plain test run look
  green while checking nothing.
- **A scripted fake model.** A deterministic stand-in for a provider speaks the
  provider protocols and drives real Codex threads, so durability and repair
  tests can count every model call without a provider or a network.
- **Controls are tested by removing them.** Each sandbox, egress and policy
  control is disabled once, on purpose, to see its test fail.
- **Behavioural tests come first.** A behavioural test is ported before the
  code it describes, and confirmed to pass against the implementation it came
  from; it then serves as the specification the new code is written to.
- **Conformance suites with recorded transcripts.** Each storage, artifact and
  execution backend has one conformance suite that every implementation runs.
  The suites carry transcripts captured from the reference implementation with
  fixed IDs and clocks, so a backend written by someone else can show it
  conforms by replaying them (M24).
- **Load tests** exercise scheduling, claims and how densely runners pack.

## Licence and platforms

atomic is licensed Apache-2.0, as Codex is. Its Codex crates come from atomic's
fork, `atomic-agentic/codex`: the upstream release tag plus two small
`utils/pty` commits that make the parent-death signal best effort where `prctl`
is refused or unsupported. Codex core is not modified. The build uses three
forks in all, listed with their pins in `deps/forks.toml`: Codex, durare (ported
to sqlx 0.9) and `transact-rs/sqlx`. The third-party notices credit Codex, each
fork and beads; the release SBOM lists every crate. Agent Substrate will be
credited where its APIs or code are used.

| Mode | Linux | macOS | Windows |
| --- | --- | --- | --- |
| `atomic` all-in-one with a local runner | Yes | Yes | Best effort, using Codex's Windows sandbox |
| `atomic serve` | Yes | Yes, for development | No |
| Kubernetes and Agent Substrate runners | Yes | No | No |
