# Model layer

An atom asks for a model tier rather than a particular provider. Resolving the
tier, applying quotas and sensitivity rules, falling back when a provider fails
and attributing cost all happen in one layer, so changing providers is a
configuration change and never an edit to atoms. The user-facing description of
tiers is in [models](../models.md).

## Tiers and routing

There are four tiers, from least to most capable: `basic`, `fast`, `advanced`
and `frontier`. The project maps each tier to an ordered list of provider and
model entries. Managed requirements narrow those lists: an entry naming a
provider or model the administrator has not allowed is removed before any
routing happens (M27). A tier entry will also be able to state the highest
sensitivity label it may receive (see
[sensitivity routing](#sensitivity-routing)) and what its model can do, for
example vision or audio, so that `atomic check` can refuse an atom whose needs
its tier cannot meet.

## Providers and portability

Codex was built around OpenAI's APIs, but it already has providers for cloud
model accounts and for local servers. The endpoint atomic talks to is any
OpenAI-compatible API, so vLLM, Ollama, LM Studio, llama.cpp and similar local
servers are ordinary providers (M34a). Every endpoint gets a concurrency limit;
when many atoms run at once against one local server, the extra requests wait
their turn instead of swamping it.

Whether models from other providers work well as Codex atoms is a question to
measure. The M12 evals will be run against several providers, and M34 will
publish the results as a cross-provider eval report. A model that struggles
inside the full agent loop can still serve lite atoms, which make one call
constrained by a schema; they stay the path that works across providers.

## Small models

One hypothesis runs through much of the design: that a dependency-aware work
graph can make open-weight models of around 30B parameters reliable enough for
long-horizon coding. The argument is that the decomposition is held by the
graph, not by the model, each step is shown a single ready item with its
dependencies, and context does not pile up from step to step. atomic adds a new
thread for each step, checks every output against its schema and allows one
repair turn, retries a failed step from its last checkpoint, and puts gates
where mistakes are costly.

Nobody has measured this yet. The M34 benchmark exists to test it. It will run
one set of long, multi-session tasks on one model twice, with the work graph and
without it, record the arm, cost and hardware of every attempt, and report pass
rates with confidence intervals. The harness will be published so that others
can repeat the run. Until results exist, the claim is only a hypothesis.

Guidance for serving local models to a team will cover prefix caching, which
helps because atomic's prompts begin with stable content, and will give a
sizing figure, how many tasks one GPU completes in an hour at each model size,
once M34 has measured it.

## Fallback

When a provider returns an error or a quota runs out, the step moves on to the
next entry in its tier and notes the fallback in its output. A fallback is never
applied partway through a turn. The model it falls back to has to be cleared for
the thread's sensitivity label, exactly like the first choice (M34b).

## Sensitivity routing

Choosing a model from a step's input is not enough to keep sensitive data on
local models. An atom can begin with harmless input and then, mid-turn, open a
labelled file or get back a sensitive tool result before it makes its next model
request. M56 therefore checks each request, not each step.

- **Labels follow content into the thread.** Nothing a tool returns joins the
  context unclassified. The label comes from where the content came from, such
  as a data-source column, a path or artifact marked sensitive, or the label of
  a retrieved chunk, and from detectors that inspect the content itself. A
  thread that has taken in sensitive content stays sensitive for the rest of its
  life, including everything the model writes afterwards.
- **One check before every send.** Every request goes through the same policy
  point. That includes the opening request of a turn and each follow-up once a
  tool has answered, repair turns, calls made by reviewers and decision atoms,
  lite atoms, embeddings, compaction, sub-agent threads and fallbacks. If the
  request's label is higher than the target provider is cleared for, it is not
  sent.
- **A blocked request restarts the step.** A model is never swapped mid-turn, so
  the step starts again from the beginning on a provider that is cleared. The
  label stays recorded on the step, which means retries, repairs and reviewers
  begin at that level. If the tier has no cleared model, the step parks. Each
  decision is audited along with its reason.
- **Prevention first.** When an atom is routed to an outside provider, its
  sandbox profile refuses reads of labelled paths and its data-source grants
  leave out sensitive columns. On the atom, `sensitivity.clearance` names the
  highest label its requests may carry, and `atomic check` rejects an atom whose
  grants would let it read data above that label.
- **Scope.** This protects data that was labelled where it came from or that a
  detector caught. Content that is neither labelled nor detected is outside the
  guarantee.

## Enforcement point

The check will exist in two forms, one per kind of profile, both generated from
the same declarations:

- **Local profile.** A hook in Codex's model-provider layer, running inside the
  atomic process.
- **Team and gateway profiles.** Model and tool traffic passes through
  agentgateway, which every provider base URL points at, and the gateway
  enforces policy on the wire. atomic does not maintain a second policy
  language for it: it compiles its own declarations (tiers, fallback order,
  quotas, cost attribution, clearances) into agentgateway configuration.
  `atomic check` validates that output, `atomic config --explain` shows its
  differences, and `atomic serve` reloads it.

In the gateway profiles each run gets a short-lived JWT from the control plane,
sent with every model and tool request. It carries these claims:

| Claim | Meaning |
| --- | --- |
| `run`, `step`, `atom`, `project` | Which piece of work sent the request |
| tenant (`org`) | The organization that owns the run |
| `tier` | The model tier the atom declared, which the gateway uses to pick a backend |
| sensitivity label | The label the thread has now; a new token is issued whenever it changes |
| `acting_for` | The person the run acts for, through delegation or a gate decision |
| `acr` | How strongly that person authenticated, which high-tier gates check |

When the gateway refuses a provider that is not cleared for the label, the
runtime gets a reason it can act on and restarts the step on a cleared model,
the same as on the local path. The gateway's token buckets, cost catalog and
billing attribution use the same claims, so a budget written in the manifest
and a limit applied on the wire come from a single declaration. Guardrails in
the gateway, such as PII patterns and moderation of requests and responses, are
a backstop behind labelling at the source; atomic reads their decisions back
over OpenTelemetry and records them in the audit log.

## Quotas and cost attribution

All workers share token buckets kept per provider, per model and per project;
in the team profile those buckets live in Postgres. An atom that finds its
bucket empty waits rather than failing. Each step records its tokens and cost,
which roll up to the workflow, the project and the team, alongside the budgets
from M12 (see [budgets](../reference/budgets.md)).

## Prompt caching

The parts of a prompt that rarely change, such as component layouts, shared
instructions and schemas, come first and carry Codex's prompt-cache keys, so an
atom that runs again can reuse the provider's cache.

## Model authentication

For local use with Codex atoms, signing in with an eligible subscription will be
a supported path next to API keys. Servers and unattended deployments use API
keys, provider accounts or workload identity unless configured otherwise. A
deployment that runs on account sign-in has to be switched on deliberately and
needs eligible access, its own scoped identity, supported storage and refresh of
the credential, and acceptance tests that cover it. A person's subscription
never becomes a shared service credential by accident, and model paths other
than Codex atoms, such as lite atoms, need their authentication verified
separately. See [subscriptions and
authentication](../getting-started/authentication.md).

## Decisions with evidence

A choice like "reviews run on the `fast` tier" should rest on something
measured: an eval pass rate on a specific model version, a benchmark, a run of
approvals with no revert. A project can record such a choice together with
conditions over atomic's own evidence and a date to look at it again.
`atomic stale` will then report the choice when that date has passed, when the
evidence is older than allowed or was produced on another model or prompt
version, or when a condition has stopped holding (M55). The date is passed to
the command, which keeps `atomic check` reproducible. The file format is
described under
[recorded decisions](../reference/manifest.md#recorded-decisions).
