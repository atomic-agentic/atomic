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
| 5 | `atomic.<profile>.yaml`, when a profile is named | the project | yes |
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
such as one left in `/tmp`, or one that is a hard link, still counts its
directory as a checkout holding the project, and a well-formed worktree link in
it is still followed, through no link, but nothing wrong with it refuses
anything. What it names still counts, though: with your home itself a checkout
(a `~/.git`), a well-formed link left in `/tmp` naming your home's repository
refuses your own configuration for projects under `/tmp`. It holds the same
settings as `atomic.yaml`, and the project's files override it — except the
`budget` caps and prices, which are ceilings a project can only tighten, as
[Budgets](budgets.md#your-own-caps-are-ceilings) describes.

The directory is found as the operating system gives it, the same way for the
denial of it to every atom's commands and the refusal of a state directory
inside it, so the three cannot name different directories: `XDG_CONFIG_HOME`,
else `HOME` with `.config` below it, else the home the password database gives
this user when the environment names no absolute one. A relative
`XDG_CONFIG_HOME` names no directory and gives way to `HOME`, as the XDG rule
says.

Set `ATOMIC_PROFILE=nightly` or pass `--profile nightly` to `atomic config`,
`check`, `run`, `eval`, `serve` or `runs resume`. The flag wins over the
variable; `--profile ''` clears a variable's selection. Atomic reads
`atomic.nightly.yaml` beside the resolved project's manifest, after
`atomic.local.yaml` and before environment variables and setting flags.
Run from a workspace member to select that member's profile file. `check`
validates the selected profile of the root and every member. A selected file
must exist; the error says where to create it. On Linux and macOS it must be
a regular file: symlinks, even to another file in the project, are refused
by the read itself. Other platforms refuse selected profiles until they have
a secure file reader. Profile names start with a
lowercase ASCII letter and use lowercase letters and digits, with single `-`
or `_` separators. `local` is reserved for `atomic.local.yaml`.

`config --explain` names the selected profile and where it came from, and
attributes each resolved setting to its file, variable or flag. A profile is
a project file: it cannot choose the operator's database URL or API key
variable, and the operator's budget caps still bind it. `serve` and `runs
resume` resolve a profile for their current process and store selection;
they do not replace the public context already recorded for a finished step.
The `--profile` flag on `atomic render` currently names a *context* profile,
which is a different input; see [atom files](atom-files.md#context-schemas).

### How files merge

Files are YAML, read with atomic's restricted loader: no anchors, aliases, tags
or duplicate keys, and no plain scalar whose spelling YAML 1.1 and YAML 1.2 do
not read the same way. Plain `yes`, `no`, `on` and `off` are refused, so quote
them, and so is every one of these numbers:

- one with a leading zero (`0200`, which YAML 1.1 reads as an octal, 128, and
  YAML 1.2 as decimal; `007`, which both read as 7, is refused for the same
  spelling);
- a sign written on either side of a base prefix (`-0x1F`, which YAML 1.1 reads
  as -31, and `0x-1F`, which no version reads as a number);
- one YAML 1.1 reads alone: binary (`0b11`), an underscore in a whole number
  (`1_000`, `0x_1F`) and the sexagesimal (`12:30`, which 1.1 reads as 750).

Quoting any of them gives the string it is read as elsewhere, and `0o200` and
`0x1F` are the ways to write an octal and a hexadecimal. Mappings merge key by
key; any other value replaces the one below it. A setting atomic does not know is
an error that names the file and the line, so a typo is never silently ignored.

### How a variable names a setting

An environment variable names a setting by its path: `model.base_url` is
`ATOMIC_MODEL_BASE_URL`. Matching ignores case and the separators `_`, `-` and
`.`, so `ATOMIC_MODEL_BASEURL` works too. A variable that matches no setting is
left alone: atomic uses other `ATOMIC_*` variables for its own purposes. A
variable set to the empty string sets nothing, as if it were unset.

A value takes the setting's type: `true` or `false` (or `1` and `0`) for
switches, a number for numbers, JSON for lists and sections. A variable atomic
cannot read as its setting's type is refused by its own name, before the layers
are merged, and so is a number spelled a way the loader refuses: `0200` names
its leading zero rather than quietly becoming the text `0200`. A variable for a
string setting is taken as the string it holds, so `ATOMIC_EXECUTOR_ID=1` is the
identity `1`.

## What nothing sets

What no layer sets, atomic looks for, and says so as `detected:` in
`--explain`. Detection discovers; it never grants a permission or a credential.

| Found | Sets |
| --- | --- |
| `OPENAI_API_KEY` is set | the model endpoint to OpenAI's API (`https://api.openai.com/v1`), or to the URL `OPENAI_BASE_URL` names when that is set too, `model.api_key_env` to `OPENAI_API_KEY`, and `model.name` to `gpt-5-mini` unless one is set |
| Ollama answers on this machine, at `127.0.0.1:11434` | the model endpoint to `http://127.0.0.1:11434/v1`, and `model.name` to the first model it lists unless one is set |

`OPENAI_BASE_URL` is read as OpenAI's SDKs read it, and only from your
environment: no file, the project's or your own, sets it, since it decides
where the key is sent. So whatever sets your environment decides where the
key goes, a shell profile or a tool that loads variables for a directory,
such as direnv or a `.env` loader, among them. It must be an `https` URL, or
`http` to a loopback IP address (`127.0.0.1` or `[::1]`; a name such as
`localhost` is refused), with no user, password, query or fragment in it.
Anything else detects no endpoint: a command that would send the key stops
and says why, and one that sends none, such as `atomic check`, carries on.
It is used only with `OPENAI_API_KEY`, when detection runs; otherwise it is
said to be set and not used. `--explain` names both variables as the
source, and a run says that the key is sent, and to which host.

The key's value is never read for detection. Looking for Ollama is one
`GET /api/tags` to the loopback address, which must answer within a second, and
nothing else is sent anywhere. `ATOMIC_DETECT_OLLAMA_ADDR` names another loopback
address, as an IP address and a port; any other address is refused and said. No
key is sent to a detected Ollama, even when `model.api_key_env` is set.
To use a different key variable, set `model.base_url` to the endpoint that
issued that key.

A model endpoint set in a file, a variable or a flag turns detection off, and so
does a ChatGPT sign-in (`atomic model login`): a configured endpoint comes first,
then the sign-in, for Codex atoms, then detection. A sign-in is personal, so
atomic declines to use it where the store is shared or under `atomic serve`,
rather than quietly falling back to something else, and a sign-in works on
Linux only. If changing `ATOMIC_CREDENTIAL_STORE` leaves a sign-in in the
other store, atomic asks you to select that store or log out before detecting
another account.

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
| `exec.connections_per_command` | `ATOMIC_EXEC_CONNECTIONS_PER_COMMAND` | |

`ATOMIC_MODEL` also names the model, over a file that set one; a flag and
`ATOMIC_MODEL_NAME` come after it. An empty `model.base_url` is no endpoint.

`runner` is `local` or `process`. `backend_wait` is a whole number of seconds, at
least 1. An empty `ATOMIC_BACKEND_WAIT` sets nothing, but a null, `~` or blank
value, in a file or a variable, is refused, so a higher layer cannot remove a
limit a lower one set: to override it, set a larger number. Without it, a step
waits for as long as it takes.

`exec.connections_per_command` is how many connections an exec atom's command
that declares hosts may hold through its proxy at once: 64 unless set, and a
whole number from 1 to 128 when it is. One more is closed at once. A
connection the proxy has not answered within 10 seconds of its first request
is closed too, and atomic reserves open files for each command by this number;
[network access](atom-files.md#network-access) says how connections are
counted.

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

## Models

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

For `atomic run --graph-epic`, the operator can route each claimed work item
to a named endpoint. Put `model.lanes` in your own configuration file, not in
the project's `atomic.yaml`:

```yaml
model:
  lanes:
    endpoints:
      local:
        base_url: http://127.0.0.1:11434/v1
        name: local-coder
      remote:
        base_url: https://models.example.com/v1
        name: review-coder
        api_key_env: REVIEW_MODEL_KEY
    routes:
      - kind: task
        labels_any: [security, durability]
        endpoint: remote
    default: local
```

Routes read the item's host graph kind and labels, in written order. The run
records one endpoint and model choice before each lane's first model turn;
author, reviewer and repair turns in that lane use the recorded choice. Editing
routes changes later lanes only. The endpoint catalog is frozen at run
admission, so changing an endpoint requires a new run. Endpoint model names
must be distinct because the budget price table is keyed by model name.
`api_key_env` records only the variable name; the key's value stays in the
operator's environment. Ordinary runs continue to use `model.base_url` and
`model.name`.

Lite calls accept response bodies up to 8 MiB, including error responses.

A lite atom's key goes only to the endpoint `model.base_url` names, and the same
holds for the key `atomic eval --live` forwards while recording. The client
follows no redirect: an endpoint that answers with one fails the call, which
says where it points, without its query or credentials. It suggests a
`model.base_url` to set only when the redirect stays on the same host, keeps
`https`, and keeps the call's own path; a redirect from `https` to plain `http`
or to another host is said as such. While recording, `atomic eval --live`
passes the redirect back without its location, so the call says only that it
was redirected. An endpoint on this machine over plain `http`, a loopback or
unspecified address or `localhost` and the names under it, is reached
directly, never through a proxy that `HTTP_PROXY` or `ALL_PROXY` names, which
would see the key in cleartext. No other name is resolved to find out: a host
name that resolves to this machine, such as one that `/etc/hosts` maps to
`127.0.1.1`, counts as elsewhere and uses the proxies the environment names, so
put it in `NO_PROXY`.

A Codex atom calls through Codex's own client, which atomic cannot tell to skip
a proxy or to refuse a redirect. When a key would go to an endpoint on this
machine over plain `http` and the client would send it through a proxy that
`HTTP_PROXY`, `http_proxy`, `ALL_PROXY` or `all_proxy` names, the atom's turn is
refused before anything is sent, naming the variable. Whether `NO_PROXY` exempts
the host is decided by the client's own rules, so the entry must name the host
as the URL does: `localhost` does not exempt `127.0.0.1`, `*` exempts names but
not addresses, and `NO_PROXY`, when it is set at all, hides `no_proxy`. Add the
host to `NO_PROXY` (for example `NO_PROXY=127.0.0.1`) or use an `https`
endpoint.

On any platform but Linux, the system's proxy settings can carry the key too,
and atomic cannot read them. With Codex's default, Codex's client may take a
proxy from them where the variables name none, and `NO_PROXY` still applies:
the turn is refused unless `NO_PROXY` exempts the host. With Codex's
`respect_system_proxy` feature on, the platform's settings and their own bypass
list decide and `NO_PROXY` changes nothing, so the turn is always refused: turn
the feature off, or use an `https` endpoint. On Linux the feature finds no
platform settings, and Codex reads the proxy variables itself: the upper-case
name of each pair wins whenever it is set, an empty value means none, and any
other value of `HTTP_PROXY` or else `ALL_PROXY` is taken as the proxy, even one
with spaces or a line break around it, so atomic refuses the turn for it unless
`NO_PROXY` exempts the host.

This machine is recognised as for a lite atom: a host name that only resolves
here is not refused and goes through the proxy, so put it in `NO_PROXY`
yourself.

Redirects stay open: Codex's client follows one, so an `https` endpoint that
redirects to `http` on the same host and port receives the key again in
cleartext. Point a Codex atom only at an endpoint you trust not to redirect.

For a lite or a Codex atom alike, a key sent over plain `http` to an endpoint
elsewhere than this machine is not refused: it travels in cleartext across the
network, and through any proxy on the way. Use `https` for any endpoint that is
not on this machine.

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
| `ATOMIC_HOME` | where every project's state is kept, instead of the platform's user data directory; it may not be your own atomic configuration directory (`$XDG_CONFIG_HOME/atomic`, by default `~/.config/atomic`) or a directory inside it, which every atom's commands are denied. A home that leads there is refused at start, before anything is created in it — whether it leads there directly, by a link, by `..`, or from the directory the command was run in — and so is a home whose path atomic cannot resolve. So is a configuration directory atomic cannot resolve: if `HOME`, or `XDG_CONFIG_HOME`, names a directory that cannot be entered, that passes through something which is not a directory, that is a link leading nowhere, or that cannot otherwise be looked up, atomic cannot tell where its state would go relative to it and refuses to run, and only fixing `HOME` or `XDG_CONFIG_HOME` helps there, not `ATOMIC_HOME`. Every `atomic` command refuses it but `atomic model`, which keeps nothing in a project's state, the `runner` role, which runs one command for a control plane, and the help and version output, which run nothing |
| `ATOMIC_CREDENTIAL_STORE` | `file`, to keep credentials in `credentials` inside your own atomic configuration directory — the same directory layer 2 reads your own settings from and every atom's commands are denied — or, with `ATOMIC_HOME` set, in `$ATOMIC_HOME/credentials`; the OS keyring is the default, and `keyring` says so. The checkpoint key is kept beside them, in `keys` in the configuration directory, or `$ATOMIC_HOME/keys`. An older atomic kept both, without `ATOMIC_HOME`, in `atomic` in the platform's configuration directory (on macOS `~/Library/Application Support/atomic`, on Windows under `%APPDATA%`): those places are still denied to every atom's commands, whether they exist or not, and while the old place still holds them (anything in `credentials`; `context.key` in `keys`) and the new one holds none, even as an empty directory, atomic refuses to use the new place, naming both: move what the old directory holds into the new one. An old place atomic cannot read counts as holding them, and a new one it cannot read refuses too, until its permissions are fixed. With a relative `HOME`, the old places resolve under the directory the command was run in, and one that is missing there and would be made inside a directory an atom's commands may write refuses the turn. With neither `HOME` nor a home in the password database, as on Windows, there is no configuration directory: set `ATOMIC_HOME` |
| `ATOMIC_DETECT_OLLAMA_ADDR` | the loopback address to probe for Ollama |
| `ATOMIC_EXEC_MAX_FILE_BYTES`, `ATOMIC_EXEC_MAX_STEP_BYTES` | how much an exec atom's command may hand over by reference: 1 GiB a file and 4 GiB a step by default, in bytes; see [large results go by reference](atom-files.md#large-results-go-by-reference) |

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
