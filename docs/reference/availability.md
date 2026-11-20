# Availability and delivery stages

These docs sketch Atomic's intended product experience. As of this draft, this
repository contains plans and example definitions, not a released Atomic runtime.
The CLI snippets are proposed product behavior and have not been executed against
an Atomic binary.

The first-release path and the full interactive CLI arrive at different stages.
The documentation leads with the complete product picture so readers can see why
the coding agent and framework belong together.

| Stage | Target experience |
| --- | --- |
| **v0.1 · one developer** | `init`, `run`, `runs resume`, local headless `serve`, `check`, evals and run inspection; file-defined Codex/lite/exec atoms; sequential durable workflows; local gates; local artifacts, budgets and basic spans |
| **v0.5 · a team** | Full interactive CLI attachment, live agent supervision, work graph and messages, Postgres, initial identity/audit controls, full OpenTelemetry integration and optional local trace helper, Slack approvals, OpenAPI tools, triggers, parallel composition and merge queue |
| **v0.7 · models and data** | Planner and code context, broader model routing, TypeScript in atoms on Deno, application data sources, typed files, retrieval, multimodal work and outcome-based learning |
| **v1.0 · platform** | Remote workers/environments, Kubernetes/Substrate execution, extended TUI panes, TypeScript workflows, published MCP workflows, A2A, broader enterprise controls and web console |

Codex's native web search and MCP client already exist upstream; Atomic
configuration and policy integration remain planned; native web search does not
depend on the later RAG milestone. Slack MCP access, inbound Slack triggers and
Slack gate approvals are separate integrations, with Atomic adapters still planned.

The first authoring skill, `atomic-create-atom`, is installed today by
`atomic setup claude` and `atomic setup codex`; the rest of the pack and
higher-level project generation follow later. Workspace member discovery
and shared-schema resolution are built; sharing atoms between members belongs
to M11b/M39b in v0.5.

