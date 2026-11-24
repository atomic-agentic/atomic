# Review evidence, section labels and citations

Two review Atoms that answer one question each, from records a host wrote:

- **`review-evidence`** — for every *required check*, does the host hold evidence
  that ran against **this candidate revision**?
- **`review-plan`** — for every *required outcome*, which proposed task covers it,
  and what host record answers it?

Both start from what was required, never from what happened to be recorded. That
order is the point: a required check with no matching execution is the case a
review must show, and it still gets a row.

The fixtures also hold the pair `review-evidence` renders for the same records
with and without Markdown syntax — a section label, a record link, an emphasized
phrase — because a prompt layout is only worth comparing when everything else
about the run is identical.

## The records are the fixture

Everything is built from three immutable files, and nothing in the example
invents a state on the way to the model:

| File | Who wrote it | What it holds |
| --- | --- | --- |
| `evidence/candidate.json` | the host | the candidate revision and provenance, the required checks, the required outcomes with the check each one is answered by, the proposed tasks, and the record ID each one names |
| `evidence/host-executions.json` | the host | executions that happened, each with its own ID, the revision it ran against, `pass` / `fail` / `unavailable`, whether its contents can be opened, and its findings |
| `evidence/author-claims.json` | the author | what the author says about a check, and the plan's assumptions |

`tools/host-records.mjs` reads them and packages the three bindings the prompt
shows — required checks, host records, author claims — without ever joining them.
The join is what a decision depends on, so it lives in host code:
`tools/score-evidence.mjs` and `tools/score-plan.mjs`. Those two files are also
what a gate would run, and what the golden answers are computed from, so a
fixture and a score cannot drift apart.

`evidence-bad-join/` is a fourth, deliberately broken set: two required checks,
one execution record, and `docs-build` naming the run that was `docs-links`. No
code reads it except `fixture-check.test.mjs`, which asserts the reader refuses
it and that undoing that one field makes the same files load.

The record shape is one shared schema, `schemas/execution-record.schema.yaml`,
referenced by both Atoms: a record cannot have more fields or wider access in one
view than in the other. No record carries a pre-decided state — no
`is_current`, no `justified_state` — because the fields that decide (`revision`,
`outcome`, `access`) are the ones the reviewer should be reading.

## How a row is decided

One rule set, applied in this order. Both Atoms use it; the plan view adds the
task axis.

Before any of the states below, one join rule: **a row may only answer a record
that is a run of the check that row is about.** A host record says which criterion
it ran for in `criterion`; the check side names itself, and a required outcome
names the check that answers it in `expected_criterion`, which the host writes in
`candidate.json` — a plan cannot choose which run answers its requirement. Each
half is refused in two places: by `tools/host-records.mjs` when it loads the
records, and again in `tools/score-evidence.mjs` and `tools/score-plan.mjs`,
because an input can arrive from a file, an eval case or a workflow binding and
none of those paths re-reads the fixture. Without the rule, one passing run
answers for every row allowed to point at it: `docs-build` naming `docs-links`'
pass would read as `satisfied` for a check that never ran, and `REQ-SCAN`'s task
naming that same run would read as `covered_current` for a scan that could not
run.

Three refusals belong to that rule on the plan side, and all of them are the
reader's: an outcome with no `expected_criterion`, an `expected_criterion` no
required check states, and a task naming a record for an outcome that expects no
check at all (`none`). An outcome expecting `none` is answered by no record, so
the honest value for its task's record is `none`, which scores
`covered_no_record` — covered by a task with nothing to cite, still a state and
never a refusal. A record that is simply absent is `covered_no_record` too, and
an outcome no task covers stays `uncovered` whatever the records hold: the rule
refuses a *wrong* record, never a missing one.

Note that an outcome's own `record` and its task's `record` are different fields
and are deliberately not equated — `REQ-PRICING` names `ownership-review` while
its task names `pricing-scan`. A covered row answers its task's record; the
requirement's own `record` is what an uncovered row shows as the evidence it was
looking for.

| What the host holds | evidence state | plan state | outcome |
| --- | --- | --- | --- |
| a passing run for this revision, openable | `record_current_pass` | `covered_current` | `satisfied` |
| a failing run for this revision, openable | `record_current_fail` | `covered_failed` | `failed` |
| a required run that never reported a state | `record_unavailable` | `covered_unassessable` | `unassessable` |
| a run for another revision | `record_stale` | `covered_stale` | `missing_evidence` |
| a record whose contents cannot be opened | `record_inaccessible` | `covered_inaccessible` | `missing_evidence` |
| nothing recorded, or a task whose record is nowhere | `record_missing` | `covered_no_record` | `missing_evidence` |
| no proposed task covers the outcome | — | `uncovered_no_task` | `uncovered` |
| no task, and no record named either | — | `uncovered_no_record` | `uncovered` |

Three more rules do most of the work:

- **A record you cannot open credits nothing.** Its ID stays on the row, so the
  author can go and look for it, and it is cited by nothing. Its `detail` is not
  rendered either: contents that cannot be opened do not travel into the prompt.
