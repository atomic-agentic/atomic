# Durability

atomic's workflows survive a crash without redoing finished work. This page sets
out how: how an atom's result becomes durable, the rules every step follows,
what happens to effects outside atomic's database, and the ledgers that keep
recovery from guessing. The user-facing explanation is
[durable execution](../concepts/durable-execution.md).

Durability comes from a fork of durare, reached only through the
`DurableRuntime` trait in `atomic-durable`. Workflow code calls durable steps;
each completed step's result is checkpointed, and on restart the workflow
re-executes from the top while completed steps return their recorded results.

## Candidates and publication

An atom invocation is not one step. It is lowered to sibling durable operations:

1. **Execute a candidate.** The model turn runs outside any database
   transaction. Its result is checkpointed as private candidate state: the
   output and artifact hashes, the proposed graph mutations and their digest,
   the claim token, the attempt identity and references into the usage ledger.
   A candidate is not an accepted result.
2. **Review it,** when a review policy applies. Each reviewer and each repair
   round is its own step, and each replacement candidate has its own identity.
   Without inline review, passing the schema and validators selects the
   candidate.
3. **Publish it.** A brief transaction first confirms that the run and claim
   epochs are current, that acceptance was recorded and that the digest still
   matches; then, in the same commit, it writes every graph change in the batch,
   stores the result others may now read, and marks its own checkpoint done.
   Retrying publication is harmless: the key built from run, invocation and
   candidate is unique, so a second attempt just returns what the first one
   stored.

The consequences:

- A crash before the candidate is saved may re-run the turn and spend tokens
  again. A crash after it needs no new author call.
- Publication applies the whole batch or none of it. A stale or conflicting
  batch publishes nothing and parks for revalidation.
- A rejected candidate stays private, with its findings, for inspection. Its
  proposed closes, follow-ups and messages never reach the graph.
- No graph change is published early to show progress; transient progress is
  telemetry.

Claims, budget reservations, effect intents and human command journals are not
part of the candidate. They are separate durable control records, committed in
their own short transactions.

## Rules every step follows

