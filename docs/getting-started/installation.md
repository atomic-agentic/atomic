# Installation

Atomic packages the [Codex foundation](../concepts/architecture.md#what-comes-from-open-source-openai-codex)
with its own framework in one executable.

Its local experience is designed around one Atomic executable and a model
connection. You do not need Python, a JavaScript SDK, Postgres, Docker or a cluster
to begin using the CLI. Tools needed by your app, such as Node or Rust, remain
prerequisites for building that app.

## Release status

No Atomic release has been published here: there is no download location and no
supported installation command. The check under
[verifying a download](#verifying-a-download) is what an archive from a
published release will face; there is nothing to check against yet.

Atomic itself is in this repository and builds from source. With a container
engine, git and curl on the host, from a checkout:

```sh
scripts/dev.sh cargo build --release --locked -p atomic-agentic
```

That writes `target/release/atomic`, a Linux executable (the container is a
Linux image): the package is `atomic-agentic`, the program it builds is
`atomic`, and `atomic --help` lists its commands.
[Developing atomic](../development.md) covers the development container, the
pinned toolchain and the prebuilt V8 the build links.

Which commands are built and which are a later stage's plan is listed in
[availability](../reference/availability.md); read it before trying a feature.

## What you will need

- A supported development machine. Linux and macOS are the initial local targets;
  Windows support is planned on a best-effort basis.
- Model access through an eligible ChatGPT subscription with Codex access, an
  API key, or a local OpenAI-compatible endpoint. Codex atoms take the
  subscription sign-in from `atomic model login`, which atomic accepts on Linux
  for now; the API-key path detects `OPENAI_API_KEY`.
- Git that supports `--no-lazy-fetch` for worktree and fixture operations.
  On macOS, install a current Git with Homebrew if the bundled Git lacks that
  option. Atomic looks for Homebrew Git at its standard installation paths.
- Git and a container engine (Docker, or Podman) to build Atomic from source; the
  executable itself runs with no container. `scripts/dev.sh` also wants `curl`,
  which fetches the pinned V8 archive the first time its cache needs it; the
  Rust toolchain and the other build tools are in the
  [development container](../development.md#the-development-container).

Atomic's Astro compiler is built into the binary and runs on the V8 engine it
embeds, so `atomic check` and `atomic render` need no Deno, Node or npm. From
v0.7, atom code and scripts run on a [Deno](../guides/atom-code-runtime.md)
that Atomic downloads and verifies itself the first time a project needs it.

Read [subscriptions and authentication](authentication.md) for model access:
`atomic model login` signs Codex atoms in with ChatGPT, and an endpoint with an API key
serves any atom. Keep credentials out of atom files and committed configuration.
To use a local model, set `ATOMIC_MODEL_BASE_URL` to the server's OpenAI-compatible
endpoint and `ATOMIC_MODEL` to the model it serves. If the server requires an API
key, set `ATOMIC_MODEL_API_KEY_ENV` to the name of the variable that holds it.
Lite atoms use Chat Completions on local servers, and Codex atoms require the
Responses API. See [models](../models.md) for compatibility details.

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

In a fresh project, `atomic init` creates the lite atom `summarize-note` and
one sample eval case. Configure an [endpoint for lite atoms](authentication.md#lite-atoms),
then record a model answer for the sample.

```sh
atomic eval --live
```

The command calls the configured model and writes its answer to the sample's
cassettes. A passing result includes `PASS summarize-note: 1 of 1 cases passed`.
Keep the same endpoint URL and model name and replay the answer.

```sh
atomic eval
```

Replay checks the recorded answer without calling the model. See the
[eval reference](../reference/eval-cases.md#cases-golden-fixtures-and-recordings)
for cassette behavior.

Continue with the [CLI quickstart](quickstart.md), or go straight to
[your first atom](first-atom.md) for the planned v0.1 automation path.
