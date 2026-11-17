# Installation

Atomic packages the [Codex foundation](../concepts/architecture.md#what-comes-from-open-source-openai-codex)
with its own framework in one executable.

Its local experience is designed around one Atomic executable and a model
connection. You do not need Python, a JavaScript SDK, Postgres, Docker or a cluster
to begin using the CLI. Tools needed by your app, such as Node or Rust, remain
prerequisites for building that app.

## Release status

This repository currently contains the product plan and examples. An installable
Atomic release, download location and supported installation command have not
been published here. Installation instructions will be added with the first
release; the commands in these docs describe the target CLI.

See [availability](../reference/availability.md) before trying a feature.

## What you will need

- A supported development machine. Linux and macOS are the initial local targets;
  Windows support is planned on a best-effort basis.
- Model access through an eligible ChatGPT subscription with Codex access, an
  API key, or a supported local provider. Subscription sign-in exists upstream;
  Atomic still needs to expose it. The API-key path detects `OPENAI_API_KEY`.
- Git and your usual build tools for repository work.
- Nothing extra for TypeScript in atoms. From v0.7, Atomic downloads and
  verifies its own pinned [Deno](../guides/atom-code-runtime.md#you-do-not-install-deno)
  the first time a project needs it.

Read [subscriptions and authentication](authentication.md) for the verified
upstream login paths and the planned Atomic integration. Keep credentials out of
atom files and committed configuration. Exact Atomic login commands and local-model
setup instructions will accompany the matching release.

## After installation

For the **v0.5 interactive CLI**, open a directory and start Atomic:

```sh
cd my-app
atomic
```

For **v0.1 file-defined automations**, initialize a project:

```sh
atomic init
atomic config --explain
```

`init` chooses an embedded `.atomic/` layout in an existing application repository
and a top-level layout in an empty directory. Detected configuration is inspectable;
explicit settings take precedence within your organization's policy.

Continue with the [CLI quickstart](quickstart.md), or go straight to
[your first atom](first-atom.md) for the planned v0.1 automation path.
