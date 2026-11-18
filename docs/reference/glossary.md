# Glossary

| Term | Meaning |
| --- | --- |
| **Atom** | A focused task with typed input and output. `codex` runs an agent thread, `lite` makes a structured model call, and `exec` runs code with no model. |
| **Workflow** | A definition that coordinates atoms, value mappings, dependencies and gates. It describes execution, not a queue of project work. |
| **Run** | One execution with a durable ID, saved input and pinned definitions. A new `atomic run` creates a new identity; `runs resume` continues an existing unfinished one. |
| **Step** | A durable workflow operation whose committed result can be reused during recovery. |
| **Attempt** | One actual execution of a step. An interrupted or failed operation may have another attempt when policy permits. |
| **Checkpoint** | Saved progress/result at a durable boundary. It does not make arbitrary external effects exactly once. |
| **Thread** | A Codex agent conversation and its tool activity. Its lifetime is distinct from a durable workflow run. |
| **Gate** | A recorded approval proposal and decision between steps, with an approver and deadline. Different from a transient question inside a turn. |
| **Item** | A work-graph record with criteria, status, dependencies and links to results. It may lead to one or more runs. |
| **Claim** | Exclusive, recoverable ownership of a work item by an eligible worker. It does not itself complete the item or approve a gate. |
| **Work graph** | Atomic's built-in, beads-inspired store of work items, blocking relationships, claims and messages. It is distinct from a workflow's step graph. |
| **Artifact** | A durable, identified file/blob referenced by a step result, such as a report, patch or saved evidence. |
| **Candidate** | The exact code result proposed for integration, identified by a runtime-owned commit rather than a model-invented SHA. |
| **Effect ledger** | Durable intent/outcome records for external mutations, used with idempotency and reconciliation. |
| **outcome_unknown** | A parked operation whose external outcome is uncertain. Resume alone cannot authorize resending it. |
| **Replay** | Reuse of saved execution/results for recovery or inspection. Recording-based checks are distinct from new live model calls. |
| **Variant** | A second version of an atom in `<name>.<variant>.atom.astro`. It references the base atom's schemas explicitly, since nothing is inherited, and changes only instructions and settings. |
| **Component** | A local `.astro` file of shared prompt text that an atom imports. It takes typed props and can wrap the caller's content in a slot. |
| **Eval case** | An input and a list of CEL assertions for one atom, in a YAML or JSONL file. |
| **Fixture** | A prepared input, expected output or seed repository used by checks and evals. A golden fixture is written or reviewed by a person; it is not a model result. |
| **Recording** | A stored model call and result under its effective execution identity, reusable for regression checks without a live call. The plan also calls it a cassette. |
| **Scorer** | A check that gives each eval case a number, where an `expect` assertion gives pass or fail. Scorers other than `expect` are not specified yet. |
| **Budget** | Cumulative token, active-time and priced-cost limits for an atom or a whole run. |
| **Policy lock** | `atomic.policy.lock`, the committed record of every atom's and workflow's resolved permissions and limits, so a change to authority shows up in review. |
| **Decision** | A recorded choice in the manifest with the evidence it rests on and a review date. `atomic stale` reports the ones that need another look. |
| **View** | One way of drawing the same workflow or run: structure by default, authority with `--view risk`, budgets and spend with `--view cost`. |
| **Script atom** | An exec atom whose separate script performs executable work. Astro frontmatter is interpreted configuration, never a module that runs. |
| **Credential** | A named way for an atom's code to sign in to an outside service, defined in the manifest. For an AWS role, Atomic issues keys per step that expire in minutes. |
| **Fetched values** | A script step's typed output, passed to a subsequent Astro atom as ordinary input and referenced in a workflow as `steps.<id>.output`. |
| **Managed Deno** | A pinned runtime host outside project source. The compiler uses Deno 2.9.8; the development image supplies it and the setup script verifies platform archives. Broader automatic runtime management is staged. |
| **Codex** | [OpenAI's open-source coding-agent foundation](../README.md#open-source-openai-codex-underneath-atomic-on-top) embedded by Atomic; hosted inference/search remain services. |
| **durare** | The embedded durable execution engine used to checkpoint and recover workflows. |
| **OpenTelemetry / OTEL** | Instrumentation and export conventions for traces, metrics and logs; compatible backends such as Datadog are optional. |

Continue with [the mental model](../concepts/mental-model.md),
[durable execution](../concepts/durable-execution.md), or
[architecture](../concepts/architecture.md).
