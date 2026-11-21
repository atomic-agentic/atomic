# Atoms

An atom is a small, typed, single-purpose agent: a function from an input to an
output, run as one model turn whose final answer must satisfy a schema. This
page describes the contract every atom keeps and how atomic runs one. The
authoring format is specified in [Astro atom files](../reference/atom-files.md);
[atoms and workflows](../concepts/atoms-and-workflows.md) introduces the ideas.

## The atom contract

Every atom, whatever it is written in, has the same shape:

- an **input** and an **output**, each a JSON Schema 2020-12 contract;
- **instructions**, fixed per atom version so they cache well;
- a **kind**, which says what runs the turn;
- **configuration**: model tier, sandbox, network, tools, budget, retry and
  approval policy;
- optional **validation** beyond the schema.

When a workflow loads, atomic confirms that each atom's output schema is
compatible with the input of the atom it feeds, to the extent a conservative
structural comparison can show. In Rust, the compiler checks it
too. Each run records the atom's identity and a content hash of everything that
shaped the prompt, so a result can always be traced to the exact definition
that produced it.

## Atom kinds

| Kind | Runs as | Use for |
| --- | --- | --- |
| `codex` | A complete Codex thread with its sandbox, shell, patch editing and allowed tools | Reading and changing code, running tests |
| `lite` | One schema-constrained model call through Codex's model layer | Classify, extract, summarize, route, review |
| `exec` | A command or script with typed JSON in and out, no model | Deterministic work, and every write that follows a gate |
| `a2a` | A task sent to an approved remote agent (v1.0, M22) | Delegating to another organization's agent |
| `decision` | A small decision model returning calibrated probabilities (M46) | Gate thresholds and cheap routing |

Lite is the default for a declarative atom. A Codex atom declares
`kind: 'codex'` and gets a fresh thread per step, so context never accumulates
across steps.

## How an atom runs

The same pipeline runs for every model-backed atom:

1. **Load,** once per file version. Parse the file, resolve references,
   validate the configuration against the atom schema, lower the output schema
   to the provider's generation profile and type-check every expression. Errors
   name the file, line and path.
2. **Render,** once per run. Evaluate the template against the input. Every
   inserted value goes into the user turn, wrapped in a delimited element that
   names its path and origin. Each wrapper ends in a suffix generated for that
   run and field, and atomic verifies the suffix is absent from the value, so no
   value can terminate its own wrapper; the value goes in byte for byte, without
   escaping. Values never enter the developer instructions.
3. **Start** a thread, or a single call for a lite atom, with the atom's model,
   sandbox, network policy and only its listed tools. In v0.1 Codex atoms run
   through Codex's `ThreadManager`; from v0.5 they run on the embedded
   app-server so the CLI can attach (see
   [people in the loop](human-in-the-loop.md)).
