# CLI reference

This is a sketch of the proposed CLI, not a command list verified against a
released executable. [Availability](availability.md) identifies the planned stages.
Angle brackets below mean a value you supply, not literal shell syntax.

## Start with the command for your task

| Command | Purpose | Planned stage |
| --- | --- | --- |
| `atomic` | Open the interactive coding client; attach to an existing runtime or start locally | v0.5 interactive CLI |
| `atomic init` | Create a manifest and starter definitions | v0.1, built |
| `atomic run <name-or-file> --input <file.json>` | Run an atom or workflow with typed input, by its name or by the path to its file | v0.1 |
| `atomic curl <name> -d '<json>'` | Send JSON to an atom or workflow on a running runtime and print the typed result | v0.5 |
| `atomic check` | Validate project definitions and references | v0.1 |
| `atomic eval [<atom>…]` | Evaluate atoms against their cases, from cassettes unless `--live` or `--no-cassettes`; exits non-zero below an atom's threshold. `--case`, `--sample`/`--seed`, `--junit`, `--report`, `--revision`, `--allow-unscored`. See [eval cases](eval-cases.md#cases-golden-fixtures-and-recordings) | v0.1 |
| `atomic config --explain` | Inspect resolved values, sources and detected defaults | v0.1, built |
| `atomic runs show <run-id>` | Inspect saved results, metadata and run state | v0.1 |
| `atomic runs resume <run-id>` | Continue the same unfinished local run in the foreground | v0.1 |
| `atomic serve` | Run the local headless runtime and recover eligible unfinished runs | v0.1 local; team service v0.5 |

The CLI discovers a project from `atomic.yaml` or `.atomic/atomic.yaml`. For
interactive coding, the target entry experience also works in an ordinary
directory without a project manifest.

