# Design

These pages describe atomic's intended design: how it is built, why, and what
each milestone must prove before it counts as done. They are written for
contributors and for anyone who wants to know the reasoning behind the
user-facing docs.

**They describe the design, not the current build.**
[Availability and delivery stages](../reference/availability.md) says what is
built today. Where a design page and a reference page disagree about how
something behaves now, the reference page is right and the design page is a
target.

## How to read these pages

Start with [architecture](architecture.md) and [durability](durability.md):
they set the rules every other part follows. [Milestones and acceptance
tests](milestones.md) states, for every milestone, the check that shows it is
done; work is tracked against those checks. The other pages go deeper on one
area each and link to the user-facing page that covers the same ground.

## Pages

| Page | What it covers |
| --- | --- |
| [Architecture](architecture.md) | Control plane and runners, the two databases, the crates, the durare fork, crash isolation |
| [Design decisions](decisions.md) | The decisions that shape everything else, why they were made and what would reverse them |
| [Atoms](atoms.md) | The atom contract, atom kinds, how an atom runs, declarative definitions and their collections |
| [Workflows](workflows.md) | The workflow interpreter, pinned runs, load-time checks, review before integration, coding fixtures |
| [Durability](durability.md) | Candidates and publication, step rules, gates and recovery, the side-effect contract and its ledgers, the artifact store |
| [Work graph](work-graph.md) | Items, edges, claims and leases, provenance of stored text, sync, import and export, messages |
| [Planning, code context and verification](planning.md) | The planner, executable criteria, code context, planning modes, goals and deciding not to act |
| [People in the loop](human-in-the-loop.md) | The CLI on Codex's app-server, human-only tools, gates of every kind, approvals outside the terminal, the web console |
| [Schemas and APIs](schemas-and-apis.md) | JSON Schema as the contract, OpenAPI toolsets, atomic's own API and the rules every surface follows |
| [Code, scripts and runtimes](runtimes.md) | Exec atoms, files, toolchains, code in atoms on Deno, credentials and untrusted files |
| [Data sources, retrieval and media](data.md) | Application databases, retrieval and multimodal work |
| [Model layer](model-layer.md) | Tiers, providers, fallback, sensitivity routing and its enforcement point, quotas and authentication |
| [Deployment and interop](deployment.md) | Storage profiles, remote people and execution, MCP, A2A and Java integration |
| [Execution backends](execution-backends.md) | Backends, run ownership and fencing, Agent Substrate, kagent and container readiness |
| [Security, compliance and enterprise controls](security.md) | Trust boundaries, identity, audit, encryption and tenancy, data lifecycle, supply chain |
| [Observability](observability.md) | Telemetry conventions, traces for long and replayed work, metrics and content capture |
| [Developer experience](developer-experience.md) | Conventions and defaults, packs, tooling, the local loop, authoring skills and importers |
| [Operating and evolving atomic](operating.md) | Upgrades, operations, how atomic itself is tested, licence and platforms |
| [Milestones and acceptance tests](milestones.md) | The phases, their gates, and every milestone's acceptance test |
| [Directions](directions.md) | Ideas built on the same machinery, scheduled or still candidates |
| [Risks and open questions](risks.md) | Technical risks with their mitigations, and questions not yet settled |

## What atomic is for

atomic suits multi-step work that changes real systems, where a crash must
neither lose progress nor repeat it, and where a person approves the decisive
moves.

Coding comes first. A Codex atom is a full agent with a sandbox, a shell
and patch editing, so a workflow can plan a change, implement it in an isolated
worktree, review the candidate and wait for a person to accept it. From there
the same pieces cover pull-request review, CI failure triage, dependency
upgrades, large migrations split into many claimed work items, test generation,
documentation and scheduled codebase hygiene. Long-horizon work on small
self-hosted models is a design target: the work graph keeps the plan outside the
model and hands each step one ready item.

Outside code, the durability and the sign-offs stay the same; lite atoms, exec
atoms, OpenAPI toolsets and A2A supply the reach: incident response with every
production change gated, document processing into typed records, support drafts
approved before they are sent, evidence gathering on a schedule, research with
review gates, back-office approvals, workflows that include agents run by other
organizations, and scheduled evaluation of other agents.

Some work fits poorly. One-off questions with no follow-up are better served by
an ordinary chat product. Decisions needed in milliseconds do not suit a step
that is a model turn. Pure retrieval question answering is simpler as a search
or retrieval service.
