# Atoms and workflows

An atom does one bounded job. Its contract says what it accepts and what it
returns; its instructions or code say how to do the job.

Examples include reviewing one change, extracting one invoice record, computing
an order total or drafting one support reply. A smaller task is easier to test,
retry, budget and review than an open-ended goal.

## Choose an atom kind

| Kind | Execution | Typical work |
| --- | --- | --- |
| `codex` · default | Coding agent with a thread and permitted tools | Read a repository, implement a change, investigate a failing test |
| `lite` | One model call with structured output | Classify, extract, summarize or judge supplied data |
| `exec` | Sandboxed command or script with typed JSON input/output, and no model | Parse, calculate, transform or render; read from or write to an outside service |
| `a2a` · later phase | Remote agent handoff | Delegate a typed task to an approved external agent |

Use deterministic code for deterministic work. Use the full coding agent when
it needs to explore or change a workspace. Use a lite atom when supplied context
is enough. [Availability](../reference/availability.md) lists the delivery stages.

## Contracts at the boundaries

Atoms declare JSON Schema input and output contracts. Atomic validates inputs
before execution and checks outputs against the full contract. Generation may use
a provider-specific schema subset; provider acceptance alone does not establish
that the result meets the contract. Failed output checks get a bounded repair
attempt before the step fails.

A schema can describe a well-formed review. It cannot by itself prove that the
review found a bug. Add behavioral evals and executable checks for that.

## Compose explicit workflows

A workflow names the steps, maps their inputs and declares dependencies. The
first version can be sequential. Later workflows can fan out work, coordinate
through the graph and integrate candidates through a merge queue.

Use a gate at a real decision: accept a plan, approve a change or authorize an
external mutation. A reviewer returning `approve` is data until the workflow's
review policy or an authorized person uses it to make a decision.

## Keep runtime facts separate

The model returns the atom's typed output. Atomic records runtime metadata such
as the run/thread identities, attempt, usage and candidate commit SHA. A model
cannot establish a runtime fact by putting a plausible value into a JSON field.

For coding workflows, Atomic creates the candidate commit after the turn. Use
`steps.<id>.metadata.commit` when passing that candidate to another step. Review
and approval must cover the exact candidate that will be integrated.

## Use the work graph as work grows

The work graph stores items, dependencies, claims, messages and linked decisions.
An agent claims a ready item, works on that bounded task and records the result.
Follow-up work becomes a linked item instead of silently expanding the current
assignment.

A short sequential workflow does not require you to learn the graph. It becomes
useful when several agents share a plan or work continues over many sessions.

Next: [your first atom](../getting-started/first-atom.md),
[your first workflow](../getting-started/first-workflow.md), or
[design principles](design-principles.md).
