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

The initial authoring-skills pack is targeted at v0.1; the rest of the pack and
higher-level project generation follow later. Workspace member discovery and
shared-schema resolution belong to M11b/M39b in v0.5.

Declarative atoms now use Astro exclusively. The production loader supports local
components, slots, bounded TypeScript-shaped expressions and typed validators.
`atomic check` checks source and generates editor types; `atomic render` previews
prompts without a model. Markdown and MDX atom frontends have been removed.
The Astro compiler and full JSON Schema gates execute in this repository; broader
workflow scheduling, interactive commands and integrations retain their staged status.

Several authoring aids arrive in stages. Eval cases in JSONL files are part of
v0.1. Compact schemas, variants, the `--plan` cost preview, declared effect caps
and the staged `--effects preview` mode are planned for v0.5. The `file` tag
follows with code context in v0.7. The committed `atomic.policy.lock`, and
`atomic import` and `atomic export`, are planned for v1.0.

Mermaid diagrams written by hand in docs need nothing from Atomic. The
[diagram exporters](../guides/diagrams.md) for workflows, the work graph and
runs, their `--view risk` and `--view cost` views, the stale-diagram check and
`mermaid` fences in atom instructions are planned for v0.5. Rendering in the web
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

## Observability stages

The local baseline is saved run inspection and basic spans in v0.1 (M12).
Full OpenTelemetry correlation/export and the optional `atomic dev --otel` local
viewer helper belong to M44, assigned to v0.5 in the plan and these docs.
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
