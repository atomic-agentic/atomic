# Atomic example catalog

These are examples of the **proposed atomic format**, reviewed against the scaffold
plan. This repository has no atomic implementation or published format schema yet.
The local checks below validate the files and execute the Python transforms; they
do not establish that an atomic runtime accepts or executes the projects.

| Example | First phase | Demonstrates |
| --- | --- | --- |
| [01 · Classify a note](01-classify/README.md) | v0.1 | Smallest manifest, inferred name/version, sidecar schemas, lite atom, typed examples and evals |
| [02 · Reviewed coding change](02-change-request/README.md) | v0.1 | Three focused atoms, immutable base, runtime-owned commit, sequential composition, local gates before acceptance |
| [03 · Order report](03-order-report/README.md) | v0.1 | Exec → lite → gate → exec; integer arithmetic and rendering outside the model |
| [04 · Embedded diff review](04-embedded-review/README.md) | v0.1 | `.atomic/` layout, a source-relative partial, patch text instead of fake commit paths |
| [05 · Support draft](05-support-draft/README.md) | v0.5 | External input, independent reviewer, explicit SSO approvers, draft approval without a send side effect |
| [06 · Workspace contracts](06-workspace/README.md) | v0.5 | A shared schema by `$id`, workspace discovery, member manifests |
| [07 · A2A translation](07-a2a-translation/README.md) | v1.0 | Typed remote handoff, secret reference, bounded wait, mocked contract fixtures |
| [08 · Template tags in both forms](08-templating/README.md) | v0.5 | A loop, a partial with a parameter and a slot, a renderer bound to a schema; the same atom as `.atom.md` and `.atom.mdx` |
| [09 · Compact schemas, a variant and JSONL cases](09-compact-and-variants/README.md) | v0.5 | Inline compact schemas with their expanded form, a `strict` variant sharing schemas and evals, eval cases in one JSONL file |
| [10 · A decision recorded with its evidence](10-recorded-decision/README.md) | v0.7 | A `decisions` block in the manifest: a choice, the eval condition it rests on, an age limit for that evidence and a review date for `atomic stale` |
| [11 · Read from and write to S3 with the AWS SDK](11-aws-s3/README.md) | v0.7 | TypeScript in an atom's `---ts` block that reads S3 before the model call, and an exec script that writes after the gate; `npm:` imports on Deno with a lockfile; hosts, environment and a named short-lived credential declared in the atom; a gate that shows the hash of what the model read; local tests with a stand-in for S3 |

Workspace discovery/shared-schema resolution belong to M11b/M39b in v0.5.
Later-phase examples are not v0.1 quickstarts. [Further recipes](recipes.md) explain OpenAPI, database, retrieval,
multimodal, TypeScript and parallel-code extensions without inventing missing APIs.

## Conventions used throughout

- Start with `atomic: 1` and a project `name`. Default collections, generated paths,
  ignores and detected providers do not need to be repeated.
- A filename supplies the atom/workflow name and version defaults to 1. Only
  `kind: lite`, `exec` and `a2a` need to override the default Codex kind here.
- Each atom has adjacent `<name>.input.schema.yaml` and
  `<name>.output.schema.yaml`. YAML is the default schema format; a `.schema.json`
  file is accepted too, but not both for one schema. Explicit references are
  reserved for actual reuse. Fixtures and typed examples stay JSON, because they
  are data, not definitions.
- Atom bodies use `{% … %}` tags in `.atom.md`, the default form. Example 08 also
  shows the optional JSX-style `.atom.mdx` form; a file is one form or the other.
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

The checker rejects duplicate YAML keys, validates JSON Schemas and all fixture
pairs, resolves schema references offline, checks source-relative partials and
input tags, checks workflow names/dependencies/bindings, and runs exec fixtures.
For template tags it checks value paths, loop sources, partial parameters and
slots, and schema-bound renderers in both forms, and that example 08's two
forms of one atom are equivalent. It compiles example 09's compact schemas and
requires them to equal their committed expanded form, checks that a variant
shares its base atom's schemas, and validates JSONL eval cases line by line. Its `.atom.mdx` reader is a small approximation
of an MDX parser, not the parser atomic will use.
It regenerates the workflow diagrams committed under an `atomic:diagram` marker in
examples 02 and 05, in the default and risk views, and fails when a committed
copy differs. It validates example 10's `decisions` block and compares its review
date only with dates it is given; it does not evaluate the conditions.
For example 11's atom code block and Deno script it checks, without running
Deno, that the block's `config` is a plain literal, that every package import
is pinned and recorded in the lockfile, that the atoms declare their hosts and
effects, that credentials are secret references, and that the instructions use
only `$prepared` paths present in a committed sample. The code is run by the
example's own `deno test`.
It also exercises failure cases for invalid quantities and mismatched orders,
and verifies reproducible coding-fixture preparation from unrelated directories
without source modifications or unsafe destination reuse.
It is not an atomic loader, a CEL interpreter, a model eval or a crash-test harness.

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