`atomic run` returns a typed result when successful and provides a run identity
for inspection. `--run-id` names the new run: 1 to 128 characters, each an
ASCII letter, a digit, `.`, `_`, `:` or `-`, starting with a letter or a digit,
since atomic's own lines show a run's id as it is. Any other id is refused
before anything is recorded, and the refusal does not repeat it. An id that is
already in the store is refused, since that run continues only with
`atomic runs resume`. Local workflow gates can wait for your decision in the terminal.
Use `atomic runs resume <run-id>` after an interruption, from the same project
and state store. It uses saved inputs and definitions; there is no replacement
`--input`. If another local runtime owns the project, it reports that owner
and does not start a competing executor. See [recovery](../concepts/durable-execution.md#resume-existing-work)
for gate deadlines, budgets and recovery limits.

`atomic runs show <run-id>` prints what the store recorded about a run: its
workflow, status, input and output; each attempt, which is each time the run
executed, from its start or from its checkpoints; each recorded step with the
attempt that ran it, so after a kill and a resume it shows which steps the
resume found checkpointed and which it ran again; its model calls and tokens,
counting what a Codex turn still running, or stopped before it settled, last
recorded having reported; its gates; and what each published turn recorded,
the step's accepted candidate, whether its author or a repair round made it,
and each of its reviewer's verdicts: the Codex thread that holds a Codex atom's
transcript, or the provider's id for a lite atom's last response, since a lite
atom keeps no transcript. A turn whose atom's kind the
run did not pin, as a workflow written in Rust pins none, is shown only as a
recorded turn. `--json` gives the same as one document, with each step's output
and each recorded turn's atom kind. It reads the store alone, and runs
nothing: it needs no model, does not take the project's execution lock, and
answers while a run holds it.

## Choose what to run and what to send

Inside a project directory, name the atom or workflow:

```sh
atomic run classify-note --input note.json
atomic run classify-note --data '{"text": "Export crashes on large files."}'
cat note.json | atomic run classify-note --input -
```

`--input` takes a file, or `-` for standard input. `--data` takes the JSON
itself, which is the quickest way to try an atom.

From anywhere else, point at the atom's file, or name the project:

```sh
atomic run ~/work/note-tools/atoms/classify-note.atom.astro --data '{"text": "…"}'
atomic --project ~/work/note-tools run classify-note --input note.json
atomic --project ~/work/note-tools/atomic.yaml run classify-note --input note.json
```

A path to an `.atom.astro` or `.workflow.yaml` file runs that
definition; Atomic finds its project by walking up from the file. `--project`
takes the manifest's directory or the `atomic.yaml` file itself.

## Send JSON to a running runtime

`atomic run` executes in the process you started. `atomic curl` instead sends
the input to a runtime that is already serving, over the same HTTP API an
application would call. It is planned for v0.5, with that API.

```sh
atomic curl classify-note -d '{"text": "Export crashes on large files."}'
atomic curl classify-note -d @note.json -i
atomic curl classify-note -d @note.json --print
```

| Option | Meaning |
| --- | --- |
| `-d <json>`, `-d @<file>`, `-d @-` | The input: inline, from a file or from standard input, as in curl |
| `-i` | Also show the run ID and status |
| `--print` | Do not send. Print the equivalent plain `curl` command, with the token masked |
| `--remote <url>` | Send to that runtime and not the local `atomic serve` |

The command finds a local `atomic serve` by itself and handles the address and
the token for you, which is what makes it shorter than curl. `--print` is the
bridge to your own code: it shows the request to copy. The result is the
atom's typed output on standard output. If a workflow stops at a gate, the
command reports the run ID and what the run is waiting for. With no runtime
serving, it fails and suggests `atomic run`.

## Coding fixture selection

`--project` selects a project explicitly, by its manifest directory or
manifest file. A workflow that changes code, one with a `worktree: true` step,
also needs its repository named, with the pair `--fixture <repo>` and
`--fixture-rev <revision>`:

```sh
atomic --project /path/to/project run change-request --input request.json \
  --fixture ../seed-repository --fixture-rev main
```

`--fixture` is resolved from the directory you run the command in, so the same
command from an unrelated directory binds the same repository and commit.
When the run is admitted, atomic resolves the revision to its commit, copies
the objects that commit reaches into a snapshot in the project's state
directory, and runs every worktree step in a worktree of that snapshot. The
repository you named and your own checkout are never written to, and a branch
that moves later does not change an admitted run. Nothing is fetched, and
nothing the repository's own configuration names, its hooks, file system
monitor, filter drivers or remotes, ever runs.

`--fixture` names a repository's top, or a bare repository, never a directory
inside one. A revision such as `HEAD` is that checkout's own, so a linked
worktree binds at its own `HEAD`. The atom works at the top of its worktree of
the snapshot, wherever the project itself is. Each worktree step starts from
the bound commit or from a commit an earlier step of the same run made; a
step whose base is anything else, such as a commit another run made on the
same fixture, fails before its turn. Commit fields in the input name commits
of the fixture's history, and one whose schema allows `null` may be `null`.

Before anything runs, atomic refuses, with the kind of failure in `--json`:

| Kind | When |
| --- | --- |
| `fixture.required` | The workflow changes code and no fixture is named. When the project's own directory offers no worktree, the refusal says what was found about it: that git refused it — which it says alike of a directory holding no repository and of a repository it cannot read — or that git never got to answer for it |
| `fixture.path` | git finds no repository at `--fixture`, or it names a directory inside a repository rather than that repository's top |
| `fixture.unreadable` | Nothing was learned about `--fixture`, because git never got to answer for it: it could not be run on that directory, was stopped, or printed what atomic cannot read. The description carries what stopped git, and atomic does not guess whether a repository is there |
| `fixture.revision` | `--fixture-rev` is not a commit, branch or tag of it |
| `fixture.unsupported` | Its commit has submodules or Git LFS content; the repository borrows objects from another (alternates), is shallow or has grafts, is missing objects its commit reaches (as a partial clone may be), or names its objects with SHA-256 |
| `fixture.schema` | The workflow's input types a commit where atomic does not check it, such as under `oneOf` |
| `fixture.input` | An input field typed `format: git-commit` is not in the fixture's history at the revision, or its `base` is not the bound commit |
| `fixture.snapshot` | The snapshot could not be made, or something atomic did not make is where it goes; atomic never removes it |

Lite atoms and workflows that change no code need neither flag. `atomic eval`
takes the same pair when it is built.

## Model access

| Command | Purpose | Planned stage |
| --- | --- | --- |
| `atomic model login` | Sign in with ChatGPT through Codex's login, for Codex atoms; `--device-auth` on a machine with no browser | v0.1 local, Linux |
| `atomic model status` | Say who is signed in (the email masked), on which plan and where it is kept, never a token; creates nothing; `--json` for scripts | v0.1 local, Linux |
| `atomic model logout` | Revoke the sign-in where the issuer allows, and forget it in both the keyring and the file store, saying what it found | v0.1 local, Linux |

With no model endpoint configured, Codex atoms use the sign-in and
`ATOMIC_MODEL`, ahead of a detected `OPENAI_API_KEY` or Ollama.
Lite atoms need an endpoint. See [subscriptions and
authentication](../getting-started/authentication.md).

## Gates and supervision

| Command or surface | Purpose | Planned stage |
| --- | --- | --- |
| `atomic gates enroll` | Enroll your operator credential for the project, from a terminal; `--replace` rotates it or replaces a lost one | v0.1 local, Linux |
| `atomic gates list` | Inspect durable gates; on a shared store, only the project's runs' gates | v0.1 local |
| `atomic gates approve <gate-id>` | Approve a recorded proposal as the local operator | v0.1 local |
| `atomic gates reject <gate-id>` | Reject a recorded proposal as the local operator | v0.1 local |
| `atomic gates answer <gate-id> <answer>` | Answer a question gate as the local operator | v0.1 local |
| The prompt in `atomic run` and `atomic runs resume` | Decide or answer each gate the run reaches, at the terminal | v0.1 local |
| `atomic parks list` | List the project's runs parked on a budget cap and not yet released; on a shared store, no other project's | v0.1 local |
| `atomic parks release <park-id>` | Release a parked run as the local operator, with your enrolled credential; `--tokens`, `--minutes` and `--cost-usd` raise the refusing allowance first (`--scope` names another of the run's own), `--comment` is recorded, `--command-id` makes a repeat safe | v0.1 local, Linux |
| `atomic workflows show <name>` | Inspect a workflow definition and its steps | v0.5 |
| `atomic graph` | Inspect work items, status and blocking dependencies | v0.5 |
| `atomic graph import <file>` | Import beads JSONL into the local SQLite work graph; `-` reads stdin, `--dry-run` reports without writing, and a repeat leaves existing items unchanged | v0.2, SQLite |
| `atomic graph export` | Write the local work graph as beads JSONL on stdout, one item per line | v0.2, SQLite |
| `atomic graph ready [--limit N]` | List committed items ready to claim without changing the store; global `--json` returns item objects | v0.2, SQLite |
| `atomic graph show <id>` | Inspect one committed item, including imported acceptance criteria, notes, blockers and outgoing edges; global `--json` returns an object | v0.2, SQLite |
| `atomic graph search <phrase-or-id> [--status <status>] [--limit <n>]` | Search committed titles, descriptions and released handoffs, or resolve a punctuated exact ID; supports global `--json` | v0.2, SQLite |
| `atomic graph search-rebuild` | Rebuild the local FTS5 indexes from committed items and handoffs | v0.2, SQLite |
| `atomic graph pin <item>` | Pin an imported open epic with the enrolled operator credential so a graph run can work its children; repeating the command is safe | v0.2, SQLite |
| `atomic graph reconcile-failed <epic> --run <run>` | Release the named terminal failed graph run's epic binding after checking that it holds no item lease; requires the enrolled operator credential | v0.2, SQLite |
| `atomic graph policy set <item> --require commit` | Set an item-specific close rule; `--require verified-commit --branch refs/heads/<name>` binds host-verified evidence to a branch; `policy show` and `policy clear` inspect and remove the rule | v0.2, SQLite |
| `atomic graph criteria set <item> --file <path>` | Replace the item's structured criteria from a JSON list; `--file -` reads stdin, and `criteria show` and `criteria clear` inspect and remove them. Writes require the enrolled operator credential | v0.2, SQLite |
| `atomic graph verify-commit <item> --run <run> --step <step> --attempt <n>` | Check a recorded worktree attempt against the item's authorized branch and store verified commit evidence | v0.2, SQLite |
| `atomic delivery start <source-run> --step <graph-step>` | Open a human gate for a committed coding item with two clean independent reviews, self-review, selected controls and a passing trial merge; the protected project delivery adapter supplies the exact remote target | v0.2, SQLite |
| `atomic delivery resume <delivery-run>` | Continue approved push, pull request and merge queue effects, reconciling an unknown remote outcome before any further action | v0.2, SQLite |
| `atomic delivery observe <delivery-run>` | Read the protected provider's queue state after enqueue and durably record pending, merged, dropped or failed observation for the approved PR head; rerun to refresh a nonterminal result, or let `atomic serve` poll due observations with bounded backoff | v0.2, SQLite |

| Live thread controls | Answer, steer, stop and take over work | v0.5 CLI |
| Workflow and graph inspection commands with `--json` | Use the supervision surface from scripts; import/export already support `--json` | v0.5 |
| Dedicated workflow/graph/gate panes | Navigate larger sets of work in the terminal | v1.0 TUI extensions |

The protected adapter and target file used by the delivery commands are
described in [Delivery adapter](delivery-adapter.md).

A gate decision carries your operator credential, a secret atomic keeps in the
OS keyring (the Secret Service on Linux) and checks against the gate store.
Run `atomic gates enroll` once per project. After upgrading a PostgreSQL
store with username-only enrollments, rerun that command in each project to
register its existing secret under the project. Rotation then applies only
to that project. The local deployment identifies its single operator by
account name within the project. Separate people using the same account name
and project share that role. Gate creation and approval both use `USER`, then
`LOGNAME`. With neither set, Unix services use the effective UID's account
name, or `uid-<number>` if the UID has no account name. For a gate created
by an older service under `local:operator`, enroll and decide with
`USER=operator`. On a
machine with no OS keyring, such as a headless server or a container, atomic
refuses with an error rather than store the secret elsewhere; to keep it in a
directory readable by you only (`$ATOMIC_HOME/credentials`, or
`~/.config/atomic/credentials`), opt in with `ATOMIC_CREDENTIAL_STORE=file`.
Credentialed decisions are available on Linux only for now (see
[availability](availability.md)).

Live-thread cancel, pause and take-over command spellings and keyboard shortcuts
remain unspecified. Run recovery uses the v0.1 command above. The
[control guide](../guides/controlling-agents.md) describes live controls without inventing flags.

## Develop reusable tasks

| Command | Purpose | Planned stage |
| --- | --- | --- |
| `atomic dev` | Watch definitions and run affected checks/evals | v0.5 |
| `atomic dev --otel` | Start the optional local observability setup with Jaeger or Langfuse | v0.5 / M44 |
| `atomic eval <atom> --live` | Evaluate with live calls, metered under the atom's budget, and record each answer in the atom's cassettes. A Codex atom is the exception: cassettes do not stand in for its endpoint yet, so `atomic eval` runs it only with `--no-cassettes`, which records nothing. See [eval cases](eval-cases.md#cases-golden-fixtures-and-recordings) | v0.1 |
| `atomic eval <atom> --compare <revision>` | Compare behavior, cost and output across definitions | v0.5 |
| `atomic render <file.atom.astro> --input <file.json>` | Print developer/user prompt parts without a model; optional `--context-settings`, `--profile`, `--run-id`, `--step-id` | Implemented direct invocation |
| `atomic eval <atom> --plan`, `atomic run <workflow> --plan` | Print the expected model calls and a cost range, then exit without calling a model | v0.5 |
| `atomic run <workflow> --effects preview` | Run a workflow in staged mode: external effects are recorded and shown, none is applied | v0.5 |
| `atomic schemas expand <atom>` | Print an atom's input and output contracts as atomic enforces them, compact forms expanded | Implemented |
| `atomic runs replay <run-id> [--from <step>]` | Inspect recorded execution from a chosen point | v0.5 |
| `atomic schemas` | Write JSON Schemas for workflow files and the manifest into the generated directory, with the project's atom names, their input fields and its schema IDs, for editors; see [VS Code](../guides/vs-code.md#workflow-and-manifest-files) | Implemented |
| `atomic types` | Generate editor types without a full check (`atomic check` writes them today) | v0.5 |
| `atomic setup claude`, `atomic setup codex` | Install the `atomic-create-atom` authoring skill for Claude Code or Codex; see [coding agents](../guides/coding-agents.md) | Implemented; more agents and skills later |

Read [testing and evals](../guides/testing-and-evals.md) before treating recording
replay as a live model result.

## Review and maintain a project

| Command | Purpose | Planned stage |
| --- | --- | --- |
| `atomic check --write-lock` | Write `atomic.policy.lock`, the resolved permissions and limits of every atom and workflow | v1.0 |
| `atomic check --fix` | Planned Astro-aware escaping assistance; literal syntax currently uses entities | v0.5 |
| `atomic stale [--as-of <date>]` | List [recorded decisions](manifest.md#recorded-decisions) that need another look: the review date has passed, or the evidence is too old, was produced on a different model or prompt version, or no longer holds | v0.7 |
| `atomic runtimes install [--from <archive>]` | Fetch and verify the Deno version this Atomic release pins, ahead of time; `--from` installs from a file on a machine with no network | v0.7 |
| `atomic runtimes lock` | Write `deno.lock` from the packages that atom code and scripts import | v0.7 |
| `atomic test` | Run the project's code tests on the managed Deno, with network access limited to the local machine | v0.7 |
| `atomic deno <args>` | Run the managed Deno directly, for a project's own tests or to update `deno.lock` | v0.7 |
| `atomic import`, `atomic export` | Convert agent and prompt definitions from other tools into atoms; write a lite atom as a Dotprompt or Prompty file | v1.0 |

The [policy lock](../guides/security-and-permissions.md#review-permission-changes-as-a-diff)
makes a change to permissions visible in review. `atomic stale` takes its date
as an argument and is meant to run on a schedule; `atomic check` never reads
the clock. A local run downloads the managed Deno by itself the first time a
project needs it, so `atomic runtimes install` matters for CI, images and
offline machines; see
[you do not install Deno](../guides/atom-code-runtime.md).

## Diagrams

| Command | Purpose | Planned stage |
| --- | --- | --- |
| `atomic workflows show <name> --format mermaid` | Print a workflow's steps, gates and dependencies as a Mermaid diagram | v0.5 |
| `atomic graph --format mermaid` | Print work items and their blocking dependencies, with each item's status | v0.5 |
| `atomic runs show <run-id> --format mermaid` | Print a run's steps with their current state | v0.5 |
| `--view risk`, with `workflows show` or `runs show` | Show what each step is allowed to do: sandbox, network access, tools, effect caps and the approvers of each gate | v0.5 |
| `--view cost`, with `workflows show` or `runs show` | Show each step's budget and, for a run, the tokens and spend so far | v0.5 |
| `atomic docs --write` | Refresh generated diagrams that are committed in Markdown files | v0.5 |

```sh
atomic workflows show summarize-and-review --format mermaid > review-flow.mmd
```

A view changes what each node shows, never the source, and no view depends on
color. `atomic graph` has one view. The output is text, so it can be piped, committed or pasted into a pull request.
None of this is built yet, and neither is the guard that would keep a committed
diagram current: `atomic check` checks the project's atoms, workflows, toolset
operations and configuration, and regenerates the editor declarations; it never
compares a diagram committed in a doc with its source. Until the exporters
arrive, keeping a committed diagram current is yours to check. See
[diagrams with Mermaid](../guides/diagrams.md) for examples of each output.

## Serve and attach

```sh
atomic serve
atomic --remote <url>
```

`atomic serve` runs a local headless runtime in the foreground in **v0.1**. Keep
it running under a supervisor for progress after your terminal exits. It recovers
eligible unfinished runs in the selected project store; approve or reject gates
from a second terminal. It is not a detach command.

`atomic run`, `atomic runs resume` and `atomic serve` each hold the project's
execution lock while they run. A second one is refused with the process id and
command of the one that holds it, so two processes never execute the same
project's runs. The lock is released only when its process exits.

`atomic serve`, `atomic run` and `atomic runs resume` stop on SIGTERM, SIGINT,
SIGHUP or SIGQUIT, except one they were started ignoring, as under `nohup` or
as a shell's background job, which they keep ignoring. No step starts after the first signal, and running steps get
up to 10 seconds to reach a checkpoint; a second signal ends that wait at
once. A command still running then is killed, with everything it
started, also under `--runner process`, and the process exits without recording
how the command ended. On Linux it exits, freeing the project's execution lock,
only once every process it started has ended, one in another process group or
session, such as a sandboxed command, included; atomic's own git is let finish,
within its ten-minute time limit, rather than killed. A step whose atom ends
while atomic stops records nothing, whatever ended it, a killed command, a
model turn's or a compiler's included, so it stays unfinished too. It names any process still running after ten seconds, and
again every thirty, and goes on waiting: only SIGKILL ends that wait, as it
ends atomic at once. The same holds however the command ends, by an error
included, though not when atomic itself panics. A process that left atomic's
descendants, as one that daemonizes outside its sandbox's process namespace
does, is not found. Elsewhere than Linux, atomic kills each command's process
group and exits without waiting, so a process that left the group may outlive
it and its lock. Then it exits: `atomic serve` with status 1, `atomic run` and
`atomic runs resume` with status 130, leaving the run for `atomic runs resume`. Unfinished work stays recorded for the
next `serve` or `runs resume`, and an interrupted command runs again only if it
declares `effects.external` as `read-only` or `idempotent`; any other is parked
as `outcome_unknown`. A step waiting for room on the execution backend gives up
its wait at the first signal. Besides 130, `atomic run` and `atomic runs resume`
stop with 75 when a step found no room within `--backend-wait`, on the
execution backend or on the disk for atomic's own writes (its run's build caches
and its worktree's directory), and with 78 when
the sandbox cannot start; either way the run stays unfinished, with nothing
recorded, for `atomic runs resume`. Each command runs in a process group of its own (under
`--runner process`, its runner's), and on Linux it is killed if atomic dies.
Whatever a command leaves running in its group when it ends is stopped then
(under `--runner process`, when its runner ends). Output that arrives more than
two seconds after a command exits is not its output, so a process that left
its group, such as one started with `setsid`, cannot hold the step open,
though atomic stops it only when it stops itself. A runner
ignores the stop signals a command in its group might send it.

One process recovers a run at most five times at the same step before it fails
it as dead-lettered (a run that gets further counts afresh), so a step whose process keeps dying stops soon; restarting
`atomic serve` does not count against a run. The engine parks a run
(`MAX_RECOVERY_ATTEMPTS_EXCEEDED`) when it stops without finishing more times than
the engine allows, and at once when atomic's own workflow code panics: a panic is
a bug in atomic, and would likely happen again. `atomic run`, `atomic runs resume`
and `atomic serve` (when it happens while serving, and when it starts) name the
cause, with the panic's message redacted; the message is kept in the project's
private state. A parked run goes on with `atomic runs resume`, once the cause is
fixed.

A resumed run goes on a queue of the resuming executor's own: only
`atomic runs resume` and `atomic serve` of that executor run it, and
`atomic run` only ever runs the run it started.

Each run is stamped with the layout of the durable steps atomic records, not
with the build that started it, so a rebuilt or upgraded atomic with the same
layout resumes and recovers it. A run of another layout is refused by
`atomic runs resume` and listed by `atomic serve`, which leaves it as it is.

On a PostgreSQL store several projects share, `atomic serve` lists, takes
and recovers only its own project's runs. Give each project's processes an
executor identity of their own: when an identity, set in user configuration
for example, owns another project's unfinished run that recovery would
take, `atomic serve` refuses to start rather than run that project's run in
this one's code, and says how many such runs there are without naming them.
Atomic refuses to start when the durable engine's own version variable
(`DBOS__APPVERSION`) is set.

| Setting | Meaning |
| --- | --- |
| `ATOMIC_PROFILE` (`--profile` on config, check, run, eval, serve and runs resume) | Select `atomic.<profile>.yaml` beside the resolved project or workspace member's manifest; the flag wins. A missing file is refused. On resume, this selects current process settings and does not replace a finished step's recorded context. See [configuration](configuration.md#the-layers) |
| `ATOMIC_MODEL_BASE_URL`, `ATOMIC_MODEL` (`--model-base-url`, `--model`) | The OpenAI-compatible endpoint and model that model atoms use; `ATOMIC_MODEL_API_KEY_ENV` names the variable holding its key |
| `ATOMIC_DATABASE_URL`, `ATOMIC_DATABASE_SCHEMA` | PostgreSQL instead of the project's local SQLite store; the schema defaults to `atomic` |
| `--owner-stopped` (`runs resume` only) | On PostgreSQL, take over a run another executor owns, asserting that executor has stopped; without it such a resume is refused |
| `ATOMIC_EXECUTOR_ID` (`--executor-id`) | This process's identity as owner of its runs: unique to the process and the same across its restarts, so a restart recovers its own unfinished runs and no one else's. Required on PostgreSQL |
| `ATOMIC_RUNNER` (`--runner`) | `local` (the default) runs exec atoms' commands as child processes; `process` runs each in an `atomic runner` process of its own, which sees only `PATH` from its parent |
| `backend_wait`, `ATOMIC_BACKEND_WAIT` (`--backend-wait`) | How many seconds, a whole number of at least 1, a step waits for room: on the execution backend for its workspace, or on the disk for atomic's own writes (its run's build caches and its worktree's directory), each wait with its own clock; a checkout git cannot finish fails the step instead. Then `atomic run` and `atomic runs resume` stop, the run unfinished with nothing recorded and exit code 75, for `atomic runs resume`; `atomic serve` sets the step aside, says so, and keeps trying at most 30 seconds apart, saying so each time, and the run goes on once there is room. `atomic eval` waits no longer for each case. Without it a step waits for as long as it takes |

`atomic --remote <url>` and shared/team service access arrive in **v0.5**, with
identity and transport controls.

`atomic runner` is the same binary in its other role: it reads one command
request on standard input, runs it, writes the result on standard output and
exits with a code from a fixed set, separated by what the control plane does
about it. It creates no state directory.

| Exit code | Meaning | Control plane |
| --- | --- | --- |
| 0 | Ran the request | Reads the result |
| 64 | Usage error | Fails the attempt, no retry |
| 78 | Configuration or credential error | Fails the attempt, no retry until fixed |
| 75 | Workspace lease held elsewhere | Waits, then tries again a bounded number of times |
| Any other, or a signal | Crash | Never runs the command again: it may have run, so the step's attempt is interrupted, and only a command declared `read-only` or `idempotent` runs again; any other is parked as `outcome_unknown` |

Its stderr is attached to a failure as it is, never parsed. Runners reached over
the network and other execution backends are planned for the platform stage.

See [local and team operation](../operations/running-atomic.md) for roles and storage.

`atomic render` without `--run-id` is an ephemeral preview: each invocation uses
current atom sources, settings and declared environment values. Supply
`--context-settings` for each contextual preview. With an explicit `--run-id`,
Atomic checkpoints public context and restores it on subsequent invocations;
changing the compiled atom requires a new run identity. Checkpoints carry a
keyed authentication tag, checked using a private key in the host state directory.
Unsigned checkpoints from earlier implementations are refused; use a new run id.
The key and checkpoints must remain outside model-accessible paths. Their public
SHA-256 digest alone is a consistency check, not proof of host authorization.
