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

On Linux, atomic has verified that an atom's sandbox cannot reach the gate store
or the credential: the private part of the state directory, Codex's own records
and the credential and key directories are denied to its commands, which also
cannot use the kernel keyrings or connect to a socket. On macOS and Windows this
has not been verified, so atomic keeps no operator credential there and refuses
credentialed gate decisions.

A `workspace-write` atom cannot write `/tmp`, `/var/tmp` or `$TMPDIR`: each turn
gets a temporary directory of its own, named by `TMPDIR` and removed when the
turn ends (a killed atomic's are removed at its next start). A tool that ignores
`TMPDIR` and writes to `/tmp` directly, such as a JVM's default
`java.io.tmpdir`, fails there; point it at `$TMPDIR` (for the JVM,
`-Djava.io.tmpdir=$TMPDIR`).

Several authoring aids arrive in stages. Eval cases, in YAML and in JSONL files,
are loaded and checked, and the eval runner reports pass rates against each
atom's threshold and scorer means against their minimums and a baseline; the
`atomic eval` command that runs it from the terminal is not built yet. A
recorded workflow-file run can be replayed from its checkpoints as a new run:
each atom step that finished is served its recorded result, with no model call
and no command run, while gates open and wait again; the `atomic runs replay`
command that starts one is not built yet. Compact schemas, variants, the `--plan` cost preview, declared effect caps
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
reaches a cap. A released park admits the call again against the caps as stored;
the command a person uses to raise a cap and release the park is not built yet.
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
