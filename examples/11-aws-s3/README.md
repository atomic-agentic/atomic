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

Nothing in CI runs these tests. CI checks this example with
`python3 tools/check_examples.py`, which uses the pinned Deno only as the host
for the shipped Astro compiler: it checks script imports against the lockfile,
the atom declarations and the fixtures, and it never runs a script or a test. So
run these by hand before changing a script or one of its tests.

From this directory, with pinned Deno 2.9.8 on `PATH`:

```sh
export DENO_DIR="${DENO_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/atomic-sdk-deno}"
deno cache --lock=deno.lock --frozen tools/fetch_report.ts tools/put_object.ts
deno test -A --lock=deno.lock --frozen --cached-only test/
```

`DENO_DIR` must be exported for both commands: it names the package cache, and
`--cached-only` means the test run reads the SDK from that cache instead of
reaching the registry. Use the same `DENO_DIR` for `deno cache` as for
`deno test`, or the second command finds nothing cached. Inside the development
container `HOME` is the cache directory mounted from the host, so put `DENO_DIR`
under it to keep the packages between runs; `scripts/dev.sh` does not pass
`DENO_DIR` through, so set it in the command you give that script.

The harness starts a local fake S3 server. Tests exercise the read and conditional
write paths, permission denial and checkpoint reuse, and type-check report values.
[scripts_test.ts](test/scripts_test.ts) runs each script as the child process a
step is, with only the permissions and variables the atom declares;
[fetch_report_test.ts](test/fetch_report_test.ts) calls the fetch in the test
process, which is what shows whether a refusal ended its own transfer or the
process simply exited. No AWS account, provider model or live S3
bucket is involved. The harness simulates checkpoint behavior; it does not
establish production workflow crash recovery.

Evals for the summarizer receive committed fetched-report input. Evals never run
external writes. Real workflows must enforce declared permissions, inject secret
credentials through the dedicated channel, and reuse completed fetch/script/model
checkpoints on restart. A gate authorizes storage but does not deduplicate it.
