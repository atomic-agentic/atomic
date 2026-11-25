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

No build cache an atom writes outlives its run. A worktree step's commands,
and an exec step's command, get build caches of the run's own, which they may
write and which are removed when the run ends, whether it finishes or fails.
A run stopped to be resumed, parked, or taken over by another process keeps
them; one resumed after they went builds cold:
`XDG_CACHE_HOME`, npm's, pip's and Go's caches
(`GOCACHE`, `GOMODCACHE`) and `ATOMIC_CACHE_DIR` all point into one private
directory in the state directory, shared by the run's worktree and exec steps,
repair rounds included. No other run's command may write it, though one can
still read it by its path, so keep nothing secret there.
`CARGO_TARGET_DIR` is each worktree's own, and each exec step's, so no step
runs what another built. A worktree step's `HOME` is the attempt's own, empty,
directory, which its commands may write, and `CARGO_HOME` is `.cargo` inside
it, never shared and never yours.
`RUSTUP_HOME` points at the toolchains installed for you (your `RUSTUP_HOME`,
or `~/.rustup`), which the commands may only read: a step does not start if
it is inside, or holds, a directory the turn may write. A read-only step finds
the same variables and writes nothing. A variable the atom declares in `env`
wins over these, except that no build cache variable may point into the
temporary directory every turn shares. Each run therefore builds from cold.

### Review

```yaml
summarize:
  atom: summarize-note
  with:
    text: input.text
  review:
    reviewer: review-summary
    with:
      source: input.text
      draft: candidate.output.summary
    rounds: 2
```

`review` has a reviewer atom check the step's output before any later step sees
it. The reviewer is a lite or codex atom other than the author, such as the
`review-summary` atom of [your first workflow](../getting-started/first-workflow.md).
Its output is a verdict: `verdict`, one of the values atomic's examples use,
with `approve` among them:

| `verdict` | What happens |
| --- | --- |
| `approve` | The output is published. |
| `request_changes`, `revise` | The author gets a repair round, while any are left; then a person decides. |
| `reject`, `escalate`, `insufficient_context` | A person decides. |

For graph-driven `coding: true` steps, a reviewer may return typed findings
with `file`, optional `line`, `severity` (`blocker`, `major`, `minor`/`low`, or
`nit`), `message`, and an editorial `category` (`spelling`, `grammar`,
`formatting`, or `broken_link`). A `request_changes` or `revise` verdict with
only low editorial findings becomes an approval with follow-up work if the host
can prove the cited paths are changed regular Markdown blobs outside selected
control rows. That proof is limited to `docs/` and `examples/`; a symlink,
missing or ambiguous category, security-relevant path or finding text, or an
item labeled `security` requires repair even if the reviewer called it low.
A high-severity finding requires repair even when the reviewer wrote `approve`.
Accepted low findings are filed with `discovered-from` links under a separate
pinned goal, so they do not hold the active epic open. If the final panel
goes to a human gate, approval files that candidate’s eligible follow-ups
when the item closes. Delivery still requires two explicit independent
approvals, including when the graph closes an item with follow-ups. Older
reviewers with plain-text findings keep their verdicts unchanged.
The graph run records this routing policy at admission. A run recorded before
the policy was introduced keeps its original review behavior when recovered.

The rest of the verdict, such as `findings` or a `reason`, is the reviewer's
own. Its `with` maps its input from what the step can see and from
`candidate.output`, the output under review, and for a `worktree: true` step
`candidate.metadata.commit`; each binding must fit the reviewer's input. The
reviewer runs in the project, not in the step's worktree, so a generic
worktree review can use the commit's SHA; a `coding: true` review below
receives a patch computed from that commit. What
it reads of the candidate is external data, so a reviewer that accepts only
more trusted input is refused. `rounds` (0 to 5, default 1) is how many repair
rounds the author gets; an author that accepts only more trusted input than the
reviewer's verdict carries gets none, and its review goes to a person.

The author's output stays unpublished while it is reviewed. An approval
publishes it. A request for changes with rounds left runs the author again on
the same input, given its rejected output and the whole verdict as data, in a
fenced block it cannot close; the new output is reviewed in turn. Any other
verdict, or a request for changes with no rounds left, opens a gate,
`<step>.review`, that shows the output, the verdict and any commit: a person
approves the output as it stands, or rejects it and the run fails. A review
never passes silently. The author's turn, each verdict and each repair round
are durable steps of their own (`<step>.review.<n>`, `<step>.repair.<n>`), and
what accepted the output is recorded as `<step>.accepted`, so a run resumed
after a crash repeats none that finished; a replay serves what the review
accepted and reviews nothing again. Repair rounds are charged to the author's
first turn's atom budget, and later verdicts to the first verdict's, so a step
spends at most its author's and its reviewer's atom budgets however many rounds
it takes. An exec atom cannot be reviewed, since a command cannot read a
verdict; a gate cannot either, and a step's name has no dot in it.

