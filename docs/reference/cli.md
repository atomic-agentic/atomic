# CLI reference

This is a sketch of the proposed CLI, not a command list verified against a
released executable. [Availability](availability.md) identifies the planned stages.
Angle brackets below mean a value you supply, not literal shell syntax.

## Start with the command for your task

| Command | Purpose | Planned stage |
| --- | --- | --- |
| `atomic` | Open the interactive coding client; attach to an existing runtime or start locally | v0.5 interactive CLI |
| `atomic init` | Create a manifest and starter definitions | v0.1 |
| `atomic run <name-or-file> --input <file.json>` | Run an atom or workflow with typed input, by its name or by the path to its file | v0.1 |
| `atomic curl <name> -d '<json>'` | Send JSON to an atom or workflow on a running runtime and print the typed result | v0.5 |
| `atomic check` | Validate project definitions and references | v0.1 |
| `atomic eval <atom>` | Evaluate an atom against its cases | v0.1 |
| `atomic config --explain` | Inspect resolved values, sources and detected defaults | v0.1 |
| `atomic runs show <run-id>` | Inspect saved results, metadata and run state | v0.1 |
| `atomic runs resume <run-id>` | Continue the same unfinished local run in the foreground | v0.1 |
| `atomic serve` | Run the local headless runtime and recover eligible unfinished runs | v0.1 local; team service v0.5 |

The CLI discovers a project from `atomic.yaml` or `.atomic/atomic.yaml`. For
interactive coding, the target entry experience also works in an ordinary
directory without a project manifest.