Declarative atoms now use Astro exclusively. The production loader supports local
components, named slots with fallback content, frontmatter locals, template
literals, bounded TypeScript-shaped expressions, typed validators and
[TypeScript helpers](atom-files.md#typescript-helpers), which run in an
embedded, capability-free isolate per call with the step's recorded clock.
`atomic check` checks source and generates editor types, including
[types from shared schema files](atom-files.md#types-from-shared-schema-files); `atomic render` previews
prompts without a model. Markdown and MDX atom frontends have been removed.
The Astro compiler and full JSON Schema gates execute in this repository; broader
workflow scheduling, interactive commands and integrations retain their staged status.

The workflow interpreter is built: a workflow file runs as one durable run, with
no rebuild, through sequential atom steps (Codex, lite and exec atoms), local
gates and `worktree: true` steps, which run a Codex atom in its own worktree at
the step's `base` and record atomic's commit in `steps.<id>.metadata.commit`.
A run keeps the definitions it started with.

`atomic run`, `atomic runs resume` and a local foreground `atomic serve` are
built. `atomic run` takes an atom or workflow by name, by file or with
`--project`, and input from a file, standard input or `--data`; a run cut short
continues with `atomic runs resume <run-id>`, or under `atomic serve`, from its
checkpoints. One process at a time executes a project's runs. Errors carry a
description and an action, separately in `--json` output. `atomic gates enroll`,
`list`, `approve` and `reject` decide gates from a second terminal; a gate
decision counts only with the credential its person enrolled. The terminal
prompt for gates in `atomic run` is not wired up yet.

`atomic serve` also runs on PostgreSQL (`ATOMIC_DATABASE_URL`) when the process
has an executor identity (`ATOMIC_EXECUTOR_ID`). `atomic runner` and
`--runner process` run exec atoms' commands in a separate process with an
exit-code contract; Codex turns and their shell still run in the control
plane's process, and a runner reached over the network is not built yet.

Every atom run goes through the execution backend: once its prompt is
rendered, which runs the atom's helpers, it opens a workspace there before any
model call or command, and closes it however the run ends, including a panic or
a cancelled run, and only once a command running in it has ended. A backend
with no room for a workspace makes the run wait, recording nothing and saying
on stderr that it waits, and the run goes on once there is room. The local backend is the only one with a Codex shell, and
it states that its workspaces are durable. A backend that states anything else,
or a value atomic does not recognize, is treated as ephemeral, and a workflow
with a `worktree: true` step is refused on it before anything runs.

A command an atom runs, whether an exec atom's or one a model runs in a Codex
turn, gets the platform's core variables and what the atom declares, never
atomic's own environment or its credentials. A turn's commands run with no
login shell; their `HOME` (and `ZDOTDIR`) is a private scratch directory of the
attempt's own, and their `TMPDIR` is one private directory every turn of the
project shares (Codex's sandbox keeps a registry there that turns sharing a
writable root must share), so no shell startup file runs. Tools found through `HOME` are
therefore not found: rustup, cargo, pyenv, nvm, Maven's `~/.m2` and the like.
A codex or exec atom names them in its `env`, as in
`env: { RUSTUP_HOME: '/home/me/.rustup', CARGO_HOME: '/home/me/.cargo' }`. An
exec atom's command gets its own scratch directory as `HOME` and `TMPDIR`.
Scratch directories live in the project's state directory, where no sandboxed
turn may write; atomic refuses to run when that would be under `/tmp`,
`$TMPDIR` or the project. A turn's shell always runs on the machine its backend
names; `CODEX_EXEC_SERVER_URL` and an `environments.toml` in Codex's home have
no effect.

`atomic run` and `atomic serve` do not read public context settings yet, so they
refuse a step whose atom declares a context.

On Linux, atomic has verified that no atom's commands can reach the gate store
or the credential: a Codex turn's commands and an exec atom's alike run under
Codex's sandbox, which denies them the atomic home's `private/` directory, where
every project keeps its database, its execution lock and its context
checkpoints; the current project's
Codex records; the credential and key directories; the default atomic home's
data and configuration when `ATOMIC_HOME` points elsewhere; the kernel keyrings;
the network; and sockets. Other projects' Codex records are not denied yet. An
exec atom's command may write only its scratch directory and the project's
caches; `--runner process` confines it the same way. It runs in a process
namespace of its own, so it can neither see nor signal atomic, its runner or
any other process outside, and in a session of its own, with no controlling
terminal and a `/dev` and `/proc` of its own, so it cannot type into, read or
draw on the terminal atomic runs in, whether through `/dev/tty`, the
terminal's path in `/dev/pts` or another process's descriptors. A step that runs commands
needs bubblewrap and unprivileged user namespaces there: where the sandbox
cannot start, `atomic run` and `atomic runs resume` stop, saying what it needs,
before the step runs and without recording an outcome, and the run continues
when resumed where the sandbox can start. `atomic serve` instead leaves that run
unfinished, says so, and goes on serving the others; the run continues when a
later process recovers it. A sandbox that fails in a way that may pass, such as
on a full disk, makes the step wait, as a backend with no room does. Atoms that run no commands run as usual. On macOS and
Windows this has not been verified: exec atoms' commands run unconfined there,
so atomic keeps no operator credential and refuses credentialed gate decisions.

A `workspace-write` atom cannot write `/tmp`, `/var/tmp` or `$TMPDIR` as atomic
found them: its commands' `TMPDIR` is the project's private one above. A tool
that ignores `TMPDIR` and writes to `/tmp` directly, such as a JVM's default
`java.io.tmpdir`, fails there; point it at `$TMPDIR` (for the JVM,
`-Djava.io.tmpdir=$TMPDIR`).

Several authoring aids arrive in stages. Eval cases, in YAML and in JSONL files,
are loaded and checked, and the eval runner reports pass rates against each
atom's threshold and scorer means against their minimums and a baseline; the
`atomic eval` command that runs it from the terminal is not built yet. A
recorded workflow-file run can be replayed from its checkpoints as a new run:
each atom step is served the result it published, with no model call and no
command run, while gates open again, titled as a replay's, and wait. A replay
never runs an atom live: a step with no published result, or whose input differs
from the one it was recorded with, fails it, and a run recorded under another
layout, or whose results make writes, is refused. The served results do not take
over the recorded run's model threads. A step that maps a gate's `decision` cannot
be replayed past that gate: the new decision's time, decider and comment differ
from the recorded one's, so the step's input does too. The `atomic runs replay` command that
starts one is not built yet. Compact schemas, variants, the `--plan` cost preview, declared effect caps
and the staged `--effects preview` mode are planned for v0.5. The `file` tag
follows with code context in v0.7. The committed `atomic.policy.lock`, and
`atomic import` and `atomic export`, are planned for v1.0.

Mermaid diagrams written by hand in docs need nothing from Atomic. The
[diagram exporters](../guides/diagrams.md) for workflows, the work graph and
runs, their `--view risk` and `--view cost` views and the stale-diagram check are
planned for v0.5; literal `mermaid` fences in atom instructions are built. Rendering in the web
console follows at v1.0.

[Recorded decisions](manifest.md#recorded-decisions) in the manifest and the
`atomic stale` command that evaluates them are planned for v0.7.

Running an atom by the path to its file, and passing input with `--data` or on
standard input, are part of the v0.1 `atomic run`. `atomic curl` arrives with
the HTTP API in v0.5. [Code on the Deno runtime](../guides/scripts-and-cloud-sdks.md) is planned
for v0.7: separate SDK scripts, `.atom.ts` code atoms and
`exec.script`, with `env` and `runtime.deno`, the manifest's `runtime.deno`
settings, named credentials, `atomic test`, and the Deno that Atomic downloads
and manages for them. A plain
`exec.command` is v0.1.
TypeScript workflows follow in v1.0.

Budgets are partly built: the `budget` field on atoms and workflows, its
configured defaults and price table, and the durable usage ledger that reserves
each model call before it is dispatched, settles it once, and parks a run that
reaches a cap. A released park admits the call again against the caps as stored.
`atomic parks list` and `atomic parks release` let the local operator, with
their enrolled credential, raise a cap and release a park, recorded with who did
it.
The commands that show budgets and spend come with run inspection.

## Observability stages

The local baseline is saved run inspection and basic spans in v0.1 (M12).
Full OpenTelemetry correlation/export and the optional `atomic dev --otel` local
viewer helper belong to M44, assigned to v0.5.
Datadog and other compatible backends remain optional. See [observability](../operations/observability.md).

## Read examples by stage

Start with [the first atom](../getting-started/first-atom.md) and
[first workflow](../getting-started/first-workflow.md) for the v0.1 target. The
[CLI quickstart](../getting-started/quickstart.md) describes the interactive target.
Each larger project in the [example catalog](../examples/README.md) identifies
its intended stage and any unresolved integration contracts.

## What has been checked in this repository

The existing example catalog includes offline schema/fixture checks and executable
Python transforms. One example's TypeScript also runs today under Deno, against
a local stand-in for S3. None of this establishes live-model quality, command compatibility
or crash recovery in an Atomic runtime. Those need the implementation and phase
acceptance tests.

Installation packaging, standalone-session behavior and some control commands
remain open decisions, tracked with the rest of the work in the project's issue
tracker. No feature in this table should be read as already shipped.
