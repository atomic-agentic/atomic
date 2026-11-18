# Fetch, summarize and store a report

This staged SDK integration example has four explicit workflow operations:

1. [fetch-report](atoms/fetch-report.atom.astro) runs [fetch_report.ts](tools/fetch_report.ts), returning the report text, byte count and SHA-256.
2. [summarize-report](atoms/summarize-report.atom.astro) receives that result as ordinary typed `input.report` and drafts a digest.
3. A person approves the digest and the hash of the report it summarizes.
4. [put-digest](atoms/put-digest.atom.astro) runs [put_object.ts](tools/put_object.ts), using conditional creation to avoid overwriting an existing object.

The [workflow](workflows/digest-report.workflow.yaml) composes these as sibling
steps. The Astro files contain declarative configuration and prompts; SDK code
lives in separate scripts and creates no nested durable steps. The gate reads
`steps.fetch.output.report.sha256`. There is no implicit prepared-value namespace
or executable script fence inside a prompt atom.

The SDK is pinned in [deno.lock](deno.lock). Script declarations specify allowed
hosts, public environment values, named short-lived credentials and whether the
external effect is read-only or idempotent. The production Astro loader does not
yet enable these credential/network integrations. Their policy wiring and workflow
scheduling remain staged; the example is executable through its local test harness.

## Check the files

From the repository root, with pinned Deno 2.9.8 available:

```sh
python3 tools/check_examples.py
```

The checker validates all fixture contracts, type-checks the Astro prompt through
the shipped compiler, renders its input and executes its typed validators. It also
checks pinned script imports and declarations without contacting S3.

## Exercise the scripts against a fake service

From this directory:

```sh
export DENO_DIR="${DENO_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/atomic-sdk-deno}"
deno cache --lock=deno.lock --frozen tools/fetch_report.ts tools/put_object.ts
deno test -A --lock=deno.lock --frozen --cached-only test/
```

The harness starts a local fake S3 server. Tests exercise the read and conditional
write paths, permission denial and checkpoint reuse, and type-check report values.
No AWS account, provider model or live S3 bucket is involved. The harness simulates
checkpoint behavior; it does not establish production workflow crash recovery.

Evals for the summarizer receive committed fetched-report input. Evals never run
external writes. Real workflows must enforce declared permissions, inject secret
credentials through the dedicated channel, and reuse completed fetch/script/model
checkpoints on restart. A gate authorizes storage but does not deduplicate it.
