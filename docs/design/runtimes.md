# Code, scripts and runtimes

Whatever Codex can already do in its sandbox works inside a Codex atom: run a
Python script, read a spreadsheet, turn a PDF into text. This page is about
everything else: steps that need no model, files treated as typed values,
toolchains pinned in advance, the code atoms can carry, and how that code is
given the authority its declaration states and nothing beyond it. For the
user-facing view, read [scripts and cloud
SDKs](../guides/scripts-and-cloud-sdks.md) and [how atomic runs and confines
atom code](../guides/atom-code-runtime.md).

## Exec atoms

An atom with `kind: 'exec'` runs a script or a command, takes typed JSON in,
returns typed JSON and never calls a model. That keeps deterministic work
deterministic. In a document pipeline one exec atom converts a workbook to
JSON, a lite atom adds judgment only where it is needed, and a second exec atom
writes the result. Exec steps cost less and fail less often than a model turn,
which counts double with small models. Any step without a model belongs in one,
and that includes every write made after a gate. Exec atoms arrive in v0.1 with
the declarative atoms (M11a).

- `exec.command` is an argument array taken literally; no shell is involved.
- The process starts in the pinned, read-only project bundle, rooted at the
  manifest's directory. Scratch space it may write to is kept apart.
- Exactly one JSON value goes in on standard input and exactly one comes back on
  standard output; diagnostics belong on standard error. A non-zero exit fails
  the attempt, and the runner checks both values against their schemas.
- When a runtime is declared, `exec.script` names a file for it in place of a
  command, and atomic assembles the command line (M52).
- A script that runs for a long time is a step of its own, with a timeout, a
  heartbeat to keep the run's lease alive, and a clean kill when cancelled.

