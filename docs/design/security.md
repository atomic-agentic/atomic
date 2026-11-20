# Security, compliance and enterprise controls

atomic's core design already follows the shape most organizations use to govern
risk: a durable record of what happened, human sign-off at chosen points, least
privilege declared in files, and secrets and policy kept away from untrusted
code. The work that remains for larger deployments is identity, a tamper-evident
audit log, key management, data lifecycle and supply chain. Each is a planned
milestone (M14 and M27 to M32), described below. The user-facing summary is in
[security and permissions](../guides/security-and-permissions.md).

## What the design provides

- **Separated trust.** A trusted control plane holds workflows, the graph, gates
  and credentials; sandboxed runners hold an atom run's workspace and shell. On
  [Agent Substrate](execution-backends.md) each runner is a gVisor or microVM
  actor that has an identity of its own, can reach only what its network policy
  allows, and holds no credentials.
- **Least privilege by declaration.** Whatever an atom may touch (tools, hosts,
  trust level, sandbox, effects) is written in its own file, and loading checks
  it. The resolved result is written to a committed policy file, so widening
  access is a visible diff.
- **Human oversight.** Durable gates sit between steps, and human-only admin
  actions require a credential the sandbox cannot read.
- **Injection defence.** Origin and trust labels with taint tracking (see
  [Trust boundaries](#trust-boundaries)); admin tools are never model-callable.
- **Separate egress paths.** Imported OpenAPI calls go through atomic's HTTP
  executor rather than model-written requests. Model traffic, hosted web search,
  MCP connections and notification adapters each have their own destination,
  credential and data policy; restricting the shell's network does not restrict
  hosted search. The managed Deno (v0.7) is designed to download from just two
  places: its own release archive, which must match a hash built into the
  atomic binary, and the package registry during the setup step, whose packages
  must match the lockfile.
- **A reproducible record.** Every step checkpoint records the content hash of
  the atom, workflow and code that ran.
- **Change control.** Definitions and code are pinned by hash; code-carrying
  atoms load only from trusted origins; declarative files never execute.

## What Codex provides

atomic builds on controls that already exist in the Codex crates rather than
inventing parallel ones:

| Control | What it gives atomic |
| --- | --- |
| Managed requirements (`requirements.toml`) | Administrator ceilings on login methods, model providers and catalog, approval policies, sandbox modes, permission profiles, MCP and plugin allowlists, exec-policy and network rules, telemetry destinations, residency, and update and feedback switches. atomic extends this file instead of defining its own policy format |
| Enterprise model access | Configurable providers, including cloud-hosted model accounts |
| Workload identity | Exchange of a workload assertion, including a SPIFFE identity, for a short-lived model token |
| Process hardening | Core dumps and ptrace disabled, `LD_PRELOAD` stripped, a network proxy with allow and deny rules, credentials in the OS keyring |
| TLS | rustls on the `aws-lc-rs` provider, which has a FIPS-validated build; a FIPS build of atomic is an investigation, not a promise |

## Trust boundaries

Untrusted text reaches atoms through issue bodies, web pages, pull-request
comments, API responses, database rows, files and retrieved documents. M14 gives
every such value a label and keeps labelled content from steering anything that
matters.

- **Origin and trust.** Every graph item and inserted value carries its origin:
  `team`, `agent` or `external`. An atom declares the highest-risk origin it
  accepts with `trust`.
- **Who wrote it.** Stored text records whether a person wrote it (`human`), a
  person approved it (`approved`), an agent wrote it (`agent`) or it arrived from
  outside (`external`). Only `human` and `approved` text is put into a prompt
  automatically. Agent-written and imported text is fetched on request as data,
  an agent cannot rewrite human text, and nothing an agent or importer writes can
  raise its own level.
- **Taint per field.** An atom that reads external content has the affected
  output fields tagged. Taint is tracked per field rather than per atom, so one
  quoted issue body does not poison an unrelated verdict.
- **Restrictions on tainted work.** A tainted atom runs without network, cannot
  create gates, change workflow structure or reach admin tools.
- **Declassification is a human act.** A person can declassify content at a gate;
  no model or tool call can.

The acceptance test runs an injection corpus, such as an issue body telling
agents to approve every gate, and expects no admin action, no gate and no
network access.

## Identity and access

atomic consumes identity; it does not implement an identity provider.

- **OIDC on every endpoint** of `atomic serve`, the console and the APIs (M27a).
  Any OIDC provider is accepted. Keycloak is the reference issuer: one realm can
  be the trust root for the server, the console, the model and tool gateway
  (agentgateway) and Substrate actor audiences.
- **Groups from claims.** Gate approvers resolve from group claims in the token.
  Where a listing is needed, such as an approver picker, atomic reads the
  issuer's admin or SCIM API under a service account. atomic never serves a SCIM
  endpoint; provisioning goes into the issuer (M27b).
- **Separation of duties.** An atom's author cannot approve their own change, and
  a user outside a gate's approver group cannot decide it.
- **Service principals and delegation.** Each service principal is an issuer
  client with a service account. When an atom acts for a person, the control
  plane exchanges its token for a delegated token scoped to that agent and that
  person (token exchange), under the issuer's fine-grained permissions (M27b).
- **Step-up for high-tier gates.** A gate tier can require a fresh
  authentication context (ACR), such as a passkey. A decision whose token lacks
  a fresh ACR is refused in server mode; the ACR is recorded on the decision and
  in the audit entry (M45).
- **Terminal sign-in.** `atomic login` uses the device authorization grant and
  stores tokens in the OS keyring already used for the local gate credential
  (M27b).
- **Authorization model.** Project roles and per-gate approver rules sit on a
  relationship-based authorization model; model allowlists come from managed
  requirements.
- **Policy is selected, never created.** Calls over the API, MCP or the CLI can
  only choose among policies that files define, never write or widen one; see
  [protocol rules](schemas-and-apis.md#protocol-rules-for-atomics-own-surfaces).

## Audit log

M28 adds a dedicated, append-only audit stream, separate from step outputs:
gate decisions, tool calls, egress requests, admin actions, logins and routing
decisions. Records are hash-chained, so editing or removing one breaks
verification. The stream exports over OpenTelemetry logs to a SIEM, has its own
retention, and backs an evidence export per workflow run and a published
mapping from atomic's controls to common AI-governance and security frameworks.

## Encryption, secrets and tenancy

- **Transport and secrets (M29a, v0.5).** TLS on every listener and to Postgres;
  a pluggable secrets backend. A secret's value is never written to a database
  row, checkpoint, artifact or trace, and a rotated secret is picked up without a
  restart.
- **Keys at rest (M29b, v1.0).** mTLS between control plane and runners (SPIFFE
  identities on Substrate); customer-managed keys for Postgres and object
  storage; envelope encryption of transcripts, prompts, artifacts and messages
  through a cloud KMS or Vault; workload identity federation instead of stored
  keys.
- **Tenancy.** A tenant is an organization in the issuer. One realm holds one
  organization per tenant, each with its own identity providers, domain routing,
  invitations and organization admins. The tenant claim on every token is the
  organization, and every run, gate decision and effect is bound to it. Storage
  isolates tenants by namespace (mapping to Substrate's tenant-scoped
  namespaces), row-level security or a database per tenant, with per-tenant
  keys.
- **Connected accounts.** A person links an external account, such as Google,
  Microsoft or GitHub, once through the issuer's identity brokering with stored
  tokens. The control plane holds the broker role that may read those tokens,
  and turns the person's external token into a named credential with a short
  life that only the atom's own code receives. It never reaches the model's
  context or a Codex shell. Keeping those tokens fresh is the job of a refresh
  service of atomic's own, since the issuer does not promise to renew stored
  tokens.

## Data lifecycle

M30 builds on the sensitivity labels of M56 (see [model
layer](model-layer.md#sensitivity-routing)):

- detectors for data loss prevention, with a check wherever data leaves;
- PII redaction in logs, transcripts and captured telemetry content;
- region-pinned projects and model endpoints;
- retention policies per project and label;
- one deletion workflow that reaches checkpoints, Codex transcripts, the graph,
  messages, artifacts, embeddings and snapshots, removing or crypto-shredding a
  subject's data;
- legal hold, which blocks deletion.

## Supply chain

- **M31a (v0.1):** a CycloneDX SBOM, Sigstore-signed release binaries, a
  `cargo-deny` licence and advisory policy, and tracking of every fork at its
  pinned revision. The repository's checks are described in
  [developing atomic](../development.md).
- **M31b (v1.0):** SLSA provenance and signed container images.

A dependency with a disallowed licence fails CI, and the SBOM lists each fork
with its pin.

## Enterprise builds

M32 covers installs that must run with no internet access: offline bundles that
include the pinned Deno, telemetry, updates and feedback off by default through
managed requirements, corporate proxies, private registries and mirrors, and a
FIPS build investigated across the whole dependency tree before it is offered.
The acceptance test captures packets during an offline install and run against a
private model endpoint and expects no traffic to unapproved hosts.

## Hard problems

- **Deletion versus durable history.** A deletion must reach durable checkpoints,
  transcripts, the graph, artifacts, snapshots and backups. Git-ref sync makes
  erasure hard, so regulated tenants use Postgres with no git sync, encrypt
  sensitive fields with per-tenant or per-subject keys so that destroying a key
  erases the data, and keep nothing sensitive in git.
- **Snapshots contain memory.** A runner snapshot can hold whatever was in its
  memory. Three things limit the exposure: the bucket holding snapshots is
  encrypted with customer-managed keys, runners by default never receive
  credentials or transcripts, and snapshots are kept only for a bounded time.
- **Encrypted local SQLite is not free.** SQLCipher replaces SQLite itself, and
  with one `libsqlite3-sys` in the binary it would switch Codex's database too.
  Laptops rely on OS disk encryption; servers use Postgres with encryption at
  rest.
- **Certification belongs to the deployment.** Any certification is earned by
  the operator of a deployment. What atomic contributes is the controls and the
  evidence for them: SBOMs, the change history, access reviews and exports of
  the audit log.
- **Local mode is one trust domain.** A laptop runs the control plane and the
  runner in the same process under the same user, so the boundaries are Codex's
  sandbox and the admin credential. On a server, an MCP server an atom launches
  over stdio is started inside the runner, away from the control plane and its
  credentials.
