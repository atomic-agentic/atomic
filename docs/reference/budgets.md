# Budgets

Budgets bound the work an atom or workflow can attempt. They are part of the
**v0.1 target**. The `budget` field, its defaults, the price table and the usage
ledger described below are implemented and tested against the scripted fake
model. `atomic config --explain` shows the budget settings and where each came
from, and `atomic runs show` shows a run's model calls and tokens.

You can start with finite runtime defaults. Inspect them with
`atomic config --explain`; override a cap when the task needs it. The current
defaults are provisional and may change as they are tested against real
providers.

## Set a cap

Add this frontmatter fragment to a model-backed atom:

```yaml
budget:
  tokens: 200000
  minutes: 20
```

For API-billed calls with reliable model pricing, you can also set:

```yaml
budget:
  tokens: 200000
  minutes: 20
  cost_usd: 1.00
```

A cap must leave room for at least one model call. With the default call bound
below, one call reserves 80,000 tokens and 10 minutes, so these caps admit two
calls' worth of tokens at a time. A smaller cap fails configuration, with a
diagnostic that names the `call` settings, or the model's `max_input_tokens` and
`max_output_tokens`, that make up the reservation.

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

## Defaults and prices

Omitted limits come from the `budget` section of the configuration, which layers
like every other setting: your own configuration
(`~/.config/atomic/config.yaml`), then `atomic.yaml`, then `atomic.local.yaml`,
then `ATOMIC_BUDGET_*` variables such as `ATOMIC_BUDGET_ATOM_TOKENS`, each limit
taken from the last of them that sets it. A variable atomic cannot read as its
limit is refused, naming the variable. `pricing` is one setting, not one per
model: a layer that sets it replaces the whole table below it rather than adding
to it.

```yaml
budget:
  atom:            # what each atom invocation inherits
    tokens: 500000
    minutes: 30
    cost_usd: null # no dollar cap unless you set one
  workflow:        # what a whole run inherits, children included
    tokens: 2000000
    minutes: 120
    cost_usd: null
  call:            # the most one model call may use, when the model has no entry below
    input_tokens: 64000
    output_tokens: 16000
    minutes: 10
  pricing:         # US dollars per million tokens, by model name
    gpt-example:
      input_usd_per_mtok: 2.5
      output_usd_per_mtok: 10
      max_input_tokens: 128000   # optional: this model's own maximum per call
      max_output_tokens: 16000
```

Token and time caps always resolve to finite values. A dollar cap, written on an
atom or workflow or set as a default, needs every model it covers in `pricing`;
otherwise configuration fails before the run starts, with a hint to cap tokens or
minutes instead. Every value must be finite and positive (a price may be zero);
anything else, or an amount too large to account exactly, fails configuration.

A run resolves its caps, call bounds and prices once, when it starts, and keeps
them: a run resumed after the configuration changed is metered as it began.
`atomic config --explain` shows each budget setting with the layer it came
from.

Every model call a run makes is metered: a step whose atom calls a model and
that the run has no caps for is refused, never run without a meter. A live
`atomic eval` is metered as a step is (see [eval cases](eval-cases.md)). The
hidden developer diagnostics `atomic debug codex-prompt` and `atomic debug probe`
make the only model call atomic makes without a meter, and say so on stderr.

## Bound a whole workflow

A workflow can set the same top-level block:

