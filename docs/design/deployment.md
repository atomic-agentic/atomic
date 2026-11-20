# Deployment and interop

atomic keeps its state in one of two storage profiles. It can run an atom's
workspace on another machine, spread runs across several workers, and both
offer and use MCP and A2A. Whichever way a request comes in (MCP, A2A or HTTP),
it starts or reads the same durable runs, and anything that enters from outside
carries the external origin. Day-to-day operation is covered in
[Run locally or with a team](../operations/running-atomic.md).

## Storage profiles

| | Local (default) | Team or server |
| --- | --- | --- |
| Durable engine | A single SQLite file | Postgres |
| Work graph, messages, artifact index | Kept in the engine's database; replicas exchange changes over a git ref | Postgres, where a claim is one transaction that all workers observe; the git ref remains available for offline copies or for sharing with another organization |
| Wake-ups | Events inside the process | Postgres `LISTEN/NOTIFY` |
| Codex state database | One SQLite file per host, managed by Codex | The same; threads are referred to by ID |
| How it is deployed | A single binary on a laptop or a CI machine | `atomic serve` on one host or several |

- **Two implementations behind each store trait.** Workflow state, the graph and
  messages each sit behind a storage trait implemented for both SQLite and
  Postgres. CI runs one shared conformance suite against the pair (M18). If the
  suite cannot reach its database it fails rather than skipping.
- **The Postgres driver comes from Codex's sqlx source.** Codex replaces
  `sqlx-core` and `sqlx-sqlite` with crates from the transact-rs/sqlx fork, and a
  stock `sqlx-postgres` does not build against those. atomic therefore takes
  `sqlx-postgres` from that same fork, at the same revision, through the
  workspace's patch block (see [deps/forks.toml](../../deps/forks.toml)).
- **The engine's schema is left alone.** On Postgres the durable engine keeps the
  DBOS-compatible schema, so DBOS tools can inspect atomic's workflows.
- **Model work gets a weaker database role.** Under Postgres, the role used for
  model-driven work holds no grant for deciding gates or for raising the trust
  level of stored text, so not even a hand-written query from that side can do
  either.

## Running remotely

- **People elsewhere.** `atomic serve` offers the embedded app-server over a
  WebSocket protected by a bearer token. The terminal client connects with
  `--remote`; Codex's desktop and mobile clients connect through remote control
  (see [people in the loop](human-in-the-loop.md)).
- **Workspaces elsewhere (M20).** Codex's exec-server and its environment
  registry let an atom's sandbox, shell and worktree live on a different machine
  while orchestration stays where it is. An atom will choose where with an
  `environment` field set to `local`, the name of an SSH host, or a container
  pool. That lets a machine with GPUs, or a tightly controlled build host, carry
  out the commands without hosting atomic, while the thread and checkpoints stay
  with the control plane.
