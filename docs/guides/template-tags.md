# Template tags and the JSX-style form

An atom's body is its instructions. Tags in the body insert values, include
shared text, repeat a block for each item in a list and keep one data type looking
the same in every prompt. Tags are declarative: they never run code.

You can write the same atom in two forms. `.atom.md` uses `{% … %}` tags and is the
default. `.atom.mdx` uses JSX-style tags. Both mean the same thing and are checked
the same way; pick one per file.

The first tags (`$input` values, `if`, a plain `partial` and `example`) are part
of the planned v0.1 format. Loops, partials with parameters, `render` and the
`.atom.mdx` form are planned for v0.5; see
[availability](../reference/availability.md). The examples below come from
[the templating example](../../examples/08-templating/README.md).

## Insert a value

```markdown
Work item {% $input.item.id %}: {% $input.item.title %}
```

The path is checked against the atom's input schema when the project loads, so a
misspelled field fails `atomic check` before any model call.

Atomic places inserted values in the user turn and wraps each one in an element
that names where it came from. The wrapper's name carries a suffix that cannot
occur in the value, so text inside a value cannot end its own wrapper:

```text
<data-7f3a path="input.item.title" origin="team">Charge for every unit on an order line</data-7f3a>
```

The value itself goes in unchanged. You do not escape anything, and code in a
value stays intact. The exact wrapper spelling is a proposal.

The wrapper and its origin label belong to the v0.1 render step. The v0.1
target handles inputs the team supplies, so its labels are expected to read
`team`. Carrying an origin through atom outputs, stored text and the work
graph, and the controls for external input, arrive in v0.5.

## Include text only when it applies

```markdown
{% if has(input.plan) %}
Plan: {% $input.plan %}
{% else /%}
The customer's plan is not known.
{% /if %}
```

The condition is a CEL expression over the input, written without `$`. It is
checked against the input schema when the project loads. `{% else /%}` is
optional. Use `has(…)` to test a field the schema does not require.

## Repeat a block for each item

```markdown
Acceptance criteria:

{% for c in input.item.criteria %}
- {% $c %}
{% /for %}
```

`for` takes a variable and a list from the input. Inside the block the variable
is a value like any other: `{% $c %}` for a list of strings, `{% $f.file %}` for a
list of objects. Looping over something that is not a list fails at load.

## Reuse a fragment, with parameters

A partial is a shared piece of instructions in a file whose name starts with `_`.
It can declare parameters in its own frontmatter and mark where enclosed content
goes:

````markdown
---
params:
  strict: { type: boolean }
---
Sort the findings into what must be fixed now and what can follow up.
{% if params.strict %}
A missing test for new behaviour counts as a blocker.
{% /if %}
{% slot /%}
````

An atom passes the parameters and, optionally, content for the slot:

```markdown
{% partial file="_triage-rules.md" strict=true %}
Findings marked blocker always go in must_fix.
{% /partial %}
```

A partial with no parameters is still `{% partial file="_rules.md" /%}`. Passing a
parameter the partial does not declare, leaving one out, or giving content to a
partial that has no slot fails `atomic check`.

## Render a type the same way everywhere

When several atoms show the same kind of data, such as a review finding, bind one
partial to its schema:

````markdown
---
renders: https://atomic.example/schemas/finding-v1
---
- {% $value.severity %} in {% $value.file %}: {% $value.message %}
````

Then render any value of that type without repeating its layout:

```markdown
{% for f in input.findings %}
{% render $f /%}
{% /for %}
```

Atomic finds the partial by the value's schema ID. Exactly one partial may render
a given schema, and the fields it reads are checked against that schema.

## Give typed examples

An example is a named pair of JSON values: an input and the output you expect
for it.

````markdown
{% example name="ordinary-note" %}
```json input
{"text":"We fixed CSV export for file names containing accented characters."}
```
```json output
{"summary":"CSV export now handles file names containing accented characters."}
```
{% /example %}
````

Both blocks are validated against the atom's schemas when the project loads, so
an example cannot drift from the contract. Atomic shows the pair to the model as
a worked example and also runs it as an eval case. This is the one place JSON
belongs in an atom's body. In an atom with code, add a third block,
`json prepared`, with the values `prepare` would return for that example.

## Use values computed by code

An atom whose file starts with a `---ts` block can fetch or compute values
before the model call. Whatever the block's `prepare` function returns is
available as `$prepared`:

```markdown
Report ({% $prepared.report.bytes %} bytes):

{% $prepared.report.text %}
```

A prepared value is inserted the way an input value is: in the user turn, in
its own wrapper. Each path is type-checked against what `prepare` returns. The
body itself still runs no code. Planned for v0.7; see
[call AWS and other services with an SDK](scripts-and-cloud-sdks.md).

## Show the model a specific piece of code

A coding atom can include one function, or a range of lines, from the repository
it works on:

```markdown
The total is computed here:

{% file path="src/pricing.ts" symbol="totalFor" /%}

The rounding rules are in:

{% file path="src/money.ts" lines="10-50" /%}
```

The file is read from the step's worktree at its pinned commit, so a retry sees
the same code. The content arrives as labelled data in the user turn, with a size
cap. A path that does not exist fails at load where the repository is known; a
symbol or range that cannot be found fails the step and names what was missing.
Planned for v0.7, with the code-context work.

