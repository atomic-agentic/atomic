# Subscriptions and authentication

Codex atoms can run on an eligible ChatGPT subscription, signed in through
Codex's own login, or on an API key at any OpenAI-compatible endpoint. Lite
atoms call a model API themselves and always need an endpoint.

## Sign in with ChatGPT for Codex atoms

```sh
atomic model login                # sign in in your browser
atomic model login --device-auth  # with no browser: a code to enter elsewhere
atomic model status               # who is signed in, on which plan, kept where
atomic model logout               # revoke the sign-in and forget every copy
```

`atomic model status` shows the account's email masked (`d***@example.com`),
and creates nothing. `atomic model logout` forgets the sign-in in both the OS
keyring and the file store, whichever `ATOMIC_CREDENTIAL_STORE` chooses now,
and says what it found in each; if the keyring cannot be reached, it says so.

The sign-in uses your account's Codex entitlement and limits; API keys use
separate API billing. Check current eligibility in
[OpenAI's authentication guide](https://learn.chatgpt.com/docs/auth). Device
login must be enabled for the account or workspace.

When no model endpoint is configured (`ATOMIC_MODEL_BASE_URL` unset, and no
`model.base_url` in a configuration file or flag), Codex atoms use the sign-in,
with no API key. Set `ATOMIC_MODEL` to a model your plan offers for Codex. A
configured endpoint always comes first, and the sign-in comes before what atomic
detects: with someone signed in, `OPENAI_API_KEY` and a local Ollama are not
used. `atomic config --explain` shows the sign-in as `model.sign_in`, how and
where it is kept, never a token or an email, and whether it is used; a run says
which model goes to ChatGPT. The sign-in is looked for only when a model will
be called on it: never for a replay or a run with no model atom.

The sign-in is yours, for local use. atomic refuses it on a shared
(PostgreSQL) store, whether a variable or your configuration file names it, and
for `atomic serve`. Nothing takes its place there, neither `OPENAI_API_KEY` nor
a local Ollama: configure `model.base_url` (with `model.api_key_env` naming the
key's variable), or run `atomic model logout` to have atomic detect them again.
On a project's own store it serves `atomic run`, and `atomic runs resume` for
whichever run in that store you resume.

Codex's managed login restrictions hold for the sign-in as they do for Codex's
own: `forced_login_method`, `forced_chatgpt_workspace_id`, and a machine's
managed `allowed_login_methods` and `allowed_chatgpt_workspaces`. Login is
refused, or limited to the allowed workspaces, and a run on a sign-in they do
not allow is refused before its first model call.

atomic keeps the sign-in once per user, apart from every project and from
your personal Codex sessions:

- in the OS keyring by default (on Linux, the Secret Service);
- with `ATOMIC_CREDENTIAL_STORE=file`, for a machine with no keyring, in a
  directory readable by you only (`$ATOMIC_HOME/credentials/codex-auth`, or
  `~/.config/atomic/credentials/codex-auth`).

atomic never prints the token, and no atom's commands see it: Codex sends it
to the model from atomic's own process, which cannot be dumped or read through
`/proc`; Codex atoms' commands and exec atoms' alike run under Codex's sandbox,
which denies them the credential directory; and they can neither use the
kernel keyrings nor reach the Secret Service. A signed-in atom has none of
ChatGPT's connectors, plugins or hosted tools: the token reaches the model and
nothing else, and an atom's tools are the ones its file declares. As with the
operator credential, this has been verified on Linux only, so `atomic model
login` is refused on macOS and Windows for now. atomic does not reuse an
existing `codex login`. A sign-in kept inside a checkout that holds the
project (`ATOMIC_HOME` pointed there, directly or through a link), the
project's own or one around it such as a submodule's superproject or the main
checkout of a linked worktree, is refused, since a repository could have
supplied it. That includes the default location when your home directory is
itself a git checkout (a `~/.git`) holding the project: set `ATOMIC_HOME` to a
directory outside it. A link on the way that leads nowhere is refused too, and
so is a checkout whose `.git` file or `commondir` is not a small regular file,
as git writes them: that refuses your own configuration as well. A `.git`
another user owns, or one that is a hard link, still counts as a checkout, and
is followed, through no link, when it is a well-formed worktree link, but
nothing wrong with it refuses anything. What it names still counts, though:
with your home itself a checkout (a `~/.git`), a well-formed link someone left
in `/tmp` naming your home's repository refuses your default sign-in location
for projects under `/tmp`. A login that
makes the sign-in directory first forgets whatever the store still holds for
it, which outlived an earlier directory and is no sign-in. It is forgotten, not
revoked, and a later logout can no longer revoke it. Under
`ATOMIC_CREDENTIAL_STORE=file` only the file store is forgotten: a keyring
entry stays, and is read again if you go back to the keyring store. A login
that fails removes the directory it made, unless the store then holds a
sign-in saved since it began: one it forgot first, or, if it failed before
forgetting, one a save left lock files for. If the store cannot be read, a
directory holding a lock file stays; if the forgetting itself failed, the
directory goes regardless. One stopped with Ctrl-C leaves the directory, with
nothing in the store. A logout removes the directory once both stores are
checked, so later runs read no store, and removes one it made itself even if
the keyring cannot be checked. Otherwise, while the keyring cannot be checked,
the directory stays, and atomic says the sign-in could not be read. A sign-in
whose directory is there but which cannot be used, because its store cannot be
read or because it is kept where it is refused, refuses a run that calls a
model: an `OPENAI_API_KEY` or a local Ollama is never used in its place, since
model calls would then go to another account.

Keeping the token out of reach has side effects on atomic's own process, and so
on everything it starts:

- `LD_*` variables, such as `LD_PRELOAD` and `LD_LIBRARY_PATH`, are removed
  from its environment, and its commands do not inherit them;
- it writes no core file, and its commands inherit a core-file size limit of
  zero;
- on macOS, it refuses a debugger.

Codex's sandbox and patch helpers, which atomic runs as itself, are left as
they are.

## API keys

Point atomic at an OpenAI-compatible Responses endpoint and name the variable
that holds its key:

```sh
export ATOMIC_MODEL_BASE_URL=https://api.openai.com/v1
export ATOMIC_MODEL=<model>
export ATOMIC_MODEL_API_KEY_ENV=OPENAI_API_KEY
```

Keep the key's value out of committed files.

## Lite atoms

A lite atom calls its endpoint directly, so a ChatGPT sign-in does not cover
it. With only a sign-in configured, a run that would reach a lite atom is
refused before it starts, with a message that says so and names the settings
it needs: `ATOMIC_MODEL_BASE_URL`,
`ATOMIC_MODEL`, and `ATOMIC_MODEL_API_KEY_ENV` if the endpoint asks for a key.
Subscription access for Codex atoms does not establish support for lite atoms,
embeddings or other provider paths; those need their own supported model
access.

## Automation and team deployment

OpenAI recommends API keys for programmatic use and also documents headless login,
advanced trusted account-auth CI setups and Enterprise Codex access tokens.
Subscription authentication is therefore not technically limited to a visible
interactive terminal. See the
[upstream automation guidance](https://learn.chatgpt.com/docs/auth#login-on-headless-devices).

Atomic's server and unattended deployment defaults are API keys, enterprise
providers or workload identity. A documented account-auth path requires explicit
deployment policy, eligible account access and supported credential/identity
handling. A local personal login does not implicitly become a shared service
credential. Choose the authentication path for the deployment and provider you
actually operate.

## Public implementation evidence

The [Codex foundation](../README.md#open-source-openai-codex-underneath-atomic-on-top)
includes reusable authentication code:

| Upstream component | Capability |
| --- | --- |
| [`codex-login` exports](https://github.com/openai/codex/blob/237d8b104ed06ae46c5ed9a54693672dea21856c/codex-rs/login/src/lib.rs) | Browser/device login, auth management and credential storage |
| [Browser login server](https://github.com/openai/codex/blob/237d8b104ed06ae46c5ed9a54693672dea21856c/codex-rs/login/src/server.rs) | OAuth callback and credential persistence |
| [Authentication manager](https://github.com/openai/codex/blob/237d8b104ed06ae46c5ed9a54693672dea21856c/codex-rs/login/src/auth/manager.rs) | ChatGPT/API-key modes, account state, refresh and login restrictions |
| [CLI login commands](https://github.com/openai/codex/blob/237d8b104ed06ae46c5ed9a54693672dea21856c/codex-rs/cli/src/login.rs) | Login, device login, status and logout |

Source snapshot checked November 16, 2026. Live account eligibility follows
OpenAI's current guidance; public code availability does not itself grant access.

Next: [your first atom](first-atom.md), [CLI quickstart](quickstart.md), or
[local/team operation](../operations/running-atomic.md).
