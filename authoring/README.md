# Atomic Astro authoring

This package contains the production Astro parser, bounded expression interpreter,
TypeScript checker and contributor/editor probes. The Rust loader in
[`atomic-core`](../crates/atomic-core/src/atom.rs) uses its bundled worker.
`.atom.astro` replaces both previous declarative atom formats.

Atom source is never imported as JavaScript or rendered through Astro's page
runtime. Local components, typed props, slots and expression-bodied validators
are interpreted. Full input/output/case/judge schemas remain authoritative in
Rust; generated TypeScript is an additional load-time and editor check.

- [Atom authoring reference](../docs/reference/atom-files.md)
- [Expression semantics and limits](SYNTAX.md)
- [Public execution context](CONTEXT.md)
- [VS Code setup](../docs/guides/vs-code.md)
- [Production context example](../examples/12-execution-context/README.md)
- [Embedded Deno research](GENERATED-EMBEDDED.md)

## Run the implementation

The Rust binary contains the parser, TypeScript libraries and WASM, and runs them
on its embedded V8 in a child process of its own; projects need no Deno, runtime
npm install, node_modules or website build.

```sh
atomic check examples/01-classify
atomic render examples/01-classify/atoms/classify-note.atom.astro \
  --input examples/01-classify/fixtures/classify-note/export-crash.input.json
```

The worker has a cleared environment and reads only its own embedded WASM: atomic
reads atom sources itself and sends them in each request. Its host
(`crates/atomic-core/src/compiler_host/host.js`) offers no network, environment,
filesystem or subprocess access. Runtime cache and direct-run context
checkpoints live outside source. Generated `.atomic/types` declarations are
separate disposable authoring artifacts.

## Contribute to the compiler

Use Node 24.21.0 (`.node-version`) and npm 12.2.2. Contributor dependencies and
integrities are pinned in `package-lock.json`.

```sh
cd authoring
npm ci --ignore-scripts --no-audit --no-fund
npm run generate
npm run build
npm test
npm run probe:lsp
npm run probe:workspace
npm run probe:inferred
npm run probe:controls
```

`src/production-controls.mjs` probes the worker under a Deno CLI's own
permissions (`ATOMIC_DENO=/path/to/deno node src/production-controls.mjs`, with
the development image's Deno). atomic itself no longer runs the worker on Deno;
the embedded host's controls are tested in `crates/atomic-core/src/compiler_host`.

`npm run build` bundles the trusted compiler into `crates/atomic-core/assets`.
`python3 scripts/check-authoring-assets.py` rejects stale sources or changed assets.
The stock language server probes test completion, hover, definitions and negative
type diagnostics. Direct relative imports of generated declarations and explicit
`Props` bindings also work in inferred projects without tsconfig. Headless probe
success does not establish the state of a running VS Code GUI session.

`npm run probe:format` measures prompt changes caused by formatting. Format-on-save
is disabled because whitespace is prompt content. Some execution sandboxes close
subprocess pipes; an unreachable language server fails the probe rather than skipping.

The isolated `embedded/` research host demonstrates an actual in-process Deno
`MainWorker` without runtime node_modules. It remains separate from the combined
Atomic/Codex binary because of its native dependency and release-packaging work.
Production runs the worker on `deno_core` alone, without Deno's runtime crates. Workflow-level durable
context dispatch and secret credential integration are separate tracked work.
