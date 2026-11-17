# Template tags in both forms · v0.5

One task, deciding what to do about review findings, written twice:
[with `{% … %}` tags](atoms/triage-findings.atom.md) and
[with JSX-style tags](atoms/triage-findings-jsx.atom.mdx). The two bodies mean the
same thing, and the catalog checker fails if they drift apart.

The example uses the tags planned with M11b:

- **A loop.** `for` repeats a line for each acceptance criterion and each finding.
- **A partial with a parameter and a slot.** [`_triage-rules.md`](atoms/_triage-rules.md)
  declares `strict` in its frontmatter and places the content it is given at
  `{% slot /%}`.
- **A renderer bound to a schema.** [`_finding.md`](atoms/_finding.md) declares
  `renders:` with the ID of [the finding schema](schemas/finding.schema.yaml), so
  `render` shows every finding the same way without repeating its layout.

The JSX-style atom reuses the first atom's schemas through explicit `$ref`s, the
case the conventions reserve them for, and includes the same `.md` partials: a
partial follows its own extension. An atom cannot exist in both forms under one
name, which is why the second file has its own.

Both forms wrap the criteria in a `<criteria>` section. A lowercase tag alone on
its line is literal prompt text and must be closed. The shared partial also
shows the one place the forms differ: it writes `src/<module>.py` as plain prose,
which a `.md` file allows. In a `.mdx` file the same text has to sit in a code
span, because there braces and angle brackets are syntax; the file is parsed,
never executed. See the
[template tags guide](../../docs/guides/template-tags.md).

Future commands, from this directory:

```sh
atomic check
atomic render triage-findings --input fixtures/triage-findings/blocker.input.json
atomic eval triage-findings
atomic eval triage-findings-jsx
```

`atomic render` prints the prompt without calling a model. The fixtures are
team-curated; findings that arrive from outside the team need v0.5's
external-origin controls.
