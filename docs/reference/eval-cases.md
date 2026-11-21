# Eval case files

Eval cases provide inputs and behavioral assertions for one atom. File-based cases
are part of the **v0.1 target**. They are distinct from model-response recordings.

## Location and format

For `atoms/summarize-note.atom.astro`, the default directory is
`<manifest-dir>/evals/summarize-note/`. The atom's `evals` field can override it
with a path relative to that atom file. Each `.yaml` file holds one case; its relative
filename identifies it in results. A `.jsonl` file holds
[many cases](#many-cases-in-one-file). A `.yml` file is refused rather than
skipped; other files are ignored.

The directory must be inside the project, however its path is written, and it is
read as the project's collections are: files that `.gitignore` or the manifest's
`ignore` globs exclude are not cases, a symbolic link is not followed and a nested
project's directory is left to that project. A directory the atom names must exist;
the default one may be absent, and then the atom has no cases. A case file is at
most 16 MiB, and an atom has at most 10,000 cases.

```yaml
input:
  text: The export bug may relate to file-name encoding; this is unconfirmed.
expect:
  - "output.summary.size() > 0"
  - "!output.summary.contains('has been fixed')"
```

| Key | Contract |
| --- | --- |
| `input` | Required JSON-compatible value matching the atom's input schema; an object when the schema requires one |
| `expect` | Required nonempty list of CEL expressions; every expression must evaluate to `true` |
| `case` | Optional metadata for the atom's [scorers](#scorers-and-baselines), checked against the atom's `case` schema |
| `prepared` | Reserved for the values an atom [computes in code](atom-files.md) before its model call, from v0.7; the runner refuses a case that sets it today |

A case missing `input` or `expect`, with an empty `expect`, with any other key,
with an input that does not fit the atom's input schema, or with an assertion
that reads a field the schemas do not have, fails the load at its file and the line
of the field at fault (in a JSONL file, the case's line).

A number in `input` or `output` takes its CEL type from the atom's schema, as
[Numbers in CEL](workflow-files.md#numbers-in-cel) describes: an `integer` field
is an `int`, and a `number` field is a `double`.

An atom whose code declares a write to an outside system has no eval cases:
Atomic never runs such code in an eval. Cover it with code tests and
`atomic test`.

The format uses these keys; there is no per-case `atomic`, `atom` or `output`
field. Use JSON-compatible YAML values, quote CEL expressions and avoid duplicate
mapping keys. The selected atom comes from the command:

```sh
atomic check
atomic eval summarize-note
```

## Many cases in one file

For more than a handful of cases, put one case per line in a `.jsonl` file in
the same directory. Each line is a JSON object with the same keys, in strict JSON:
a line that repeats a key is refused rather than read for its last value.

```json
{"input": {"subject": "Charged twice", "body": "Two charges for March."}, "expect": ["output.queue == 'billing'"]}
{"input": {"subject": "Nobody can log in", "body": "The whole team is locked out."}, "expect": ["output.urgent == true"]}
```

A case is identified by its file and line number. YAML and JSONL cases can sit
side by side, and cases captured from rejected gates can be appended to a JSONL
file. See [the example](../../examples/09-compact-and-variants/README.md).

## Pass rate and threshold

A run of an atom's cases reports its pass rate, the share of cases whose every
assertion held. It passes when the pass rate reaches the atom's threshold, which
is every case unless the atom says otherwise in its `evals` field:

```ts
evals: { pass_rate: 0.9 },              // the share of cases CI requires
evals: { dir: 'cases', pass_rate: 0.9 }, // with the cases somewhere else
evals: 'cases',                          // the directory alone, every case required
```

The directory is relative to the atom file. A run with no cases does not pass.
The report can be written as JUnit XML, one test case per eval case and per
scorer, and one for the run as a whole that fails with each of the run's own
failures (no cases, a pass rate below the threshold, a baseline that cannot be
compared, a selector that names no case), so CI shows which failed. Text that XML
cannot carry, such as control characters in a model's output, is dropped from it.
A run can be limited to chosen cases (a file's name selects all its cases) and to a
seeded sample of them; the same seed picks the same cases. A selector that names
no case fails the run rather than run fewer cases than asked.

Each case's input is rendered as external input, as anything that did not come
from a person on the team is: a case file may hold text copied from anywhere.

## Scorers and baselines

A [scorer](atom-files.md) gives each case a number from 0 to 1 through a judge,
where an assertion gives only pass or fail. Each scorer declares `min_mean`, the
mean its scores must reach, and may declare `max_drop`, how far its mean may fall
below the mean of a baseline run, the report of an earlier revision:

```ts
scorers: {
  quality: { judge: 'reviewer', with: ({input}) => ({text: input.text}),
             score: ({judge}) => judge.ok ? 1 : 0, min_mean: 0.75, max_drop: 0.05 },
},
```

Only cases whose assertions held are scored; the report lists, for each scorer,
the cases it scored and the cases it skipped. The run fails when a scorer's mean
is below its `min_mean` (a mean that misses it by floating-point rounding alone
reaches it), or, given a baseline, when the mean has fallen by more than its
`max_drop`, even while it is above the minimum. A baseline is compared only when it
is a report for the same atom over the same cases; otherwise the run fails, as it
does when the scorer declares `max_drop` and the baseline has no mean for it.
Calling a judge model is the caller's: a run without a judge reports the pass rate
alone and lists the scorers it did not run, which JUnit shows as skipped.

## What gets checked

The loader checks the input schema and expressions. Evaluation obtains a model or
exec result, applies the output schema and the atom's `validate` rules, then checks
each case's `expect` expression against `input` and `output`. A false assertion or
an evaluation error fails the case. These illustrative string checks catch known
regressions; they do not establish the factual quality of arbitrary summaries.

Literal `config.examples` declarations supply typed example inputs and outputs. Schema
checks establish their shape; handwritten example outputs do not establish model
performance. Use case assertions for the behavior you need to evaluate.

## Cases, golden fixtures and recordings

- A case defines an input and assertions.
- A golden fixture is an expected result written or reviewed by a person.
- A recording stores an actual call/result under its effective execution identity.

`atomic eval` answers model calls from **cassettes**, recordings kept beside the
cases in `evals/<atom>/cassettes/`, so evals run offline and give the same answers
every time. A recording is keyed by the request as sent: the endpoint (its scheme,
host and path, with the default port left out) and path, and the body, which holds
the compiled messages and input, the generation schema, the model and its settings,
the output limit and any tool definitions. The body's `prompt_cache_key`,
`metadata`, `user` and `client_metadata` are left out, since they change between
runs without changing what is asked. A recording answers only that request:
changed instructions, a changed schema, another model or other settings miss, and
the eval fails saying how to record it, whatever its pass rate. The same request
made twice, as two cases with the same input make it, is recorded twice and
replayed in the same order. The order counts within one run of `atomic eval`: the
first such request a session makes gets the first recording, the second the
second. A run that skips one of the cases, with `--case` or `--sample`, can
therefore be answered with the recording another of them was given; the answers
to repeated requests are only told apart by their order. A recording that cannot be read whole, or that
records another request, is reported as corrupt. A change that sends nothing new,
such as a `validate` rule, still replays, and the new rule is checked against the
recorded answers. Results from cassettes are reported as a historical check of
recorded answers, never as a live pass.

```sh
atomic eval summarize-note --live   # call the model and record each answer
atomic eval summarize-note          # replay the recordings, with no model call
atomic eval --no-cassettes          # call the model and record nothing
```

Replaying needs the model endpoint and model configured as they were when the
answers were recorded, since both are part of what a recording answers; no call
reaches them, and no API key is read or sent. Set `model.base_url` and
`model.name` in `atomic.yaml` (or `ATOMIC_MODEL_BASE_URL` and `ATOMIC_MODEL`) to fix
a replay's endpoint and model wherever it runs. Without them the endpoint is
detected as for a run: from `OPENAI_API_KEY`, or by probing for a local Ollama, so
a replay in CI can find another endpoint than the recording did and miss. The
budget's call bound and price table set the output cap each request carries, so a
different call bound or price table in CI than when the answers were recorded
also makes replays miss.
A recording holds the request body and the response, never a header, so not the
endpoint's key; an endpoint whose URL holds credentials or a query is refused. A
recording is written whole, never through a symbolic link, and a cassette
directory reached through one is refused. The body holds
what the atom was asked, case inputs included: review a cassette like any other
test data before you commit it. A hand-authored fixture is never a live-model pass.
`--compare` is a v0.5 feature.

A live eval, `--live` or `--no-cassettes`, is metered as a workflow step is: each
atom's calls are admitted against its `budget` block and the defaults in the
project's usage ledger, and the spend is reported. A budget with no room for a call
stops the eval before any call, and a live eval does not start if the ledger cannot
be opened. A replay spends nothing.

`atomic eval` exits non-zero when any atom's report fails, so CI fails with it.
`--case <id>` runs chosen cases; with no atom named, only the atoms that have
them run, and a case no atom has is an error. `--sample N --seed S` runs a seeded
sample, `--junit <file>` writes JUnit XML (a suite per atom, with failures, errors
and skipped scorers counted), `--report <file>` writes the reports as JSON, and
`--revision <rev>` records the revision being evaluated in each report. An atom
with no cases is named in the output. Scorers need a judge, and `atomic eval` has
none to call yet: an atom that declares scorers fails unless `--allow-unscored` is
given, and then they are reported as skipped. `--baseline` is refused for the same
reason: there is no scorer mean to compare.

Until evals run in disposable fixture worktrees, `atomic eval` refuses the atoms
that could write to the project's checkout or to anything else. On Linux, where
exec commands run confined, that holds for what it does run too; elsewhere an exec
command runs unconfined (see [availability](availability.md)), so the declared
effects are all that stands between it and the project. It refuses:

- a Codex atom with a `workspace-write` sandbox;
- an exec atom that does not declare `effects: { external: 'read-only' }`. Its
  command runs in the project, under the same confinement as in `atomic run`, for
  every case and every replay, so only a command declared to read and nothing more
  is run. One that declares `idempotent` writes to an outside system and is never
  run; one that declares nothing could do anything. If a command only reads its
  input, declare it `read-only`, as the order-report example's atoms do;
- a Codex atom under cassettes: it runs only with `--no-cassettes`, since cassettes
  do not yet stand in for every part of a Codex thread's endpoint.

A refused atom is listed with the reason and what to change, and the other atoms
run. The command fails while any atom is refused, unless `--skip-refused` is
given; then refused atoms are reported as skipped, in JUnit too.

A live eval's spend is recorded in the usage ledger under an eval scope,
`eval:<atom>:<time>-<process>`, named in the output. It is a ledger scope, not a
run: `atomic runs` does not show it.

## Coding fixture repositories

The v0.1 target selects a prepared local Git repository explicitly. `atomic eval`
does not take these flags yet; this is the design it is built to:

```sh
atomic --project /path/to/project eval review-patch \
  --fixture /path/to/seed-repository --fixture-rev <revision>
```

The same paired flags apply to `atomic run <name> --input <file>`, where they
are built (see the [CLI reference](cli.md#coding-fixture-selection));
`atomic eval` is not built yet. `--project` selects the manifest directory
independently of your working directory; fixture paths resolve from the
caller's directory. Coding development runs require the fixture/revision pair.
Lite and non-repository tasks need neither.

The revision is resolved to an immutable commit at admission, the Git objects
it reaches are copied into Atomic state, and each worktree step runs in a
detached worktree of that snapshot. No source checkout is edited. Inputs marked
`format: git-commit` must name real commit objects in the snapshot; base fields
must match the selected revision. Invalid paths, missing objects and
placeholder hashes fail setup before any model call. Submodules, LFS content,
external alternates, a shallow or grafted history, a SHA-256 repository and a
directory inside a repository rather than its top fail setup; there is no
implicit network fetch or checkout fallback.

Trusted setup is performed by the operator before invoking Atomic. The
[coding example and preparation helper](../../examples/02-change-request/README.md)
create a deterministic seed and a separate project copy with real SHAs in its eval
inputs. The runtime has no placeholder substitution or implicit setup-script rule.
Repository content/revision and tool fixture versions participate in a
recording's identity. These are proposed runtime contracts; the helper can be run today.

See [testing and evals](../guides/testing-and-evals.md) and
[the source examples](../../examples/README.md).
