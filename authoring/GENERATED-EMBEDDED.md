# Generated schema types and embedded Deno

This note records the retired Deno embedding experiment. The production compiler
uses native Rust and runs its worker and captured helpers on Boa.

## Authoring

The source of the types is [contracts.schema.json](schemas/contracts.schema.json).
[atoms.json](schemas/atoms.json) associates each atom with its input, output,
eval-case and judge schemas. Atom files now use stable type imports:

```ts
import type { Props, Input, Reply, EvalCase, Judges } from '../../.atomic/types/draft-reply';
```

Components import schema types directly, for example `import type { Email } from
'../../.atomic/types/schemas'`. The path is relative to each authored file.
Bindings use `const { input }: Props = Astro.props` (or the component's prop).
These forms work in configured and inferred TypeScript projects, including when
Astro cannot find a project install. The earlier `atomic:atoms/...` path aliases
caused TS2307 in the live editor when it fell back to an inferred project; the
initial repository-root config workaround did not resolve that user report and
has been removed. `npm run probe:inferred` now reproduces the old TS2307 and
checks the actual fixtures without tsconfig, package.json or node_modules,
while retaining field, component-prop, helper and scorer diagnostics.
The old handwritten fixture `types.ts` and atom `.d.ts` sidecars are removed.
The framework's `authoring.d.ts` still defines the generic config API and the
host intrinsic's signature; those are library declarations, not schema copies.

```sh
cd authoring
npm ci --ignore-scripts --no-audit --no-fund
npm run generate
npm run check:types
npm test
npm run probe:generate
```

Generation produces ignored `.atomic/types/*.d.ts` plus a manifest containing
schema/registry fingerprints, resolved schema associations and conventional
name/version/kind metadata. It does not rewrite authored files. Repeated runs
leave unchanged files untouched. A stale check detects schema changes, edited
or missing generated files, and stale atom declarations. Regeneration removes
declarations for deleted registry entries. Tests and LSP commands generate first.

The small generator supports local `$defs` references, closed object schemas,
required/optional properties, arrays, primitive types and selected primitive
enums. Date-time and integer constraints become TypeScript string/number types;
the original schema remains authoritative. Unknown keywords, unsupported
formats, remote references and recursive schemas fail explicitly. This is an
intentionally limited generator, not support for all of JSON Schema or a
replacement for runtime schema validation. Type aliases and compiler manifests
are derived artifacts; the schema fingerprint is not yet a full compiled-atom
dependency digest or a mandatory runtime freshness gate.

The editor probe works in a disposable copy: it adds `Meeting.room` only to
JSON Schema, observes a stale check failure, regenerates, then confirms the
unmodified Astro LS offers `room` inside the validator. Existing field/helper,
judge-binding and score-type diagnostics still work. No declaration is edited
by hand. This proves regeneration plus editor resolution; a live watch service
and VS Code GUI testing are outside the spike.

Repository and standalone-spike VS Code settings now hide `node_modules`,
generated output, build output, package/config files and implementation/test
folders. Authored atoms, components, schemas and reports remain visible.
Explorer exclusions do not delete files or prevent the language server from
reading dependencies. The generated directory is local to the spike for this
experiment; production placement remains part of the compiler/state design.

## A real embedded worker

The experimental host created a
`deno_runtime::worker::MainWorker` directly in Rust and drives its event loop on
the same thread. It does not call `deno::main`, re-exec itself, or launch a Deno
CLI. Rust's process ID and `Deno.pid` match. The embedding process itself is
launched by the test harness through `scripts/dev.sh`.

Pins: Rust 1.95.0 through the development container, `deno_runtime` 0.268.0,
`deno_core` 0.413.0, V8 15.0.245.2-rusty. These are the runtime crate versions
associated with Deno 2.9.8; this host's `Deno.version.deno` reports the runtime
crate version, 0.268.0, and its built-in TypeScript version reports `n/a`.
It uses the existing TypeScript 5.9.3 parser from the trusted compiler bundle.

The trusted compiler, generator and interpreter are bundled with esbuild
0.27.0 for the browser/WASM Astro compiler API. A small POSIX adapter supplies
explicit filesystem services and a Rust SHA-256 op. Atom files remain data
parsed by the compiler; the bundle does not execute their frontmatter modules.
Node compatibility services and runtime npm resolution are disabled.

The probe copies only fixtures, schemas, framework declarations, the compiler
bundle and Astro WASM into a fresh project. There is **no `node_modules` or
unbundled compiler source** in that project. It deletes generated declarations
after computing the Node reference result, forcing the embedded worker to
regenerate them. Generation metadata, prompt text/bindings, validator results,
judge input, score and mean aggregation all match Node exactly.

The worker grants reads to that project and writes to generated `.atomic`
output. The [context follow-up](CONTEXT.md) adds a separate external state
directory and permits reads/writes there for checkpoints. Six
checks require `NotCapable` errors for reads outside the project, writes outside
generated output, environment, network, subprocess and FFI. A test-only
`--control-allow-read` switch removes the read restriction; the read-denial check
then fails. This is an isolated experimental host, not an atom-facing CLI.
Its trusted filesystem module loader and the SHA op are host capabilities;
the test is not a complete OS sandbox/security certification.

## Evidence and boundaries

At this milestone the JavaScript suite had **67 passing tests, zero skips**:
the earlier 61 plus six generation tests. The [context follow-up](CONTEXT.md)
adds 17 tests and extends the embedding with external snapshot storage. The source mutation controls fail 7, 1, 11, 1, 1 and 3
tests respectively for removed data isolation, frontmatter rejection, contract
AST validation, meeting containment, score threshold and schema freshness.
The separate native permission control also fails as required. Reports are
written to ignored `results/generation.json` and `results/embedded.json`.

The experiment left the combined SQLite dependency and standalone release packaging unresolved. Atomic subsequently adopted the Rust Astro compiler and Boa.
