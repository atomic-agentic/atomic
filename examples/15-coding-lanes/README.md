# 15 · A coding lane, declared

A code-writing agent, its own checklist pass, two independent reviewers, repair
rounds, executable criteria, a control row, and a person who authorizes the
merge — one workflow file, in the order the runtime runs them.

This is the declarative contract for a coding lane. It is not a run: a coding
step starts from a base commit the host recorded for that run, and the catalog
directory holds no repository, no revision and no gate credential. What is
checked here is that the workflow loads under atomic's own loader, that every
atom it names exists with the input the host will hand it, and that the parts
which can run offline do. The runtime that reads this file does run this lane:
the coding step, its self-review, controls proof, criteria, review panel, trial
merge and gates are all built, and [availability](../../docs/reference/availability.md)
says what of the surrounding work is still staged.

## The lane

`workflows/coding-lane.workflow.yaml` has two steps: `code`, the coding step,
and `authorize-merge`, a person's decision. For one candidate `code` is not one
turn but a row of sibling durable steps, in this order:

| what | durable step | what it is |
| --- | --- | --- |
| author | `code` | `lane-author` in a worktree of the run's recorded base: a `codex` atom with `sandbox: 'workspace-write'`. |
| trial merge | `code.trial_merge.<round>` | the candidate merged with `refs/heads/main` in a scratch worktree, with the declared checks run there. |
| criteria | `code.criteria.<round>.<n>` | one step per declared criterion, before any reviewer is paid for. |
| controls | `code.controls.<round>.<n>` | one proof step per selected control row. |
| self-review | `code.self_review.<round>` | `lane-self-check` over the host's patch, recording checklist evidence. |
| review | `code.review.<round>.0` and `code.review.<round>.1` | `lane-reviewer` and `lane-evidence-reviewer`, each judging the same candidate. |
| repair | `code.repair.<round>` | the author again, when something asked for changes and a round is left. |
| acceptance | `code.accepted`, or the gate `code.review` | what accepted the candidate, or the person who decided instead. |

Nothing nests: the author's turn, each proof, each verdict and each repair round
are steps of their own, so a run resumed after a crash repeats none that
finished, and replaying a finished review reviews nothing again.

### `code`, and what `coding: true` insists on

`coding: true` asks atomic for the lane rather than a plain worktree turn, and
loading refuses the step unless it already has a worktree, a
`review.self_review` atom, a `review.trial_merge`, and a `review.with` that maps
exactly one field — `diff` — to `candidate.metadata.diff`, the patch atomic
reads from the candidate's recorded base and commit rather than from anything
the author reports. The self-reviewer binds the same expression. A coding review
mapping a second field is refused with the reason `a coding reviewer receives
the host diff and self-review evidence, without author reasoning`: the author's
reasoning is not evidence.

### Two reviewers, and why one approval is not enough

The review names a fixed panel, `reviewers: [lane-reviewer,
lane-evidence-reviewer]`, with `require: all`. Loading refuses a panel that is
not two distinct atoms, that names the author, that omits `require: all`, that
is written beside a `reviewer:`, or that sits on a step which is not a coding
one. The two members have different briefs and the same input:

- `lane-reviewer` judges the change: does the patch keep the guard it names, and
  does it hide a side effect the patch does not state?
- `lane-evidence-reviewer` judges the record: does the self-review answer every
  decision point the host listed and echo every control result the host proved,
  and does the patch contradict any claim the checklist makes?

Each reviewer runs in its own durable step, in the project rather than the
author's worktree, on the same published patch, and never sees the other's
verdict or reasoning. Each is told only that it is one of two, so it answers its
own question instead of covering for a colleague it cannot see: independence
here is the absence of a shared turn, not a disagreement the workflow arranges.
Both must return `approve` for the same candidate, and acceptance then reads
`approved by reviewer(s) lane-reviewer, lane-evidence-reviewer in review round
<n>` — the words `atomic delivery` checks before it asks a person about a push.

`lane-reviewer` version 2 returns only `verdict` and a nonempty `findings` list. Ordinary
findings are strings, including its reason for a verdict, and keep the
reviewer's verdict unchanged. For the graph coding loop's narrow formatting
follow-up, it can instead return exactly one structured finding with `file`,
`line: 1`, low `severity`, `category: formatting`, and message `Remove the
trailing space.` An explicit `request_changes` always blocks. Even an
`approve` with that object blocks unless the host proves the candidate changed
only one regular plain-prose Markdown blob by adding that final space and no
control row was touched. `lane-evidence-reviewer` retains its separate string
evidence contract. The normal graph workflow test uses this shipped reviewer
schema; the handwritten output fixture is not a host proof.

