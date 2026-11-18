# Astro prompt constructs

| Construct | Purpose |
| --- | --- |
| `{input.field}` | Interpolate a schema-typed value into a tagged user binding |
| `{condition ? <Fragment>…</Fragment> : null}` | Select prompt content |
| `{items.map(item => <Fragment>…</Fragment>)}` | Repeat content |
| `<Component prop={value} />` | Use an explicitly imported local component |
| `<Component>…</Component>` and `<slot />` | Pass and render caller content |
| `<rubric>…</rubric>` | Literal prompt section |
| `config.examples` | Declare named, fully schema-validated examples |
| `config.validate` | Declare interpreted typed output validators |
| `context.config`, `context.env` | Read verified public host context |

Markdown/MDX atom parsers and the old tag vocabulary have been replaced by Astro.
See the [component guide](../guides/template-tags.md) and
[atom reference](atom-files.md) for syntax, escaping, provenance and interpreter
limits. Other future content integrations are not enabled merely by choosing a tag.
