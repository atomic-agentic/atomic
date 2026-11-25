# Atomic example catalog

These examples cover the implemented authoring contract and later-phase designs.
The local catalog checker validates schemas, fixtures, rendered Astro prompts and
Python transforms offline. Example 14 also runs through `atomic check` and
`atomic render` as a fresh configured and inferred project; example 15 runs
through the production `atomic check` loader. Later-phase examples
can describe staged contracts that the current runtime refuses.

| Example | First phase | Demonstrates |
| --- | --- | --- |
| [01 · Classify a note](01-classify/README.md) | v0.1 | Smallest manifest, inferred name/version, sidecar schemas, lite atom, typed examples and evals |
| [02 · Reviewed coding change](02-change-request/README.md) | v0.1 | Three focused atoms, immutable base, runtime-owned commit, sequential composition, local gates before acceptance |
| [03 · Order report](03-order-report/README.md) | v0.1 | Exec → lite → gate → exec; integer arithmetic and rendering outside the model |
| [04 · Embedded diff review](04-embedded-review/README.md) | v0.1 | `.atomic/` layout, a source-relative partial, patch text instead of fake commit paths |
| [05 · Support draft](05-support-draft/README.md) | v0.5 | External input, independent reviewer, explicit SSO approvers, draft approval without a send side effect |
| [06 · Workspace contracts](06-workspace/README.md) | v0.5 | A shared schema by `$id`, workspace discovery, member manifests |
| [07 · A2A translation](07-a2a-translation/README.md) | v1.0 | Typed remote handoff, secret reference, bounded wait, mocked contract fixtures |
| [08 · Astro prompt components](08-templating/README.md) | v0.5 | Typed loops, conditions, imported components, props and a slot |
| [09 · Shared schemas, a variant and JSONL cases](09-compact-and-variants/README.md) | v0.5 | A `strict` variant that references the base atom's schemas explicitly, eval cases in one JSONL file |
| [10 · A decision recorded with its evidence](10-recorded-decision/README.md) | v0.7 | A `decisions` block in the manifest: a choice, the eval condition it rests on, an age limit for that evidence and a review date for `atomic stale` |
| [11 · Read from and write to S3 with the AWS SDK](11-aws-s3/README.md) | v0.7 | Separate fetch and write script atoms around an Astro summarizer and a human gate; `npm:` imports on Deno with a lockfile; hosts, environment and a named short-lived credential declared in the atom; a gate that shows the hash of what the model read; local tests with a stand-in for S3 |
| [12 · Public execution context](12-execution-context/README.md) | Implemented compiler/direct invocation | Typed readonly profiles, public env, project/run/step metadata and external checkpoint reuse |
| [13 · Review evidence and coverage](13-review-evidence/README.md) | v0.5 | Revision-scoped host execution records, separate author claims, required-outcome coverage, and offline evidence scoring |
| [14 · Review authoring pilot](14-review-authoring/README.md) | v0.2 | Two distinct review atoms sharing a schema-derived component and helper; configured and inferred checks, exact prompt snapshots and stock Astro editor smoke |
| [15 · Coding lanes](15-coding-lanes/README.md) | v0.2 | Author, self-review, controls proof and two independent reviewers; executable criteria, trial merge and a human merge gate |

Workspace discovery/shared-schema resolution belong to M11b/M39b in v0.5.
Later-phase examples are not v0.1 quickstarts. [Further recipes](recipes.md) explain OpenAPI, database, retrieval,
multimodal, TypeScript and parallel-code extensions without inventing missing APIs.

## Conventions used throughout

- Start with `atomic: 1` and a project `name`. Default collections, generated paths,
  ignores and detected providers do not need to be repeated.
- A filename supplies the atom/workflow name and version defaults to 1. The default kind is `lite`;
  repository workers declare `codex`, scripts declare `exec`, and remote examples
  retain their staged `a2a` integration.