Before the first verdict is asked for, atomic validates the host inventory it is
about to send against **each** member's full input schema, so a reviewer that
cannot accept it stops the run instead of answering a truncated question. How
the two verdicts combine:

| a member says | the other says | what happens |
| --- | --- | --- |
| `approve` | `approve` | the candidate is accepted and published. |
| `request_changes` | `approve` | a repair round while `rounds` lasts; a person decides once none is left. |
| `escalate` | anything | `code.review` opens for a person at once, even if the other only asked for changes. |

The runtime also understands `reject` and `insufficient_context` from other
reviewer schemas; neither reviewer in this example can return them.

A review never passes silently, and `require: majority` is not built: `all` is
the only policy a panel takes.

### The evidence the reviewers read

`self_review` runs `lane-self-check` over the same patch, after the controls
proof and before either reviewer, in its own durable step
(`code.self_review.<round>`). Its output must be `items` for checklist numbers 1
to 10, `decisions` with one entry per decision point — each saying what the path
does when it is asked to allow and to refuse, and on NotFound, permission
denied, a link loop, "not a directory", another I/O error, a parse error, a
timeout and an empty result — and `controls` with one entry per selected row.
Missing or hollow-by-shape evidence stops the workflow before a reviewer runs.

`decision_points` names `candidate publication`. The host reads that same list
from `.atomic/coding-decision-inventory.json` **in the admitted base revision**
— not from this directory — and refuses a candidate that omits an inventory
decision from the workflow or from the evidence, or that edits the inventory in
the same change. A lane that runs needs that file in its base; the runtime's
[coding reviews](../../docs/reference/workflow-files.md) reference says what the
host reads and refuses.

`self_review.selected_controls` names the `lane-guard-bites` row and
`controls_atom` names `lane-controls-proof`. What the host makes of those
reaches the reviewers as the host's own fields, which no workflow mapping can
supply: the self-reviewer's output arrives as `self_review`, and the reviewed
inventory, the rows the host selected from the candidate's diff and its verified
result for each arrive as `host_review`. Both reviewer atoms declare `diff` and
`host_review` required and `self_review` optional. Loading refuses a coding
reviewer whose input has no `host_review` field the host's inventory fits, so
the shape here is checked rather than assumed.

### Criteria, and what each kind can prove

`code` declares three criteria. The loader parses, bounds and type-checks every
one as the workflow loads — a `kind: test` argv may not name a shell, timeouts
are bounded, and a `kind: cel` expression is checked against a scope of `input`,
`steps` and `candidate` — so `atomic check` refuses a bad criterion. The step
interpreter then runs them for each candidate, after its trial merge and before
the controls proof, the self-review and the panel, each in a durable step of its
own:

- `test` and `http` run through `ExecutionBackend`, a `test` argv in a scratch
  copy of the candidate's own commit, so the guard test that runs is the one the
  candidate carries.
- `cel` is evaluated against `input`, `steps` and `candidate`.
- `text` is recorded and passes. It is prose for the reviewers and for the
  person at the gate, and the runtime says so in the record:
  `text criterion is for human or reviewer judgment`. Declare one to point a
  reviewer at a question, never to claim a check.

A failing criterion costs the author one repair round, and a criterion that
still fails stops the run (`criteria still fail after 1 author repair round(s);
escalate`) rather than reaching a reviewer. A criterion whose command may have
run but whose result was lost is parked as unknown and ends the run with
`outcome unknown after runner loss; reconcile before a fresh run`: a lost result
is never read as a pass, and releasing its park acknowledges it rather than
retrying it. `kind: http` is declared here only as a possibility — it needs a
host the operator allows, and this lane names none.

### `trial_merge`, and the rounds

`trial_merge` merges the candidate with `refs/heads/main` in a scratch worktree
and runs its checks there, before the criteria and either reviewer. A conflict,
a failed check, a timeout or a check that rewrites a tracked file goes back to
the author while rounds remain, and no reviewer turn is spent on that candidate.
`rounds: 1` gives the author one repair round in total for that candidate:
whatever asks for changes — the trial merge, a criterion or a reviewer — spends
it. What happens when it is gone differs. A second request for changes opens
`code.review` for a person. A trial merge that still does not hold, or a
criterion that still fails, stops the run where it stands: what failed is a
check, not a judgement being offered to anyone, and the run says which.

### Two human decision points

