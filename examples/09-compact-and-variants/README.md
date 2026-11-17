# Compact schemas, a variant and JSONL cases · v0.5

A small ticket labeller that shows three authoring shortcuts. JSONL cases are part
of the v0.1 target; compact schemas and variants are planned for v0.5.

**Compact schemas.** [The atom](atoms/label-ticket.atom.md) declares its input and
output inline, in the compact form:

```yaml
input:
  subject: string, the ticket's subject line
  body: string, the customer's message
  plan?: string, the customer's plan when known
```

A field is `name: type, description`. Fields are required unless the name ends in
`?`, and extra properties are not allowed. [`expanded/`](expanded/label-ticket.input.schema.yaml)
holds the JSON Schema this compiles to, which is what `atomic schemas expand`
would print; the catalog checker fails if the two disagree. In Atomic a `?` field
may be omitted but may not be `null`. A schema that needs a constraint such as
`minLength` is written as full JSON Schema.

**A variant.** [`label-ticket.strict.atom.md`](atoms/label-ticket.strict.atom.md)
is variant `strict` of the same atom. It shares the base atom's schemas and eval
cases and changes only the instructions, so the two can be compared directly.

**Many cases in one file.** [`cases.jsonl`](evals/label-ticket/cases.jsonl) holds
one eval case per line, with the same `input` and `expect` keys as a YAML case.
The matching [`cases.outputs.jsonl`](fixtures/label-ticket/cases.outputs.jsonl) is
an illustrative golden output per line for the offline schema check, not a
recording of a model.

Future commands, from this directory:

```sh
atomic check
atomic schemas expand label-ticket
atomic eval label-ticket --plan
atomic eval label-ticket
atomic eval label-ticket --compare strict
```

`--plan` prints the expected number of model calls and a cost range without
calling a model. The exact spelling for comparing against a variant is still to
be settled. The cases include ticket text that tries to give instructions; ticket
text from outside the team needs v0.5's external-origin controls.