The reference is [exec atoms](../reference/atom-files.md#exec-atoms).

## Files as typed inputs and outputs

Documents and data arrive as files, not just JSON (M52).

- **A file type.** A schema may declare
  `{ $ref: "atomic:file", mediaType: [...] }`. The bytes live in the
  [artifact store](durability.md#artifact-store). Files coming in are stored
  under their content hash and mounted read-only at `/inputs`. Whatever an atom
  writes under `/outputs` is gathered, hashed and handed back as a typed
  reference. Checkpoints keep only the hashes, so replay does not repeat work.
- **Scratch workspaces.** Work with no repository behind it gets a new, empty
  directory for each attempt where a coding step would get a worktree. Retries
  then start clean, as file changes must.
- **Query large files instead of pasting them.** A bundled DuckDB tool exposes
  CSV, XLSX and Parquet files under `/inputs` as tables. It follows the same
  named-query and read-only SQL rules as [data
  sources](data.md#databases-as-data-sources).

## Declared toolchains

An atom without network access cannot fetch packages in the middle of a turn,
so it names its toolchain up front: a container image pinned by digest, a `uv`
lockfile (or something similar) installed by a setup step that may reach only
the package registry, or the Deno runtime. Setup output is cached. On a laptop
atomic falls back to the host's tools and warns that the result may not be
reproducible; on a server the runner uses the pinned image, which is how one
atom yields the same output locally and remotely.

```yaml
runtime: { image: "registry.example.com/tools/python@sha256:…" }
# or: runtime: { uv: { lock: ./tools/uv.lock } }
# or: runtime: { deno: { sys: [osRelease] } }
```

## Code in atoms

atomic keeps prompt text and executable code apart.

| Form | What runs | Phase |
| --- | --- | --- |
| `.atom.astro` declarative atom | Nothing: the compiler parses a bounded TypeScript-shaped syntax tree and interprets it | v0.1 |
| [TypeScript helper](../reference/atom-files.md#typescript-helpers) imported by an atom | A pure function, one call at a time, in an isolate with no network, files, environment or host clock | v0.1 |
| Exec script on Deno | A script step with declared hosts, environment and credentials | v0.7 (M52) |
| `.atom.ts` code atom | A whole atom in TypeScript on Deno | v0.7 (M17a) |
| `.workflow.ts` | A workflow in TypeScript with deterministic replay | v1.0 (M17b) |
| Rust atom | Native code behind the `Atom` trait | v0.1 |

Two names recur for code an atom owns:

- **`validate`** checks an output beyond what its schema and CEL rules can say.
  In a declarative atom it is a list of expression-bodied typed callbacks over
  `{input, output}` in the atom's `config`, which may call helpers; every one
  must hold, or the output goes to a repair turn. A Rust atom implements the
  same check as a method on the `Atom` trait. See
  [typed validation](../reference/atom-files.md#typed-validation).
- **`prepare`** belongs to `.atom.ts` code atoms (v0.7). It runs before the
  model call to compute context the prompt needs, may only read, and saves each
  `ctx.step` it makes as a checkpoint, so a retry or replay reuses what it
  already fetched. Its result is visible to the workflow as
  `steps.<id>.prepared`, a fact computed by code rather than claimed by a model,
  which a gate can show as evidence.

A declarative atom has no `prepare` and never runs. Work that reads or changes
the outside world, whether an SDK call, file I/O or a fetch, is a separate
script step placed next to the model atom in the workflow. A fetch step returns
what it read, for instance a report's text with its SHA-256, and the model atom
takes that as ordinary typed input; a gate can display the hash. Fetching and
the model turn then checkpoint independently.

Where code may come from is restricted: the team's own repository, or a package
whose hash is on the allowlist. Packages from anywhere else may contain
declarative atoms only.

## Replay and determinism

Every step runs at least once, and the code inside must tolerate that.

- **A script step is one durable step.** Its output is checkpointed, and a
  finished step is not run again on recovery. Script code may not open workflow
  steps inside itself.
- **Helpers are reproducible.** A helper sees the step's recorded clock, seeded
  randomness and a fixed timezone, and its calls are stored with the step, so a
  resumed run renders an identical prompt.
- **TypeScript workflows start over after a crash.** A `.workflow.ts` runs again
  from its first line; `ctx.run`, `ctx.step`, gates, `ctx.map` and the merge
  queue hand back the results already checkpointed. Everything between those
  calls must give the same answer every time, so workflow code gets no network,
  file or environment access, `Math.random` is seeded and `Date.now` answers
  from the record. Any I/O it needs is done by atoms.
- **Each run stores** hashes of the code and of the lockfile along with the
  Deno version, and replays against those.

## Permissions from the declaration

No one writes runtime permission flags. atomic derives the Deno command line
from what the atom declares:

| Declaration | Becomes |
| --- | --- |
| `network.allow` | `--allow-net` for exactly those hosts |
| `env` | The entire environment the code gets; `--allow-env` for those names plus `HOME`, pointed at scratch; any other variable reads as unset |
| `runtime.deno.sys` | The machine facts code may read, the OS release for example |
| Project and inputs | `--allow-read` |
| Scratch workspace | `--allow-write` |
| Always | `--no-prompt`, a frozen lockfile fetched from cache only, no modules loaded from a URL, no subprocesses, no native code |

Code therefore begins with nothing. It reaches no host, sees only its declared
variables and no machine facts, reads only the project and its inputs, writes
only to scratch, runs no install scripts, and imports only exact versions
present in the lockfile. A refused permission is an error naming the host or
variable involved. These requirements are what atomic asks of a runtime for
atom code: network rules per host, no package install scripts, and a lockfile
enforced when code runs rather than only at install time.

Under `runtime.deno` a manifest can narrow things further: how old a release
must be before it can be locked (one day unless changed), which packages may
run install scripts (none unless named), an `allow_packages` list that is the
complete set of packages atom code may import, and `audit_permissions`, which
writes every permission actually used into the step's trace. Managed
requirements may tighten each setting and restrict which hosts and facts atoms
may declare. Nothing at any layer can allow subprocesses or native code. See
[settings for atom code](../reference/manifest.md#settings-for-atom-code).

The OS sandbox (Landlock on Linux, Seatbelt on macOS) stays in force; Deno's
permissions sit inside it as a second layer. What the second layer adds is that
authority is written down once, in the atom, and the same declaration also
drives the policy lock and the risk view.

`atomic runtimes lock` finds every package that atom code imports, applies the
manifest's age and package rules, and writes `deno.lock` for review and commit.
`atomic check` rejects an import missing from the lockfile and tells the author
to run that command.

## Reads before the turn, writes in their own step

atomic cannot see what a script transmits, so the code has to say:

- Code marked `effects.external: read-only` may be retried at will and may run
  during evals, renders and previews.
- Code marked `effects.external: idempotent` writes, so it is never part of a
  dry run; after a crash it is safe to run again.
- Code marked neither way parks when an attempt crashes, rather than retrying.

Writing to an outside system is always a deterministic step of its own, placed
after any gate and governed by the
[side-effect contract](durability.md#side-effect-contract). It has to tolerate
repetition. The S3 example writes with create-only semantics and, if the object
exists, compares checksums, so a repeated step recognises its own earlier write
and never overwrites some other object.

Nothing that writes runs in a dry run. `atomic eval`, `atomic render` and
`--effects preview` execute `read-only` code and skip anything that declares a
write. An eval of an atom that writes is refused and points at `atomic test`; a
preview notes that the step would have run, together with its input. Code gets
tested as code: The project's tests run under `atomic test`, on the managed
Deno, and can open connections to the local machine and nowhere else. See [test
code as code](../guides/testing-and-evals.md#test-code-as-code).

## Named credentials

Atoms refer to a credential by name, and the manifest, or a per-environment
profile overlay, says what it is.

```yaml
credentials:
  read-reports:
    duration: 15m
    aws_role: arn:aws:iam::111122223333:role/report-reader
```

At dispatch the control plane assumes the role and puts the resulting temporary
keys into that one step's environment. They are written nowhere else: no file,
checkpoint, trace, log or model context, and never the shell of a Codex atom,
which runs as a separate process. The session name carries the run and step, so
the provider's audit log shows which run made each call, and the keys lapse on
schedule whether the step finished or not. atomic authenticates as whatever
identity the machine already has, a developer's login on a laptop or a workload
identity on a server. Reads and writes use separate roles. For a service that
only offers static keys, `env` can still carry a `{ secret: <name> }` reference.
Naming a credential the manifest lacks fails the load. See
[named credentials](../reference/manifest.md#named-credentials).

## The managed Deno

There are two Deno pieces in atomic, and they differ in status.

**Today.** The bundled Astro compiler, which every declarative atom passes
through, runs on the `deno_core` and V8 linked into the atomic binary, in a child
process of atomic itself with no network, environment, filesystem or subprocess
access; no Deno executable is needed. The same engine runs helper isolates.

**From v0.7.** Code atoms and scripts run on a Deno that atomic downloads and
manages itself, so that no one installs it and two machines never disagree on
the version.

- Each atomic release fixes one Deno version and embeds the SHA-256 of the
  official archive for every platform it supports. The first time a project
  needs it, atomic downloads that archive, verifies it against the embedded hash
  rather than any checksum file fetched alongside, and unpacks it under
  atomic's user-data directory.
- Projects without atom code or Deno scripts never download it, and once
  installed it is not fetched again.
- `atomic runtimes install` installs it in advance for CI and image builds;
  with `--from <archive>` it installs from a local file on a machine with no
  network, under the same hash check. Offline bundles and container images ship
  with it.
- A server does not download at run time. In team mode `atomic serve` refuses to
  load and names the install command. Managed requirements may forbid downloads
  or redirect them to a mirror, which the hash check keeps safe.
- It has its own package cache and its update check is off, so it neither uses
  nor disturbs any Deno a developer installed. `runtime.deno.binary` chooses
  `managed` (the default), `system` or an explicit path, and whichever is chosen
  must be a version the release supports.
- `atomic deno <args>` calls the managed binary directly.

Linking the whole Deno runtime into the atomic binary, instead of downloading
it, is a possible later option. It would first have to fit Codex's native
dependencies and meet size, upgrade and portability requirements.

## TypeScript workflows

In v1.0 (M17b) a workflow may be written as a `.workflow.ts` file. It runs on
the same durable engine as a YAML workflow, and each kind can call the other.

- `atoms:*` imports give typed handles generated from the registry, so the
  editor catches a misspelled atom or a wrong input field.
- `ctx.run`, `ctx.step`, gates, `ctx.map` and `ctx.mergeQueue` hand back
  checkpointed results.
- Durable `sleep` and `waitUntil` record a checkpoint before waiting, wake on
  time after a restart, and cost nothing while they wait.
- Workflow code is granted no permissions whatsoever, since a runtime's
  permissions apply to its whole process.

When the same logic can be written declaratively, that form is preferred;
TypeScript is for cases where code makes things much simpler. A separate SDK
for other runtimes, talking to the HTTP API, may come later, though durability
and trust are harder to guarantee in a process atomic did not start.

## Untrusted files

Files someone uploaded or a step fetched count as external data.

- They carry the external tag, so taint rules apply to them, and atomic assumes
  their cells or text may contain prompt injection.
- Parsing happens inside the sandbox without network access, subject to size
  and media-type limits, and the type is detected from the bytes, not taken from
  the file extension.
- `.xlsm` macros are never executed, archives are screened for zip bombs, and
  CSV output is escaped so that spreadsheet formulas cannot be injected.

Put together, these let a document or data pipeline run without any coding agent:
pulling fields out of invoices, reconciling spreadsheets each month, reviewing
a folder of contracts. Such pipelines are durable, gated, typed and audited,
and they run well on small local models, because exec atoms do the
deterministic parts and the model is asked only to judge.