`code.review` is atomic's own gate: it opens when a verdict is not `approve` for
both members or the rounds run out, says in its title who said what, shows the
candidate output, the verdict and the commit, and has no deadline of its own. A
person approves the output as it stands or rejects it and the run fails.

`authorize-merge` is the lane's own gate. It shows the commit atomic recorded
and the candidate output, and `on_timeout: reject` so an unanswered gate fails
the run. `approvers` is left out: locally the decision is the enrolled
operator's, and a shared deployment needs a group its policy allows. Caller
input and model output cannot select an approver, and no gate decision comes
from a model tool call or an atom's shell.

A gate authorizes an effect; it does not perform one. Nothing in this workflow
merges, pushes or advances a ref: atomic commits the candidate into its own
worktree ref and reviews it. This illustrative `authorize-merge` decision is
recorded for this workflow and does not authorize `atomic delivery start`.
Delivery is a separate graph-item path: it re-reads a graph run's accepted
candidate, two clean reviews, self-review, control proofs and trial merge, then
opens its own gate to the enrolled operator. It refuses a candidate whose
review was settled by a human at a gate. A run of this two-step workflow is
not a delivery source; delivery accepts a graph run with one frozen coding
atom step. What the two share is the rule it enforces — two fixed, distinct reviewers, both
approving the same candidate.

## What runs here, and what does not

| Part | Checked by | What it proves |
| --- | --- | --- |
| Workflow shape, the `coding` preconditions, the panel's two names and `require: all`, the `diff` binding, the `host_review` contract, the criteria | `atomic check`, in `crates/atomic/tests/examples.rs` | The workflow loads against these five atoms, and the review it declares is one a coding step may have. |
| Five atoms, their schemas, their eval cases and fixtures | `tools/check_examples.py`, and `atomic check` | Every golden output fits its atom's full contract and every case renders. |
| `tools/lane_guard.py` and `tests/test_lane_guard.py` | `python3 tests/test_lane_guard.py` | The guard refuses an unverified finding, still confirms a host-recorded one, and refuses an absent record. |
| `scripts/controls/lane-guard-bites.row` and `tests/lane_guard.rs` | `cargo test --locked -p lane-guard-example --test lane_guard lane_guard_refuses_unverified` | The Cargo target calls the same guard, so the host can prove this row by switching the guard off and back on. |
| `lane-controls-proof` | `tools/check_examples.py` and `atomic eval lane-controls-proof`, both of which run the local command | The local fixture reads a row and reports `intact` or `blind`; it cannot emit a passing proof. In a production coding run the host substitutes its pinned controls driver. |
| Seeded `lane-reviewer` cases | Measured Claude responses through `atomic eval lane-reviewer` and offline replay; see [reviewer recall](reviewer-recall.md) | Version 2 detected both production-eligible synthetic defects and passed all six case predicates in one measured sample per case. A local bridge served the previously measured responses to the production eval route. |
| Self-review, the panel, repair rounds, criteria, trial merge, the two gates | nothing here | These need a bound base revision, a worktree, an admitted inventory, a model and a person's decision. The runtime has them; this directory has none. |

