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

Atomic's Astro compiler is built into the binary and runs on the V8 engine it
embeds, so `atomic check` and `atomic render` need no Deno, Node or npm. From
v0.7, atom code and scripts run on a [Deno](../guides/atom-code-runtime.md)
that Atomic downloads and verifies itself the first time a project needs it.

Read [subscriptions and authentication](authentication.md) for model access:
`atomic model login` signs Codex atoms in with ChatGPT, and an endpoint with an API key
serves any atom. Keep credentials out of atom files and committed configuration.
Local-model setup instructions will accompany the matching release.

## Verifying a download

Each release archive and SBOM is signed with [Sigstore](https://www.sigstore.dev/)
by the repository's release workflow, with no long-lived key: the signature's
certificate names the workflow file and the tag it ran on, and the signature is
recorded in Sigstore's public transparency log. The signature bundle sits beside
each file on the release page as `<file>.sigstore.json`.

To check an archive before unpacking it, download it and its bundle and run
[cosign](https://docs.sigstore.dev/cosign/system_config/installation/) 3 or
later, with the release's tag in place of `v<version>`:

```sh
cosign verify-blob \
  --bundle atomic-<version>-<target>.tar.gz.sigstore.json \
  --certificate-identity https://github.com/atomic-agentic/atomic/.github/workflows/release.yml@refs/tags/v<version> \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  atomic-<version>-<target>.tar.gz
```

It prints `Verified OK` and exits 0 only when the archive is the one the
workflow signed at that tag. On any other result, do not use the file. The SBOM, `atomic-<version>-<target>.cdx.json`, checks
the same way with its own bundle, and it records the SHA-256 of the `atomic`
binary inside the archive ([checking an SBOM](../development.md#releases)).
`SHA256SUMS` lists every file's checksum but is not itself signed; the bundles
are what tie a file to the release.

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
