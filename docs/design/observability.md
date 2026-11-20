# Observability

atomic emits OpenTelemetry once, following the GenAI semantic
conventions and adding names of its own under `atomic.*`. Redacting, sampling
and routing that telemetry are the job of an OpenTelemetry Collector placed
after it. The difficult cases are the ones agents create: runs that span days,
recovery that replays code and must not count work twice, context that has to
cross many boundaries, and prompts that may hold sensitive text. How a user
turns this on, phase by phase, is in
[local observability and telemetry](../operations/observability.md).

## Building on Codex and Substrate

- **Codex** ships OTLP exporters for logs, traces and metrics. It already tags
  model calls with GenAI token-usage attributes, publishes many `codex.*`
  metrics (turns, hooks, sub-agents), and lets an administrator fix where
  telemetry goes through managed requirements. atomic adds its workflow and step
  context to that stream through one shared integration rather than a second
  pipeline.
- **Agent Substrate** already publishes telemetry about its own workers and
  actors, lifecycle events included. atomic consumes those instead of measuring
  the same things again.

## Conventions

| In atomic | Span or attribute |
| --- | --- |
| A workflow run | `invoke_workflow`, with the workflow ID repeated as `gen_ai.conversation.id` |
| An atom run | `invoke_agent`; `gen_ai.agent.name` holds the atom name |
| A model call | `chat` (emitted by Codex) carrying `gen_ai.usage.*` |
| A tool call or an OpenAPI operation | `execute_tool` carrying `gen_ai.tool.name` |
| MCP traffic | Named as the MCP semantic conventions specify |
| Facts only atomic knows | `atomic.workflow.id`, `atomic.step`, `atomic.attempt`, `atomic.gate`, `atomic.trust`, `atomic.taint`, `atomic.cost_usd`, `atomic.replay` |

Because those GenAI names have not been declared stable, atomic fixes one
version and confines the mapping to a single module, so a change in the
conventions is a change in one place. While names move, the Collector can
rewrite the old ones.

## Traces for long-running, replayed work

- **A trace per attempt, not per run.** If one root span covered a workflow that
  sits at a gate for a day, it would never be exported, and many backends would
  reject it. Instead, each attempt at a step is a trace of its own. Every span
  carries the workflow ID, and links connect each trace to the one that started
  the workflow. When a gate opens, atomic emits an event and updates an
  open-gates metric straight away; the span for the wait is written when the
  gate closes, using the times stored durably.
- **Replays add nothing.** Recovery runs workflow code again from the top. Steps
  whose results come out of checkpoints produce no spans; only work that really
  executes does. Spans from a recovered run carry `atomic.replay`. Token and cost
  totals therefore stay accurate.
- **Trace context is checkpointed.** A step's saved checkpoint includes its
  trace context, so after a restart the resumed step joins the trace it began in.

## Propagation across every boundary

The W3C `traceparent` header travels with every hop:

- from the CLI and the TUI to the control plane;
- from the control plane into Codex turns;
- outward to runners, whether exec-server or Substrate's router carries the call;
- through MCP calls and A2A tasks;
- on requests the OpenAPI executor sends out, and on webhooks and triggers that
  come in;
- through approval channels, so the trace of a gate decision points to the trace
  that opened the gate.

An application that passes a trace header when it starts a run sees a single
trace from its own request to the run's result.

## Metrics

| Area | What is counted |
| --- | --- |
| Workflows | Runs that started, finished, failed or parked, per workflow; how long steps take; how long work waits in a queue |
| People | Time spent waiting at gates; how quickly approvals come; how often people steer or take over |
| Quality | How often schemas need repair; how often validation fails; each atom's eval pass rate |
| Cost and models | Tokens and spend broken down by tier, provider, project and team; fallbacks taken; throttling by quotas |
| Graph and sync | Contention for claims; how far sync and message cursors lag |
| Runners | How busy runners are; resume latency, taken from Substrate's metrics |

Run and workflow IDs are never used as metric labels, since that would make the
number of series unbounded. A metric point can instead carry an exemplar that
links to one representative trace.

## Content, evals and sampling

- **Prompt content is off unless enabled.** Prompts, completions and tool
  arguments are not captured by default. A project or a single atom can turn
  capture on; the content is then redacted where it is produced (a basic pass in
  M44, full data-loss prevention in M30) and sent only to a backend meant for
  agent quality, never to general operations tooling. That redaction is
  deliberately aggressive, which is why it is applied to telemetry only and never
  to the text a model works on, where it would damage code.
- **Quality signals travel as telemetry.** atomic records each eval result, gate
  decision, steering message and take-over as an event on a span. Spotting a
  quality drop after a model change becomes a query, and the [learning
  loop](developer-experience.md#learning-from-human-decisions) finds the cases
  it needs in the same data.
- **The Collector samples by outcome.** It keeps everything that errored, every
  rejected gate, budget breach, schema repair and unusually slow step, and
  samples ordinary runs. Metrics are not sampled at all.

## Platform pairings

Because the output is plain OTLP, any compatible backend works. Common choices,
grouped by purpose:

| Purpose | Compatible backends | Notes |
| --- | --- | --- |
| Operations alongside an APM already in use | Datadog, Dynatrace, Splunk, Grafana Cloud, Honeycomb | How well each ingests the GenAI conventions depends on the backend and its version |
| Operations, self-hosted or offline | The Grafana stack (Tempo, Loki, Prometheus or Mimir), SigNoz | Can share a cluster with the runners so data never leaves it |
| Agent quality: traces with content, evals, datasets | Self-hosted Langfuse | Accepts OTLP; treat the workflow ID as its session |
| Agent quality using OpenInference | Arize Phoenix | Convert in the Collector instead of emitting two formats |
| Audit | The organization's SIEM | Fed separately, from the audit log (M28) |

Over the phases: the first release relies on durable local run records read
through `atomic runs show`, plus basic spans (M12). v0.5 adds full correlation
and export, and `atomic dev --otel` can start a local Jaeger or Langfuse (M44).
Larger deployments send operations data to their existing APM, keep content in a
self-hosted quality backend where it must stay in region, and feed audit to a
SIEM. See [availability](../reference/availability.md#observability-stages).
