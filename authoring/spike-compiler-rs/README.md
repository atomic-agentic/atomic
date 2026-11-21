# Spike: Astro's Rust compiler as atomic's parser

Research only. Production still parses with `@astrojs/compiler` 4.0.0 (Go, as
WebAssembly) in the compiler worker. This directory compares it with
`@astrojs/compiler-rs` 0.5.3 and with the oxc fork that compiler-rs builds on,
used as a pinned Rust dependency.

## Run

From `authoring/`, with Node 24 and `npm ci --ignore-scripts` done. Every
package used is already in `package-lock.json`.

```sh
npm run probe:compiler-rs   # parity of trees, renders, errors and type-error positions
npm run bench:compiler-rs   # parse times, worker cost, sizes (set ATOMIC_DENO for the worker rows)
node spike-compiler-rs/parity.mjs --strict   # exit 1 if any valid source differs
```

The Rust trial crate has its own workspace and lockfile. Build it through the
development container, from the repository root:

```sh
scripts/dev.sh env CARGO_TARGET_DIR="$PWD/target-spike" \
  cargo build --release --locked --manifest-path authoring/spike-compiler-rs/rust/Cargo.toml
node authoring/spike-compiler-rs/native-vs-napi.mjs   # the crate matches the npm binding
```

## What each script does

| Script | Question |
| --- | --- |
| `parity.mjs` | Parses every `.astro` file in `examples/`, `authoring/fixtures` and `crates/`, the cases in `cases.mjs`, and every source the authoring tests parse (recorded through `record-hook.mjs`) with both compilers, normalises both trees to one shape (`normalize.mjs`) and reports structural, position and diagnostic differences |
| `parity-render.mjs` | Renders the example fixtures with each parser and compares the prompts |
| `parity-errors.mjs` | Compiles each case with atomic's renderer under each parser and compares what atomic reports |
| `tsx-positions.mjs` | Locates the language-server probe's type errors three ways: atomic's checker, the same checker on `@astrojs/astro2tsx` output, and the stock language server |
| `bench.mjs` | Parse time per source, cold start, the worker's per-request cost, sizes |
| `native-vs-napi.mjs` | Whether the trial crate's AST equals the npm binding's |
| `rs-as-go.mjs`, `rs-hook.mjs` | Run atomic's renderer, unchanged, on compiler-rs's parse: `node --import ./spike-compiler-rs/rs-hook.mjs --test test/*.test.mjs` |

Results are written to `results/`.