```yaml
budget:
  tokens: 600000
  minutes: 45
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
request keeps its maximum reservation charged. atomic has no lookup of what one
request used, and OpenAI, Azure OpenAI and local servers, as atomic calls them,
offer none: on the Responses API atomic asks for its responses not to be kept for
retrieval (`store: false`), it does not ask for Chat Completions to be stored,
OpenAI's usage reports are totals over time rather than per request, and local
servers keep no record. An unknown or partly known charge therefore keeps its
value. A new actual retry needs another reservation rather than reusing uncertain
allowance.

For a supported bounded request, reserve its whole provider maximum: intentional
unreserved overshoot is zero. Any declared excess is bounded by `max(0, B - R)`
per in-flight request, where `B` is the provider maximum and `R` the reservation;
parallel bounds sum. Unknown bounds cannot support a hard dollar cap. Provider
billing deviations still require reconciliation, so this is not an invoice ceiling.

Admission reserves the call's whole bound: the model's `max_input_tokens` and
`max_output_tokens` from `pricing`, else the `call` defaults, priced at the model's
rate, and the `call` time. The call is held to that bound: a lite call is sent
with the bound's output tokens as its output limit, in the field its endpoint
reads (`max_completion_tokens` on OpenAI's and Azure OpenAI's Chat Completions,
in any region or cloud, `max_tokens` on other OpenAI-compatible servers,
`max_output_tokens` on the Responses API), and is cut off when its reserved time
runs out. The field is told from the endpoint's URL. A lite runtime can name the
field itself, as one must for a gateway the URL does not identify, but no
configuration setting or command-line flag exposes that yet. An answer cut off
at its output limit fails as such, without a repair turn. A lite request larger
than the bound's input tokens is recorded in the attempt's declared maximum.

The usage a response reports is settled before its answer is judged, so a refusal
or an empty answer is charged what it spent. A response that reports no usage is
settled as unknown: its tokens and dollars stay charged at its reservation, and
it is never taken as free. Its active time can always be measured, so it is
charged the time that passed, at most what was reserved. A call still running
when its process is stopped by a signal is settled this way as the process
stops. While a call runs, its meter records every few seconds that it is still
alive, so a call whose process was killed outright is charged only until shortly
after the last such record, however long the process stayed down, once its atom,
run again, admits its next call.

A Codex turn can make several provider requests and run tools between them, so
the `call` time does not bound it. A turn reserves the active time its atom has
left (or less, if the run has less), and is interrupted when that runs out; the
run then parks, as at any other cap, and the park's reason names the allowance
that ran out. With less time left than one request's bound, a turn is not
started at all: the run parks without spending. A turn that completes is settled
with the usage it reported. A turn stopped with a provider request in flight is
charged what its requests reported and an estimate for the request that reported
nothing: one request's bound, or the size of the turn's last reported request if
that was larger, since a turn's requests grow and can pass the configured input
maximum. It is never charged less than its reservation. A turn interrupted when
its time ran out is taken to be in flight unless it had completed: a tool that
yields back to the model while it runs leaves no sign of the requests made
meanwhile, so a turn interrupted while a tool ran may be charged one request too
many, never one too few. A turn that failed is in flight when a response began
and broke off, including a stream that Codex retried before giving up, or one
that ended incomplete; one whose request the provider refused outright, without
retrying, is charged what it reported. A turn's reported tokens can exceed its
reservation, since every request sends the conversation again; the ledger
records the overshoot, including the estimate added to it. A running turn records
what it has reported each time that grows, charged the same way, with one request
taken to be in flight, so a turn whose process is stopped by a signal or killed
before the turn settles stays charged at least what it last recorded having
reported and that request, never just its reservation. Usage reported after the
last record, and before the stop, is not known.

An exec atom's command counts toward `minutes` the same way: its step reserves
the active time its atom has left (or less, if the run has less), at most the
command's own five-minute limit and the 30 seconds its runner is allowed on top.
The command is stopped when that time runs out or at its own limit, whichever
is first. The step is charged from the command's admission to its settlement:
the command itself, and also starting its runner, trying again while another
runner held its workspace, and storing its output files. Only the command is
stopped at the cap, so a command the cap stops can take its run past the cap by
that extra time: up to its runner's 30 seconds, two retries a second apart, and
about 1.4 seconds of waiting while a cleanup holds the artifact store, so about
35 seconds, plus the time its output files take to store. Each exec step
running at the same time can go past by that much. A
command uses no tokens and costs nothing, so an exec atom's budget needs no
price, even under a dollar cap, and only its time is checked: a run whose
tokens or dollars went past their caps on earlier model calls still runs it.
With less than half a second left, the command is not started: the run parks,
and once a person raises the cap and releases it, the command runs. An exec
atom's `minutes` cap below half a second, or a run's when it has exec atoms,
fails configuration, since no command could ever start under it. A command
the cap stops mid-run parks the run the same way if its atom declares
`effects: { external: 'idempotent' }` or `'read-only'`, and once the cap is
raised it runs again from the start; any other fails, since whether its effects happened is
unknown, and the error says so. Time at a gate is never charged. An exec atom's
validators and the commits atomic makes after a worktree step are not metered
yet.

A run resolves the model each atom calls with its budget, and the atom runs on
that model for the whole run. Under a dollar cap, a call to a model the run's
budget does not price is not made.

When a cap prevents more work, the run parks for inspection rather than silently
starting another attempt. The call is not made, the park is recorded with its
reason, and the run waits on it; resuming it finds it parked again. Uncertain
provider usage stays charged; a configured cap is not a promise of a
perfectly bounded provider bill. Pending usage is not treated as free after a
crash.

A released park runs the parked atom again, as new attempts at the same requests,
and admits its calls against the caps as they are stored then. Releasing alone
raises nothing, so a run released with no more room parks again. Room comes from
raising the cap in the usage ledger, from a person entering an attempt's real
usage in the ledger in place of its reservation (no command does that yet), or,
for a refusal caused by a sibling's reservation, from that sibling settling.

Inspect the run with `atomic runs show <run-id>`. This draft does not yet define
the command that raises a cap and releases the park; resuming alone cannot bypass
a cap.

See [atom fields](atom-files.md), [workflow fields](workflow-files.md#top-level-fields)
and [observability](../operations/observability.md).
