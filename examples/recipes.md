# Further recipes and the contracts they need

Use the smallest existing atom kind that can do the work. These recipes extend
the complete catalog; the table distinguishes behavior specified in the plan
from integration syntax that still needs design. They are not copy-paste runtime
configuration.

| Recipe | Phase | Composition and best practice | Contract still needed |
| --- | --- | --- | --- |
| CI failure triage | v0.5 | Deduplicated webhook → read-only diagnosis → proposed patch → review → human acceptance. The trigger tags log text external; a suggested command is data. | Ingress-to-workflow field binding and authenticated CI/repository identity |
| Parallel migration | v0.5 | Plan bounded items → gate → isolated implementers → candidate reviews → integration gate → merge queue. Rebase/conflict resolution triggers tests and review of the new SHA. | Stable runtime metadata for each fan-out child and the queue's reviewed-SHA contract |
| Incident diagnosis through OpenAPI | v0.5 | Import the monitoring spec; allowlist only read operations for diagnosis; propose remediation as typed output. A deterministic effect step performs an approved change through the ledger. | Exact toolset namespace, per-operation effect/reconciliation policy, and how external-input trust interacts with approved read tools |
| Refund proposal over an application database | v0.7 | Named read query → typed proposal → gate → deterministic transaction with durable application marker. Keep integer cents and row preconditions; never expose a write-capable SQL tool to the model. | Query discovery, mutation identity and application-marker configuration; a value matching the proposal alone does not prove that operation applied |
| Spreadsheet reconciliation | v0.7 | Exec parses typed file references → deterministic reconciliation → lite judgment for exceptions → gate → exec writes an output artifact. Parse untrusted files in the runner. | File mounts, media limits, runtime-image contract and reproducible toolchain provisioning from M52 |
| Permission-filtered research | v0.7 | Durable ingestion → retrieval under the run identity → grounded draft → citation validation → gate. Pin the index version and propagate source ACLs and sensitivity. | Exact retrieve-step schema, authorization snapshot/revocation behavior, citation and stale-chunk handling |
| Screenshot verification | v0.7 | Coding candidate → sandboxed browser capture → independent visual reviewer → gate showing the same candidate SHA and image hashes. | Media tag grammar and browser-tool permissions; the gate must not show a screenshot from a different commit |
| Mixed TypeScript atom | v1.0 | Checkpoint needed context in prepare siblings → model turn → validation. Use code only for logic CEL cannot express, not for repeated config defaults. | Final exported config schema, prepare-output types and durable capability API; current SDK snippets are design sketches |
| Cross-organization workflow | v1.0 | Use the A2A example with pinned identities, an outbound trust policy and a durable remote-task link. | Card pinning and operation-status reconciliation supported by the remote agent |

For each deployed recipe, keep secrets and admin credentials outside the runner,
pin actual execution inputs, and bind acceptance to the exact output or effect.
Runtime-owned metadata must be separate from model-generated fields. A dependency
on a finished reviewer is not a conditional branch on its verdict.
