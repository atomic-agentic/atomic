# Authoring atoms in VS Code

Install the Astro extension (`astro-build.astro-vscode`) and open the project root.
Atoms use `.atom.astro`; no custom language association is needed.

Run `atomic check` after schema changes to regenerate `.atomic/types`. Import the
actual generated file with a relative path and explicitly annotate the props binding:

```astro
---
import type { Props, Input, Output } from '../.atomic/types/classify-note';
import type { AtomConfig } from '../.atomic/types/authoring';
const { input }: Props = Astro.props;
export const config: AtomConfig<Input, Output> = {
  validate: [({output}) => output.summary.length > 0],
};
---
Classify {input.text}.
```

The Astro language server can complete schema properties, inspect types, navigate
local components and diagnose wrong props and callback results. Direct generated
imports also resolve in inferred projects without a root tsconfig. Virtual
`atomic:atoms/…` imports are not part of this contract.

Generated context types are deeply readonly. The editor catches unknown public env
keys, invalid profile comparisons and attempts to mutate configuration. The runtime
independently validates complete schemas and the allowed expression AST; ordinary
Astro tooling accepts syntax that Atomic deliberately rejects.

Project-local `node_modules` are not required for running atoms. The shipped compiler
contains its parser, TypeScript checker and WASM; its host uses pinned Deno 2.9.8.
The authoring package's npm dependencies are contributor tooling for rebuilding and
testing those assets. The production loader checks fresh virtual declarations even
when the on-disk editor files are stale.

Recommended workspace settings:

```json
{
  "files.exclude": {"**/.atomic/types": true, "**/node_modules": true},
  "search.exclude": {"**/.atomic/types": true, "**/node_modules": true},
  "[astro]": {"editor.formatOnSave": false}
}
```

Generated files are gitignored. Hide `.atomic/types`, rather than an entire `.atomic`
source layout when a project keeps its atoms there. Formatting is opt-in because
whitespace is prompt content.

If a declaration import cannot be found, run `atomic check`, confirm the relative
path and reload the Astro language server. Schema/field mistakes require changing
the source; suppressing TypeScript diagnostics does not bypass Atomic's loader.