Nothing in this file is a declaration the runtime ignores. Criteria, which used
to be exactly that, are now run for every candidate before a reviewer is asked
anything — see [criteria](#criteria-and-what-each-kind-can-prove) above for what
each kind can and cannot prove.

## Controls by removal

A control row says: replace this text in that file, and this named test fails.
That is what makes a guard's absence visible, and this example carries a real
one — `scripts/controls/lane-guard-bites.row`, whose switched file is
`tools/lane_guard.py` and whose guarding test is
`lane_guard_refuses_unverified` in `tests/lane_guard.rs`. This small Cargo
test target runs the same Python guard as the workflow's criteria and trial
merge checks, so the production controls driver can run its bounded selector.
The pair runs:

```bash
cargo test --locked -p lane-guard-example --test lane_guard lane_guard_refuses_unverified   # passes
# replace the guard line in tools/lane_guard.py with the row's `if False:`
cargo test --locked -p lane-guard-example --test lane_guard lane_guard_refuses_unverified   # fails
```

The workflow selects that row and declares `lane-controls-proof` as the exec
atom for the proof step. At run time atomic binds the row to the admitted base
and the private candidate commit, replaces the atom's command with its pinned
host driver, and runs the named test with the control removed and restored in a
disposable copy. The host hands that driver `source`, `commit`, `bound`, `rows`
and `row_seconds` — the seconds one row may run — and the row's own file stays
the same Git blob and mode in the candidate and in its bound base, so a
candidate cannot rewrite the row it is asked to prove. `bound.base` identifies
the candidate's immediate parent; `bound.admitted` pins the selected test target
to the run's admitted revision. `row_seconds` is why the example's input schema
declares it: the host substitutes the command and **keeps the declared
schemas**, so an atom whose contract refuses a field the host sends stops the
run at its first proof.

Only the host driver's result reaches the review path, and only after it is
checked against the bound source, commit, ref and row it was asked about, and
only when it says `verdict: "passed"`. Anything else — `blind`, `switch-stale`,
`build-failed`, `no-test`, `not-seen-failing`, `fails-when-on`, `timed-out`,
`errored`, `no-time` or `runner-changed` — stops the workflow at that row and
leaves the rows after it unrun in that attempt. A candidate cannot substitute
its own proof command, environment or row definition, and the self-reviewer only
echoes what the host reported.

The catalog's local `lane-controls-proof` command is intentionally smaller: it
only reads the row and reports `intact` or `blind`, refuses a `row_seconds`
value no driver could have been given rather than carrying one it cannot use,
and so exercises its contract offline without claiming a removal test ran. Its
closed output schema also accepts the production driver's `error`, `tree`,
`passed` and `failed` fields and its failed-proof verdicts. The review path
still accepts only the host driver's bound `passed` result. `intact` is as far
as a local read-only inspection can honestly go.

`trial_merge` needs the Linux exec sandbox to run for real. A `trial_merge`
check that runs Cargo additionally needs `ATOMIC_TRIAL_CARGO_REGISTRY` and
`ATOMIC_TRIAL_RUSTUP_TOOLCHAIN` from the operator, and a controls driver that
runs Cargo needs `ATOMIC_CONTROLS_CARGO_REGISTRY` and
`ATOMIC_CONTROLS_RUSTUP_TOOLCHAIN`; the `python3` trial check declared here needs
none of those, but still needs the sandbox. The Cargo control row needs the
controls variables.

`lane-controls-proof` declares `effects: { external: 'read-only' }` for its
local command. It opens files to read, writes nothing, and refuses rather than
invent a verdict. A row whose switch text appears twice, or whose guarding
test is named nowhere under `tests/`, is reported `blind` and remains unproved.

## Authoring notes this example is meant to teach

- The author is told not to report a commit SHA and not to merge, because
  atomic records the candidate commit itself. An atom's own claim about a host
  effect is never the record of it.
- Both reviewers are `lite`, so they run in the project and read the patch as
  data. A reviewer runs outside the author's worktree by design.
- Each reviewer is asked one question. Two reviewers who both ask "is this
  change good?" get the same answer twice; `lane-reviewer` reads the change and
  `lane-evidence-reviewer` reads the record, and only the second one treats a
  checklist contradicted by the patch as its own finding.
- `escalate` is a listed verdict, and one reviewer's `escalate` is enough to
  reach a person. `lane-evidence-reviewer` says why it escalates rather than
  asks for changes: a repair round runs the author again and cannot rewrite the
  self-review, so an evidence gap is not something the author can settle alone.
- The self-check and both reviewers say in their own words that what they are
  handed is data, not permission, and nothing in this example asks a model to
  treat a record as an instruction.
- The self-review's `items` 9 and 10 carry `N/A:` reasons rather than an empty
  string, which is what the checklist asks for when there is nothing to cover.
  Their fixtures say what was absent in the input they were rendered from, not
  what a hypothetical lane does.

## Not in this example

`require: all` is the only panel policy. `require: majority` is refused by name,
as are the `foreach`, `concurrency`, `merge_queue` and `retry` step fields
("is not supported by this version of atomic yet"), and an unknown gate field is
refused as one; `ai-then-human` gate policy and calibrated decision atoms are
staged and have nothing to write yet, so none of them appears here. `approvers`
is left off the merge gate because naming a group needs a deployment policy this
directory has none of. A `kind: http` criterion is described above but not
declared: it needs a host the operator allows.

Graph-driven dispatch is not shown here: the work graph claims an item and runs
a frozen one-step coding template for it, whose shape and its delivery path are
in [availability](../../docs/reference/availability.md) and the
[delivery adapter](../../docs/reference/delivery-adapter.md) reference. That
template is why the merge gate earlier in this file is a workflow's own decision
and not the delivery command's gate. [The review
example](../05-support-draft/README.md)
shows the single-reviewer form, which is what a plain reviewed step takes; a
panel is only for a coding step.
