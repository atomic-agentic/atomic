# Prompt expressions and components

An atom's prompt is written in `.atom.astro`. Atomic parses it with Astro's own
parser and walks it with a bounded, TypeScript-shaped interpreter. The source is
never executed as an Astro page and never imported as a JavaScript module.

Everything below is built unless it says otherwise.
[Availability](../reference/availability.md) is the summary for the whole
product; [atom files](../reference/atom-files.md) is the reference for
frontmatter, contracts, helpers and the resource limits.

## What the format gives you

| Construct | Purpose | Built |
| --- | --- | --- |
| `{input.field}` | interpolate a schema-typed value into a tagged user binding | yes |
| `{flag && <Fragment>…</Fragment>}` and `{cond ? <Fragment>…</Fragment> : null}` | select prompt content | yes |
| `{items.map(item => <Fragment>…</Fragment>)}` | repeat content; `filter`, `some` and `every` work the same way | yes |
| `<Component prop={value} />` | use a local component you imported by name | yes |
| `<Component>…</Component>`, `<slot />` | pass caller content in, render it where it belongs | yes |
| `<slot name="policy">fallback</slot>` | one named part of a layout, with fallback content | yes |
| `<rubric>…</rubric>` | a literal prompt section | yes |
| a frontmatter `const` local | bind a supported expression, evaluated when the component renders | yes |
| `config.examples` | named examples, checked against the complete contracts and appended to every prompt | yes |
| `config.validate` | interpreted typed validators over `{input, output}`; `case` is typed, but a run never supplies it | yes |
| `context.config`, `context.env` | read verified public host context | in the compiler and `atomic render`; see [Public context](#public-context) |
| a ` ```mermaid ` fence in the body | a diagram, read as literal text | yes |
| Markdown and MDX atoms | a Markdown or MDX prompt body | removed; `.atom.md` and `.atom.mdx` are refused by name |

## Start from generated types

Bind `Astro.props` explicitly against the declarations `atomic check` generates.
The types come from the atom's own schemas, so a missing property, a wrong
component prop or a validator that returns anything but a boolean fails the check
before any model is called.

```astro
---
import type { Props, Input, Output } from '../.atomic/types/triage-findings';
import type { AtomConfig } from '../.atomic/types/authoring';
import TriageRules from './TriageRules.astro';
import Finding from './Finding.astro';
const { input }: Props = Astro.props;
export const config: AtomConfig<Input, Output> = {
  description: 'Decide what to do about review findings on one work item.',
  validate: [({ input, output }) =>
    output.decision !== 'accept' || output.must_fix.length === 0],
};
---
<TriageRules strict={true}>
Findings marked blocker always go in must_fix.
</TriageRules>

Work item {input.item.id}: {input.item.title}

<criteria>
{input.item.criteria.map((c) => <Fragment>
- {c}
</Fragment>)}
</criteria>

{input.findings.length === 0 ? <Fragment>
No findings were reported.
</Fragment> : <Fragment>
Findings:
{input.findings.map((f) => <Finding finding={f} />)}
</Fragment>}
```

[The complete example](../../examples/08-templating/atoms/triage-findings.atom.astro)
uses the same compiler as production. `atomic check` enforces the interpreter
subset as well as the complete runtime schemas, not the type projection alone.

## Components and slots

A local `.astro` component declares a `Props` interface and destructures its
props. A `<slot />` receives the caller's nested prompt content.

```astro
---
interface Props { strict: boolean }
const { strict }: Props = Astro.props;
---
Sort the findings into immediate fixes and follow-up work.
{strict ? <Fragment>A missing test is a blocker.</Fragment> : null}
<slot />
```

Named slots take the caller's content for one part of a layout, with fallback
text for when the caller supplies none:

```astro
<policy><slot name="policy">Follow the team's default change policy.</slot></policy>
<slot />
```

```astro
<ReviewFrame>
  <Fragment slot="policy">Hold every change to the payments database.</Fragment>
  Review the attached change.
</ReviewFrame>
```

`Astro.slots.has('policy')` tells a component whether its caller filled the
slot, as Astro defines it: explicit content, even an empty fragment, is present
and suppresses the fallback; whitespace and comments are not content; and a slot
forwarded through a layout is present only when the slot it forwards is. See
[supported syntax and limits](../reference/atom-files.md#supported-syntax-and-limits).

Import components explicitly by their local filename, as a default import
ending in `.astro`. They share the atom's source containment rules and are
captured in its compiled source graph, so a run keeps the components it started
with. Cycles and imports that escape the project fail. There is no npm component
execution, no implicit schema-bound renderer lookup and no separate partial
parameter language. A component used without being imported is an error.

## Sections and grouping

Lowercase section tags such as `<criteria>` and `<rubric>` organize the prompt;
the model reads them as prompt structure, so the tags of an atom and of each
component it uses must balance, inside `{…}` expressions as well as outside
them. The load fails, naming the file, the line and the tag, when a closing tag
names another element than the innermost one open
(`<criteria><rubric>…</criteria>`), when a closing tag has nothing open to
close, and when an element is still open at the end of the file. A void HTML
element such as `<br>` or `<input>` and a self-closing tag such as `<Note />`
open nothing, so `<input>…</input>` is refused. Names match exactly, so the
component `<Input>` is not the element `<input>`. Which tags there are, and
where each closes, is read from Astro's own parse, so comments, a
```` ```mermaid ```` fence and the frontmatter hold none; where a start tag
ends is read by atomic, its quoted values and braced expressions passed over.
An element Astro reads as ending somewhere other than where it is written is
refused too: `<slot>`, `<template>`, `<noscript>`, `<frameset>` and `<head>`
after leading text or a leading expression, and a closing tag inside a `{…}`
expression the element did not open in. A literal section takes no attributes.
`<Fragment>` groups content without adding a section, and takes only a `slot`
attribute. The `<data-…>` prefix is reserved for atomic's own value wrappers,
and `<data-…>`, `<script>` and `<style>` are rejected wherever you write them.

## Data and prompt text

Interpolated values keep their input provenance and appear in tagged user
bindings, each carrying its path and origin; static text stays in the developer
instructions, which only point at those bindings. Components, sections, slots
and fragments preserve that separation, and an inserted string is never parsed
again as template syntax.

Expressions read input, verified public context, component props and callback
locals, and cover property reads, one-step optional reads such as
`input.extra?.note`, literals, template literals, comparisons, arithmetic,
logical operators, conditionals, object construction and destructuring. `map`,
`filter`, `some` and `every` take one expression-bodied callback, and a
predicate must return a boolean. [Supported syntax and
limits](../reference/atom-files.md#supported-syntax-and-limits) lists the whole
subset and what it refuses.

Use entities for literal angle brackets and braces: `&lt;module>` and
`&#123;placeholder&#125;`. A ` ```mermaid ` fence in the body is read literally,
so a diagram's braces and angle brackets stay diagram syntax; an unclosed one is
an error at its opening line, because everything after it would otherwise be
literal text. Elsewhere a prompt body is not Markdown, so a code fence protects
nothing: escape the angle brackets and braces in a shown example too. Whitespace
is prompt content, so review formatter output before accepting it.

## Validators and examples

```astro
---
export const config: AtomConfig<Input, Output> = {
  examples: [{ name: 'clean', input: { /* … */ }, output: { /* … */ } }],
  validate: [({ output }) => output.findings.length > 0],
};
---
```

`atomic check` validates every declared example against the atom's complete
input and output contracts, which is where constraints the type projection
erases, such as a `maxLength`, still apply. Validators are parsed into a private
AST and interpreted; they must return booleans, and a rejection quotes the rule
that failed into the repair turn. Quoted CEL validators are rejected. CEL stays
the language of YAML workflow mappings, gate titles and eval assertions.

Examples are prompt content as well as check fixtures. Every render appends each
declared example after the atom's own content: `Example "clean"`, then its
`input` and its `output`, bound in the user turn as team-origin data, the way
every interpolated value is. An example is therefore shown to the model, so it
must hold nothing the model should not see, and nothing keeps examples out of
the prompt.

## Public context

`context.profile`, `context.config`, `context.env`, `context.project`,
`context.run` and `context.step` are built in the compiler and in the types, and
`atomic render --context-settings` renders with them, checkpointing a context
under a `--run-id` and restoring it later. A validator may read `context` too.
See [context schemas](../reference/atom-files.md#context-schemas).

Nothing hands a running workflow its context settings yet. `atomic run` and
`atomic serve` pass none, so a step whose atom declares a context is refused
before its model is called, and where those settings come from at run time is
not settled. The pipeline step that would resolve a context once per run and
step, and replay the recorded one on a retry, is already written; it waits for a
command to supply the settings.

## What is gone

Markdown and MDX atom frontends and their tag vocabulary are removed. An
`.atom.md` or `.atom.mdx` file in a project is refused by name, not quietly
parsed by anything else; the compiler rejects unsupported executable syntax
before a model call, and changing an extension does not bypass that.

## Related

- [Atom files](../reference/atom-files.md) for frontmatter, contracts,
  TypeScript helpers and resource limits
- [How Atomic runs and confines atom code](atom-code-runtime.md)
- [Test and improve an atom](testing-and-evals.md)
- [VS Code](vs-code.md)
