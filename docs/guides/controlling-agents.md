# Watch, steer and approve agents

The Atomic CLI is a place to collaborate with agents and supervise durable work.
It connects your decisions to running agents and brings their questions and
results back to you.

## Two directions, distinct authority

| You can… | Agents can… |
| --- | --- |
| Start a task or an allowed workflow | Start an allowed workflow through model-safe tools |
| Read a run's results and live thread | Read the work and context their tools permit |
| Answer a question or steer a turn | Ask for clarification or report a blocker |
| Stop work or take over | Propose a follow-up item or post a message |
| Approve or reject a human gate | Produce a reviewer verdict for the configured review policy |

Agents do not receive your admin credential. A tool that can post work or report
status does not also authorize a human-only approval. Your ability to control
agents does not imply they can run arbitrary commands in your terminal.

## Follow a run

A run ID is the handle for durable work:

```sh
atomic runs show <run-id>
```

With the interactive CLI, open the associated agent thread to see its transcript,
commands and progress. Several threads can belong to one workflow. Thread IDs
identify conversations; the run ID identifies the workflow execution you supervise.

To see where a run stands at a glance, the planned `--format mermaid` option
prints its steps and their states as a [diagram](diagrams.md#a-run). With
`--view cost` the same diagram shows what each step has used against its budget.

## Follow work from the graph to a thread

The terminal's work-graph tools show the same items the framework schedules:
ready, blocked and claimed work, dependencies, messages and linked results. Open
an item's associated run and thread to see which agent is carrying it out, then
use the thread controls to inspect or steer the work.

The framework acquires a claim before starting a graph-driven atom and supplies
the item's criteria and relevant context. The result links back to the item,
while discovered work becomes a follow-up. Terminal edits, workflow updates and
allowed agent tools all operate on that shared graph through Atomic's controls.

The graph is built into Atomic and inspired by beads; it requires no separate
beads process. Initial tools/commands and the later dedicated `/graph` pane are
staged separately. See the
[architecture](../concepts/architecture.md#the-work-graph-connects-the-terminal-and-framework).

## Steer a live turn

Use the interactive thread to add context or correct scope. The target CLI can
queue input for later, steer the current turn or replace it. Stopping or replacing
work is a deliberate control action; a regular follow-up should not silently
cancel the current job.

The exact shortcuts and thread-selection UI will be documented when the TUI ships.
The longer-term workflow, graph and gate panes extend the same operations.

## Answer a live question

An agent may need an answer while its turn is running, or request permission for a
command. Those prompts belong to the live turn. If that turn is lost during a
restart, recovery may need to ask again.

Use a workflow gate for a decision that must survive an interruption.

## Decide a durable gate

A gate records the proposed result or operation, its approvers and its deadline.
Inspect the evidence, then approve or reject it as yourself. In a local workflow,
the terminal prompts the local operator; another terminal can use:

```sh
atomic gates list
atomic gates approve <gate-id>
```

An approval applies to the recorded proposal. A changed candidate needs its own
review and authorization. A review step finishing does not automatically mean
its verdict was `approve`; the workflow's policy decides how verdicts affect work.

In team deployment, gate decisions are checked against configured approver groups.
Slack approvals are planned first, with other channels later. You do not need
those integrations for a local workflow.

## Let agents coordinate without losing oversight

Allowed workflow and work-graph tools let agents propose follow-ups, exchange
messages and hand tasks to reusable automations. Their inputs and outputs remain
typed and subject to budgets, trust rules and gate policy.

For example, an interactive agent can request a review workflow, receive a typed
finding and ask you whether to fix it. This is the framework participating in the
coding session, with the CLI keeping the person involved.

## Follow one task across the two experiences

Suppose your project already defines a `change-request` workflow. In the coding
session, ask:

> Use our change-request workflow to fix quantity handling in invoice totals.
> Keep me involved at the plan and final acceptance gates.

The agent supplies the workflow's typed input through its allowed workflow tool.
Atomic starts a run and shows its identity. You approve the plan, then follow the
implementer's thread. If needed, steer that agent to preserve a compatibility
constraint. The reviewer returns findings, and the final gate presents the
candidate for your decision.

The original conversation can discuss the result while the runtime retains the
workflow's durable record. You have used the CLI as a coding workspace and as a
supervision interface for reusable agents within the same task.

See [availability](../reference/availability.md) for the stages of live control,
local gates and team approvals.
