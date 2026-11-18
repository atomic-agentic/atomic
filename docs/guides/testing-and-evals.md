# Test and improve an atom

Check the definition before calling a model, then evaluate the behavior you care
about. A valid JSON result and a correct result are different checks.

## Check the project

```sh
atomic check
```

This validates definitions, schema references, mappings and instruction tags to
the extent the loader can check them. It should report a file location and an
actionable error. It does not establish that an atom's judgment is good or that
the code it produces passes tests.

## Add small eval cases

For the atom in [your first atom](../getting-started/first-atom.md), create
`evals/summarize-note/uncertainty.yaml`:

```yaml
input:
  text: The export bug may be related to file-name encoding; we have not confirmed it.
expect:
  - "!output.summary.contains('has been fixed')"
```

Run the atom's evals:

```sh
atomic eval summarize-note
```

Begin with representative inputs, then add failures you actually observe. Include
ambiguous inputs and adversarial text when the atom will encounter them. A small
assertion can catch a known regression; it is not a comprehensive quality metric.

See the [eval-case reference](../reference/eval-cases.md) for the file format,
CEL scope and the difference between cases, fixtures and recordings.

## Test code as code

An eval judges what a model returns. Code that an atom carries is tested like
any other code. `atomic test` runs the project's tests on the managed Deno with
network access limited to your own machine, so a test cannot reach a real
service. An atom whose code writes to an outside system has tests and no eval
cases: Atomic never runs such code in an eval. Planned for v0.7; see
[call AWS and other services with an SDK](scripts-and-cloud-sdks.md).

## Use deterministic checks for deterministic requirements

For a coding task, run the relevant test commands and inspect their output. For
an order total, use integer arithmetic in an exec atom. For a schema requirement,
use the validator. Let a model judge the aspects that require judgment.

[The order-report example](../../examples/03-order-report/README.md) demonstrates
exec and lite atoms in one workflow. Executable workflow acceptance criteria are
planned later; do not treat an agent's list of test names as proof of execution.

## Separate recordings from live evaluation

Recorded model responses can make regression checks cheaper and faster. When a
recording is applicable, validation can run again without another model call.
Changing the effective prompt, prepared context, generation schema or model
settings needs fresh evaluation rather than an unqualified pass on old output.
A recording's identity also includes prepared artifacts, generation/lowering profile,
provider endpoint and tool/mock/seed-repository versions. Validation-only reuse is
labelled a historical regression check, never a live-model pass.

The later development loop adds:

```sh
atomic dev
atomic eval summarize-note --live
atomic eval summarize-note --compare HEAD~1
```

Live calls consume tokens. The CLI should distinguish live results from checks
of historical recordings. A golden fixture you wrote by hand is not a recording
of what a model returned.

## Preview the prompt and the cost

Before a live run, `atomic render <file.atom.astro> --input <file.json>` prints the exact
prompt for an input without calling a model. Review its separate developer/user
parts when changing instructions or shared components. Persisted prompt snapshots
and regression comparison remain planned; the current check command validates
source, types, schemas and declared examples. See [prompt components](template-tags.md).

A script atom fetches values as a separate workflow step. Its output becomes the
Astro model atom's ordinary typed input. Evals use committed input/output fixtures
and never run a script that writes to an external system.

`atomic eval <atom> --plan` shows what a run would cost before it spends anything:
the number of model calls, the size of each prompt and an estimated cost range,
beside the budget cap. The estimate for a Codex atom is loose, because the number
of turns is not known in advance, and the output says so. Both are planned for
v0.5.

## Compare two versions

`atomic eval <atom> --compare <revision>` runs the same cases against two
versions of an atom and reports the difference in results and cost.

To try a different wording without losing the current one, add a
[variant](../reference/atom-files.md#variants) such as
`summarize-note.short.atom.astro`. Point it at the atom's own schemas with `$ref`,
since a variant inherits nothing, so the comparison is like for like.

`--compare` is also how you decide whether an addition to an atom's instructions
earns its tokens. A [diagram in the instructions](diagrams.md#put-a-diagram-in-an-atoms-instructions-only-when-it-earns-its-place),
for example, should stay only if the comparison shows better results than the
plain-text version. Comparison and variants are planned for v0.5.

## Fail CI on a regression

`atomic eval` reports a pass rate, and CI fails when it falls below the atom's
threshold. The plan also lets an atom declare a minimum mean for each scorer and
a tolerated drop against a baseline revision, so a regression fails CI even
while the result is above the minimum. A scorer is a check that gives each case
a number, where an `expect` assertion gives only pass or fail.

Astro config declares typed scorer `with` and `score` callbacks and `min_mean`.
Production helpers check full candidate/case/judge schemas and enforce numeric
score bounds and mean thresholds. Live judge orchestration and regression CLI
commands remain separate work. See [atom files](../reference/atom-files.md).

## Record a decision with its evidence

An eval result often settles a design choice: this atom is good enough without
repository access, or reviews can run on a cheaper model. The result that
justified the choice goes out of date when the model, the prompt or the cases
change, and nothing tells you. Record the choice in the manifest with what it
rests on:

```yaml
decisions:
  release-notes-without-repo-access:
    chose: kind lite for atom write-release-note, so it reads the change description and not the repository
    rests_on:
      - "evals['write-release-note'].pass_rate >= 0.9"
    evidence_max_age: 30d
    review_by: 2027-02-01
```

Then ask which decisions need another look:

```sh
atomic stale
atomic stale --as-of 2027-03-01
```

A decision is stale when any of these is true:

- Its review date has passed.
- Its evidence is older than `evidence_max_age`.
- Its evidence was produced on a different model version or prompt than the one
  now in use.
- One of its conditions no longer holds.

Each line of the report names the decision and the reason, so the fix is usually
one live eval run or one edit to the decision.

`atomic stale` is separate from `atomic check` on purpose. `atomic check` never
reads the clock, so checking an unchanged repository gives the same answer today
and next month, and a pull request cannot start failing because a date went by.
Run `atomic stale` on a schedule; a stale decision can open a work item.
`--as-of` sets the date to judge against, which also makes the report
reproducible.

Planned for v0.7. The block's keys follow the plan's sketch and may change. See
[the manifest reference](../reference/manifest.md#recorded-decisions) and
[the worked example](../../examples/10-recorded-decision/README.md).

## Turn review feedback into cases

A rejected gate or a human correction is useful input for the next eval. The
planned learning tools let you capture it with the rejected output and reason.
Review the resulting case before accepting it into the project's test set.

For repository-backed cases, pass the paired `--fixture` and `--fixture-rev`
options. Trusted setup happens before invocation; the runtime pins real commits
and uses isolated worktrees. See the [fixture contract](../reference/eval-cases.md#coding-fixture-repositories)
and [executable preparation example](../../examples/02-change-request/README.md).
Your live checkout is never an implicit fallback.

See [availability](../reference/availability.md) for the eval and developer-tool stages.
