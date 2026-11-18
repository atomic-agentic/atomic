# Typed contracts in Astro atoms

This experiment demonstrates typed validators and scorers in
[draft-reply.atom.astro](fixtures/typed/draft-reply.atom.astro). Stock Astro
language-server checking works inside these expressions. The compiler parses
and interprets them without evaluating the atom as a JavaScript module.

```ts
validate: [
  ({ input, output: { meeting } }) =>
    !meeting ||
    (meeting.minutes > 0 &&
      input.free_slots.some(slot => fitsWithin(meeting, slot))),
],
```

The scorer binds the judge's email, reply and criteria through another typed
expression. Its score uses `filter(verdict => verdict.met).length / ...`,
with an explicit zero-verdict result of zero. It retains the original 0.8 mean
threshold. Production Astro atoms now use these interpreted typed contracts.
The [full metadata fixture](fixtures/full/draft-reply.atom.astro) uses the adopted
callback dialect too. Full input/output/case/judge schemas are enforced in the Rust
integration, alongside mandatory source type diagnostics. See [the current contract](SYNTAX.md).

## Reproduce

Use the versions and installation instructions in [README.md](README.md).

```sh
npm test
npm run probe:contracts
npm run probe:lsp -- fixtures/typed/draft-reply.atom.astro
npm run probe:controls
npm run probe:deno
```

`probe:contracts` checks an accepted 30-minute meeting and a rejected 31-minute
meeting, returns the judge's structured input, computes 0.8 from scripted
verdicts, and checks a 0.9 mean against the 0.8 threshold. No judge/model is
called. `probe:deno` compares both prompt rendering and the contract probe's
complete output with Node, under the same restricted offline container used by
the initial spike.

## Why the editor understands it

[authoring.d.ts](authoring.d.ts) supplies `AtomConfig<Input, Reply, EvalCase,
Judges>`. The explicit config annotation gives callbacks contextual types:
input/output/case for validation and judge bindings, and the selected judge's
output for scoring. `Judges` maps judge names to their input/output contracts.
The follow-up [generation and embedding spike](GENERATED-EMBEDDED.md) now
generates the atom's declarations from JSON Schema into hidden `.atomic/types`
files. Imports reference those files directly, and explicit `Props` annotations
on bindings preserve types in inferred editor projects as well as configured
projects. The earlier virtual aliases depended on tsconfig discovery and were
replaced after reproducing editor TS2307.

`fitsWithin` is a named import from the compiler's authoring declarations.
The interpreter recognizes that exact registered import and dispatches to its
own deterministic host implementation. It never loads an author-supplied
helper implementation. The declaration is not an executable npm module.

The unmodified Astro LS 2.17.3 with TypeScript 5.9.3 reports:

| Probe | Observed |
| --- | --- |
| Valid typed atom | Zero errors. |
| `meeting.` inside validator | Completes `minutes`, `start`, `title`. |
| `judge.` inside scorer | Completes `verdicts`. |
| `verdict.` inside filter callback | Completes `met`. |
| `fitsWithin` hover | `fitsWithin(meeting: Meeting, slot: TimeSlot): boolean`. |
| `meeting.minuttes` | TS2551. |
| `verdict.mett` | TS2551. |
| A string passed instead of a slot | TS2345. |
| Email object used as judge criteria | TS2740; expected `string[]`. |
| A string returned as a score | TS2322; expected `number`. |

The LSP probe makes invalid edits in memory only. It also retains the earlier
template completion, hover, component navigation and negative diagnostic
checks. The VS Code GUI has not been tested. The stock server also suggests
some irrelevant Astro snippets; its subset awareness is unchanged.

## How evaluation works

Only a config annotated with the registered imported `AtomConfig` type accepts
contract callbacks. Untyped literal configs retain their earlier behavior.
Callbacks are permitted at `validate[]`, `scorers.*.with`, and
`scorers.*.score`. They must have one parameter and a synchronous expression
body. Identifier parameters and plain nested object destructuring are
supported. Functions with statement bodies, defaults, rest parameters and
async callbacks are rejected.

The compiler checks every branch before a contract can run. It rejects free
variables and captures from template scope, arbitrary calls, constructors,
assignment, property reflection, spreads, host globals, and unknown helper
imports. A contract cannot call methods or functions supplied by input data.

Templates and contracts use the same expression checker/evaluator. The added
operations are object construction, multiplication/division, and bounded
`some`, `every`, and `filter` alongside `map`. Collection operations are
implemented by the interpreter over arrays; it does not invoke a receiver's
method. Predicates must produce booleans. Arithmetic rejects non-finite
results. The renderer preserves external origin/path information on derived
objects and filtered data. Its internal data marker is an unforgeable symbol.

Contract contexts must contain plain JSON values, without functions or
accessors. Context validation caps depth at 32 and visited values at 10,000;
collection receivers are limited to 1,000 elements and each contract has a
10,000-operation budget. Validators must return booleans, scores must be finite
and between zero and one, and mean aggregation requires nonempty valid scores.

`fitsWithin` requires explicit timezone offsets and valid calendar dates. It
accepts seconds and optional 1–3 fractional second digits, requires positive
whole minutes, and uses an inclusive end boundary. It rejects naive dates and
leap seconds. This is a deliberately small helper contract, not an exhaustive
implementation of CEL timestamp/duration semantics.

## Evidence and limits

The typed-contract milestone had **61 passing tests, zero skips**: 33 original
renderer tests plus 28 typed contract tests. The generation follow-up adds six
more, bringing that milestone to 67. The [context follow-up](CONTEXT.md) adds
another 17, bringing that historical milestone to 84. They cover missing meetings, exact slot
boundaries, oversized/out-of-slot meetings, empty/later free slots, timezone
offsets, invalid dates, judge input preservation, score computation and mean
thresholds, rejected code, input wrappers/accessors, shared origin handling,
non-finite results, and input/operation limits.

Five disposable mutation controls fail as expected:

| Removed control | Failing tests out of 61 |
| --- | --- |
| External data isolation | 7 |
| Frontmatter statement rejection | 1 |
| Contract AST validation | 11 |
| Meeting containment | 1 |
| Scorer threshold | 1 |

Node 24.21.0 and Deno 2.9.8 produce identical probe results. Deno has project
read permission and the TypeScript configuration environment reads documented
in the README, without network, subprocess, write or FFI permissions. This is
a CLI/container check. The [follow-up](GENERATED-EMBEDDED.md) separately verifies
an in-process `MainWorker`.

Production adoption now wires the interpreter into the Rust loader and model runtime.
Fresh schema-derived TypeScript diagnostics are mandatory, with source-mapped type
errors. Full input/output/case/judge schemas are validated independently in Rust;
offline reference scopes and siblings survive inlining, and unsupported recursion
fails explicitly. Compiled fingerprints include full constraints, even those erased
from TypeScript. The current semantics and limits are documented in [SYNTAX.md](SYNTAX.md).

The stock editor still accepts a larger language than Atomic's interpreter.
Config type aliases, `satisfies`, arbitrary helpers and executable modules remain
outside the subset. Astro formatting can change prompt whitespace. Workflow-level
durable context dispatch, registry-backed notes and live judge orchestration remain
separate work. The owner selected Astro as the sole declarative frontend; the
combined embedded-runtime decision remains open.