- Each atom has adjacent `<name>.input.schema.yaml` and
  `<name>.output.schema.yaml`. YAML is the default schema format; a `.schema.json`
  file is accepted too, but not both for one schema. Explicit references are
  reserved for actual reuse. Fixtures and typed examples stay JSON, because they
  are data, not definitions.
- Atom bodies use typed Astro expressions and explicit local components in `.atom.astro`.
- A workflow has explicit input/output contracts and uses names to call atoms.
  Every `with` value is CEL, not a template or a string literal by default. A CEL
  string literal inside YAML needs its own quotes, e.g. `title: "'Review draft'"`.
- The default sandbox is read-only with no shell network. Only the implementer
  requests `workspace-write`; it writes to an attempt's isolated worktree.
  No example grants optional tools it does not use.
- Pure calculations and rendering use exec atoms. The model handles judgment.
  A model's returned verdict or test list is evidence to assess, not permission
  to merge, approve a gate or send a message.
- Gates show the actual candidate and verdict. A rejected or expired gate stops
  dependent work. Local gates use the local operator; the team example names an
  approver group explicitly. No example auto-approves its gates.
- Checkpoints, transcripts, worktrees, caches and secrets are runtime state,
  stored outside these source directories. Provider selection and deployment
  credentials come from the environment or operator configuration.

## Validate the catalog now

From the repository root, with Python 3, PyYAML, jsonschema and referencing:

```sh
python3 tools/check_examples.py
```

The checker rejects duplicate YAML keys, validates complete JSON Schemas and fixture
pairs, resolves schema references offline, checks workflow names/dependencies/bindings,
and runs Python exec fixtures. It invokes the shipped Astro compiler for parsing,
mandatory schema-derived TypeScript checks, prompt rendering and typed output
validators. Generated direct declaration imports resolve in VS Code. Example 08
uses local components and slots; example 09 reuses full schemas explicitly across
variants. JSONL cases are checked line by line. Its Astro checks run on
the compiler built into atomic: build atomic first (`cargo build`), or set
`ATOMIC_BIN` to an atomic binary; Deno and runtime Node/npm are not required.
It regenerates the workflow diagrams committed under an `atomic:diagram` marker in
examples 02 and 05, in the default and risk views, and fails when a committed
copy differs. It validates example 10's `decisions` block and compares its review
date only with dates it is given; it does not evaluate the conditions.
For example 11's standalone Deno scripts it checks pinned imports and lockfile
entries, declared hosts/effects and secret references without running SDK code.
The example's own tests exercise the scripts against a local fake service.
Example 12 exercises schema-typed public context using synthetic host values.
It also exercises failure cases for invalid quantities and mismatched orders,
and verifies reproducible coding-fixture preparation from unrelated directories
without source modifications or unsafe destination reuse.
The compiler is shared with the production loader. For imported shared schema
types, the checker asks `atomic check` in a disposable project copy for fresh
declarations, then supplies those to the catalog compiler. It leaves no runtime
state in the source project and needs no project npm dependencies. The checker
does not execute workflow CEL, model evals or workflow crash recovery.

`evals/<atom>/<case>.yaml` is the proposed atomic eval case. The matching
`fixtures/<atom>/<case>.input.json` and `.output.json` are an illustrative golden
pair for offline schema checks; they are not recorded model cassettes and are
not proof that a model passes the CEL assertions. Inputs are duplicated in evals
so the eval file stands on its own; the checker rejects drift between the two.

## Run once the relevant atomic phase exists

From the chosen example's project directory:

```sh
atomic config --explain
atomic check
atomic eval classify-note
atomic run classify-note --input fixtures/classify-note/export-crash.input.json
```

Those commands are a future quickstart for example 01; they have not been run
against atomic. Use the names and fixtures in each project's README for other
examples. Model calls consume tokens. Repo-backed coding evals additionally need
paired `--fixture` / `--fixture-rev` flags; example 02 includes executable setup
that creates a separate project with real Git inputs.
