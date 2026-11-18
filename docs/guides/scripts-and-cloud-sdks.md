# Scripts and cloud SDKs

Keep executable work in scripts and prompt instructions in `.atom.astro` files.
Declare a model-free `exec` atom, give it input/output schemas, and compose it with
model atoms in a YAML workflow. Each operation becomes a sibling durable step;
script code must not create nested workflow steps.

```astro
---
export const config = {
  description: 'Compute an order total in integer cents.',
  kind: 'exec',
  exec: {command: ['python3', 'tools/total_order.py']},
};
---
```

An exec script reads JSON on standard input and writes its JSON result on standard
output. The complete input/output schemas remain the contract. The Python
[order report example](../../examples/03-order-report/README.md) executes its golden
fixtures in the offline catalog checker.

## Deno SDK scripts

The [S3 example](../../examples/11-aws-s3/README.md) separates fetching a report,
summarizing its typed output, human approval, and writing the digest. SDK imports
are pinned in a lockfile. Scripts declare the hosts, environment names and named
credentials they need. The credential backend and runtime permission integration
are staged product work, rather than capabilities granted by the file format.

```astro
---
export const config = {
  description: 'Fetch one operations report.',
  kind: 'exec',
  exec: {script: '../tools/fetch_report.ts'},
  network: {allow: ['acme-reports.s3.us-east-1.amazonaws.com']},
  env: {AWS_REGION: 'us-east-1', REPORTS_BUCKET: 'acme-reports'},
  credentials: ['aws-reports-reader'],
  runtime: {deno: {sys: ['osRelease']}},
  effects: {external: 'read-only'},
};
---
```

The fetch result includes report text, byte count and a SHA-256. A subsequent model
atom reads `input.report`; gate evidence uses `steps.fetch.output.report.sha256`.
This replaces implicit prepared values inside a prompt atom. Fetching and model
execution have independent checkpoints.

Writes require stable keys and an idempotency or reconciliation strategy. A human
gate authorizes the write; it does not deduplicate it. The S3 write uses conditional
creation and tests retries against a local fake service. Evals consume fixture
inputs and outputs; they do not execute external writes.

## Public configuration and credentials

Public profiles, project configuration and declared environment values may enter
readonly execution context. They are frozen for a run and carry provenance. Secrets
belong to the credential/tool channel, not prompt bindings or context checkpoints.
Astro expressions cannot access `Deno.env`, process APIs, files, sockets or npm.

Read [availability](../reference/availability.md) for the staged CLI/runtime
features and [atom files](../reference/atom-files.md) for the implemented compiler
contract.
