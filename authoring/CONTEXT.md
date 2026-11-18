# Typed execution context

This experiment adds a host-supplied execution context to the Astro spike.
It describes the experiment's Node tooling (`npm run render:context`), not the
`atomic` CLI; see [atom files](../docs/reference/atom-files.md#context-schemas) for that.
Open [the contextual atom](fixtures/context/draft-reply.atom.astro):

```astro
---
import type { Props } from '../../.atomic/types/draft-reply';
const { input, context }: Props = Astro.props;
---
Organization: {context.config.organizationName}
Region: {context.env.SUPPORT_REGION}
```

The same context is available to interpreted validators, judge input bindings
and scorers when the atom's config uses the fifth `AtomConfig` type parameter.
The example limits reply length using `context.config.maxReplyCharacters`.
Existing atoms with only `input` continue to work. The production Rust loader and direct invocation APIs now use the same compiler.

## Configuration and types

[settings.json](fixtures/context/settings.json) declares base configuration,
`dev`/`test`/`prod` profile patches, a default profile and an environment allowlist.
Resolution is **base → selected profile → explicit configuration overrides**.
Nested objects merge; arrays/scalars replace. Environment values are a separate
namespace and do not implicitly overwrite configuration. Profile selection is
an explicit host option, rather than an ambient `NODE_ENV` convention.

[RuntimeContext's schema](schemas/contracts.schema.json) declares:

| Property | Supplied by |
| --- | --- |
| `profile` | Selected profile, typed as `dev \| test \| prod` |
| `config` | Resolved project/profile configuration |
| `env` | Only declared prompt-visible environment values |
| `run` | Stable host-assigned run ID |
| `step` | Host-assigned current step ID |
| `project` | Declared project ID/name |

The schema registry associates `draft-reply` with this context schema. Generation
produces a deeply readonly `Context` type and adds it to `Props`. Direct imports
of generated files and explicit `Props` binding annotations preserve the editor
behavior in both configured and inferred projects.

All context data is checked against the supported closed JSON Schema subset
before publication, and again on restore. Unknown properties, missing required
fields, wrong types, invalid enum values and unsafe integers are rejected.
Unsupported schema keywords fail explicitly. Date-time context validation is
outside this implementation; it is rejected rather than treated as a plain
string constraint. This context validator is not a full JSON Schema engine or
the still-pending production input/output/judge validation gate.

## Environment values and provenance

The host reads exactly the variable names declared under `environment`.
`SUPPORT_REGION` is required; `SUPPORT_DOCS_SITE` is optional and exposed as
`context.env.DOCS_SITE`. Missing required values fail. Undeclared variables are
never read or captured. An environment declaration marked `secret: true` is
rejected before the first environment read. Actual secret values and secret
references still belong in the separate credential/tool channel; a credential
provider is not implemented by this spike. The host operator must designate
only public values for this prompt-visible context.

Resolved context and nested values are immutable copies. Interpolation carries
the path (for example `context.env.SUPPORT_REGION`) and the existing `team`
origin. Atom input keeps its original origin, normally `external`. Values go
into tagged user-turn bindings; developer instructions contain references.
Inserted strings are never parsed as template or executable code.

## Snapshot and resume

Before rendering, the host records a snapshot keyed by the stable run ID. It
contains the resolved context, its original schema, source fingerprints and an
integrity digest. Stored schemas allow restore to validate the original values
without consulting changed source configuration. Digest checks detect altered
payload/schema bytes; these checksums are not cryptographic authentication
against an actor able to rewrite the state and recompute hashes.

The JavaScript reference filesystem store creates a private candidate, flushes its contents, and
publishes it using an exclusive hard link. Concurrent writers return the same
winning snapshot. Readers flush the directory entry before using a published
checkpoint. Final files are mode 0600 in an initially private directory.
Corrupt or mismatched checkpoints fail; they are never silently re-resolved.
State must be outside the source project, including after symlink resolution.
This implementation is tested on Linux and requires filesystem support for
hard links and directory fsync.

Resume loads the published snapshot **without invoking the resolver**. A changed
profile selection, environment, settings file or configuration override cannot
replace it. Use a new run ID when intentionally starting with new settings.
Dispatch may bind a new step ID to a fresh immutable view of the same frozen run
configuration; it does not reread environment or profiles. Step IDs come from
the host, not atom output. The spike does not persist a complete workflow's step
history or implement step scheduling/deduplication.

## Try it

```sh
cd authoring
npm run generate
npm run probe:context

# Read the declared values from the actual host environment.
SUPPORT_REGION=eu-west npm run render:context -- --profile test --run-id example-1

# Same run resumes the earlier snapshot, despite these changed options.
SUPPORT_REGION=us-east npm run render:context -- --profile prod --run-id example-1

# Another dispatch step shares the run configuration.
npm run render:context -- --run-id example-1 --step-id review
```

The CLI defaults to `$XDG_STATE_HOME/atomic/astro-spike/<project-hash>` or
`~/.local/state/atomic/astro-spike/<project-hash>`. An explicit `--state-dir`
selects external storage. `--settings` and `--overrides` accept JSON files; they
are consulted only when creating a new snapshot. `--fixture-env` uses the
synthetic host projection for a self-contained demo. The deterministic probe
uses and removes a temporary external state directory. It does not read the
developer's shell environment.

`render` keeps its earlier input-only behavior. `render:context` uses the new
contextual fixture. The library requires a verified `contextSnapshot` for
contextual rendering and validation/scoring. An explicit render run ID must
match the snapshot. No atom module executes and no `process.env` or `Deno.env`
access is introduced into the expression language.

## Verification

The context milestone had **84 passing tests, zero skips**, including 17 context
tests. Production adoption adds further semantics, example and Rust integration
coverage; the current authoring suite has 89 passing tests. They cover merge precedence, allowlisting, secrets, immutability, schema
validation, corruption, concurrent publication, external state placement,
provenance, step binding and cross-process resume. A named failpoint
`CONTEXT_COMMITTED` is tested by killing a real process with `SIGKILL`; a new
process keeps the original profile/environment and makes zero environment reads.

The nine source mutation controls fail as expected. Removing snapshot reuse
fails three tests (including restart), removing integrity verification fails
the tamper test, and removing secret rejection fails its before-read test.
The earlier six controls continue to fail when removed.

The stock Astro LS completes context/config/environment members. It rejects an
undeclared environment property, an unknown config field, comparison against
`staging`, and mutation of a readonly context property. These checks pass with
the installed extension server from the repository workspace and in a fresh
inferred project without tsconfig or node_modules. GUI state is not directly
tested.

The Rust/Deno `MainWorker` now grants filesystem access to a separate external
state directory. It generates the types, creates and restores the snapshot,
renders the contextual atom and evaluates its contracts, with output identical
to Node. Live Deno environment, network, subprocess and FFI access remain
denied, as do reads/writes outside the granted roots. The embedding test still
runs with no runtime node_modules; its native permission negative control still
fails. The runtime version/build-cache/release-packaging limits described in
[GENERATED-EMBEDDED.md](GENERATED-EMBEDDED.md) remain.

Production direct invocation now exposes frozen context resolution and an external
file checkpoint through `atomic_core::context` and `atomic render`. Workflow-level
`DurableRuntime` integration and the credential backend remain separate work.
Production integration must checkpoint context at workflow start, bind trusted
dispatch metadata, pin the compiled atom/schema version used by that run, and
handle explicit migrations/retention. Astro adoption is recorded; combined runtime
embedding remains an owner decision.

The production CLI reloads the atom and full schemas before checking the saved
compiled fingerprint. A changed constraint is refused even when its TypeScript
projection is identical. It restores public settings/environment without invoking
the resolver, but does not yet restore a workflow-pinned compiler/source version;
that belongs to the workflow dispatch integration.
