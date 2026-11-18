# Budgets

Budgets bound the work an atom or workflow can attempt. They are part of the
**v0.1 target**. The fields below are the proposed format-v1 contract, shared with
the scaffold plan; they have not been exercised against an Atomic runtime.

You can start with detected model access and finite runtime defaults. Inspect
those defaults with `atomic config --explain`; override a cap when the task needs
it. No particular numeric default is promised by this draft.

## Set a cap

Add this frontmatter fragment to a model-backed atom:

```yaml
budget:
  tokens: 20000
  minutes: 5
```

For API-billed calls with reliable model pricing, you can also set:

```yaml
budget:
  tokens: 20000
  minutes: 5
  cost_usd: 1.00
```

| Field | Meaning |
| --- | --- |
| `tokens` | Positive integer; cumulative input and output tokens for actual model calls |
| `minutes` | Positive number; cumulative active execution time, excluding gate waits and time with no runtime |
| `cost_usd` | Positive number; cumulative provider model-call cost in US dollars where reliable pricing is available |

All configured limits apply. Omitted limits inherit resolved defaults. These are
execution limits, not a provider invoice or a reservation of subscription capacity.
A dollar cap cannot be enforced without reliable pricing; configuration is rejected
in that case. For subscriptions or local models without dollar pricing, use token
and time caps. Service charges from unrelated tools are outside the model-cost cap.

A cap stops a run that is already spending. To see the expected spend first, the
planned `--plan` option on `atomic eval` and `atomic run` prints the model calls
and an estimated cost range, beside the cap, without calling a model.

## Bound a whole workflow

A workflow can set the same top-level block:

```yaml
budget:
  tokens: 60000
  minutes: 15
```

Place it beside `input`, `output` and `steps` in the workflow file. This bounds the
whole run, including model-backed children and reviewers. Each atom's own limits
also apply; an atom cannot spend past the remaining parent allowance. A nested
workflow does not create a new parent budget.

## Retries and recovery

Actual retries and schema-repair calls consume the same cumulative allowance.
Reusing a completed checkpoint does not spend tokens again. `runs resume` retains
usage and reservations; it does not replenish the budget. A gate's `timeout` is a
separate wall-clock deadline and continues during an offline interval.

Before dispatch, Atomic durably reserves against the atom and every parent budget
under a stable request/attempt ID, independently of output checkpoints. Settlement
is idempotent by that ID. Failed or rejected attempts still count; an uncertain
request retains its maximum reservation until usage is reconciled. A new actual
retry needs another reservation rather than reusing uncertain allowance.

For a supported bounded request, reserve its whole provider maximum: intentional
unreserved overshoot is zero. Any declared excess is bounded by `max(0, B - R)`
per in-flight request, where `B` is the provider maximum and `R` the reservation;
parallel bounds sum. Unknown bounds cannot support a hard dollar cap. Provider
billing deviations still require reconciliation, so this is not an invoice ceiling.

When a cap prevents more work, the run parks for inspection rather than silently
starting another attempt. Already-started work and uncertain provider usage can
require reconciliation; a configured cap is not a promise of a perfectly bounded
provider bill. Pending usage is not treated as free after a crash.

Inspect the run with `atomic runs show <run-id>`. This draft does not define a
budget-increase command; resuming alone cannot bypass a cap.

See [atom fields](atom-files.md), [workflow fields](workflow-files.md#top-level-fields)
and [observability](../operations/observability.md).
