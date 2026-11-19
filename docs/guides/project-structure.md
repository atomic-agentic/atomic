# Project structure

Add a project manifest when you want file-defined atoms and workflows. Ordinary
interactive coding has a lower entry bar: open a directory and start the CLI.

`atomic init` selects a conventional layout. You can override discovery later;
you do not need to choose a package architecture first.

## In an existing application

Keep automation beside the app in `.atomic/`:

```text
my-app/
  src/
  .atomic/
    atomic.yaml
    atoms/
    workflows/
    evals/
```

## In a dedicated automation project

Use top-level source directories:

```text
note-tools/
  atomic.yaml
  atoms/
    summarize-note.atom.astro
  workflows/
    summarize-and-review.workflow.yaml
  evals/
    summarize-note/
      export-fix.yaml
```

The manifest can remain this small:

```yaml
atomic: 1
name: note-tools
```

## Follow the useful conventions

| Source | Convention |
| --- | --- |
| Atom | `*.atom.astro`; the stem supplies its default name and the name of its generated types, so it is unique in a project |
| Workflow | `*.workflow.yaml`; the stem supplies its default name |
| Atom schemas | Inline, or adjacent `<name>.input.schema.yaml` and `<name>.output.schema.yaml`; `.schema.json` is also accepted |
| Variant | `<name>.<variant>.atom.astro`, naming the same schemas as the atom explicitly · v0.5 |
| Evals | `evals/<resolved-atom-name>/` under the manifest directory; one case per `.yaml` file, or many per `.jsonl` file |
| Shared schemas | `schemas/**/*.schema.yaml`, or `.json` |
| OpenAPI toolsets | `toolsets/**/*.openapi.yaml` or `.json` |
| Shared prompt fragment | A component such as `ReviewRules.astro`, imported by an atom |
| Code tests | `test/*_test.ts` under the manifest directory, run by `atomic test` · v0.7 |
| Atom with code | A separate script named by an exec atom; packages are pinned in `deno.lock` beside the manifest · v0.7 |

A default name changes when its file is renamed. Set `name` explicitly when you
want identity to remain stable across that move. Names are unique within a scope;
package names are namespaced.

## Know which directory a path uses

CLI file arguments are relative to the directory where you run the command.
Manifest globs and paths are relative to the manifest. Atom/workflow file
references such as component imports are relative to their source file. JSON Schema `$ref`
resolution follows the schema's base URI and any `$id`.

Atomic finds the nearest `atomic.yaml` or `.atomic/atomic.yaml` while walking up
from your current directory. Defining both forms in one directory is an error.
A nested project is a discovery boundary; workspace members are loaded explicitly.

## Keep state out of source control

Commit the definitions, evals, fixtures, package lockfile, `deno.lock` when
atoms carry code and, once it exists, the generated `atomic.policy.lock`. Generated editor
types currently live under project-root `.atomic/types`, including the
[types of shared schema files](../reference/atom-files.md#types-from-shared-schema-files); other generated editor
artifacts use the configured cache exclusion. Checkpoints, transcripts,
artifacts and worktrees live in Atomic's user-data directory rather than beside
your application source.

Use environment variables or a secret backend for credentials. Personal settings
can live in a gitignored `atomic.local.yaml`.

## Grow to a workspace later

A workspace manifest can list member projects and load shared schema contracts.
Start with one project unless you need that boundary. The
[workspace example](../../examples/06-workspace/README.md) shows shared contracts
without duplicating each member's defaults.

Next: [configuration reference](../reference/configuration.md) or
[atom file reference](../reference/atom-files.md).
