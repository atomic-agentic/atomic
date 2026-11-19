# Design principles and inspirations

Atomic brings several established ideas to agent work: a durable work graph,
typed content files and conventions that explain themselves. You encounter them
as your needs grow; none requires learning the whole framework before coding.

## OpenAI Codex: the open-source foundation

[OpenAI Codex](https://github.com/openai/codex) is an open-source coding-agent
project. Atomic builds on its runtime and terminal-client source, bringing the
agent loop, coding tools, sandbox and interactive experience into Atomic's
application. The code is published under
[Apache-2.0](https://github.com/openai/codex/blob/237d8b104ed06ae46c5ed9a54693672dea21856c/LICENSE).

This is the implementation foundation. The beads, Astro and Spring Boot influences
below shape how Atomic organizes work, authors tasks and chooses defaults around
that foundation. Model endpoints and hosted services remain separate from the
open-source agent code.

## Beads: work belongs outside a conversation

[Beads](https://github.com/gastownhall/beads) models work as linked items with
dependencies. Atomic draws on that approach for its built-in work graph: ready
items, claims, follow-ups and a persistent record of what was done.

An agent can receive one bounded item instead of carrying the whole project in
its context. When it discovers more work, it can propose a linked follow-up.
A new session can recover the plan from durable records rather than reconstructing
it from an old chat.

Atomic has its own work-graph implementation and API. You do not need to install
or run the beads CLI to use Atomic.

## Astro: typed content is a good authoring interface

[Astro's content collections](https://docs.astro.build/en/guides/content-collections/)
bring schemas and references to content files. Atomic applies that idea to atoms,
workflows, schemas, toolsets and evals.

An Astro atom keeps instructions readable while literal configuration declares the
contract. Filename conventions and schema-derived types make files discoverable
and checkable. The bundled Astro parser and bounded TypeScript-shaped interpreter
check supported syntax before a model turn; they never execute the atom itself.
Business rules can live in ordinary TypeScript helpers, which run one call at a
time in an isolate that can read nothing but its arguments.

Executable SDK calls and I/O belong in separate script atoms. Workflows compose
scripts and model atoms as sibling steps. Public configuration is explicit, typed
and frozen for a run; secret credentials remain in their separate channel.

## Spring Boot: sensible defaults that back off

[Spring Boot's auto-configuration](https://docs.spring.io/spring-boot/reference/using/auto-configuration.html)
provides defaults that give way to explicit configuration. Atomic takes that
principle into local agent development.

A small manifest should be enough. Conventional filenames identify atoms;
conventional locations identify their schemas and evals. A detected provider can
get you started, and `atomic config --explain` shows what was selected and why.
You should not have to copy a page of defaults before doing useful work.

Explicit configuration operates within managed policy. Detecting a repository or
an available integration does not grant a tool permission to mutate an external
system. Convenience must remain explainable.

## Durable execution: progress is part of the runtime

Atomic uses durare as the basis for built-in workflow durability. Completed steps,
gates and recoverable runs belong to the runtime rather than ad hoc scripts around
each agent. The local path starts with SQLite; team deployment adds Postgres.

That durability works with typed boundaries and small tasks: save a result, pass
it to the next atom, and know where recovery can start. The
[durability guide](durable-execution.md) explains why interrupted steps and external
side effects still need explicit retry behavior.

## Keep a person in the loop without making every task manual

The CLI exposes running work and gives people a way to intervene. Agents can
return typed results, ask questions and coordinate through allowed tools; durable
gates reserve important decisions for the identities and policies authorized to
make them.

Next: [project conventions](../guides/project-structure.md) or
[the mental model](mental-model.md).
