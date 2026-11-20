# Eval case files

Eval cases provide inputs and behavioral assertions for one atom. File-based cases
are part of the **v0.1 target**. They are distinct from model-response recordings.

## Location and format

For `atoms/summarize-note.atom.astro`, the default directory is
`<manifest-dir>/evals/summarize-note/`. The atom's `evals` field can override it
with a path relative to that atom file. Each YAML file holds one case; its relative
filename identifies it in results. A `.jsonl` file holds
[many cases](#many-cases-in-one-file).

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
the same directory. Each line is a JSON object with the same two keys:

```json
{"input": {"subject": "Charged twice", "body": "Two charges for March."}, "expect": ["output.queue == 'billing'"]}
{"input": {"subject": "Nobody can log in", "body": "The whole team is locked out."}, "expect": ["output.urgent == true"]}
```

A case is identified by its file and line number. YAML and JSONL cases can sit
side by side, and cases captured from rejected gates can be appended to a JSONL
file. See [the example](../../examples/09-compact-and-variants/README.md).

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

Applicable recordings can support regression checks without live model calls.
Changing prompts, model settings, generation schemas or prepared context can
invalidate their applicability. The later `--live` and `--compare` development
commands are v0.5 features. A hand-authored fixture is never a live-model pass.

## Coding fixture repositories

The v0.1 target selects a prepared local Git repository explicitly:

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
placeholder hashes fail setup before any model call. Submodules, LFS content
and external alternates fail setup; there is no implicit network fetch or
checkout fallback.

Trusted setup is performed by the operator before invoking Atomic. The
[coding example and preparation helper](../../examples/02-change-request/README.md)
create a deterministic seed and a separate project copy with real SHAs in its eval
inputs. The runtime has no placeholder substitution or implicit setup-script rule.
Repository content/revision and tool fixture versions participate in a
recording's identity. These are proposed runtime contracts; the helper can be run today.

See [testing and evals](../guides/testing-and-evals.md) and
[the source examples](../../examples/README.md).