- **A scan that could not run lists no findings.** Required and empty is
  `unassessable`, never `satisfied`, and never coverage. `finding_count` stays 0,
  which is why the count is shown beside the state and not instead of it.
- **An author claim is data.** A claim sets `claimed` and keeps its own ID in
  `claim_ids`; `credit` comes from the host records alone. A required check the
  author claims and the host does not support is an `uncredited_claim`. A host
  record that passes is credited even when the author never mentions it.

Both Atoms recommend and never approve: `decision` is `approve`, `escalate` or
`reject`, and the gate that consumes it is a person's to decide.

## The cases

| Case | What it holds |
| --- | --- |
| `claimed-pass-without-host-record` | the author claims a clean secret scan; the required scan never reported a state, and lists no findings |
| `stale-pass-for-earlier-revision` | the only `pricing-scan` pass belongs to an earlier revision |
| `unavailable-scan-with-zero-findings` | the required scan, alone, with its empty findings list |
| `inaccessible-pass-for-this-revision` | a passing `changelog-check` record nobody can open any more |
| `current-pass-omitted-by-author-plain` | one claimed current pass, one unclaimed current pass, every delimiter off |
| `current-pass-omitted-by-author-markdown` | the same candidate, records and claim, with `## `, a Markdown link and `**` |
| `plan-coverage-with-uncovered-outcome` | every outcome and task, including an outcome no task covers whose named record cannot be opened, and one with no record at all |
| `required-scan-unavailable-with-zero-findings` | the outcome whose task names the scan that never ran |
| `stale-task-record-for-earlier-revision` | the outcome whose task names a run from before the candidate |
| `plan-row-without-any-record` | a required outcome with no task, no record, and an empty host record list |

`fixtures/<atom>/<case>.input.json` is the input, `fixtures/<atom>/cases.outputs.jsonl`
the golden answer beside it, and `evals/<atom>/cases.jsonl` the same input with
its CEL assertions. The golden is the host's own answer for those records; the
assertions are written by hand, so a case holds only when the two agree.

`tools/write-fixtures.mjs` re-derives all three from the records. It is committed
so a reviewer can check the derivation; the tests never run it, because the
committed files are the deliverable and drift between them is a failure there.

## Plain and Markdown, held to one wording

`display` in the input says how the prompt draws three things: a section label
(`Required criteria` or `## Required criteria`), a record reference (the bare ID,
or the same ID as the target of a Markdown link reading `open the record`), and
the emphasized phrase in the revision rule (nothing, `*…*`, or `**…**`).
Nothing else moves:
the same record IDs, the same access values, the same decision rules, the same
candidate. `score.test.mjs` asserts that the answer is identical between the two
dialects, and `cli.test.mjs` asserts that the rendered user turn — every record
ID and access value it binds — is identical too. That is what makes the pair
matched rather than merely similar.

## Run it

From this directory, with `atomic` on the `PATH`:

```sh
atomic check
atomic render atoms/review-evidence.atom.astro \
  --input fixtures/review-evidence/current-pass-omitted-by-author-markdown.input.json
atomic render atoms/review-plan.atom.astro \
  --input fixtures/review-plan/plan-coverage-with-uncovered-outcome.input.json
```

`atomic render` calls no model: it prints the developer and user turns, where the
instructions are authored text and every record value arrives as a bound
`<data-… path="…" origin="…">` in the user turn.

The scoring needs no `atomic`, no model, no network and no Factory system — only
a JavaScript runtime with the `node:` modules it imports:

```sh
node tools/score-evidence.mjs
node tools/score-plan.mjs
```

Both print the answer for every case, in the same order as the goldens. From the
repository root, the suites are:

```sh
node --test examples/13-review-evidence/*.test.mjs
```

They are written against `node:test`, so any runtime that provides it runs the
same files; `deno test --allow-read --allow-write --allow-run --allow-env
examples/13-review-evidence/*.test.mjs` is the same 37 tests where Node is not
installed.

`score.test.mjs` scores the committed records. `fixture-check.test.mjs` checks the
committed eval rows, inputs and goldens against each other and against the
records. `cli.test.mjs` runs the built `atomic` — `ATOMIC_BIN`, or this
checkout's `target/debug/atomic` — over a private copy of the project, with its
state outside the example, and switches one thing off at a time to show that a
refusal or a Markdown branch is really doing the work.

## What this example does not do

- **No model is involved.** Nothing here shows what a model answers; the goldens
  are the host's answer for the records. Running these cases against a model, or
  against recordings, needs `atomic eval` with an endpoint and cassettes, and is
  a separate piece of work.
- **No `validate` rules.** Every rule the Atoms state about the *shape* of an
  answer — one row per required check, in order, citing only its own record — is
  enforced in `tools/score-*.mjs`, where a test can see it fail. A `validate`
  callback that no committed case can violate would be a guard nobody has
  checked, so the checks live with the records instead.
- **No gate.** `decision` is a recommendation in a typed answer. A gate decision
  comes from a person's credential, not from this Atom.
- **The table is an index.** Each cell of the record table is one bound value, and
  free text — a record's `detail` — is deliberately not a column: it travels
  beside the table, addressed by ID, so nothing has to escape a pipe or flatten a
  newline.