`atomic run` returns a typed result when successful and provides a run identity
for inspection. Local workflow gates can wait for your decision in the terminal.
Use `atomic runs resume <run-id>` after an interruption, from the same project
and state store. It uses saved inputs and definitions; there is no replacement
`--input`. If another local runtime owns the project, it reports that owner
and does not start a competing executor. See [recovery](../concepts/durable-execution.md#resume-existing-work)
for gate deadlines, budgets and recovery limits.

## Choose what to run and what to send

Inside a project directory, name the atom or workflow:

```sh
atomic run classify-note --input note.json
atomic run classify-note --data '{"text": "Export crashes on large files."}'
cat note.json | atomic run classify-note --input -
```

`--input` takes a file, or `-` for standard input. `--data` takes the JSON
itself, which is the quickest way to try an atom.

From anywhere else, point at the atom's file, or name the project:

```sh
atomic run ~/work/note-tools/atoms/classify-note.atom.astro --data '{"text": "…"}'
atomic --project ~/work/note-tools run classify-note --input note.json
atomic --project ~/work/note-tools/atomic.yaml run classify-note --input note.json
```

A path to an `.atom.astro` or `.workflow.yaml` file runs that
definition; Atomic finds its project by walking up from the file. `--project`
takes the manifest's directory or the `atomic.yaml` file itself.

## Send JSON to a running runtime

`atomic run` executes in the process you started. `atomic curl` instead sends
the input to a runtime that is already serving, over the same HTTP API an
application would call. It is planned for v0.5, with that API.

```sh
atomic curl classify-note -d '{"text": "Export crashes on large files."}'
atomic curl classify-note -d @note.json -i
atomic curl classify-note -d @note.json --print
```

| Option | Meaning |
| --- | --- |
| `-d <json>`, `-d @<file>`, `-d @-` | The input: inline, from a file or from standard input, as in curl |
| `-i` | Also show the run ID and status |
| `--print` | Do not send. Print the equivalent plain `curl` command, with the token masked |
| `--remote <url>` | Send to that runtime and not the local `atomic serve` |

The command finds a local `atomic serve` by itself and handles the address and
the token for you, which is what makes it shorter than curl. `--print` is the
bridge to your own code: it shows the request to copy. The result is the
atom's typed output on standard output. If a workflow stops at a gate, the
command reports the run ID and what the run is waiting for. With no runtime
serving, it fails and suggests `atomic run`.

## Coding fixture selection

In the v0.1 target, `--project` selects a project explicitly, by its manifest directory or manifest file.
Repository-backed development runs/evals also require paired `--fixture <repo>`
and `--fixture-rev <revision>` options. The runtime pins the resolved commit and
uses private snapshot worktrees; invalid/missing commits fail before a model call.
See [fixture repositories](eval-cases.md#coding-fixture-repositories) for setup and
an example that works from an unrelated directory. These flags do not change the
minimal lite-atom quickstart.

## Gates and supervision

| Command or surface | Purpose | Planned stage |
| --- | --- | --- |
| `atomic gates list` | Inspect durable gates | v0.1 local |
| `atomic gates approve <gate-id>` | Approve a recorded proposal as the local operator | v0.1 local |
| `atomic gates reject <gate-id>` | Reject a recorded proposal as the local operator | v0.1 local |
| `atomic workflows show <name>` | Inspect a workflow definition and its steps | v0.5 |
| `atomic graph` | Inspect work items, status and blocking dependencies | v0.5 |
| Live thread controls | Answer, steer, stop and take over work | v0.5 CLI |
| Workflow and graph commands with `--json` | Use the supervision surface from scripts | v0.5 |
| Dedicated workflow/graph/gate panes | Navigate larger sets of work in the terminal | v1.0 TUI extensions |

Live-thread cancel, pause and take-over command spellings and keyboard shortcuts
remain unspecified. Run recovery uses the v0.1 command above. The
[control guide](../guides/controlling-agents.md) describes live controls without inventing flags.

## Develop reusable tasks

| Command | Purpose | Planned stage |
| --- | --- | --- |
| `atomic dev` | Watch definitions and run affected checks/evals | v0.5 |
| `atomic dev --otel` | Start the optional local observability setup with Jaeger or Langfuse | v0.5 / M44 |
| `atomic eval <atom> --live` | Evaluate with live calls and record applicable responses | v0.5 development loop |
| `atomic eval <atom> --compare <revision>` | Compare behavior, cost and output across definitions | v0.5 |
| `atomic render <file.atom.astro> --input <file.json>` | Print developer/user prompt parts without a model; optional `--context-settings`, `--profile`, `--run-id`, `--step-id` | Implemented direct invocation |
| `atomic eval <atom> --plan`, `atomic run <workflow> --plan` | Print the expected model calls and a cost range, then exit without calling a model | v0.5 |
| `atomic run <workflow> --effects preview` | Run a workflow in staged mode: external effects are recorded and shown, none is applied | v0.5 |
| `atomic schemas expand <atom>` | Print the JSON Schema that a compact schema compiles to | v0.5 |
| `atomic runs replay <run-id> [--from <step>]` | Inspect recorded execution from a chosen point | v0.5 |
| `atomic types`, `atomic schemas` | Generate editor types and schemas | v0.5 |
| `atomic setup <agent>` | Install Atomic authoring skills for an external coding agent | v0.1 initial pack |

Read [testing and evals](../guides/testing-and-evals.md) before treating recording
replay as a live model result.

## Review and maintain a project

| Command | Purpose | Planned stage |
| --- | --- | --- |
| `atomic check --write-lock` | Write `atomic.policy.lock`, the resolved permissions and limits of every atom and workflow | v1.0 |
| `atomic check --fix` | Planned Astro-aware escaping assistance; literal syntax currently uses entities | v0.5 |
| `atomic stale [--as-of <date>]` | List [recorded decisions](manifest.md#recorded-decisions) that need another look: the review date has passed, or the evidence is too old, was produced on a different model or prompt version, or no longer holds | v0.7 |
| `atomic runtimes install [--from <archive>]` | Fetch and verify the Deno version this Atomic release pins, ahead of time; `--from` installs from a file on a machine with no network | v0.7 |
| `atomic runtimes lock` | Write `deno.lock` from the packages that atom code and scripts import | v0.7 |
| `atomic test` | Run the project's code tests on the managed Deno, with network access limited to the local machine | v0.7 |
| `atomic deno <args>` | Run the managed Deno directly, for a project's own tests or to update `deno.lock` | v0.7 |
| `atomic import`, `atomic export` | Convert agent and prompt definitions from other tools into atoms; write a lite atom as a Dotprompt or Prompty file | v1.0 |

The [policy lock](../guides/security-and-permissions.md#review-permission-changes-as-a-diff)
makes a change to permissions visible in review. `atomic stale` takes its date
as an argument and is meant to run on a schedule; `atomic check` never reads
the clock. A local run downloads the managed Deno by itself the first time a
project needs it, so `atomic runtimes install` matters for CI, images and
offline machines; see
[you do not install Deno](../guides/atom-code-runtime.md).

## Diagrams

| Command | Purpose | Planned stage |
| --- | --- | --- |
| `atomic workflows show <name> --format mermaid` | Print a workflow's steps, gates and dependencies as a Mermaid diagram | v0.5 |
| `atomic graph --format mermaid` | Print work items and their blocking dependencies, with each item's status | v0.5 |
| `atomic runs show <run-id> --format mermaid` | Print a run's steps with their current state | v0.5 |
| `--view risk`, with `workflows show` or `runs show` | Show what each step is allowed to do: sandbox, network access, tools, effect caps and the approvers of each gate | v0.5 |
| `--view cost`, with `workflows show` or `runs show` | Show each step's budget and, for a run, the tokens and spend so far | v0.5 |
| `atomic docs --write` | Refresh generated diagrams that are committed in Markdown files | v0.5 |

```sh
atomic workflows show summarize-and-review --format mermaid > review-flow.mmd
```

A view changes what each node shows, never the source, and no view depends on
color. `atomic graph` has one view. The output is text, so it can be piped, committed or pasted into a pull request.
`atomic check` fails when a generated diagram committed in a doc no longer
matches its source. See [diagrams with Mermaid](../guides/diagrams.md) for
examples of each output.

## Serve and attach

```sh
atomic serve
atomic --remote <url>
```

`atomic serve` runs a local headless runtime in the foreground in **v0.1**. Keep
it running under a supervisor for progress after your terminal exits. It recovers
eligible unfinished runs in the selected project store; approve or reject gates
from a second terminal. It is not a detach command.

`atomic --remote <url>` and shared/team service access arrive in **v0.5**, with
identity and transport controls. Use `atomic runner` only when operating
a separate execution backend, planned for the platform stage.

See [local and team operation](../operations/running-atomic.md) for roles and storage.

`atomic render` without `--run-id` is an ephemeral preview: each invocation uses
current atom sources, settings and declared environment values. Supply
`--context-settings` for each contextual preview. With an explicit `--run-id`,
Atomic checkpoints public context and restores it on subsequent invocations;
changing the compiled atom requires a new run identity. Checkpoints carry a
keyed authentication tag, checked using a private key in the host state directory.
Unsigned checkpoints from earlier implementations are refused; use a new run id.
The key and checkpoints must remain outside model-accessible paths. Their public
SHA-256 digest alone is a consistency check, not proof of host authorization.
