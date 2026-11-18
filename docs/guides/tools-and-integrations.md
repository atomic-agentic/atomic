# Tools and integrations

Atomic can do useful local work before you connect an external system. Add a tool
when the task needs it and give the atom only the access required for that task.

## Choose the integration role

| You need… | Start with… |
| --- | --- |
| Current public documentation or research | [Codex's native web search](web-search.md) |
| A vendor SDK, such as the AWS SDK | [Code in the atom, or a script](scripts-and-cloud-sdks.md): reads before the model call, writes in their own step |
| An agent to read or act in a service | An approved MCP connection or typed API toolset |
| A service event to start work | A verified, deduplicated ingress adapter |
| People to approve work outside the terminal | A notification and gate-decision adapter |

[Slack](slack.md) illustrates the last three roles. Connecting a tool for the
agent does not automatically subscribe to events or enable human approvals.

## Capabilities inside an atom

A Codex atom has a coding harness, constrained by its sandbox and policy. Optional
Atomic, OpenAPI and MCP tools are declared separately. Lite atoms are suited to
supplied-context tasks; exec atoms run a command or script with no model.

Native web search comes from Codex's hosted tool support. It has its own search
mode and egress policy; a disabled shell network does not necessarily disable it.
Lite/local-model paths need an explicit compatible search capability.

Read-only and no shell network are the default atom posture. An implementer can
request workspace-write for its isolated worktree. A tool declaration selects
from capabilities the project and operator permit; it cannot widen managed policy.

## Connect in both directions

| Interface | Atomic consumes | Atomic exposes |
| --- | --- | --- |
| MCP | Tools from approved MCP servers | Selected atoms/workflows as tools; model-safe status and graph tools; separately authorized admin operations |
| OpenAPI / HTTP | Typed API operations from an imported specification | Run, graph and gate APIs; workflow endpoints |
| A2A | An approved remote agent as a typed atom | Durable workflows as tasks for other agents |

For example, a Spring application could call an Atomic workflow through MCP or
HTTP, and that workflow could call a business tool exposed by the application.
Neither direction requires embedding Atomic's Rust runtime in the application.

These integrations arrive in stages; see [availability](../reference/availability.md).
The local first-atom tutorial does not depend on them.

## Keep external effects explicit

An atom can draft an API request as typed output. A gate can authorize that exact
proposal, and a deterministic effect step can execute it under the recovery policy.
Use stable logical identities and supported idempotency/reconciliation behavior;
a human approval alone does not make a send safe to repeat.

From v0.5, an atom or workflow also declares which mutating operations a run may
request and how many of each, for example at most one comment on the triggering
issue. A request beyond its cap, or for an operation that was not declared,
fails validation before any gate opens. The declaration syntax is not final.

To see what a workflow would do without doing it, run it in staged mode:

```sh
atomic run <workflow> --input <file.json> --effects preview
```

Every external effect is recorded and shown, and none is applied. A step whose
code writes to an outside system is not run in this mode; the preview records
that it would run, and with what input.

External tool responses and documents are data with an origin. A response from a
help-desk API must not become a trusted instruction to approve a refund or change
workflow policy. Sensitivity controls also apply to model requests and fallback.

## More sources and runtimes

Application databases, typed files, retrieval and multimodal inputs are planned
extensions of the same contracts. TypeScript and Rust authoring add logic when a
declarative atom is insufficient. They are not prerequisites for the CLI or basic
workflow authoring.

Read [the recipe catalog](../../examples/recipes.md) for compositions and the
configuration contracts that remain to be designed. The
[A2A example](../../examples/07-a2a-translation/README.md) illustrates a typed
remote handoff with a mocked fixture and secret reference.
