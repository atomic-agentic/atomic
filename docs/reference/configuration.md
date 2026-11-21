# Configuration

Atomic starts from conventions and from what it finds around it. Add
configuration when you need to change a decision, and read back what you got:

```sh
atomic config --explain
```

Everything below is built unless it says otherwise.
[Availability](availability.md) is the summary for the whole product.

## Reading what resolved

`atomic config` lists the settings the project resolves to, here and now.
Without `--explain` it lists the values alone. With it, each line names where
the value came from, and for a setting nothing set, why atomic concluded it:
the source reads `detected: …`. `--json` gives the same as one document,
redacted as everything atomic prints. The manifest's own keys, which share
`atomic.yaml`, are not settings and are not listed.

Secrets are masked. A URL, the database's or the model endpoint's, is shown by
its scheme, host, port and path only: a user and a password become `***`, a
query becomes `?***`, a path segment long enough or odd enough to be a key
becomes `***`, and a value that does not read as a URL with a host is shown as
`(set; not shown)`. An API key's variable is named, with whether it is set,
never its value; a value that is not a variable's name is not shown either. An
error about a setting names the setting, never the value it found, which may be
a secret put in the wrong place.

A ChatGPT sign-in appears as `model.sign_in`: how it is kept and where, never a
token or an email, and whether model atoms use it here. It says so when it is
not used, because a model endpoint is configured or because the store is shared.

Run outside a project, `atomic config` resolves your own configuration, the
variables and the sign-in, and detects; no project's files are read.

## The layers

A value is resolved from lower to higher. Each layer overrides the one below it,
setting by setting.

| Layer | Source | Set by | Built |
| --- | --- | --- | --- |
| 1 | Built-in defaults | atomic | yes |
| 2 | Your own configuration: `$XDG_CONFIG_HOME/atomic/config.yaml`, by default `~/.config/atomic/config.yaml` | you, on this machine | yes |
| 3 | `atomic.yaml` | the project, committed | yes |
| 4 | `atomic.local.yaml`, personal and not committed | whoever holds the checkout | yes |
| 5 | `atomic.<profile>.yaml`, when a profile is named | the project | the loader supports it; **nothing selects a profile yet** |
| 6 | `ATOMIC_*` environment variables | whoever runs the command | yes |
| 7 | Command-line flags | whoever runs the command | yes |