### Coding reviews

Mark a code-writing worktree step `coding: true`. It must have `review` and a
`review.self_review` atom. Both the self-reviewer and independent reviewer map
their `diff` input to `candidate.metadata.diff`, the patch atomic reads from
the recorded base and candidate commits. The reviewer receives that mapped
diff, the self-reviewer's output in a separate `self_review` input field, and
`host_review` with the reviewed decision inventory, selected touched controls
and their verified results. All three fields are external data; the author's
reasoning is not mapped. The reviewer's input schema must accept `host_review`.
The reviewer and self-reviewer are lite or codex atoms. The reviewer runs in
the project, not in the author's worktree.

```yaml
code:
  atom: code-author
  with: {task: input.task, base: input.base}
  worktree: true
  coding: true
  review:
    reviewer: code-reviewer
    with: {diff: candidate.metadata.diff}
    self_review:
      atom: code-self-check
      with: {diff: candidate.metadata.diff}
      decision_points: [candidate publication]
      selected_controls: [self-review-evidence-required]
      controls_atom: prove-controls
    trial_merge:
      main: refs/heads/main
      checks: [[cargo, test, --offline, --locked]]
    rounds: 1
```

A coding review may instead declare `reviewers: [code-reviewer,
second-code-reviewer]` and `require: all` in place of `reviewer:`. Both names
are fixed by the workflow source, run in separate durable sibling steps, and
must approve the same candidate. A request for changes enters a bounded repair
round; a rejection or exhausted rounds goes to the human review gate.

For coding self-review, the original run-admitted revision must contain the regular JSON
file `.atomic/coding-decision-inventory.json`, for example:

```json
{"version":1,"decision_points":["candidate publication"]}
```

Atomic obtains that revision from the first worktree turn's durable,
host-recorded base. A later worktree step cannot replace its authority,
even if an earlier step published
a commit that changed the file. Atomic refuses a missing, malformed or
candidate-modified inventory. Every point in that reviewed file must occur in
both `decision_points` and the
self-review evidence. Atomic also selects control rows from the base commit
when the coding step changes a row's switched file, guarding test target or
named test, including a change that reverts an earlier step. The selector
checks each `--test` target and the final name of a
module-qualified test filter. Every selected row must occur in
`selected_controls`, be proved by the controls atom, and be reported in
self-review evidence. The candidate
cannot change row files, the shared controls driver, or a helper listed in
`scripts/controls-shared.txt` in the same coding change. The runtime selector
also includes all reviewed rows for any changed test-tree path and all rows
for a possible one-level Rust include or `#[path]` reference. Ordinary Rust
strings and comments do not count as source references. It refuses a tracked
path alias or escaped
Rust source reference whose target it cannot determine safely. This
conservative selection may require more rows than CI's
`scripts/controls-touched.py`.

The self-review atom's output must be an object with `items`, `decisions` and
`controls`. When `selected_controls` names rows, `controls_atom` names an exec
atom that declares an idempotent or read-only external effect. Atomic runs it
once per row as a durable sibling step, `<step>.controls.<round>.<index>`,
before self-review. Its input comes from the private, host-recorded candidate
commit, base and ref, the run's bound Git repository, and that one declared row.
The self-review atom receives those host results as `control_results`; its
input schema must accept that field. An unproved row, or self-review evidence
that differs from the host results, stops the workflow before the independent
reviewer. Controls proofs require Linux's exec sandbox and declare no network
hosts; a workflow that configures one is refused on platforms without that
sandbox. The host requires every selected row file to be the same regular Git
blob and mode in the candidate and its bound base commit. A candidate that
rewrites a selected row is refused before the proof step; change row definitions
in a separate trusted update. A failed control proof stops the workflow at that
row; later declared rows remain unrun in that failed attempt. `items` maps each
checklist number,
`"1"` through `"10"`, to
nonempty evidence. A `decisions` entry has nonempty `point`, `allow_refuse`,
`not_found`, `permission_denied`, `link_loop`, `not_directory`, `other_io`,
`parse_error`, `timeout` and `empty_result` strings. A `controls` entry has
nonempty `row` and `result` strings. Every declared decision point and
selected controls row must appear. If there are no decision points or rows,
item 9 or 10, respectively, needs a specific `N/A:` reason. Missing evidence
stops the workflow before the independent reviewer runs. Each self-review
turn is a durable sibling step named `<step>.self_review.<n>`.

