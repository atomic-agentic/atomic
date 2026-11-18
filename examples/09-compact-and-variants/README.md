# Schema reuse, a variant and JSONL cases

[The ticket labeler](atoms/label-ticket.atom.astro) uses full JSON Schema in
literal Astro configuration. Generated `Props` describe its required subject/body
and optional plan. Missing optional values do not implicitly become nullable.

[The stricter variant](atoms/label-ticket.strict.atom.astro) explicitly references
the same contracts under [expanded/](expanded/label-ticket.input.schema.yaml).
Its prompt changes the urgency rule. Variants are separate Astro sources with
explicit schema reuse; production loading does not infer a base contract from
punctuation in a filename.

[cases.jsonl](evals/label-ticket/cases.jsonl) holds one eval case per line.
[cases.outputs.jsonl](fixtures/label-ticket/cases.outputs.jsonl) supplies illustrative
golden outputs for offline checking, not recorded model responses. The catalog
checks the contracts, compiles/types/renders the prompt and executes typed
validators. Live comparison and cost-preview commands remain staged.

```sh
atomic check
```

The directory retains its catalog number and historical name. Compact-schema
syntax and its expansion command remain separate planned work; these Astro atoms
use the full JSON Schema contract directly.