Layer 2 is the operator's own file: the person who runs the command, not the
project. No project supplies it, and atomic refuses to read one that lies inside
the project or inside any checkout around it (a submodule's superproject, the
checkout a worktree sits in, or a linked worktree's main checkout), since the
project could have supplied it; point `XDG_CONFIG_HOME`, or `HOME`, outside it.
That includes the default `~/.config/atomic` when your home directory is itself
a git checkout (a `~/.git`) holding the project. A checkout whose `.git` file or
`commondir` is not a small regular file, as git writes them, refuses your own
configuration, and the sign-in, until it is fixed. A `.git` another user owns,
such as one left in `/tmp`, still counts its directory as a checkout holding the
project, but what it links to is not read. It holds the same
settings as `atomic.yaml`, and the project's files override it — except the
`budget` caps and prices, which are ceilings a project can only tighten, as
[Budgets](budgets.md#your-own-caps-are-ceilings) describes.

Selecting a configuration profile is planned. The loader reads
`atomic.<profile>.yaml` when a profile is named and refuses the run when the
file is missing, but no command names one yet. The `--profile` flag on
`atomic render` names a *context* profile, which is a different thing; see
[atom files](atom-files.md#context-schemas).

### How files merge

Files are YAML, read with atomic's restricted loader: no anchors, aliases, tags
or duplicate keys, and plain `yes`, `no`, `on` and `off` are refused, so quote
them. Mappings merge key by key; any other value replaces the one below it. A
setting atomic does not know is an error that names the file and the line, so a
typo is never silently ignored.

### How a variable names a setting

An environment variable names a setting by its path: `model.base_url` is
`ATOMIC_MODEL_BASE_URL`. Matching ignores case and the separators `_`, `-` and
`.`, so `ATOMIC_MODEL_BASEURL` works too. A variable that matches no setting is
left alone: atomic uses other `ATOMIC_*` variables for its own purposes. A
variable set to the empty string sets nothing, as if it were unset.

A value takes the setting's type: `true` or `false` (or `1` and `0`) for
switches, a number for numbers, JSON for lists and sections. A variable atomic
cannot read as its setting's type is refused by its own name, before the layers
are merged. A variable for a string setting is taken as the string it holds, so
`ATOMIC_EXECUTOR_ID=1` is the identity `1`.

## What nothing sets

What no layer sets, atomic looks for, and says so as `detected:` in
`--explain`. Detection discovers; it never grants a permission or a credential.

| Found | Sets |
| --- | --- |
| `OPENAI_API_KEY` is set | the model endpoint to OpenAI's API (`https://api.openai.com/v1`), `model.api_key_env` to `OPENAI_API_KEY`, and `model.name` to `gpt-5-mini` unless one is set |
| Ollama answers on this machine, at `127.0.0.1:11434` | the model endpoint to `http://127.0.0.1:11434/v1`, and `model.name` to the first model it lists unless one is set |

The key's value is never read for detection. Looking for Ollama is one
`GET /api/tags` to the loopback address, which must answer within a second, and
nothing else is sent anywhere. `ATOMIC_DETECT_OLLAMA_ADDR` names another loopback
address, as an IP address and a port; any other address is refused and said. No
key is sent to a detected Ollama, even when `model.api_key_env` is set.

A model endpoint set in a file, a variable or a flag turns detection off, and so
does a ChatGPT sign-in (`atomic model login`): a configured endpoint comes first,
then the sign-in, for Codex atoms, then detection. A sign-in is personal, so
atomic declines to use it where the store is shared or under `atomic serve`,
rather than quietly falling back to something else, and a sign-in works on
Linux only.

When the endpoint was detected, or set by a project's files rather than by
whoever runs it, `atomic run`, `atomic runs resume` and `atomic serve` say where
model calls go, in one line on stderr.

## The settings a command resolves

These are what `atomic run`, `atomic runs resume`, `atomic serve`, `atomic
gates`, `atomic runs show`, `atomic check`, `atomic eval` and `atomic config`
resolve:

| Setting | Variable | Flag |
| --- | --- | --- |
| `model.base_url` | `ATOMIC_MODEL_BASE_URL` | `--model-base-url` |
| `model.name` | `ATOMIC_MODEL` or `ATOMIC_MODEL_NAME` | `--model` |
| `model.api_key_env` | `ATOMIC_MODEL_API_KEY_ENV` | `--model-api-key-env` |
| `database.url` | `ATOMIC_DATABASE_URL` | |
| `database.schema` | `ATOMIC_DATABASE_SCHEMA` | |
| `runner` | `ATOMIC_RUNNER` | `--runner` |
| `executor_id` | `ATOMIC_EXECUTOR_ID` | `--executor-id` |
| `backend_wait` | `ATOMIC_BACKEND_WAIT` | `--backend-wait` |
| `budget.*` | `ATOMIC_BUDGET_*`, such as `ATOMIC_BUDGET_ATOM_TOKENS` | |

`ATOMIC_MODEL` also names the model, over a file that set one; a flag and
`ATOMIC_MODEL_NAME` come after it. An empty `model.base_url` is no endpoint.

`runner` is `local` or `process`. `backend_wait` is a whole number of seconds, at
least 1. An empty `ATOMIC_BACKEND_WAIT` sets nothing, but a null, `~` or blank
value, in a file or a variable, is refused, so a higher layer cannot remove a
limit a lower one set: to override it, set a larger number. Without it, a step
waits for as long as it takes.

The budget settings are the `budget` section: `budget.atom.*`,
`budget.workflow.*`, `budget.call.*` and the `budget.pricing` table.
[Budgets](budgets.md) gives the complete section and what each limit covers.
Written in your own configuration file, a cap or a price there is a ceiling on
every layer above it; written in a project's file, it is the project's own ask.

## What limits a setting

Three separate things constrain a setting. Only the first two are built.

### Precedence

A higher layer wins over a lower one, layer by layer, as the table above orders
them. That decides *which value* atomic uses, and nothing else. The one exception
runs the other way: the `budget` caps and prices in your own configuration cap
what any layer above may ask for, so the number that binds is the smaller of the
two.

### Settings only the operator may set

Some settings send something of yours wherever they point, or move your records.
They come only from your own configuration, an `ATOMIC_*` variable or a flag. A
project's file that sets one is refused, unless your own variable or flag
overrides it: `atomic.yaml`, and `atomic.local.yaml` whether git tracks it or
not, since either can arrive with a project. `atomic.local.yaml` keeps every
other override.

- `model.api_key_env` names the variable whose value is sent to the endpoint as
  its key. Its value must be a variable's name, never the key itself.
- A key is sent to a `model.base_url` the project's files set only when you set
  the same endpoint yourself, in your own configuration, with
  `ATOMIC_MODEL_BASE_URL` or with `--model-base-url`. Otherwise the key is
  withheld, the reason is said, and a run that would call a model is refused,
  naming both sources.
- `database.url` would move your run records, and the password it may carry, to
  another store.
- A `model.base_url` with a credential in it, a user, a password or a query, is
  refused from a project's files.

### Managed ceilings

Partly built. The `budget` caps and prices in your own configuration narrow what
every layer above may ask for: a project's file, an `ATOMIC_*` variable and a
written `budget` block can bring a cap down to yours, never above it, and a price
you wrote holds where a project's table would replace it
([Budgets](budgets.md#your-own-caps-are-ceilings)). Nothing else limits what a
layer may ask for: no policy file and no managed configuration narrows the choice
a layer can express. The committed `atomic.policy.lock` is planned for v1.0, as
[availability](availability.md) records it. Two things are true today and do not
depend on it: detection grants nothing, and the settings above cannot come from a
project.

## Models: one endpoint today

Built is one model endpoint per command: an OpenAI-compatible API at
`model.base_url`, serving the model named by `model.name`, unless an atom's own
`model` names another, keyed by the variable named by `model.api_key_env`. That
model value is sent to the endpoint as written; a tier name is not mapped yet. A
Codex atom can instead run on your ChatGPT sign-in, which works on Linux only.
A local server such as Ollama, llama.cpp, vLLM or Lemonade is reached the same
way, by its URL, and the model is named as that endpoint spells it:

```yaml
model:
  base_url: http://127.0.0.1:8000/v1
  name: Qwen3-8B
```

Planned is naming a **tier** instead of a model, and mapping tiers to models per
project: `basic`, `fast`, `advanced` and `frontier`, from least to most capable,
as [models](../models.md) describes. The names are settled and appear in the
authoring guidance, but no routing is built: atomic sends whichever model name
it resolved for the atom, the endpoint's or the atom's own, as written, and the
manifest's `models` key is refused by name as not supported yet. The full
mapping arrives with broader model routing at v0.7.

## Storage

With nothing set, atomic keeps a project's state in SQLite in the project's
private state directory. `database.url` (`ATOMIC_DATABASE_URL`) selects
PostgreSQL instead, and `database.schema`, `atomic` by default, names the schema
its tables live in. On PostgreSQL a process must also say who it is:
`executor_id`. See [running Atomic](../operations/running-atomic.md).

### Keep application and runtime databases separate

`ATOMIC_DATABASE_URL` selects atomic's own store. `DATABASE_URL` is never read
for it, because it commonly belongs to the application being worked on; when it
is set, `atomic config` notes that it is ignored. Named application data sources
are planned with the manifest's `datasources` key at v0.7.

## Where settings live

Settings share the project's manifest, `atomic.yaml`, or `.atomic/atomic.yaml`
for an embedded project. The manifest's own keys (`atomic`, `name`,
`collections`, `ignore`, `generated` and `workspace`) are not settings; the
configuration carries them through for the project loader. A key that shares the
file and is not supported yet, `models` or `packages` among them, is refused by
name rather than ignored. See [the manifest](manifest.md).

Run from a workspace's root, atomic uses the root's settings; a member's
`model` or `budget` applies to commands run in that member. `atomic check` reads
the settings of the root and of every member as a run would, without looking for
Ollama, so a setting a run would refuse fails the check.

## Start small

```yaml
atomic: 1
name: note-tools
```

`atomic: 1` is the manifest format version and is required; `name` defaults to
the directory's name. That is enough for a reusable-task project: no collection
globs, cache paths or database URL for local use. [The manifest
reference](manifest.md) covers discovery globs, workspaces and the keys this
version does not read yet.

Conventions that need no configuration:

| Choice | Default |
| --- | --- |
| Model access | one endpoint, or your ChatGPT sign-in for Codex atoms on Linux; a lite atom says so when there is no endpoint |
| Storage | SQLite in the project's state directory, no PostgreSQL to configure |
| An atom's sandbox and network | read-only, and no network, unless the atom declares more |
| Generated editor types | the project's `.atomic/types`, which is not runtime state |
| Personal settings | `atomic.local.yaml`, or your own configuration |

Interactive coding permissions are a separate session policy from these
read-only atom defaults, and belong to the planned interactive client; see
[coding with Atomic](../guides/coding-with-atomic.md).

## atomic's own variables

These are not layered settings: no configuration file sets them, and only the
process environment does.

| Variable | Sets |
| --- | --- |
| `ATOMIC_HOME` | where every project's state is kept, instead of the platform's user data directory; it may not be your own atomic configuration directory (`$XDG_CONFIG_HOME/atomic`, by default `~/.config/atomic`) or a directory inside it, which every atom's commands are denied. A home that leads there is refused at start, before anything is created in it — whether it leads there directly, by a link, by `..`, or from the directory the command was run in — and so is a home whose path atomic cannot resolve. So is a configuration directory atomic cannot resolve: if `HOME`, or `XDG_CONFIG_HOME`, names a directory that is missing, unreadable, or a link that leads nowhere, atomic cannot tell where its state would go relative to it and refuses to run, and only fixing `HOME` or `XDG_CONFIG_HOME` helps there, not `ATOMIC_HOME`. Every `atomic` command refuses it but `atomic model`, which keeps nothing in a project's state, the `runner` role, which runs one command for a control plane, and the help and version output, which run nothing |
| `ATOMIC_CREDENTIAL_STORE` | `file`, to keep credentials in your own atomic directory; the OS keyring is the default, and `keyring` says so |
| `ATOMIC_DETECT_OLLAMA_ADDR` | the loopback address to probe for Ollama |
| `ATOMIC_EXEC_MAX_FILE_BYTES`, `ATOMIC_EXEC_MAX_STEP_BYTES` | how much an exec atom's command may hand over by reference: 1 GiB a file and 4 GiB a step by default, in bytes; see [large results go by reference](atom-files.md#large-results-go-by-reference) |
| `ATOMIC_DENO` | the Deno that runs the Astro compiler, instead of the `deno` found on `PATH` |

## Keep credentials out of definitions

Use environment variables, a login flow or an approved secret backend. Shared
files name capabilities and credential references; they should not hold secret
values. Keep personal choices in a gitignored local file or in your own
configuration.

For signing in with ChatGPT (`atomic model login`) and for API keys, see
[subscriptions and authentication](../getting-started/authentication.md). For
layout and path resolution, see [project structure](../guides/project-structure.md).
For search and service connections, which are configured separately from model
access and are still staged, see [web search](../guides/web-search.md) and
[Slack](../guides/slack.md).
