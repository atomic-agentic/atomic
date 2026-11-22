# Schemas and APIs

atomic states every boundary as JSON Schema and treats OpenAPI documents as a
source of tools. Provider structured-output modes and Codex's tool plumbing both
pass a model only part of a schema, so atomic does not rely on them: it checks
each model answer and each tool argument against the complete contract on its
own.

## JSON Schema as the contract

A schema sits at each place where data crosses from one part to another:

- the input and output of an atom and of a workflow;
- gate decisions and the answers to question gates;
- message payloads and custom fields on work-graph items;
- the arguments and results of tools.

A Rust atom gets its schemas from its types. A declarative atom writes them in
its file or keeps them in sidecar files next to it (see
[atom files](../reference/atom-files.md#schema-locations)).

The `atomic-schema` crate keeps a registry keyed by `$id`, with every schema
versioned in the repository. Nothing is downloaded while a project loads. At
load time atomic also compares each workflow step's mapped input with the input
schema of the atom it calls. Proving that one schema is a subset of another is
undecidable in general, so this comparison is deliberately cautious: a case it
cannot settle produces a warning rather than counting as a pass.

### Two profiles per schema

Each schema exists in two forms.

- **The contract** is the schema as written, in full JSON Schema 2020-12.
- **The generation profile** is the reduced form a provider can enforce while
  the model is generating. Strict output modes commonly insist that objects close
  with `additionalProperties: false` and list every property as required, and
  they may drop or refuse keywords such as `pattern`, `format`, `minLength` and
  `minimum`. These rules vary by provider and change over time, so atomic keeps
  them in a versioned table instead of hard-coding them.

Every model answer goes through the same sequence:

1. **Lower.** Convert the contract into the provider's profile. A property that
   was optional becomes required but may be `null`; a keyword the provider cannot
   enforce moves into the description as a hint. atomic notes each spot where it
   introduced a `null` the contract does not allow.
2. **Generate** the answer under that reduced schema.
3. **Normalize.** At each noted spot, `null` stands for "left out", so the
   property is deleted. Where the contract itself allows `null`, the value stays.
4. **Validate** the result against the full contract. Only this checked value is
   checkpointed and handed to later steps.
5. **Repair once.** If validation fails, atomic sends one follow-up turn that
   lists the errors, normalizes the new answer in the same way, and fails the
   step with a schema error if it is still wrong.

Lowering problems surface at load time, before any model call, with the file and
the JSON path. One example is a `null` inside `anyOf` that could belong to more
than one branch. A property that is both optional and nullable in the contract
draws a warning instead: the model has no way to say "omitted" there, so its
`null` is kept as a real value.

### Compact schemas

Anywhere a schema is accepted, the shorter YAML notation described under
[compact schemas](../reference/atom-files.md#compact-schemas) works too, and
atomic turns it into JSON Schema 2020-12 when the project loads. In that notation
every field is required unless its name ends with `?`, and unknown properties are
refused. A `?` field may be absent but may not be `null`. Constraints such as
`pattern` need the full syntax. `atomic schemas expand` shows the full schema a
compact one becomes (M11b).

### Tool schemas

Before a tool schema reaches the model, Codex cleans it up and, when it is large,
shrinks it, sometimes by dropping detail such as local `$ref` targets. What the
model sees can therefore be looser than the real contract. atomic checks the
arguments of every tool call against the complete schema before the call runs,
normalizing them first when that schema was lowered.

Schemas cross atomic's public interfaces as plain JSON values. That keeps the API
independent of whichever Rust crate a caller uses to derive schemas.

## OpenAPI toolsets

Importing an OpenAPI document gives a project a toolset (M15).

- **One tool per operation.** The tool's input joins the path, query and header
  parameters with the request body. Its output is what its success responses
  describe: one schema where they agree, `anyOf` of the distinct ones where
  they differ. OpenAPI 3.1 already uses JSON Schema 2020-12; documents in the
  3.0 dialect, with keywords such as `nullable`, are translated first.
- **atomic sends the request.** A built-in HTTP executor performs every call. It
  takes credentials from secrets granted to that atom, only reaches base URLs the
  network policy allows, and applies rate limits and retries. Nothing the model
  supplies can swap the credential or alter the operation's URL.
- **Mutations follow the side-effect contract.** Safe methods may be retried at
  will. A mutating call is recorded in the effect ledger with an operation key,
  which doubles as the idempotency key when the API accepts one. If the document
  marks a lookup with `x-atomic-reconcile`, recovery can use it; with neither, an
  uncertain call parks the step as `outcome_unknown`. Any operation can be put
  behind a gate, through project policy or an `x-atomic-approval` extension. The
  gate decides whether the call may happen; the ledger keeps it from happening
  twice or holds it when its outcome is unclear. See
  [durability](durability.md#side-effect-contract).
- **Declared up front, with caps.** An atom or workflow names which mutating
  operations a run may ask for, how often, and on which targets. Asking for more,
  or for anything unlisted, is a validation error raised before a gate is opened.
- **Responses are external data.** Whatever an API sends back carries the
  external origin, so it can never influence gates, the shape of a workflow or
  admin tools.
- **Big APIs stay out of the prompt.** Each document becomes its own namespace,
  and its operations are offered through Codex's tool search only when the model
  looks for them.
- **Rust atoms** can use clients generated from the same document.

A general OpenAPI-to-MCP adapter could not do this job: it cannot tell which
calls are durable steps, which need an idempotency key or human approval, and
which replies must not be trusted. While a workflow is being developed, its
toolsets can point at a mock server built from the document (M39b). See
[tools and integrations](../guides/tools-and-integrations.md).

## atomic's own API

atomic will publish an OpenAPI description of itself (M16). The HTTP API reaches
the work graph, workflows and runs, gates and admin actions, and its document is
produced from the registry schemas the runtime checks against.

- Outside systems and webhooks use it to start work, and generated clients let
  other languages drive atomic.
- A workflow can be offered as an endpoint that accepts that workflow's input
  schema.
- `atomic curl <name> -d '<json>'` exercises a served atom or workflow the way
  an application would, over HTTP, and prints the typed reply; with `--print` it
  shows the same request as a plain `curl` command, token masked.
- A start request returns the run ID immediately. The result comes later, as an
  event or by reading the run, never over a single long-held request.

Models and Codex clients keep talking MCP. Either interface leads to the same
operations, under the same authorization checks.

## Protocol rules for atomic's own surfaces

Eight rules hold on every surface atomic defines. Those surfaces are its HTTP
API, its gate store, its two MCP servers (`atomic` and `atomic-admin`) and the
`ExecutionBackend` interface.

1. **State changes are idempotent commands.** The client gives each gate
   decision, and each other admin call that changes state, its own command ID.
   atomic stores the command before it acknowledges it. Sending it again returns
   the original acknowledgement; sending a different payload under a used ID is
   refused. On a gate, whichever decision is stored first wins, and any later one
   receives a typed conflict that includes the winner, so another approver's view
   updates without asking again.
2. **Admission is not completion.** The reply to a command only confirms that it
   was accepted. Results are read from durable state: the gate store, step
   outputs and the event stream. Because no result exists only in a reply,
   losing a reply costs nothing.
3. **Calls choose policy; they never write it.** Approver groups, risk tiers,
   sandbox and network rules and trust levels are defined in versioned files and
   managed requirements. An API, MCP or CLI call can pick among those
   definitions but cannot create one or loosen one.
4. **Input to a busy atom names its intent.** It asks to `queue` behind the
   current turn (the default), `steer` that turn or `replace` it, and the reply
   says which of the three was done.
5. **Runner exit codes mean something fixed.** `atomic runner` ends with one of a
   few codes, grouped by what the control plane should do next: normal shutdown;
   crash, so rerun the step; bad usage, so do not retry; broken configuration or
   credentials, so wait for a fix; and the workspace lease belonging to someone
   else. Any other non-zero code counts as a crash. Standard error is kept with
   the failure for people to read and is never parsed.
6. **Backends declare durability when they connect.** Each execution backend says
   whether a workspace outlives the runner process. The control plane schedules
   from that answer, not from what the backend is called, and treats an answer it
   does not recognize as ephemeral.
7. **Surfaces publish a schema fingerprint.** The HTTP API and both MCP servers
   report one. A client compiled against another fingerprint is warned but keeps
   working, since these surfaces only grow: clients skip fields they do not know,
   and enumerations may gain values.
8. **Conformance is proven by replay.** The test suites for execution backends
   and storage traits ship transcripts captured from the reference
   implementations with fixed IDs and clocks. Another implementation passes by
   replaying them.

Rule 1 starts with the v0.1 gate store (M45). The admin-call rules come with the
CLI (M9), exit codes and recorded transcripts with the second backend (M24), and
the fingerprint with the HTTP API (M16).
