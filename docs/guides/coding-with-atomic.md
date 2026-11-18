# Code and build apps

Use Atomic as an interactive coding agent when you want to explore a problem,
build an app or make a change together. Use an atom or workflow when you want to
repeat a task with a contract, saved progress and defined review.

Interactive coding on the [Codex foundation](../concepts/architecture.md#what-comes-from-open-source-openai-codex)
is planned for **v0.5**. The **v0.1** entry is file-defined atoms and workflows.

## Work interactively

Open the application directory:

```sh
atomic
```

Ask for an outcome and any constraints that matter:

> Add a search field to the reading list. Keep filtering on the client, preserve
> the current styling and run the existing tests before adding a new dependency.

You can ask for a plan first, inspect commands and diffs, answer questions and
steer the work. The session uses coding permissions appropriate to your environment.
Additional permission requests remain subject to your approval and managed policy.

Interactive coding operates on the workspace you opened. Durable coding workflows
use isolated attempt worktrees. That difference matters when you want a live
collaborative edit versus a candidate change that will be reviewed and integrated.

## Make a task reusable when it earns its place

A repeated request such as “review this change against these criteria” is a good
atom. Give it a stable input and output and evaluate it against examples.

A repeatable delivery process is a good workflow:

1. Plan a bounded change.
2. Approve the plan.
3. Implement in an isolated worktree and run tests.
4. Review the candidate commit.
5. Accept or reject that exact candidate before integration.

Atomic creates workflow candidate commits outside the model's sandbox. The model
reports what it changed and tested; runtime metadata identifies the actual commit.
A model-generated test summary is not a replacement for the test output.

## Keep review attached to the code

Pass immutable base and candidate commits to reviewers. If a merge queue rebases
a change or resolves a conflict, test and review the resulting candidate again.
A previous verdict describes the previous content.

The [reviewed coding example](../../examples/02-change-request/README.md) shows
the planned sequential workflow, explicit fixture flags and an executable helper
that prepares a separate repository/project copy with real commit inputs.

## Keep the first project small

You can use ordinary prompts for most of an app and automate only one useful
part, such as a release summary. You do not have to turn your whole development
process into a graph or introduce a server to benefit from Atomic.

Next: [controlling agents](controlling-agents.md),
[your first atom](../getting-started/first-atom.md), or
[more examples](../examples/README.md).
