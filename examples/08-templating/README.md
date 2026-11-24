# Prompt composition with Astro

This example demonstrates the single Astro format — typed values, conditions,
loops and explicit local components — on one triage atom shown in **three
prompt layouts**. All three read the same input, keep the same policy, the same
output contract, the same validator, and the same findings in the same order.
Only the shape of the findings section differs, so the shapes can be compared.

| Layout | Atom | Findings shape |
| --- | --- | --- |
| A | [triage-findings.atom.astro](atoms/triage-findings.atom.astro) | one bullet per finding, every field on the line, in report order |
| B | [triage-findings.records.atom.astro](atoms/triage-findings.records.atom.astro) | groups by file, each field on its own labeled line |
| C | [triage-findings.table.atom.astro](atoms/triage-findings.table.atom.astro) | groups by file as GitHub-flavoured Markdown table rows, with every cell's text kept verbatim below the tables |

The two variants declare the base atom's own schemas by `$ref`
([triage-findings.input.schema.yaml](atoms/triage-findings.input.schema.yaml),
[triage-findings.output.schema.yaml](atoms/triage-findings.output.schema.yaml)),
so one contract governs all three. `atomic schemas expand` prints the same
contracts for each.

## What each file is

- [TriageFrame.astro](atoms/TriageFrame.astro) holds everything the three
  layouts share: the policy, the work item, the criteria, the `<findings>`
  section and the rule that the item text and the findings are data. Each atom
  supplies only the findings shape into its slot.
- [TriageRules.astro](atoms/TriageRules.astro) accepts a typed `strict` prop
  and caller content through `<slot />`; it renders inside the frame.
- [NoFindings.astro](atoms/NoFindings.astro) is what every layout says when
  there is nothing to triage.
- [Finding.astro](atoms/Finding.astro) renders layout A's bullet.
- [FindingRecord.astro](atoms/FindingRecord.astro) renders one finding's labeled
  lines; [FindingRow.astro](atoms/FindingRow.astro) renders one table row. atomic
  trims the whitespace around a component's own text, so the atom's `map` supplies
  each line's leading newline and those two files end without a line break: that
  is what keeps a group's rows on consecutive lines, which a Markdown table body
  needs.
- [layout.ts](atoms/layout.ts) is a captured TypeScript helper, run one call per
  call in a fresh isolate with no host access. `findingRuns` cuts the findings
  into groups by position, and `cell` escapes one Markdown table cell. A helper
  result is as trusted as its least trusted argument, so a value computed from a
  finding stays external data in the rendered prompt and still names the finding
  it came from; nothing here reads a file, a clock or the network.

## Grouping keeps report order

A group opens where its first finding was reported. A file named again after
another file's findings opens a **new** group rather than moving rows back to
the first one, so the interleaving in
[interleaved.input.json](fixtures/triage-findings/interleaved.input.json) — A,
B, A, B, A — renders five groups and never reorders a finding.

The cut is made by position, in `findingRuns`, and not by matching the finding
being rendered against the list. Matching by value cannot work here: the finding
a layout is rendering names no position, so a finding that repeats is the same
answer wherever it appears. `A(other), A(x), B(y), A(x)` then gives the last
finding no group of its own, and `A(x), A(x), B(y)` opens a group for both copies
and prints one header twice. Those two shapes are
[interleaved-duplicates.input.json](fixtures/triage-findings/interleaved-duplicates.input.json)
and [adjacent-duplicates.input.json](fixtures/triage-findings/adjacent-duplicates.input.json);
they render as three groups of 2, 1, 1 and two groups of 2, 1. A template cannot
index an array by a value either — `input.findings[i]` is not part of the
supported expression set — which is why the grouping is a helper result rather
than an index passed around.

Each group is then rendered from the run's own findings, so the values land at
one path per position under the call, and two identical findings bind twice at
two paths instead of one dropping out.

Layout C's cells are lossy: a pipe is written escaped and a line break is
folded, because a table cell holds neither. The unescaped text is kept in the
`<verbatim>` block under the tables, bound at its own `input.findings[…]` path,
so a folded cell stays addressable.

## Values keep their provenance

`atomic render` splits the prompt into the developer instructions and the bound
user values. Static text — the policy, the labels, the table headers — stays in
the instructions, which only point at values. Every value, including a helper's
cell, is bound with its path and its origin, and an instruction hidden in a
finding stays a bound external value rather than becoming part of the
instructions.

Layout A's findings and layout C's verbatim text bind at their own
`input.findings[…]` paths. Layouts B and C take their groups from
`findingRuns`, so a group's file names itself at `helpers.findingRuns_…[i].file`
and each finding's own fields at `helpers.findingRuns_…[i].findings[j].severity`
and `.message`: paths that name the call each value came from, one per report
position, all still `origin="external"`.

## Check it

```sh
atomic check                                # all three atoms
atomic render atoms/triage-findings.table.atom.astro \
  --input fixtures/triage-findings/hard-cases.input.json
```

`fixtures/triage-findings/` holds one input per case (`blocker`, `no-findings`,
`interleaved`, `hard-cases`, `interleaved-duplicates`, `adjacent-duplicates`)
and their golden outputs; `evals/triage-findings/` holds the assertions. The
edge cases are in the fixtures: an empty finding list, a file named
repeatedly, a repeated message, a record repeated after another file's findings
and one repeated side by side, a pipe, a newline and a tab inside a message, a
message over 200 characters, and a message that reads as an instruction.

[prompt_layouts_test.py](prompt_layouts_test.py) runs the real CLI over the
matrix and compares the layouts — policy text, contracts, values, order,
origins, escaping, verbatim addressability, table and record shape, grouping of
repeated records, and determinism — and reports character and binding counts
per layout. It needs a built atomic
(`cargo build -p atomic-agentic --locked`) and nothing else, calls no model and
reaches no endpoint:

```sh
PATH=target/debug:$PATH python3 examples/08-templating/prompt_layouts_test.py
```

It refuses rather than guessing when no atomic is available.

Every render is also compared with a tracked snapshot under
[snapshots/](snapshots/), one file per layout and case: the developer turn, the
user turn, the bindings with path, origin, length and a short digest, and a
grouping outline that names each group and its rows in order. Wrapper ids are
rewritten to `data-id-0` in appearance order, because atomic recomputes them
from the run and the path. A review reads those files instead of taking the
test's word for it; after an intended prompt change, regenerate and commit them:

```sh
PATH=target/debug:$PATH python3 examples/08-templating/prompt_layouts_test.py --write-snapshots
```

The catalog checker (`tools/check_examples.py`, run by CI's Examples job) checks
the same contracts, eval cases and golden outputs with JSON Schema, which that
Python file deliberately does not reimplement.

[Prompt layout pilot](evaluation.md) records the paired local and frontier
model outcomes, token use, evidence errors and limits. In that small sample,
the bullet layout used the fewest input tokens with the same primary decisions;
review rows and citations still needed the host scorer.
