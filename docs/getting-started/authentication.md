# Subscriptions and authentication

**Subscription sign-in is implemented in upstream Codex.** Atomic plans to offer
that path for eligible local Codex-backed use, alongside API keys. This repository
does not yet implement Atomic login, so the commands below run **Codex**, not Atomic.

## Use an eligible subscription with Codex today

Start the upstream CLI and choose **Sign in with ChatGPT**, or run:

```sh
codex login
codex login status
```

This uses your account's Codex entitlement and limits. API-key authentication uses
separate API billing. Check current eligibility in
[OpenAI's authentication guide](https://learn.chatgpt.com/docs/auth).

For a machine where browser callbacks are unavailable:

```sh
codex login --device-auth
```

Device login must be enabled for the account/workspace. These commands do not
establish an Atomic login or promise automatic credential reuse.

## The planned Atomic experience

For local Codex-backed use, choose subscription sign-in or API-key access, finish
login and begin work. The exact Atomic login commands are not specified yet.
The API-key path detects `OPENAI_API_KEY`; keep its value out of committed files.

Atomic keeps workflow state and transcripts separate from personal Codex sessions.
Automatic reuse of an existing Codex login is not yet specified. Subscription
access for Codex-backed sessions does not establish support for lite atoms,
embeddings or other provider paths; those need their own supported model access.

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
