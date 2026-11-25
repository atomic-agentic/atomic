# Two review atoms in the stock Astro editor

`self-review` checks whether a candidate meets its stated checks. `review-plan`
proposes checks and repairs without claiming they ran. They share the
`ReviewPacket` input schema, `ReviewPacket.astro` component, and typed
`reviewLabel` helper. Their outputs deliberately differ: a readiness decision
and findings versus a priority and ordered steps. These are demonstration
prompts, not a gate approval or evidence of a model's judgment.

Run from this directory after building `atomic`:

```sh
atomic check --json
atomic render atoms/self-review.atom.astro --input fixtures/self-review/clean.input.json --preview-helpers --json
atomic render atoms/review-plan.atom.astro --input fixtures/review-plan/one-check.input.json --preview-helpers --json
```

The checked-in `.render.json` files are exact snapshots of the developer text
and tagged user bindings for those fixtures. They use `--preview-helpers`, so
fresh helper time in a later live step is outside this comparison. The examples
in each atom are team-authored data; `candidate` and `checks` are tagged as
external data. Nothing here calls a provider. The eval cases are contracts
awaiting recordings; their paired output fixtures are illustrative only.

From the repository root, `tools/review_authoring_pilot_test.py` copies a fresh
configured project and a flat inferred atom-file project into a temporary
directory. It checks both, renders both configured atoms and the inferred
self-review atom, and compares their exact prompt text and bindings. It needs
only the atomic binary and Python, with no project `node_modules`.

`tools/review_authoring_lsp_test.py` drives the stock Astro language server in
headless mode. Set `NODE_BIN`, `ASTRO_NODE_MODULES` and `ATOMIC_BIN` if those
are not on the normal paths. It verifies completion, hover, generated type
source navigation, component definition, wrong-prop diagnostic, schema rename
failure, and rejection of a self-review validator that reads `steps` from the
other atom's output. This is an automated editor smoke; a person still needs
to open the project in VS Code and record the GUI smoke for the release.
