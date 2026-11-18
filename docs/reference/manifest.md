# Project manifest: atomic.yaml

The manifest identifies a project and overrides discovery when conventions are
insufficient. Use `atomic.yaml` at the project root, or `.atomic/atomic.yaml` for
an embedded project. Paths and collection globs are relative to its directory.

The minimal **v0.1 target** is:

```yaml
atomic: 1
name: note-tools
```

No collection globs, provider list or database settings are needed for this layout.
`atomic init` selects a layout; `atomic config --explain` shows resolved settings.
This is a proposed schema, not a manifest verified against a released loader.

## Fields

| Key | Purpose and default | Stage |
| --- | --- | --- |
| `atomic` | Required format version, currently `1` | v0.1 |
| `name` | Project identity | v0.1 |
| `collections.atoms` | Atom globs; conventional `atoms/` files and colocated `*.atom.astro` | Astro declarative atoms; executable script and TypeScript integrations are staged separately |
| `collections.workflows` | Workflow globs; conventional `workflows/` and colocated `*.workflow.yaml` | v0.1 YAML; TypeScript v1.0 |
| `collections.schemas` | Shared schema globs, normally `schemas/**/*.schema.{yaml,json}`, with YAML the default; adjacent atom sidecars are discovered separately | v0.1 |
| `collections.evals` | Case globs, normally `evals/**/*.{yaml,jsonl}`; atom-specific case directories still bind cases to atoms | v0.1 |
| `collections.toolsets` | Imported OpenAPI specs, normally `toolsets/**/*.openapi.{yaml,json}` | v0.5 |
| `ignore` | Additional ignored paths; `.gitignore` is also respected | v0.1 |
| `generated` | Collection exclusion for generated cache artifacts; current Astro declarations use fixed project-root `.atomic/types` | Relocating Astro declarations is tracked separately; runtime state is external |
| `workspace.members` | Member-project directory globs | v0.5 · M11b/M39b |
| `packages` | Namespaced dependencies with `git`, pinned `rev` and accepted `trust`; resolved hashes go in `atomic.lock` | Later package/trust support |
| `models` | Named model tiers mapped to ordered provider/model candidates | Full routing v0.7 |
| `datasources` | Named application database capabilities; separate from Atomic state | v0.7 |
| `credentials` | Named credentials that atoms refer to, such as an AWS role to assume for each step; see [named credentials](#named-credentials) | v0.7 |
| `runtime.deno` | Project-wide settings for the code atoms carry: `binary`, `lock`, `minimum_dependency_age`, `install_scripts`, `allow_packages`, `audit_permissions`. Each has a default; see [settings for atom code](#settings-for-atom-code) | v0.7 |
| `decisions` | Recorded choices, each with the evidence it rests on and a review date; see [recorded decisions](#recorded-decisions) | v0.7 |
| `package`, `exports` | Shared-library publishing metadata | Later; detailed schema not specified |

The plan also mentions pure-function registration and deployment/provider policy.
Their complete manifest schemas are not specified; this draft does not supply
copyable syntax for them. See [availability](availability.md) before adopting a
later-stage field.

## Override discovery only when needed

For example, when the repository has a dedicated automation subtree:

```yaml
atomic: 1
name: release-tools
collections:
  atoms: [automation/**/*.atom.astro]
  workflows: [automation/**/*.workflow.yaml]
  schemas: ["automation/**/*.schema.{yaml,json}"]
  evals: ["automation/evals/**/*.{yaml,jsonl}"]
ignore: [archive/**]
```

A supplied collection replaces that collection's default patterns; unspecified
collections retain their defaults. An atom-specific `evals` override can bind its
case directory outside the convention. Definitions are referenced by registered
name, not file path; duplicate names within a scope fail checking.

## Recorded decisions

A project can record a choice together with what the choice rests on, so that
Atomic can say when it needs another look. This later-stage block is planned for
v0.7, and its keys follow the plan's sketch:

```yaml
atomic: 1
name: release-notes
decisions:
  release-notes-without-repo-access:
    chose: kind lite for atom write-release-note, so it reads the change description and not the repository
    rests_on:
      - "evals['write-release-note'].pass_rate >= 0.9"
    evidence_max_age: 30d
    review_by: 2027-02-01
```

| Key | Meaning |
| --- | --- |
| `chose` | The decision in words, for the person who reads it later |
| `rests_on` | CEL conditions over Atomic's own evidence, such as an atom's eval pass rate |
| `evidence_max_age` | How old that evidence may be before the decision counts as stale |
| `review_by` | A date to review the decision whatever the evidence says |

`atomic stale` reports the decisions that need attention and why. `atomic check`
does not: it never reads the clock, so it gives the same answer for the same
files. See [record a decision with its evidence](../guides/testing-and-evals.md#record-a-decision-with-its-evidence)
and [the worked example](../../examples/10-recorded-decision/README.md).

## Named credentials

An atom's code never holds a long-lived key. The atom names a credential, and
this later-stage block, planned for v0.7, defines it:

```yaml
atomic: 1
name: report-digests
credentials:
  aws-reports-reader:
    aws_role: arn:aws:iam::123456789012:role/reports-reader
  aws-digest-writer:
    aws_role: arn:aws:iam::123456789012:role/digest-writer
    duration: 15m
```

| Key | Default | Meaning |
| --- | --- | --- |
| `aws_role` | Required | The role Atomic assumes when a step that names this credential is dispatched |
| `duration` | `15m` | How long the issued keys last; `15m` is the shortest AWS issues |

The keys go to that step's code and nowhere else. A role ARN is not a secret,
but it differs between environments; define it in a profile overlay when it
does. See [give code short-lived credentials](../guides/scripts-and-cloud-sdks.md).

## Settings for atom code

Atom code and scripts run on Deno with only what each atom declares. Atomic
downloads and verifies its own pinned Deno, so nothing needs installing. This
later-stage block, planned for v0.7, tightens the project-wide defaults:

```yaml
atomic: 1
name: report-digests
runtime:
  deno:
    minimum_dependency_age: 2d
    allow_packages: ["npm:@aws-sdk/*"]
```

| Key | Default | Meaning |
| --- | --- | --- |
| `binary` | `managed` | Which Deno runs the code: the copy Atomic downloads and verifies, `system` for the `deno` on your `PATH`, or a path. Any choice must be a version Atomic supports |
| `lock` | `deno.lock` | The lockfile every run must match, relative to the manifest |
| `minimum_dependency_age` | `1d` | How old a release must be before it can be locked; hours or days, such as `12h` or `7d` |
| `install_scripts` | None | Packages allowed to run install scripts, named one by one |
| `allow_packages` | Any pinned package | When set, the only packages atom code may import, such as `npm:@aws-sdk/*` |
| `audit_permissions` | `false` | Record each permission the code uses in the step's trace |

No key here can widen what an atom's code may do beyond its own declaration.
See [security defaults and settings](../guides/atom-code-runtime.md)
and [the S3 example](../../examples/11-aws-s3/README.md).

## Workspace example

This later-stage fragment discovers member projects:

```yaml
atomic: 1
name: commerce
workspace:
  members: [services/*, shared]
```

Each member has its own manifest and namespace. See
[the workspace example](../../examples/06-workspace/README.md) for complete files
and its current availability limitation.

Keep secrets and runtime databases out of the manifest. Configuration overlays
and managed requirements are described in [configuration](configuration.md);
source, cache and state locations are in [project structure](../guides/project-structure.md).
