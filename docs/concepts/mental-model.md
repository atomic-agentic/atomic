# The Atomic mental model

Atomic adds typed reusable tasks and durable execution to the
[Codex foundation](../README.md#open-source-openai-codex-underneath-atomic-on-top).
The **v0.1 target** starts with file-driven automation; the **v0.5 target** adds
familiar interactive coding and supervision in the same CLI.

## The coding agent

In an interactive session, you give the agent a goal and work with it on your
project. It can inspect code, edit files and run commands under the session's
permissions. You can build a whole app this way without defining a workflow.

In the **v0.5 target**, launch `atomic` in your project, give it a task and work
through the result. Codex supplies
the agent loop, conversation threads, coding tools, model connections and sandbox;
Atomic builds its CLI on the Codex terminal client. If you come from Claude Code,
this is the familiar prompt-and-code workflow.

Atomic's framework features are available when you need them. Opening a coding
session does not require a workflow definition. The
[layered architecture](architecture.md) shows what Atomic adds above Codex.

## The framework

An **atom** turns a focused task into a reusable unit with an input and output
contract. A **workflow** defines how those units run together: what runs first,
which values are passed next and where work waits for approval.

A Codex atom has the coding harness available inside its declared permissions.
A lite atom makes one structured model call. An exec atom runs code without a
model: a calculation, a transform, or a call to an outside service. Workflows can combine these kinds, so the framework applies
to more than software development.

## Shared work connects the two experiences

Atomic's built-in, beads-inspired work graph records items, dependencies, claims,
messages and result links. In the terminal you can inspect the plan and move from
an item to its run and live thread. The framework uses the same graph to choose
ready work, acquire a claim and record what the assigned atom produced.

The agent sees the relevant item context and can use its allowed graph tools to
propose follow-ups or communicate. You supervise the work from the terminal;
progress remains in the graph across sessions. The
[architecture diagram and work loop](architecture.md#the-work-graph-connects-the-terminal-and-framework)
show how those connections fit together.

## The runtime

The runtime executes workflows, saves completed step results, stores gates and
coordinates work. A **run** is one execution of an atom or workflow with particular
inputs. A **thread** is a live agent conversation associated with that work.

A run and a thread have different lifetimes. A thread can end during a crash;
the durable run can recover using its saved progress. A conversation by itself
does not give every shell command workflow recovery semantics.

## The CLI works in both directions

| Direction | What happens |
| --- | --- |
| You → agent | Ask for work, inspect its progress, answer questions, steer or stop a turn, take over |
| Agent → you | Ask a clarifying question, report a blocker, present a result or wait for a durable gate |
| Agent → runtime | Use allowed tools to read work, post a message, propose follow-up work or start an allowed workflow |
| You → runtime | Start and inspect runs; approve or reject gates using your own identity |

Agents get tools appropriate to their task. An agent's access to workflow or graph
tools does not give it the human credential needed for admin actions.

For example, you can start a change interactively, run a reusable review on the
candidate, answer a reviewer's question and accept the result from the terminal.
The interactive and reusable paths share a workspace for supervision.

## Coming from LangChain or another framework

You can still think in tasks, contracts and orchestration. The additional idea
is that Atomic includes the agent harness and an interactive terminal experience.
You can work with an agent before writing application code to host it.

| Familiar concept | Atomic concept |
| --- | --- |
| A node or task | An atom, with explicit input and output schemas |
| A graph or orchestration function | A workflow |
| One execution | A run |
| Human-in-the-loop interruption | A durable gate between steps; a live question within a turn |
| Agent memory for work coordination | The shared work graph and its messages |
| An application hosting the framework | The Atomic runtime, local or served to clients |

These are orientation aids, not compatibility claims. You can also connect an
existing agent application to Atomic through its planned protocol interfaces.

Next: [the architecture](architecture.md), [atoms and workflows](atoms-and-workflows.md),
[durability](durable-execution.md), or [CLI quickstart](../getting-started/quickstart.md).
