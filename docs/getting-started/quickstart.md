# Quickstart

## Run your first eval in v0.1

Have the `atomic` executable on your `PATH`. For build instructions and the
current release status, see [installation](installation.md).

### Create a project

In a terminal, make a new directory and initialize it.

```sh
mkdir note-tools
cd note-tools
atomic init
```

The project contains a lite atom named `summarize-note` and one eval case.

### Configure a model

Use an endpoint and model that support structured JSON output for lite atoms.
Set the base URL and model ID to the values for your endpoint.

```sh
export ATOMIC_MODEL_BASE_URL='https://your-endpoint.example/v1'
export ATOMIC_MODEL='your-model-id'
```

Atomic uses Responses for OpenAI's API and Chat Completions for other
OpenAI-compatible endpoints. See the
[model compatibility table](../models.md#how-the-output-schema-reaches-the-model).

If your endpoint requires a key, put the key in an environment variable and set
`ATOMIC_MODEL_API_KEY_ENV` to that variable's name. For example, set
`ATOMIC_MODEL_API_KEY_ENV=OPENAI_API_KEY` if your key is in `OPENAI_API_KEY`.
ChatGPT subscription sign-in covers Codex atoms. See [lite atom authentication](authentication.md#lite-atoms).

### Run the sample eval

From `note-tools`, run the eval. This sends the sample input to the configured
model and records its answer in a cassette.

```sh
atomic eval --live
```

A passing result exits successfully and includes
`PASS summarize-note: 1 of 1 cases passed`.

To check the recorded answer without calling the model, keep the same endpoint
and model settings and run:

```sh
atomic eval
```

See the [eval reference](../reference/eval-cases.md#cases-golden-fixtures-and-recordings)
for how cassette replay works. When you are ready to write your own automation,
continue with [your first atom](first-atom.md) and [your first workflow](first-workflow.md).

## Planned interactive coding in v0.5

The v0.5 design adds a coding session on the
[Codex foundation](../README.md#open-source-openai-codex-underneath-atomic-on-top).
The commands and session behavior below describe the planned experience.
Credential reuse and support for extensions and upstream commands may vary.

### 1. Open your project

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

The planned session uses your configured model connection and coding permissions.
This entry path is designed to work without a manifest, an atom library or a
running Atomic server.

### 2. Ask for a small, useful result

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

### 3. Work in both directions

You can send a follow-up while work is in progress:

> Keep this version frontend-only. Skip authentication and a backend.

The agent can also ask you for a decision:

> Should finished books stay visible by default?

Answer in the session. You can inspect the transcript, stop work or take over
when you need to. See [controlling agents](../guides/controlling-agents.md) for
how this extends to several agents and workflow gates.

### 4. Check the result

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