The workflow's declared names cannot omit a decision in the reviewed base
inventory or a control row touched by the candidate. The host compares those
sets before the self-reviewer or independent reviewer runs.

`review.trial_merge` is optional. Before each review round, atomic merges the
private candidate with the configured main ref in a separate scratch worktree
and runs each declared check there without network access. A conflict, failed
check, timeout or tracked-file rewrite goes to the author for repair while
rounds remain; the independent reviewer is not called for that candidate.
The main ref and check commands are fixed by the workflow, and this path
requires the Linux exec sandbox. Direct Cargo checks also require the operator
to set `ATOMIC_TRIAL_CARGO_REGISTRY` to a trusted registry cache and
`ATOMIC_TRIAL_RUSTUP_TOOLCHAIN` to an installed toolchain. The check uses a
scratch-local `CARGO_HOME` and target directory, reads that registry cache
without write access, and runs Cargo offline. An absent cache refuses the run
before the author turn. When the configured cache has Git dependencies beside
its registry, the trial check reads that cache without write access too.
For every offline Cargo check, the candidate's project Cargo config must be
the same regular file and mode as in its bound base; a changed or linked config
stops before review.
Before each Cargo command, atomic also compares the scratch config files to
the merged tree and makes the project `.cargo` directory read-only for that
command. An earlier check cannot slip an untracked config into a later build.
The scratch-local Cargo home and its config files stay read-only during each check.

A coding review whose `controls_atom` runs `tools/controls-exec.py` requires
host-selected offline Cargo inputs before the author turn. The operator sets
`ATOMIC_CONTROLS_CARGO_REGISTRY` and `ATOMIC_CONTROLS_RUSTUP_TOOLCHAIN`.
The controls and trial settings can share the same values. Conflicting values
stop the run. Each proof attempt gets a fresh private Cargo home and writable
target, with read-only access to the selected registry and no network.
Atomic supplies `CARGO_HOME`, `CARGO_TARGET_DIR`, `CARGO_NET_OFFLINE`,
`RUSTUP_HOME` and `RUSTUP_TOOLCHAIN`. The controls atom cannot declare them.
Both project Cargo config spellings must match the bound base. Only empty
settings are accepted for a controls proof.

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
Use a step's [`review`](#review) or a deliberate human gate to decide what
happens next.

`atomic run` and `atomic runs resume` show each gate their run reaches at the
terminal and take the decision there: `a` or `r`, then a comment if you like.
The decision carries your enrolled operator credential, as `atomic gates
approve` does. Only what you type after a gate is shown decides it: input typed
earlier is discarded, and commands that atoms run have no terminal to type into.
Titles and what a gate shows can come from a model, so control characters and
the marks that reorder or hide text are shown escaped. So is what atomic
prints from a run: its result, `atomic runs show`, `atomic gates list`,
`atomic eval` and an error, and the `--json` form of each, where they are JSON
escapes and read back as the same text. atomic's log lines on standard error
are redacted first and then escaped, keeping their line breaks and tabs. When
standard input or standard error is not a
terminal, or the run is in the background, the run prints how to decide the
gate from another terminal and waits.

### Question gates

A gate can ask a question instead of asking for approval. It has an `ask` in
place of `title`, and the `answer` it takes: a list of choices, or a JSON Schema
for a free answer.

```yaml
  channel:
    after: [build]
    gate:
      ask: "'Which channel should ' + steps.build.output.version + ' ship to?'"
      answer: [stable, beta]
      remember: true
      timeout: 4h
      on_timeout: { default: beta }
```

The answer is the gate's decision, and later steps read it as
`steps.channel.decision.answer`, typed by `answer`. Type a choice by its value
or by the letter shown beside it, at the prompt and in `atomic gates answer
<gate-id> <answer>` alike; an answer that `answer` does not allow is refused in
both. With `remember: true`, the answer is also kept as a dated entry in the
project's feedback log, in atomic's state. `on_timeout: { default: ... }` needs a
`timeout`, and is the answer silence selects: when nobody answers in time the
gate store records the gate as resumed with it (and remembers it, if asked), the
prompt and `atomic gates list` say so beforehand, and the step resumes with it.
Without a default (or with `on_timeout: reject`), an unanswered question stops
the run, as an expired gate does.

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
