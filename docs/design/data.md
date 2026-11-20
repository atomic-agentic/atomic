# Data sources, retrieval and media

Many atoms need business records, a body of documents to search, or a picture
to look at. This page describes how atomic will query application databases
without taking them over, how it will ingest and search documents, and how
images and audio will become typed inputs. All three are v0.7 work (M51, M53,
M54); [availability](../reference/availability.md) has the current state.

## Databases as data sources

atomic separates the databases it owns from the ones it only reads.

- **atomic's state** (durable workflows, the work graph, the audit log) is kept
  in a database or schema that belongs to atomic. It can live on the same
  Postgres server as an application, but always in its own database or schema,
  under its own role and with its own migrations, and atomic finds it only
  through `ATOMIC_DATABASE_URL`.
- **An application's database**, schema and all, is a *data source*. Atoms reach
  it through tools. atomic does not migrate it or create tables in it, apart from
  one opt-in case described below.

See [Keep application and runtime databases
separate](../reference/configuration.md#keep-application-and-runtime-databases-separate).

The manifest declares each data source, and an atom requests only the access it
needs:

```yaml
datasources:
  orders:
    url: env:ORDERS_DB_URL        # resolved from the secret store, never shown to a model
    access: read                  # read | propose | write
    schemas: [public]
    include: [customers, orders, order_items, v_revenue_daily]
    limits: { statement_timeout: 5s, max_rows: 500 }
```

```yaml
datasources: { orders: read }     # in an atom's configuration
```

### How atoms use a data source

- **Tables become types.** atomic reads the database catalog and writes JSON
  Schemas into the registry, so an atom can declare a table or view as its type
  (`db:orders/customers`). Comments on columns turn into field descriptions that
  the model reads, which rewards a well-documented schema. `atomic check` diffs
  the saved snapshot against the live database and reports drift, for example a
  column that was renamed while a query still refers to it.
- **Named queries come first.** A parameterized file such as
  `queries/find-customer.sql` is offered to the model as a typed tool with typed
  rows, and it is prepared against the live schema when the project loads. The
  model never composes SQL of its own, and picking a tool is something small
  models do far more reliably than writing joins.
- **Read-only SQL for analysis.** An analysis atom may be given a `sql_query`
  tool. Each query runs in a read-only transaction under a role that can only
  select, with a statement timeout, a cap on rows and a ceiling on the planner's
  cost estimate. Big result sets are written to the artifact store.
- **Writes arrive as proposals.** To change business data, the atom returns a
  typed change set; a gate reviews it; a deterministic step then applies it in a
  single transaction. The application database and atomic's checkpoint are
  separate, so that apply step is recorded in the effect ledger, and one of two
  safeguards makes a repeat harmless. Either each row change states the values it
  expects to find, so applying it twice changes nothing the second time, or the
  data source opts in to a small table of applied change sets updated in the
  same transaction. When neither can tell whether a set went through, the step
  parks as `outcome_unknown`.
- **Declared write queries** suit trusted writes at volume. Each is a named query
  that enforces an idempotency key itself (a unique key with
  `ON CONFLICT DO NOTHING`, say), is treated as an external effect, and sits
  behind a higher-tier gate.
- **Query results are saved with their step,** so a replay reads the checkpoint
  instead of the database.

### Trust and security

Rows from an application are external input; a support ticket's text, for
instance, may contain injected instructions. Such content is marked untrusted
and taint follows it. A column marked sensitive, through a comment or the data
source's settings, is kept out of prompts and traces, and that label feeds
[sensitivity routing](model-layer.md#sensitivity-routing). Every data source
gets a database role of its own and every atom a grant of its own. Connection
credentials remain with the control plane and are never visible in a runner's
shell, and each query is written to the audit log.

### Schema migrations as a workflow

Changing a database schema becomes a workflow. A coding atom reads the schema
snapshot and drafts a migration. The migration first runs against a disposable
copy (a container, or a branch of the database where the provider supports
branching) and the tests run there. It then needs an AI-reviewed gate and a
high-tier human gate, and only after both does a deterministic step run it
against the real database.

General-purpose database MCP servers can still be used through atomic's MCP
support, but they bring none of the typed results, schema checks, per-atom
grants or proposals described here.

## Retrieval

Searching documents has two parts. Building and refreshing an index is a long
batch job, exactly the kind of work atomic makes durable. Answering from the
index needs only a few primitives. atomic provides one built-in index for each
storage profile and can use other stores; it does not try to be a vector
database.

### Ingestion

An ingestion pipeline is a normal workflow. A `foreach` walks the documents; an
exec atom parses and splits each into chunks; an embed step computes vectors;
an upsert step stores them. If the process dies partway through a large corpus,
the run picks up at the next unfinished document, and triggers such as a
schedule, a webhook or a push keep the index current.

- **Rerunning is safe.** A chunk is identified by a hash over its document's ID,
  its text and the version of the embedding model, so processing it again
  replaces the stored chunk instead of adding a copy. A document whose hash has
  not changed is not embedded again.
- **Embedding is part of the model layer.** An `embed` capability will cover
  hosted providers, local servers and an in-process ONNX model for machines with
  no network. It uses the same tiers, quotas, batching and cost accounting as
  chat calls, and it knows each server's input limit, trimming and retrying an
  input that exceeds it.
- **A built-in index per profile.** Locally that is sqlite-vec; in the team
  profile it is pgvector. Each comes with keyword search (SQLite FTS5, Postgres
  full-text search), which makes hybrid search cheap to add. Vectors are stored
  either in a schema that atomic owns or in a data source the project names,
  never in an application's tables. Dedicated vector stores can be attached as
  data sources.
- **Indexes have versions.** Switching to a different embedding model builds a
  second index next to the first. Once it has been evaluated, it replaces the old
  one in a single swap, and runs that were already under way keep using the
  version they began with.

### Two ways to retrieve

- **A `retrieve` step** performs a fixed top-k search and places the hits in the
  user turn, each labelled with a short citation handle for that run (`S1`,
  `S2`). Small models do well with this because they never have to steer a
  search.
- **Search tools** suit stronger models facing open questions: search, widen the
  context around a hit, or step up to the enclosing section. Reranking can be
  done by a lite atom or by a decision atom that scores relevance.

### Requirements for shared deployments

- **Retrieval respects permissions.** Every chunk keeps the access list of the
  document it came from, and queries filter on the identity of the run or the
  user, so nobody is shown a chunk they could not open at the source.
- **Retrieved text is external input.** It is marked untrusted, because planting
  instructions in documents is a well-known attack.
- **Erasure covers the index.** When a person's data must be deleted, the chunks
  and vectors made from it are deleted too.
- **Citations are verified.** An output schema can demand chunk handles as
  citations. A validator turns away any handle that was not part of the
  retrieved set, and a built-in step compares each quoted passage with its
  source; on a mismatch the repair turn shows the quote next to the passage it
  claims to come from.

Retrieval quality will be measured with document-level recall at k over a golden
set of questions, reported next to judged groundedness, using atomic's eval and
cassette tools (M12, M39). For code, the repository map and plain search (M48)
are usually better than embeddings, so embedding source code stays optional and
is mostly useful in very large repositories.

## Multimodal

Codex can already take images and audio as part of a turn (sent inline, by file
ID or by a local path, with a detail setting for images). It also gives the
model a tool for looking at an image in its workspace, can create and edit
images, and supports realtime voice. There is no PDF input type: a PDF is
handled through the shell, either as extracted text or as page images. All of
this works with hosted models; with local models it depends on the model, and
few local models accept audio.

atomic will add:

- **Typed media.** Images and audio become ordinary `atomic:file` values with
  `image/*` or `audio/*` media types. An image in an atom's input is sent to the
  model as an image, not described in text, with the detail level taken from the
  atom's configuration, and a template construct decides where in the turn it
  goes. Images the model creates are collected from `/outputs` as typed file
  references.
- **Screenshots as evidence for UI work.** A coding atom can drive a headless
  browser inside its sandbox, capture screenshots and inspect them. At the gate a
  reviewer compares the screenshots with the intended design and attaches them to
  its findings.
- **Extraction by vision.** For scanned pages, forms and charts, an exec atom
  turns each page into an image, a lite atom on a vision-capable model reads a
  typed record out of it, and the schema and validators check the record.
- **Capabilities in routing.** The model layer will record which models handle
  vision, audio or image generation. An atom states what it requires, and
  `atomic check` refuses an atom that needs vision on a tier whose models lack
  it. Routing can then send extraction to a local vision model and the judgment
  step to a text model.
- **Safety and privacy.** Text drawn inside an image can carry injected
  instructions, so images count as external input. Location and device metadata
  is removed before any provider receives an image, size and format limits apply,
  and traces keep the artifact hash instead of the image itself.
- **Cost and replay.** Image tokens count toward budgets, and the detail level is
  one way to spend less. Media is stored in the artifact store under its content
  hash, and cassettes refer to it the same way, so replays are exact.
- **Audio comes later.** A lite atom can already turn a recording's transcript
  into a typed record. Controlling atomic by voice is deferred until after v1.0,
  because approving a gate by voice raises questions about who is speaking.
