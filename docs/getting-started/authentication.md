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

The sign-in uses your account's Codex entitlement and limits; API keys use
separate API billing. Check current eligibility in
[OpenAI's authentication guide](https://learn.chatgpt.com/docs/auth). Device
login must be enabled for the account or workspace.

When no model endpoint is configured (`ATOMIC_MODEL_BASE_URL` unset), Codex
atoms use the sign-in, with no API key. Set `ATOMIC_MODEL` to a model your plan
offers for Codex. A configured endpoint always takes precedence.

The sign-in is yours, for local use. atomic keeps it once per user, apart from
every project and from your personal Codex sessions:

- in the OS keyring by default (on Linux, the Secret Service);
- with `ATOMIC_CREDENTIAL_STORE=file`, for a machine with no keyring, in a
  directory readable by you only (`$ATOMIC_HOME/credentials/codex-auth`, or
  `~/.config/atomic/credentials/codex-auth`).

atomic never prints the token, and atoms never see it: Codex sends it to the
model from atomic's own process, which cannot be dumped or read through `/proc`;
the credential directory is denied to every atom's sandbox; and atoms' commands
can neither use the kernel keyrings nor reach the Secret Service. A signed-in
atom has none of ChatGPT's connectors, plugins or hosted tools: the token
reaches the model and nothing else, and an atom's tools are the ones its file
declares. As with the operator credential, this has been
verified on Linux only, so `atomic model login` is refused on macOS and Windows for
now. atomic does not reuse an existing `codex login`.

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
it. With only a sign-in configured, a run that reaches a lite atom stops with a
message that says so and names the settings it needs: `ATOMIC_MODEL_BASE_URL`,
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
