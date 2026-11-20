# Workflow files

A `*.workflow.yaml` file declares a typed workflow. The filename supplies its
default name and version defaults to 1. See the
[first workflow tutorial](../getting-started/first-workflow.md) for complete files.

## Top-level fields

| Field | Meaning |
| --- | --- |
| `atomic` | Required format version, currently `1` |
| `name`, `version` | Optional identity overrides |
| `input`, `output` | Explicit workflow contracts: an inline schema, or a `$ref` to a schema file or a registered schema ID |
| `budget` | Optional cumulative [run limits](budgets.md), including child tasks |
| `steps` | Named atoms, gates and other supported operations |
| `returns` | CEL expression for the successful result |

A run records the definitions it starts with. New source edits apply to new runs;
recovery continues against the recorded versions. Use
[`atomic runs resume <run-id>`](../concepts/durable-execution.md#resume-existing-work)
to continue an unfinished run in v0.1.

## Atom steps

```yaml
summarize:
  atom: summarize-note
  with:
    text: input.text

review:
  after: [summarize]
  atom: review-summary
  with:
    source: input.text
    draft: steps.summarize.output.summary
```

`atom` refers to a registered name. `with` maps the target's input fields to CEL
expressions. `after` establishes dependencies; do not depend on YAML key order.
The loader checks required bindings and structural compatibility to the extent
it can prove them.

```yaml
implement:
  after: [approve-plan]
  atom: implement-step
  with:
    plan: steps.plan.output
    base: input.base
  worktree: true
```

`worktree: true` runs a Codex atom in a worktree of its own, checked out at the
commit its mapped `base` names; the step must map `base`. Every attempt starts
again from that commit. When the atom's turn ends, Atomic commits what it left,
outside the sandbox, and records the commit in `steps.<id>.metadata.commit`,
beside `base` and `commit_ref`. The worktrees are removed when the run finishes;
accepted commits keep their refs. A run that fails keeps its worktrees and
their refs, so it can be inspected and resumed.

No build cache an atom writes outlives its run. A worktree step's commands
get build caches of the run's own, which they may write and which are
removed when the run ends, whether it finishes or fails (a run stopped to be
resumed keeps them; one resumed after they went builds cold):
`XDG_CACHE_HOME`, npm's, pip's and Go's caches
(`GOCACHE`, `GOMODCACHE`) and `ATOMIC_CACHE_DIR` all point into one private
directory in the state directory, shared by the run's worktree steps.
`CARGO_TARGET_DIR` is each worktree's own, so no worktree runs what another
built. Their `HOME` is the attempt's own, empty, directory, which they may
write, and `CARGO_HOME` is `.cargo` inside it, never shared and never yours.
`RUSTUP_HOME` points at the toolchains installed for you (your `RUSTUP_HOME`,
or `~/.rustup`), which the commands may only read: a step does not start if
it is inside, or holds, a directory the turn may write. A read-only step finds
the same variables and writes nothing. A variable the atom declares in `env`
wins over these, except that no build cache variable may point into the
temporary directory every turn shares. Each run therefore builds from cold.

### Where each input came from

Every mapped value reaches the atom's model as data labelled with its origin,
never as instructions. Its origin follows what its expression reads, taking the
least trusted part:

| Source | Origin |
| --- | --- |
| The workflow's `input` | `external` |
| A model step's output (`kind: lite` or `codex`) | `agent` |
| An exec step's output | `external`, since it brought in outside data |
| A gate's decision | `team`, since a person made it |
| A literal the workflow file writes, such as `"'Be brief.'"` | `team` |

`origins` may lower a field's origin, never raise it:

```yaml
review:
  after: [draft]
  atom: review-draft
  with:
    draft: steps.draft.output.text
    rules: "'Cite every claim.'"
  origins:
    draft: external
```

Marking a model's output as `team`, or the workflow's input as `agent`, is an
error when the workflow loads. So is mapping a value into an atom that does not
accept its origin: an atom's `trust` is the riskiest origin it accepts, so an
atom with `trust: 'team'` takes only team data, `'agent'` takes agent or team
data, and `'external'`, or no `trust` at all, takes any. A helper an atom calls returns data as trusted as
its least trusted argument.

## CEL values and string literals

An expression such as `input.text` evaluates a value. To produce a literal string,
include CEL's quotes inside the YAML string:

```yaml
title: "'Accept the reviewed summary'"
```

Use CEL for value mappings and validation. Declarative workflows do not execute
arbitrary shell or JavaScript expressions.

## Numbers in CEL

CEL keeps `int`, `uint` and `double` apart, and its arithmetic does not mix
them. Atomic chooses each number's CEL type from the schema of the value it
belongs to, never from how the number is written:

- A field only integers can fill is an `int`, or a `uint` above
  9223372036854775807, whether the value is written `2`, `2.0` or `2e0`. That
  is a field typed `integer`, including one that is also allowed to be `null`
  (`type: [integer, "null"]`, or `anyOf` an integer and `null`).
- A field typed `number` is a `double`. An integer a `double` cannot hold
  exactly, such as 9007199254740993, is refused: type the field `integer`.
- A field the schema does not type keeps an integer as an `int` and any
  other number as a `double`.
- A number too large for a `double` (`1e400`) is refused.

Atomic converts each value when it is accepted: a model's answer, an exec
atom's output and the workflow's input. A model's answer with a number CEL
cannot read is sent back for repair, like any other schema error, so no later
expression meets it.

Comparisons work across the types, so `input.score >= 1` holds for a `number`
field. Arithmetic does not: write `input.score * 2.0`, not `input.score * 2`,
for a `number` field. Type every field you do arithmetic on. An untyped
number's CEL type follows its value (`2` is an `int`, `2.5` a `double`), so an
expression that adds to it works for one answer and fails for the next. A result that is not a finite number (`1.0 / 0.0`) is an
error, a map key in a result must be a string, and objects in a result have
their keys in sorted order.

## Gate steps

```yaml
accept:
  after: [review]
  gate:
    title: "'Accept the reviewed summary'"
    show: "{'draft': steps.summarize.output, 'review': steps.review.output}"
    timeout: 24h
    on_timeout: reject
```

`title` and `show` are CEL over the steps the gate runs after. `timeout` is a
whole number and a unit, `s`, `m`, `h` or `d`; omit it for no deadline.
`approvers` is a list of group names.

An omitted `approvers` resolves to the authenticated local operator in local mode.
Shared/team deployments require an approved group from project policy or an explicit
allowed group; missing authorization fails loading. Caller input and model output
cannot select the approver. A rejected, expired or unresolved gate does not
authorize downstream work. Its original wall-clock deadline continues while
Atomic is offline; resume applies expiry before proceeding.

`after: [review]` means the review finished, not that its verdict was `approve`.
Use a review policy or a deliberate human gate to decide what happens next.

## View a workflow as a diagram

In v0.5, `atomic workflows show <name>` inspects the definition. Add
`--format mermaid` to print its diagram:

```sh
atomic workflows show summarize-and-review --format mermaid
```

```mermaid
flowchart TD
  n1["summarize<br/>atom: summarize-note"]
  n2["review<br/>atom: review-summary"]
  n3{"accept<br/>gate: Accept the reviewed summary"}
  n4(["returns steps.summarize.output"])
  n1 --> n2
  n2 --> n3
  n3 -- "approved" --> n4
```

Each step is a node, each gate a diamond and each `after` entry an edge; the
rounded node is the workflow's result. A
diagram committed in a doc can be marked as generated, and `atomic check` then
fails when it falls behind the file. Add `--view risk` to label each step with
what it is allowed to do, or `--view cost` for its budget. See
[diagrams with Mermaid](../guides/diagrams.md).

## Runtime metadata and later composition

`steps.<id>.output` is the typed result. `steps.<id>.metadata` holds runtime facts,
including `input_sha256`, the SHA-256 of the input the step's atom was given, and a
coding candidate's `commit`. atomic records the commit itself: map
`steps.<id>.metadata.commit` into a review step rather than asking the author
model for the commit's SHA.

From v0.7, `steps.<id>.prepared` holds what the step's atom
[computed in code](atom-files.md) before its model call. Like
metadata, it is not written by a model, so a gate can show it as evidence.

The later workflow language adds `foreach`, concurrency, merge queues and richer
review policy. Every new candidate produced by rebase or conflict resolution
needs checks against its new content. See [availability](availability.md) and
[the advanced recipes](../../examples/recipes.md) before using those features.
