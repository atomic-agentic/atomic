# Set up VS Code

Atomic's files are YAML and Markdown, so VS Code edits them well with a few
common extensions. This guide lists the extensions, the settings that point
them at Atomic's generated schemas, and what to change for the JSX-style
`.atom.mdx` form. Other editors that read JSON Schema for YAML and speak the
Language Server Protocol can use the same schemas and, later, the same language
server.

The commands and generated files below are proposed behavior, like the rest of
the [CLI reference](../reference/cli.md). The schema generator is planned for
v0.5; see [availability](../reference/availability.md).

## Install the extensions

| Extension | ID | What it gives you | Needed for |
| --- | --- | --- | --- |
| YAML (Red Hat) | `redhat.vscode-yaml` | Completion, hover and inline errors in `atomic.yaml` and workflow files, from Atomic's schemas | Every project |
| Markdown Preview Mermaid Support | `bierner.markdown-mermaid` | Renders the Mermaid diagrams in atom bodies, READMEs and docs | Every project |
| MDX | `unifiedjs.vscode-mdx` | Highlighting, bracket matching and folding for `.atom.mdx` files | Atoms in the JSX-style form |
| Deno | `denoland.vscode-deno` | Type checking and completion for TypeScript atoms and shared modules | Atoms with code, from v0.7 |

Recommend them to everyone who opens the project in
`.vscode/extensions.json`:

```json
{
  "recommendations": [
    "redhat.vscode-yaml",
    "bierner.markdown-mermaid"
  ]
}
```

Add `unifiedjs.vscode-mdx` and `denoland.vscode-deno` when the project uses
`.atom.mdx` files or atom code.

## Point the YAML extension at Atomic's schemas

`atomic schemas` writes JSON Schemas for the manifest and for workflow files
into the generated cache directory, `.atomic/cache/schemas/` by default. They
include your project's own names, so the editor completes atom names in a
workflow's `atom:` field and flags a misspelled `kind`.

```sh
atomic schemas
```

Then map the schemas to the files in `.vscode/settings.json`:

```json
{
  "yaml.schemas": {
    ".atomic/cache/schemas/manifest.schema.json": "atomic.yaml",
    ".atomic/cache/schemas/workflow.schema.json": "**/*.workflow.yaml"
  }
}
```

To point a single file at a schema instead, put a comment on its first line:

```yaml
# yaml-language-server: $schema=../.atomic/cache/schemas/workflow.schema.json
atomic: 1
```

Run `atomic schemas` again after adding or renaming atoms, so completion knows
the new names. The cache directory is gitignored; each person generates it.

The YAML extension does not read the frontmatter of a Markdown file, so atom
frontmatter is checked by `atomic check` rather than in the editor.

## Edit atom files

An `.atom.md` file is Markdown. VS Code's Markdown editor and preview work on
it as they stand, and `{% … %}` tags show as text.

For `.atom.mdx`, the MDX extension supplies the highlighting. Its IntelliSense
server reads every `{…}` as TypeScript, but in an atom the braces hold CEL, so
it flags valid CEL as errors: the `s` in `{input.slots.exists(s, s.free)}`, or a
loop variable such as `c` in `<For each={…} as="c">…{c}`. Turn the server off;
highlighting comes from the extension's grammar and keeps working:

```json
{
  "mdx.server.enable": false
}
```

This turns off MDX IntelliSense for every MDX file in the workspace folder. If
your repository also holds MDX documentation that needs it, open the Atomic
project as its own folder in a
[multi-root workspace](https://code.visualstudio.com/docs/editing/workspaces/multi-root-workspaces)
and put the setting in that folder's settings.

## Format without changing a prompt

A formatter that rewraps a paragraph changes the text the model reads. With
Prettier, keep `proseWrap` at its default, `preserve`, which leaves line breaks
as you wrote them:

```json
{
  "prettier.proseWrap": "preserve"
}
```

`atomic check` keeps a snapshot of each atom's rendered prompt for its typed
examples and fails when the prompt changes without the snapshot being updated,
so a change a formatter makes shows up in review (planned for v0.5). See
[see the prompt before spending tokens](template-tags.md#see-the-prompt-before-spending-tokens).

## Keep generated files out of the way

The cache directory holds generated schemas and types. Leave it out of search
and the file watcher:

```json
{
  "search.exclude": { ".atomic/cache/**": true },
  "files.watcherExclude": { ".atomic/cache/**": true }
}
```

Runtime state, such as checkpoints, transcripts and worktrees, never lives in
the project, so there is nothing else to hide.

## Atoms with code

From v0.7 an atom can carry TypeScript, in a `---ts` block or as an `.atom.ts`
file, and share modules from `lib/`. `atomic types` writes the types for your
schemas and Atomic's own modules into the cache directory. Enable the Deno
extension for the folders that hold code:

```json
{
  "deno.enablePaths": ["lib", "atoms"]
}
```

The Deno extension checks `.atom.ts` files and `lib/` modules. A `---ts` block
inside an `.atom.md` or `.atom.mdx` file is TypeScript inside Markdown, which
the extension does not see; `atomic check` type-checks it. See
[how Atomic runs and confines atom code](atom-code-runtime.md).

## Everything in one file

A `.vscode/settings.json` for a project that uses all of the above:

```json
{
  "yaml.schemas": {
    ".atomic/cache/schemas/manifest.schema.json": "atomic.yaml",
    ".atomic/cache/schemas/workflow.schema.json": "**/*.workflow.yaml"
  },
  "mdx.server.enable": false,
  "prettier.proseWrap": "preserve",
  "search.exclude": { ".atomic/cache/**": true },
  "files.watcherExclude": { ".atomic/cache/**": true },
  "deno.enablePaths": ["lib", "atoms"]
}
```

`atomic init` writes `.vscode/settings.json` and `.vscode/extensions.json` with
these settings when the files do not exist yet, and leaves existing ones alone
(planned for v0.5).

## What comes later

Atomic's language server is planned for v0.5. It checks what the extensions
above cannot: that a tag is known, that an `$input` path exists in the atom's
input schema, and that CEL type-checks. It also completes names, jumps from a
reference to its definition, and in `.atom.mdx` treats braces as CEL rather
than JavaScript. An Atomic extension for VS Code, also planned for v0.5,
starts the language server and applies the settings in this guide, so most of
the setup above becomes one install. Until then, `atomic check` reports the
same errors from the terminal.

Next: [project structure](project-structure.md) or
[template tags and the JSX-style form](template-tags.md).
