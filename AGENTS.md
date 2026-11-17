# atomic: instructions for coding agents

atomic is a Rust framework for small, typed, single-purpose agents ("atoms").
It builds on OpenAI's open-source Codex crates and gets durable execution from
a fork of durare. A workflow of atoms survives a crash without redoing finished
work, and a person signs off at durable gates.

**Status: early development.** The build is recorded as beads. The current
phase is v0.1.

## Where things are

| What | Where |
| --- | --- |
| The work to do | Beads (`bd ready`) |
| Design notes and private references | `AGENTS.local.md`, if present (not in git) |

Read `AGENTS.local.md` when it exists: it holds maintainer-local pointers to
design material that is not part of this repository. If a bead cites a document
you cannot find, say so instead of working from memory.

## Work tracking

All work is tracked with `bd` (beads). Session hooks load `bd prime`, the full
command reference; run it yourself if that context is missing.

```bash
bd ready                  # work with no open blocker
bd show <id>              # description, acceptance criteria, dependencies
bd update <id> --claim    # claim before starting
bd close <id> --reason "…"
bd list -l M3             # everything in one milestone
bd children <epic-id>     # the tasks under an epic
```

How the beads are organized:

- **Three levels.** A phase epic (`v0.1`, `v0.5`, `v0.7`, `v1.0`) holds
  milestone epics (`M0` to `M56`, some split into `a` and `b` parts), which hold
  tasks.
- **Labels.** The phase and the milestone, plus `spike`, `decision`, `test`,
  `security`, `docs` and `benchmark`.
- **Every milestone ends in an acceptance bead** whose criteria state the
  milestone's acceptance test. A milestone is done when that bead closes, not when
  its tasks do.
- **Later phases are blocked by the phase gate before them.** Do not work
  around a blocker; if a dependency is wrong, fix the dependency and say why.

Rules for working a bead:

1. Read its acceptance criteria and the design section it cites before
   writing anything.
2. Close it only when the acceptance criteria hold and you have run the check
   that shows it.
3. File what you discover as a new bead under the same parent, linked with
   `--deps discovered-from:<id>`, and keep the current bead to its scope.
4. Beads of type `decision` are the owner's to decide. Gather evidence, record
   it on the bead and stop there. The same goes for phase gates.
5. Do not use `bd edit` (it opens an editor). Use `bd update` flags.

## Architecture rules

These are the rules that are easiest to break:

- **One Cargo workspace, one binary.** Crates: `atomic-core`, `atomic-durable`,
  `atomic-atoms`, `atomic-graph`, `atomic-schema`, `atomic-exec` and the
  `atomic` binary.
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
- **Declarative files never run code.** Expressions are CEL. Atom bodies are
  parsed with the `markdown` crate and nothing else.
- **JSON Schema is the contract.** atomic validates every model output and
  tool argument against the full schema itself.
- **Runtime state never lives in the source tree.** It goes in the per-project
  state directory.
- **Gate decisions and admin actions come only from a human credential,**
  never from a model tool call or an atom's shell.

## Build and test

There is no workspace yet, so there is nothing to build. The first beads set
it up: a development environment (`atomic-1a1`), the pinned Codex rev
(`atomic-90c`), the workspace (`atomic-wl9`), the toolchain pin (`atomic-a5c`)
and CI (`atomic-4yj`). The toolchain must match the one Codex pins at the
chosen rev.

When those land, replace this section with the real commands. Bead
`atomic-dxw` tracks that; do not document a command here that you have not
run.

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

## Git

- Commit only when asked. Never push, and never run `bd dolt push`, unless
  asked.
- Commits carry the repository owner's git identity and nothing else: no AI
  author, no `Co-Authored-By` trailer, no "generated with" line.
- The bead database is not in git. Only bd's configuration and hooks under
  `.beads/` are tracked.

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
