# Start coding with Atomic

The **v0.5 target** provides a familiar coding workflow on the
[Codex foundation](../README.md#open-source-openai-codex-underneath-atomic-on-top):
open a terminal in your project, describe a change and work with the agent.
Launch `atomic` where you would normally launch `codex`. You can inspect files,
edit code, run commands, answer questions and review results without writing an
atom definition. Credential reuse, extensions and every upstream command are not
promised to carry over automatically.

For the **v0.1 target**, start with [your first atom](first-atom.md) and then
[your first workflow](first-workflow.md). See [installation](installation.md)
for release status; this repository does not yet ship an executable.

For model access, the target onboarding includes **Sign in with ChatGPT** for
eligible Codex subscriptions as well as API keys. The subscription support is
present in upstream Codex; Atomic
[still needs to integrate the login flow](authentication.md).

## 1. Open your project

In an existing app:

```sh
cd my-app
atomic
```

Or start in a new directory:

```sh
mkdir reading-list
cd reading-list
atomic
```

Atomic opens a Codex-based interactive coding session in that directory. It uses
your configured model connection and coding permissions. You do not need a manifest,
an atom library or a running server for this entry path.

## 2. Ask for a small, useful result

For example:

> Build a small reading-list app. Let me add a book, mark it finished and filter
> the list. Use local storage. Explain the approach before changing files, then
> run the tests and show me how to start it.

For an existing repository, try:

> Find the code that calculates invoice totals. Explain how it works, then add a
> regression test for a quantity greater than one.

The agent can inspect files, make changes and run commands within the session's
permissions. Atomic shows the work in the terminal. Review requests for additional
permissions before allowing them.

Codex also includes [native web search](../guides/web-search.md). Ask for current
public documentation when needed; availability and mode follow search policy.
Service context such as Slack uses a separately authorized connection.

## 3. Work in both directions

You can send a follow-up while work is in progress:

> Keep this version frontend-only. Skip authentication and a backend.

The agent can also ask you for a decision:

> Should finished books stay visible by default?

Answer in the session. You can inspect the transcript, stop work or take over
when you need to. See [controlling agents](../guides/controlling-agents.md) for
how this extends to several agents and workflow gates.

## 4. Check the result

Review the diff, inspect the test results and run the app. Ask for the specific
change you want next. You can stay in this interactive workflow for everyday
coding; reusable automation is optional.

## When you want to repeat the work

Suppose you keep asking for the same review, report or release summary. Make it
an [atom](first-atom.md): instructions plus typed input and output. Compose atoms
into a [workflow](first-workflow.md) when the job needs several steps, a durable
approval or recovery after an interruption.

An ordinary conversation and a durable workflow are different kinds of work.
Workflow checkpoints provide recovery guarantees; opening a coding chat does
not automatically make every command in that chat a durable step.

Next: [the mental model](../concepts/mental-model.md),
[how Atomic layers on Codex](../concepts/architecture.md), or
[your first atom](first-atom.md).