## Mark sections with XML-style tags

Lowercase element tags are a common way to structure a prompt, and both forms
accept them. They reach the model exactly as written:

```markdown
<criteria>

{% for c in input.item.criteria %}
- {% $c %}
{% /for %}

</criteria>
```

- **A section tag alone on its line must be closed.** An unclosed or mismatched
  section fails `atomic check`.
- **In `.atom.md`, an angle bracket inside a line of prose is plain text.**
  `Vec<String>` and `OUT_DIR/<stage>.log` need no escaping and are never checked.
- **Tags that start with `data-` are reserved.** Atomic uses them to wrap
  inserted values, so writing one yourself is rejected.
- **Leave a blank line after the opening tag.** Markdown otherwise treats the
  lines that follow as raw HTML, and Atomic's checks of headings inside the
  section are skipped. `atomic check` warns about a missing blank line.

A tag in a prompt is text for the model. Nothing in it runs, and Atomic's own
screens escape prompt text when they display it. Section tags come with each
form: v0.1 for `.atom.md`, v0.5 for `.atom.mdx`.

## The same atom with JSX-style tags

Name the file `.atom.mdx` and write the tags as elements. Values and expressions
go in braces:

````mdx
<Partial file="_triage-rules.md" strict={true}>
Findings marked blocker always go in must_fix.
</Partial>

Work item {input.item.id}: {input.item.title}

Acceptance criteria:

<For each={input.item.criteria} as="c">
- {c}
</For>

<If cond={input.findings.size() == 0}>
No findings were reported. Return accept unless a criterion is plainly unmet.
<Else />
Findings:

<For each={input.findings} as="f">
<Render value={f} />
</For>
</If>
````

| `.atom.md` | `.atom.mdx` |
| --- | --- |
| `{% $input.item.title %}` | `{input.item.title}` |
| `{% if <CEL> %}…{% else /%}…{% /if %}` | `<If cond={<CEL>}>…<Else />…</If>` |
| `{% for c in input.list %}…{% /for %}` | `<For each={input.list} as="c">…</For>` |
| `{% partial file="_x.md" strict=true %}…{% /partial %}` | `<Partial file="_x.md" strict={true}>…</Partial>` |
| `{% slot /%}` | `<Slot />` |
| `{% render $f /%}` | `<Render value={f} />` |
| `{% file path="src/a.ts" symbol="f" /%}` | `<File path="src/a.ts" symbol="f" />` |
| `{% example name="case" %}…{% /example %}` | `<Example name="case">…</Example>` |

What to know about this form:

- **Braces hold CEL, not JavaScript.** The body is parsed and interpreted. It is
  never compiled or executed, and `import` and `export` are rejected in it.
  Code goes in a `---ts` block at the top of the file, in either form.
- **Capitalized tags are Atomic's fixed set.** An unknown one fails the load.
- **Lowercase tags are literal text,** as in `.atom.md`. See
  [mark sections with XML-style tags](#mark-sections-with-xml-style-tags).
- **`<` and `{` in prose must be escaped.** The file is strict MDX, so a loose
  `<` or `{` fails at load with its line and column, and the error shows the
  line with the escape applied. Write `\<` and `\{`: the model sees a plain `<`
  and `{`. A code span works too, but the model then sees the backticks as well.
  `atomic check --fix` escapes every loose `<` and `{` in a file and changes
  nothing else. Comments are `{/* like this */}`, not `<!-- -->`.
- **The file stays valid MDX.** Formatters, editor extensions and other MDX
  tools read an `.atom.mdx` file as they read any MDX file.
- **One form per file.** An atom cannot exist as both `review.atom.md` and
  `review.atom.mdx`, and the two tag styles cannot be mixed in one file. A partial
  follows its own extension, so an `.atom.mdx` atom can include a `.md` partial.

## Which form to use

Use `.atom.md` unless you have a reason not to. Its tags almost never collide
with prompt text, which matters for instructions full of code, generics,
placeholders and email addresses: in `.atom.md`, a line such as
`From: {% $input.name %} <{% $input.from %}>` needs no escaping. Choose
`.atom.mdx` when your team already writes JSX, wants its MDX tools to work on
atom files, or structures prompts with XML-style sections. To switch an
existing atom, `atomic atoms convert` rewrites it in the other form.

## See the prompt before spending tokens

```sh
atomic render triage-findings --input fixtures/triage-findings/blocker.input.json
```

`render` prints the exact prompt the atom would send for that input, without
calling a model. `atomic check` also keeps a snapshot of the rendered prompt for
each typed example and fails when the prompt changes without the snapshot being
updated, so a change to instructions or to a shared partial shows up in review.
Both are planned for v0.5.

## What is checked when the project loads

- Every value path exists in the input schema, or in the loop, partial or
  renderer it is used in. A `$prepared` path exists in the type the atom's
  `prepare` returns.
- A `for` source is a list, and every block is closed.
- A partial's parameters match what it declares, with the declared types.
- Exactly one partial renders each schema that a `render` tag needs.
- Section tags on their own line are balanced, and none uses the reserved
  `data-` prefix.
- In `.atom.mdx`, every capitalized tag is known, and no `<` or `{` is left loose
  in prose.

Next: [atom file reference](../reference/atom-files.md),
[test and improve an atom](testing-and-evals.md) or
[diagrams](diagrams.md).
