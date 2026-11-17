# Examples

Choose an example for the next thing you want to learn. You can start with a prompt
and add reusable definitions as needed.

| Example | What you learn | Target |
| --- | --- | --- |
| [Build a reading-list app](../getting-started/quickstart.md) | Ordinary interactive coding, follow-ups and agent questions | Full interactive CLI |
| [Summarize a note](../getting-started/first-atom.md) | A small typed lite atom in one file | v0.1 |
| [Review and approve a summary](../getting-started/first-workflow.md) | Sequential composition and a local durable gate | v0.1 |
| [Classify an engineering note](../../examples/01-classify/README.md) | Minimal configuration and adjacent schemas | v0.1 |
| [Review a coding change](../../examples/02-change-request/README.md) | Isolated implementation and runtime-owned candidate commits | v0.1 target; executable fixture setup |
| [Compute and report an order](../../examples/03-order-report/README.md) | Deterministic exec atoms around a model judgment | v0.1 |
| [Add review to an existing repository](../../examples/04-embedded-review/README.md) | Embedded `.atomic/` layout and patch-text input | v0.1 |
| [Draft a support response](../../examples/05-support-draft/README.md) | External text, independent review and team approval | v0.5 |
| [Share workspace contracts](../../examples/06-workspace/README.md) | Shared schema IDs across member projects | v0.5 · M11b/M39b |
| [Delegate translation over A2A](../../examples/07-a2a-translation/README.md) | Typed remote handoff and secret references | v1.0 |
| [Write one atom in both template forms](../../examples/08-templating/README.md) | Loops, partials with parameters, schema-bound rendering, and the JSX-style `.atom.mdx` form | v0.5 |
| [Use compact schemas, a variant and JSONL cases](../../examples/09-compact-and-variants/README.md) | Short inline schemas, two versions of one atom side by side, many eval cases in one file | v0.5 |
| [Record a decision with its evidence](../../examples/10-recorded-decision/README.md) | A choice in the manifest, the eval result it rests on and the date to review it | v0.7 |
| [Read from and write to S3 with the AWS SDK](../../examples/11-aws-s3/README.md) | TypeScript in an atom that imports an SDK from npm, declared hosts and short-lived credentials, a gate that shows what the model read, and a write that is safe to repeat | v0.7 |

The [source catalog](../../examples/README.md) includes complete project files,
schemas and fixtures. Its Python checker can validate those artifacts now; there
is no Atomic runtime in this repository to execute the CLI tutorials yet.

[Further recipes](../../examples/recipes.md) cover migrations, incidents, SQL,
spreadsheets, retrieval, screenshots and cross-organization workflows. They call
out missing contracts rather than presenting guessed integration configuration.
