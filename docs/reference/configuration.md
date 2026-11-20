# Configuration

Atomic starts with conventions and detected capabilities. Add configuration when
you need to change a decision, and inspect the result with:

```sh
atomic config --explain
```

The output identifies each setting's value, source and reason, with secrets
masked. Configuration explanation is part of the intended first-release CLI.

## Start small

```yaml
atomic: 1
name: note-tools
```

The [manifest reference](manifest.md) covers discovery globs, workspaces, packages
and other staged settings. [Budgets](budgets.md) can be overridden on an atom or
workflow without copying every default into the manifest.

That is enough to identify a reusable-task project. It does not need explicit
collection globs, cache paths or a database URL for local use.

## Precedence

Values are resolved from lower to higher priority:

1. Built-in defaults and detected capabilities.
2. Committed `atomic.yaml`.
3. Gitignored `atomic.local.yaml`.
4. Selected `atomic.<profile>.yaml`.
5. Supported `ATOMIC_*` environment variables.
6. Command-line overrides.

Managed requirements constrain the final result. No layer can grant a permission
that managed policy forbids. Detection identifies an available integration; it
does not grant credentials or mutation rights.

The profile-selection and general environment-key mapping syntax are still to be
finalized. This draft documents only established names rather than guessing flags.

## Common conventions

| Setting | Local behavior |
| --- | --- |
| Model access | Planned ChatGPT subscription sign-in for Codex-backed coding, API-key access including `OPENAI_API_KEY`, and local providers in stages |
| Storage | Local SQLite and user-data storage without configuring Postgres |
| Atom kind/version | Codex / version 1 |
| Atom permissions | Read-only workspace and no shell network unless allowed overrides apply |
| Generated files | A project generated-cache directory; not runtime state |
| Personal settings | A gitignored local file or user configuration |

Interactive coding permissions are a separate session policy from read-only atom
defaults. See [coding with Atomic](../guides/coding-with-atomic.md).

## Search and service connections

[Native web search](../guides/web-search.md) uses Codex search settings, separately
from command-network permissions. [Slack](../guides/slack.md) and similar services
need their own authorized connection and a per-atom tool selection. Model
subscription login does not authorize those services. Atomic's default workflow
search mode and service-connection configuration remain to be finalized.

## Keep application and runtime databases separate

`ATOMIC_DATABASE_URL` selects Atomic's team-state database. `DATABASE_URL` is not
used for that purpose because it commonly belongs to the application being worked
on. Application data sources are separately declared capabilities.

For signing in with ChatGPT (`atomic login`) and API keys, see
[subscriptions and authentication](../getting-started/authentication.md).

## Keep credentials out of definitions

Use environment variables, a login flow or an approved secret backend. Shared
files name capabilities and credential references; they should not contain secret
values. Use local settings for personal choices, and keep those files gitignored.

For layout and path resolution, see [project structure](../guides/project-structure.md).
For deployment roles, see [running Atomic](../operations/running-atomic.md).
