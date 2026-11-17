# Security and permissions

Atomic separates permission to run a task, access data and authorize a consequential
change. This page describes the planned product contracts; see
[availability](../reference/availability.md) for their stages.

## Local execution defaults

Model-backed atoms default to a read-only workspace and no shell network. A coding
atom declares `workspace-write` when it needs to edit its isolated attempt worktree.
A durable coding run does not edit the developer's checkout directly. Atomic
creates its candidate commit outside the sandbox after the turn finishes.

The **v0.5 interactive session** instead works in the directory you opened, under
session permissions. Session edits and durable worktree attempts have different
workspace lifetimes. Inspect permissions before running unfamiliar code.

`atomic config --explain` shows resolved configuration and its sources, with
secrets masked. Managed requirements constrain every override; detection of a
provider, repository or tool does not authorize it.

## Tool and network access

An omitted `tools` block grants no optional Atomic, OpenAPI or MCP tools. Codex's
built-in tools remain constrained by session/atom policy. A tool allowlist selects
from permitted capabilities; it cannot grant access that managed policy denies.

| Channel | Permission boundary |
| --- | --- |
| Shell and subprocesses | Sandbox, workspace and command-network policy |
| Model provider | Account access, model selection and data/residency policy |
| Hosted web search | Separate search mode, domains and query-data policy |
| MCP / imported HTTP tools | Service credentials, allowed operations and destination policy |
| Slack ingress and decisions | Verified sender, installation identity, deduplication and approver authorization |
| Telemetry | Approved export destination; content capture separately enabled |
| Runtime and package downloads | One pinned Deno, verified against a checksum built into Atomic; packages only from the registry, verified against the lockfile; both can be forbidden or mirrored by managed policy |

[Code that an atom carries](scripts-and-cloud-sdks.md), in a `---ts` block or a
script, runs on Deno in its own process. It reaches only the hosts in its atom's
`network.allow` and receives only the environment its atom declares, with
short-lived credentials that never reach a model. Atomic reads those
settings without running the code. Planned for v0.7.

Packages from npm get no more than that. A run uses only versions recorded in
the lockfile, runs no install scripts, loads nothing from a URL and cannot
start a process. A project can go further in its manifest, for example by
limiting which packages atom code may import; see
[security defaults and settings](atom-code-runtime.md#security-defaults-and-settings).

There is no single shell-network switch that covers all these paths. Cached
[web search](web-search.md) still sends a query to a hosted service. Subscription
login authorizes model access, not Slack or another third-party account.

### Review permission changes as a diff

An atom's effective permissions come from its own file, any base atom it
extends, project defaults and managed policy, so a source edit does not always
show what changed. The planned `atomic check --write-lock` writes
`atomic.policy.lock` beside the manifest: the resolved sandbox, network, tools,
effect caps and budgets of every atom and workflow. Commit it. `atomic check`
fails when it is stale, so widening network access or raising a cap appears as
a one-line change in review. Planned for v1.0.

To see the same authority laid out along a workflow, print its risk view:
`atomic workflows show <name> --format mermaid --view risk`. Each step is
labelled with its sandbox, network access, tools and effect caps, and each gate
with its approvers, so a reviewer can find the steps that can write or reach
out, and what stands in front of them, without opening every atom file. Planned
for v0.5; see [choose a view](diagrams.md#choose-a-view).

## Untrusted input stays data

A web page, Slack message, repository file or tool result cannot change the
workflow's permissions or appoint an approver. Instructions embedded in those
values remain untrusted input. Keep private data out of public search queries and
unapproved model/provider fallbacks.

The v0.1 tutorials use team-curated inputs. From v0.1, each value inserted into
a prompt is [wrapped and labelled with its origin](template-tags.md#insert-a-value).
Tracking an origin through atom outputs, stored text and the work graph, and
stronger external input controls, arrive in v0.5; sensitivity-based routing and broader enterprise
controls follow later. Do not treat the early local release as a production
boundary for arbitrary outside callers.

## Human authority and durable gates

A live command approval is an in-turn permission decision. A workflow gate is a
saved decision between steps, bound to the recorded proposal. Neither is a tool
that the model can use to approve itself.

In local mode an omitted `approvers` means the authenticated local operator.
Shared/team server mode requires an allowed explicit group or approved policy default; a
missing group fails definition loading. The human-only credential is unavailable
to agent tools and sandboxed shells. Rejection or expiry blocks downstream work;
resuming a run cannot override it.

Approval does not deduplicate an external mutation. Use the
[effect ledger and reconciliation](../concepts/durable-execution.md#durable-does-not-mean-every-effect-happens-exactly-once)
for operations that might have succeeded before a crash.

## Credentials, records and export

Use login storage, environment variables or approved secret references. Atom
code that calls a cloud service names a
[credential](scripts-and-cloud-sdks.md#give-code-short-lived-credentials): Atomic
assumes a role for each step and hands the code keys that expire in minutes, so
no long-lived key is stored. Do not
commit credentials to atom files, manifests, fixtures or transcripts. Keep runtime
state outside application repositories and protect it as sensitive local data.

Local run inspection works without exporting telemetry. Prompt, completion and
tool content capture is off by default; enabling export is a separate choice.
Team identity, audit and transport controls are staged deployment features. See
[authentication](../getting-started/authentication.md), [operation](../operations/running-atomic.md)
and [observability](../operations/observability.md) for their boundaries.