- **More than one worker (M19).** With Postgres, several `atomic serve` processes
  draw from shared durable queues. Concurrency can be capped for each worker and
  overall, and the queues support rate limits, priorities, deduplication and
  partitioning. Each worker keeps a stable executor identity, so recovering the
  interrupted runs of one worker leaves the others' runs alone. When a worker
  has lost its lease yet is still alive, ownership epochs keep it from doing
  harm; see [run ownership](execution-backends.md#run-ownership-and-scaling).

Upgrades raise the same identity question. The durable engine only resumes runs
whose recorded application version matches, so atomic will set that version on
its own terms and decide whether a run pinned to older definitions continues or
is refused with an explanation (see
[operating atomic](operating.md#upgrades-and-format-stability)).

## MCP in both directions

- **Using MCP servers.** Since Codex can already act as an MCP client, an atom
  just names the servers it is allowed, and Codex's MCP OAuth support signs in.
  When project policy says a tool only reads, or is safe to repeat, atomic may
  retry it at will. What a server says about its own tools is treated as a hint
  only, and all remaining calls are recorded in the effect ledger.
- **Offering atoms and workflows (M21).** Apart from `atomic` and
  `atomic-admin`, a project can pick atoms and workflows to expose as MCP tools.
  Each such tool uses the atom's own schemas for input and output and returns its
  answer as structured content. A long workflow's tool answers straight away
  with a run ID, reports progress while it runs, and makes the final result
  available as a resource. Any MCP client can then drive a durable workflow and
  get typed output back.
- **Transports.** Standard input and output on one machine; streamable HTTP with
  OAuth across the network.
- **A single MCP implementation.** atomic builds against the MCP crate revision
  Codex pins, so both use the same types.

No atom or workflow is exposed unless a project says so. Each entry point checks
its own credentials, and calls that arrive through one carry the external origin
and are subject to budgets and gates.

## A2A in both directions

A2A publishes its data model (agent cards, skills, tasks, messages, parts and
artifacts) as JSON Schema 2020-12, the same language atomic uses for contracts,
so the two line up directly (M22).

| A2A concept | When atomic serves | When atomic calls out |
| --- | --- | --- |
| Agent card and skills | Workflows and atoms the project chose, listed under `/.well-known/agent-card.json` | Each remote agent's card is fetched once and pinned, and its signature verified when it has one |
| Task | One durable run, whose ID is the task ID, so a remote caller's task outlives a crash | A `kind: a2a` atom; the send is a durable step in the effect ledger that keeps the remote task ID |
| Reading or subscribing to a task | The run's status and event stream | Following a crash, atomic re-attaches to the task it saved, without sending a second message |
| Cancelling | The run is cancelled | atomic cancels the remote task if its own workflow is cancelled |
| A task that wants input | A gate or an atom question awaiting a person | Appears as a gate in atomic's workflow |
| Artifacts | What the workflow returns, typed | Validated with the atom's output schema, then marked as external |

The message ID atomic sends is its operation key. If a send's outcome is
uncertain, sending again is safe only when the remote side deduplicates or the
old dispatch is provably fenced; otherwise the step parks. An A2A atom names the
remote agent card and skill, its schemas and a credential reference:

```yaml
kind: a2a
remote: { card: "https://agents.example.com/.well-known/agent-card.json", skill: translate }
auth: { secret: vendor-a2a-token }
budget: { minutes: 30 }
```

The [A2A translation example](../../examples/07-a2a-translation/README.md) shows
a complete project. The Rust A2A libraries are still young, so atomic will begin
with the official types, test against the official A2A command-line client, and
keep its own transport layer small.

## Java and Spring integration

Spring applications and atomic can call each other, mostly over protocols atomic
supports anyway. A run can last minutes or days, so starting one always answers
immediately with a run ID and later reports progress as events; no call waits
for a run to finish.

**From Spring into atomic.**

| Option | Mechanism | Suits |
| --- | --- | --- |
| MCP | atomic offers workflows as MCP tools (M21), and a Spring AI chat client using the MCP client starter calls them as it would any tool | An in-app assistant that hands long jobs over |
| A2A | atomic acts as an A2A server (M22), reached through the Java SDK for A2A | Passing a job that takes hours from one agent to another |
| REST | A Java client built from atomic's OpenAPI document can start runs, receive events over webhooks or server-sent events, and decide gates | Deliberate integration where the application keeps full control |

**From atomic into Spring.**

| Option | Mechanism | Spring-side work |
| --- | --- | --- |
| OpenAPI toolset | A toolset (M15) is pointed at the OpenAPI document springdoc produces, turning each endpoint into a typed tool | Nothing |
| Spring AI MCP server | Bean methods marked `@Tool`, served by Spring AI's MCP server starter, which atoms use as MCP tools | Annotations only |
| A2A atom | A JVM agent reachable over A2A takes part as a `kind: a2a` step | An A2A endpoint |

The second direction matters for code work: an atom fixing a defect in a billing
service can call that service's own domain logic rather than reconstructing the
business rules from source.

A small Spring Boot starter built on the REST API would make a good later
project. It would offer a client with `startRun`, `awaitRun` and `approveGate`,
sending an idempotency key with each start; Java records generated from atom
schemas; listener annotations that put gates in the application's own screens,
decided under the signed-in Spring Security user mapped to atomic's OIDC
identity; W3C trace propagation from the incoming HTTP request through to the
merged change; an outbox, so a rolled-back transaction never leaves a run
behind; and, for local development, an optional sidecar that launches
`atomic serve`.

Two things are deliberately left out. Loading atomic into the JVM through native
bindings would be fragile across platforms and would put a second runtime and
SQLite inside the JVM. Atoms written in Java add nothing, since A2A and MCP
already let Java code take part. Spring AI is a library for calling models, not
a server that hosts them, so atomic does not treat it as a model provider.

The public example (M50) is a Spring Boot application in which one request
launches an atomic workflow, one of its atoms invokes a Spring `@Tool`, and the
whole round trip appears as a single trace.
