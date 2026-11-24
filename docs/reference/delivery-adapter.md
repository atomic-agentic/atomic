# Delivery adapter protocol

`atomic delivery start` asks a human to authorize pushing a reviewed commit, creating or updating its pull request, and enqueuing that pull request. The operator installs an executable for their hosting provider. Atomic invokes it once per request and keeps the operation ledger in the project's SQLite store.

## Installation

Install `adapter` and `target.json` in `$ATOMIC_HOME/private/<project-id>/delivery/`, outside the checkout. When using the default atomic home, use the equivalent project private directory there. The executable runs with that directory as its working directory, a cleared environment and no arguments. It reads one JSON object from standard input and writes one JSON object to standard output. Atomic launches it directly. Use absolute paths for tools the adapter needs and obtain provider credentials from protected operator storage.

On Unix, the private project directory, the delivery directory and both files must belong to the operator and permit writes only by that owner. Use regular, single-link files for the configuration and executable. Mark the executable runnable.

This target creates a new delivery branch.

```json
{
  "repository": "team/widgets",
  "branch_ref": "refs/heads/atomic/fix-42",
  "base_ref": "refs/heads/main",
  "expected_old": null,
  "pull_request_number": null,
  "title": "Fix issue 42",
  "body": "Reviewed fix for issue 42."
}
```

The adapter maps `repository` to the provider repository. The two refs must be distinct, fully qualified branch names. Set `expected_old` to the expected full commit SHA when updating a branch. A null or omitted value requires an absent branch. Set `pull_request_number` to a positive number when targeting a known PR. Otherwise, use the unique open PR for the approved repository, head ref and base ref, or create it. Refuse ambiguous matches. Atomic supplies the reviewed commit SHA as `head` and presents the resolved requests at the gate. Keep credentials out of these displayed values.
The configured `base_ref` must equal the protected branch ref used by the
source's successful trial merge. The gate also shows that trial's checked
branch head SHA and its proposal digest.

## Requests and observations

Each request carries `version` (currently `1`), `method` (`apply` or `inspect`), `slot`, a stable 64-character hexadecimal `key`, the operation's `request` object and `number`. The envelope's `number` is null until enqueue, when atomic supplies the PR number from its recorded receipt.

| Slot | Fields in `request` | Provider obligation |
| --- | --- | --- |
| `push` | `repository`, `branch_ref`, `head`, `expected_old` | Atomically compare the old ref with the approved expectation and update it to the exact reviewed SHA. |
| `pull_request` | `repository`, `branch_ref`, `base_ref`, `head`, `title`, `body`, `expected_number` | Create or update the identified PR at the approved head. Honor the specified number when present. |
| `enqueue` | `repository`, `branch_ref`, `base_ref`, `head` | Enqueue the numbered PR only while its head still equals the approved SHA. |

An `apply` request carries a single dispatch authorization. Use its key for provider idempotency when supported. An `inspect` request is read-only. Read authoritative provider state for the original operation and key. Echoing the request alone cannot establish that an operation happened.

Return `{"version":1,"key":"<request key>","state":...}` with exit code zero for a readable result. Use JSON null for absent state. For a non-null state, include the following fields.

| `kind` | Fields to return |
| --- | --- |
| `push` | `repository`, `branch_ref`, observed `head` |
| `pull_request` | `repository`, `branch_ref`, `base_ref`, observed `head`, positive `number`, `title`, `body`, boolean `open` |
| `queue` | `repository`, `branch_ref`, `base_ref`, accepted PR `head`, `number`, `status` (`queued`, `merged`, or `removed`) |

Atomic accepts only an exact match. A merged entry can prove queue admission when the provider can identify the reviewed PR head that entered the queue. A merge commit SHA alone cannot establish that relationship. For unreadable or ambiguous provider state, exit unsuccessfully. The protocol bounds each request and stdout response to 64 KiB, retains at most 4 KiB of stderr and allows 30 seconds per invocation.

## Recovery

Atomic commits the intent before calling `apply`. A lost response, failed process or interrupted run leaves that slot eligible for inspection. An exact match records the receipt. Otherwise the workflow parks as `outcome_unknown`. After investigating the provider state, release the named park and run `atomic delivery resume <run-id>`. This resumes inspection using the original key. The next slot starts only after its predecessor has a confirmed receipt.
