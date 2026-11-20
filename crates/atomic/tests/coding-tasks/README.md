<!-- SPDX-License-Identifier: Apache-2.0 -->

# Coding tasks

Eval fixtures for atoms that change code. Each task is a small project with a
fault or a missing feature, a request written the way a person would report it,
and a checker that decides whether an attempt did the job. They are synthetic,
in Python 3 and POSIX shell, so that grading needs no toolchain beyond
`python3`, `bash` and the usual POSIX tools.

## The contract

Each task under `tasks/<name>/` has:

| Path | What it is |
| --- | --- |
| `prompt.txt` | The request: symptoms and constraints, never where the cause is |
| `seed/` | The project as the attempt starts it: 11 to 27 files with a README and visible tests |
| `check.sh` | The checker, run as `check.sh WORKSPACE`: exit 0 when the task is done, 1 when it is not, 2 for bad usage |
| `hidden/` | Tests the attempt never sees, laid over a copy of the workspace when it is graded; no hidden file shares a path with the seed. `backup-exclude-sh` has none and checks in `check.sh` |
| `solution/` | A reference solution: files laid over the seed |

Every checker sources `lib/check-lib.sh`. It grades a copy of the workspace,
never the workspace itself, and leaves the workspace as it found it. A
workspace holding a link, a special file or a Python startup hook
(`sitecustomize.py`, `usercustomize.py`, `conftest.py`, a `.pth` file) fails
whatever else it holds. The checks grade honest attempts; they are not
hardened against an agent that sets out to game them.

`tests/coding_tasks.rs` holds the contract: every seed fails its checker with
exit 1 (not some other failure, so a checker that crashes cannot pass for one
that grades), every seed with its solution laid over it passes, and the checker
changes nothing it grades. While each task was written, at least one plausible
wrong fix was also tried and failed its checker; those wrong fixes are not
kept.

## The tasks

A cause one module away from its symptom:

| Task | The fault |
| --- | --- |
| `config-cache-profile` | A memoized loader whose key ignores keyword arguments |
| `tz-offset-report` | A UTC offset parser that applies the sign to the hours only |
| `cursor-pagination-ties` | A cursor with no tie-breaker, which drops rows when equal timestamps straddle a page |
| `invoice-rounding` | Float rounding, and totals summed before the lines are rounded |
| `order-replay` | An event log sorted by timestamp instead of sequence before a state machine replays it |
| `scheduler-retry` | Two ordering bugs in an event queue: retry timing and tie-breaking |
| `utf8-truncate` | Byte truncation that splits a UTF-8 code point |
| `search-normalize` | Queries normalized differently from the documents they search |

A feature that touches several places, each checked:

| Task | The feature |
| --- | --- |
| `notes-since-flag` | A `--since` flag through the CLI, its filter, the help, the README and tests that must catch a filter that ignores it |
| `backup-exclude-sh` | A repeatable `-x` option for a POSIX sh tool, through its parser, file list, manifest, help and README |

The rest:

| Task | What it asks |
| --- | --- |
| `humanize-refactor` | Fold three near-duplicate functions onto one helper, every quirk of the output kept, checked on thousands of inputs |
| `semver-wrong-test` | Fix the code to its tests, except one test that contradicts the README, which is corrected instead |
| `include-regression-test` | Fix an include resolved from the current directory, with a regression test that must fail on the old code |
| `pipeline-exit-code` | A pipeline whose stages and driver lose their errors and exit status, seen only by running it |