- **Steps run at least once.** Anything outside a step's own checkpoint must be
  idempotent, deduplicated by a stable key, reconcilable, or parked as
  `outcome_unknown` (see the [side-effect contract](#side-effect-contract)).
- **No transaction stays open across a model turn.** A turn checkpoints a
  private candidate; a short transaction publishes it.
- **Steps never nest.** The engine keys each step by a deterministic position
  in the workflow and rejects a step started inside another step. An atom's
  turn, its reviewers and each repair round are siblings composed in workflow
  code, and anything a step needs from the database is its checkpointed input
  or an earlier step's output, never a fresh read on replay.
- **One worktree per code-changing step.** A retry on a half-edited tree would
  not be idempotent, so each such step resets its own worktree to a base SHA,
  runs the atom and returns a commit SHA. Later steps build on SHAs. Build and
  package caches live outside worktrees. Retries may leave orphan git objects;
  what must hold is a single accepted candidate for each finished invocation,
  and attempts nothing refers to are removed by retention cleanup.
- **atomic commits, and it does so outside the sandbox.** The write sandbox
  leaves `.git` read-only, so atoms are instructed not to commit; once the turn
  is over, the step records the commit itself. Anything in a worktree an atom has
  touched counts as untrusted, so git runs there with an emptied environment and
  with hooks, external diff and textconv drivers, signing and pagers all
  disabled (M4).
- **Data between atoms goes through step outputs and the work graph,** never
  through Codex's in-memory messaging, which loses undelivered mail in a crash.
- **Checkpoints stay small.** Large results go to the
  [artifact store](#artifact-store); the step output holds a reference.

## Gates and recovery

A durable gate sits between atoms. Codex's in-turn approval waits in memory and
dies with the process, so it is kept for sandbox escalations only; human
sign-off is a gate.

Each gate pairs a row in atomic's gate store with a durable `recv` that is
completed by the decision. The row holds the workflow and step, a title, what to
display, hashes of the proposal and evidence that never change, the approvers,
a risk tier, a deadline, and finally the decision and its author. Because none
of this depends on the work graph, durable gates are part of v0.1 (M45). Once
the graph exists in v0.5, every open gate also appears there as a `gate` item,
which lets ready and blocking queries see it, while the gate store stays
authoritative.

Decisions arrive as idempotent commands carrying a human credential. Sending the
same decision again has no effect, and a conflicting later decision is answered
with a typed conflict that includes the decision already recorded. Who may
approve is never taken from a model's answer, from a field a caller sends or
from anything written to the graph. What a gate shows is stored by content hash
and kept as long as the gate's records are; if the candidate or that evidence
changes, its digest changes and a fresh gate has to be opened.

Recovery is explicit in v0.1. `atomic run` creates a run and prints its ID;
`atomic runs resume <run-id>` continues that unfinished run in the foreground
against its pinned definitions and saved inputs, preserving completed
checkpoints, budget usage, effect records, pending gates and their original
wall-clock deadlines. Local `atomic serve` recovers eligible runs headlessly. A
project lock keeps two runtimes from executing against one store, and `resume`
reports the owner instead of starting a second executor. Gate timeouts keep
running while nothing is running and are applied on recovery before downstream
work. Resume never bypasses a rejected or expired gate, revives a terminal
failure, resets a budget or resolves `outcome_unknown`; each needs its own
authorized action.

## Side-effect contract

Steps run at least once. The only writes that happen exactly once are those
committed inside the step's own checkpoint transaction. A gate gives permission
for an effect; it does nothing to prevent the effect repeating.

| Effect | Guarantee | How |
| --- | --- | --- |
| File changes in a worktree | Idempotent | Every attempt starts from the base SHA and ends with a commit |
| Pushing a branch | Idempotent | A deterministic branch per attempt; orphaned branches are swept |
| Proposed graph and message writes | One accepted publication | The candidate batch is released by the publication transaction |
| Claims, gate decisions, control records | Idempotent commands | Short transactions keyed by command identity |
| Safe HTTP methods | At least once | Retried freely |
| Mutating HTTP calls | Once, or `outcome_unknown` | The effect ledger |
| MCP tool calls | Once, or `outcome_unknown` | A tool retries freely only when project policy calls it read-only or idempotent, since annotations from the server are treated as hints. Every other tool goes through the ledger |
| A2A sends | Once, or `outcome_unknown` | The operation key is the message ID; recovery re-subscribes to the task |
| Writes to data sources | Once, or `outcome_unknown` | Preconditions or an applied-changes marker, through the ledger |
| Script and shell network calls | At least once | atomic cannot observe them, so the atom declares `effects.external`: `read-only` code retries freely, `idempotent` code retries after a crash and runs only as a real step, and anything else parks after a crashed attempt |
| Model tokens | At least once per attempt | Every attempt is charged to the budget |

## The effect ledger

Every external call that changes something is written to a ledger kept in
atomic's database. After a crash, recovery reads the ledger rather than
guessing what was sent.

- **Logical identity.** Before dispatch, atomic durably allocates an identity
  of project, run, step occurrence and operation slot, independent of payload
  and attempt. Two intentional identical mutations use different slots; retries
  reuse the same one. The first admission binds the request hash, target and
  authorization; changed arguments under the same identity are refused. The
  provider key is an encoding of that identity within provider limits, and the
  exact key and the provider's retention deadline are recorded.
- **Intent, then outcome.** The intent row commits in its own transaction
  before sending, because it must survive the crash it guards against. The
  outcome commits when the response arrives. A later attempt of the same
  operation gets the recorded outcome back and sends nothing.
- **An intent without an outcome is `outcome_unknown`.** Recovery resends with
  the same idempotency key where the API takes one, within its retention window.
  Without a key, a declared reconciliation read can record a found outcome, but
  a negative read permits a resend only with authoritative proof that every
  earlier dispatcher can no longer transmit and earlier requests have settled.
  Without that proof, the step parks; a person records the outcome or
  authorizes a resend, and every choice is audited.
- **An approval covers one operation** with one request hash. If a retried turn
  comes up with a different request, that request must be approved again.
- **The first release uses deterministic effect steps:** an atom returns one
  checkpointed proposal per operation, and after policy and gates a separate
  step sends exactly that request. Free-form mutating calls inside retried model
  turns stay disabled until their own design passes acceptance.
- **Effects are declared and capped** (M15): which operations a run may
  request, how many and against which targets, with content rules such as
  allowed link domains. An undeclared or over-cap request fails validation
  before any gate opens. `--effects preview` (M39b) records every effect a run
  would apply and applies none.

## One database for workflows and the graph

durare, the work graph and messages share a database: a single SQLite file on a
laptop and a single Postgres database for a team. Sharing it lets the publication
transaction apply the graph batch and complete its checkpoint in one commit.
It does not enlist other writes into an ordinary execution checkpoint, and no
transaction spans a turn or a review. Codex's state database stays separate.

## Artifact store

Bulky results such as patches, reports, test logs, generated files and API
responses are kept in a store addressed by content. Step outputs, graph items
and messages carry a reference to them instead of the bytes.

- **Addressing:** `artifact://sha256/<digest>`, plus a short manifest giving the
  media type, the size, the workflow and step that produced it, and its origin
  and sensitivity labels. The same bytes are only ever stored once.
- **Typed references:** a schema can require an artifact of a media type, and
  validation checks that the reference exists and matches.
- **Access:** atoms read only what their workflow produced or was given;
  runners never hold storage credentials.
- **Backends and lifecycle:** a local directory in v0.1 (M35a); object storage
  backends, retention and garbage collection in v1.0 (M35b); customer-managed
  keys, deletion and legal hold with the security milestones.

## The model-usage ledger

Budget accounting is a durable ledger of its own, independent of candidate
outputs and telemetry (M12).

- **Reserve before dispatch.** A short transaction assigns a stable request and
  attempt ID and reserves the request's bounded maximum usage against every
  ancestor workflow and atom allowance under one lock, so parallel children
  cannot both take the last of an allowance.
- **Settle once** by request ID when trusted usage arrives. The attempt record
  stays even when the turn fails or its candidate is rejected.
- **Uncertain usage stays charged.** A crash after the provider consumed tokens
  keeps the reservation charged at its full bound, or, for a Codex turn that
  recorded its progress, at what it last recorded having reported and one
  request in flight, if that is more; only a person entering the real usage
  can later correct it. A fresh retry gets a new reservation; if the
  allowance is gone, the run parks. Allowance goes back into the pool in two
  cases only: the request provably never left, or reconciliation showed it
  unused.
- **Monetary caps need a bound.** A `cost_usd` cap requires reliable pricing and
  a documented maximum per request; for a provider without one, atomic rejects
  the cap rather than claim bounded billing.

Telemetry and result metadata are projections of this ledger, never inputs to
it. Fields and defaults are in the [budget reference](../reference/budgets.md).