4. **Turn.** The lowered schema constrains the final answer.
5. **Normalize and check.** Remove each `null` that lowering introduced, then
   validate against the full contract, then run the atom's validators. On
   failure, send one repair turn quoting the errors; if that fails too, the step
   fails with a schema error. The detail is in [JSON Schema as the
   contract](schemas-and-apis.md#json-schema-as-the-contract).
6. **Record** the typed output with the thread ID, model, token use, cost, the
   prompt hash and any human actions taken during the turn, as the step's
   private candidate (see
   [durability](durability.md#candidates-and-publication)).

An exec atom skips the model: its command receives one JSON input on standard
input and must print one JSON result, and both sides are validated against the
schemas.

## Declarative definitions

A declarative atom is an `.atom.astro` file. Its frontmatter declares literal
configuration and typed contracts; its body is prompt text with typed
expressions and local Astro components. atomic reads the file with the bundled
Astro parser and walks it with a bounded TypeScript-shaped interpreter. It never
imports an atom as a JavaScript module and never runs Astro's page renderer.
Unsupported syntax fails the load; it is never evaluated as code.

The one code a declarative atom can run is a TypeScript helper it imports: a
named function from a local module, called one call at a time in an isolate
that has no capabilities and can read nothing but its arguments, with the
step's recorded clock. Executable SDK calls and I/O belong in separate script
atoms, composed as sibling steps in a workflow.

The format follows a few choices:

- **Configuration is a literal.** What an atom may do is readable without
  running anything, so `atomic check` and the committed policy file can read it.
- **One expression language per layer.** Atom templates and validators use the
  TypeScript-shaped subset; workflow mappings, gate titles and eval assertions
  use CEL. JSON Schema is the runtime contract throughout.
- **Prompt sections stay text.** Lowercase element tags such as `<rubric>` are
  prompt structure the model reads; section tags must balance, in the atom and
  its components and inside expressions, and an unbalanced one fails the load,
  naming its line ([M11](milestones.md#m11-declarative-atoms)). The `<data-…>`
  prefix is reserved for atomic's value wrappers.
- **Schemas are YAML by default (M11).** Sidecar schemas sit beside the atom
  file; a compact form is accepted for simple shapes. YAML is restricted to what
  JSON can express.
- **Data stays JSON.** Fixtures, examples, artifacts and step outputs are JSON.

Syntax, components, slots and limits are in
[Astro atom files](../reference/atom-files.md) and
[prompt expressions and components](../guides/template-tags.md).

## Collections, references and editor support

Atoms, workflows, schemas, toolsets and evals are **collections**: each is a set
of globs relative to the project manifest plus a schema every entry must pass.
Discovery deduplicates files by canonical path, and a nested manifest is a
discovery boundary unless it is a workspace member.

References are typed. A workflow step's `atom`, a component import, a schema
`$ref`, a toolset and an eval directory are all references; the loader rejects
any that point nowhere or at the wrong kind of file, and in CI `atomic check`
walks every one of them. When two scopes define one name, the project wins over
a shared workspace member, which wins over the user's own scope; atoms from
packages always carry their namespace. No schema is ever downloaded while a
project loads.

For editors, `atomic check` generates TypeScript declarations from the schema
registry and JSON Schemas for workflow and manifest files into the generated
directory, so a stock Astro or YAML language server gives completion and inline
errors. See [Set up VS Code](../guides/vs-code.md).

From v1.0 (M36b), `atomic check --write-lock` writes `atomic.policy.lock`: the
sandbox, network, tools, effect caps and budgets each atom and workflow resolves
to after defaults and managed policy. A change that widens access appears as a
one-line diff a reviewer cannot miss.

## Fields planned for later phases

These fields are part of the design but not of the current authoring contract:

| Field | Phase | Meaning |
| --- | --- | --- |
| `owner` | v0.5 | Who answers for the atom: an OIDC subject or group, inherited from the manifest's `owner` when omitted. Findings, cost alerts and escalated gates go to this owner. A missing owner draws a warning from `atomic check`, and a server will not load a manifest that has none |
| `sensitivity.clearance` | v0.7 (M56) | The most sensitive label any request from this atom is allowed to send to a provider; each tier entry carries a clearance that is compared with it. `atomic check` refuses an atom whose data grants reach data above its clearance. See [sensitivity routing](model-layer.md#sensitivity-routing) |
| `context.principal` | v0.5 onward | A typed projection of the run's token (subject, organization, groups as role names, the person acted for, authentication strength) through a claim mapping under the policy lock. Claims the schema did not ask for are dropped, and a run with an empty required field is refused. Locally it is the operator from the keyring |
| Organization overlay | v1.0 (M36b) | One `organizations/<org>.yaml` per tenant; the organization claim on the run's token picks it, and it sits above the profile and below the environment layer. It is validated against the context schema, versioned and audited, frozen into the run, and can never widen a permission |

The context an atom sees today is described under [prompt and execution
context](../reference/atom-files.md#prompt-and-execution-context).

## Rust atoms

An atom can also be a Rust type implementing the atom trait, with `In` and `Out`
types that derive their schemas. A Rust atom may reuse an `.atom.astro` file for
instructions and configuration and add checks that neither the schema nor a
validator can express, such as "every finding names a file in the diff". A
`#[atomic::atom]` macro (v0.5, M11b) checks at build time that the file's
schemas match the derived ones. Workflow files refer to Rust atoms by name,
exactly like file-defined atoms.
