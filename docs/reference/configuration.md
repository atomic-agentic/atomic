# Configuration

Atomic starts with conventions and detected capabilities. Add configuration when
you need to change a decision, and inspect the result with:

```sh
atomic config --explain
```

The output identifies each setting's value, source and reason, with secrets
masked. A URL, the database's or the model endpoint's, is shown by its scheme,
host, port and path only: its user and password become `***`, a query becomes
`?***`, and a value that does not read as a URL is shown as `(set; not shown)`.
An API key's variable is named, with whether it is set, never its value; a
value that is not a variable's name is not shown either. An error about a
setting names it, never the value it found. `--json` gives the same as one
document. Without `--explain`, `atomic config` lists the values alone.

## Detected defaults

What nothing sets, atomic looks for, and says so as `detected:` in
`--explain`. Detection discovers; it never grants a permission or a
credential.

| Found | Sets |
| --- | --- |
| `OPENAI_API_KEY` is set | the model endpoint to OpenAI's API (`https://api.openai.com/v1`), `model.api_key_env` to `OPENAI_API_KEY`, and the model to `gpt-5-mini` unless one is set |
| Ollama answers on this machine, at `127.0.0.1:11434` | the model endpoint to that Ollama, with the first model it lists unless one is set |

The key's value is never read for detection. Looking for Ollama is one
`GET /api/tags` to the loopback address, which must answer within a second,
and nothing else is sent anywhere; `ATOMIC_DETECT_OLLAMA_ADDR` names another
loopback address, as an IP address and port, and any other address is
refused. No key is sent to a detected Ollama, even when `model.api_key_env`
is set. A model endpoint set in a file, a variable or a flag turns detection
off.

When the endpoint was detected, or set by the project's files rather than by
whoever runs it, `atomic run`, `atomic runs resume` and `atomic serve` say
where model calls go in one line on stderr.

The settings `atomic run`, `atomic runs resume` and `atomic serve` resolve:

| Setting | Variable | Flag |
| --- | --- | --- |
| `model.base_url` | `ATOMIC_MODEL_BASE_URL` | `--model-base-url` |
| `model.name` | `ATOMIC_MODEL` or `ATOMIC_MODEL_NAME` | `--model` |
| `model.api_key_env` | `ATOMIC_MODEL_API_KEY_ENV` | `--model-api-key-env` |
| `database.url` | `ATOMIC_DATABASE_URL` | |
| `database.schema` | `ATOMIC_DATABASE_SCHEMA` | |
| `runner` | `ATOMIC_RUNNER` | `--runner` |
| `executor_id` | `ATOMIC_EXECUTOR_ID` | `--executor-id` |
| `budget.*` | `ATOMIC_BUDGET_*` | |

`model.api_key_env` names the variable whose value is sent to the endpoint as
its key, so it is the choice of whoever runs the project: set it in
`atomic.local.yaml`, the variable or the flag. A committed `atomic.yaml` that
sets it is refused. A variable set to the empty string sets nothing, and a
variable for a string setting, such as `ATOMIC_EXECUTOR_ID=1`, is taken as the
string it holds.

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

For upstream subscription support and Atomic login status, see
[subscriptions and authentication](../getting-started/authentication.md).

## Keep credentials out of definitions

Use environment variables, a login flow or an approved secret backend. Shared
files name capabilities and credential references; they should not contain secret
values. Use local settings for personal choices, and keep those files gitignored.

For layout and path resolution, see [project structure](../guides/project-structure.md).
For deployment roles, see [running Atomic](../operations/running-atomic.md).
