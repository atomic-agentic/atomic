# Classify one note · v0.1

A lite atom classifies team-curated engineering notes. It has no tools, a new
thread per input, and a bounded schema. Use Codex atoms when repository access is
needed; this task only needs a model call.

Read [atomic.yaml](atomic.yaml), [the atom](atoms/classify-note.atom.astro), and its
[input](atoms/classify-note.input.schema.yaml) and
[output](atoms/classify-note.output.schema.yaml) contracts. The frontmatter only
declares the format, description and non-default kind. The typed example is also
an eval case in the proposed runtime.

Future commands from this directory:

```sh
atomic check
atomic eval classify-note
atomic run classify-note --input fixtures/classify-note/export-crash.input.json
```

The instruction-in-text case checks useful behavior; it is not a guarantee of
injection resistance. This v0.1 example uses curated fixtures. Deploying it on a
public issue feed requires M14's external-origin controls and an explicit trust
declaration in v0.5. The loader's default budget and detected provider should be
visible in `atomic config --explain`; the example does not choose them for you.
