# Developer experience

The goal is that `atomic init && atomic run` works on a new laptop without any
configuration, that every default in a project can be explained, and that every
error says how to fix it. This page covers the conventions, packs, commands,
development loop and authoring aids built toward that goal. Commands are listed
in the [CLI reference](../reference/cli.md); conventions as users see them are
in [project structure](../guides/project-structure.md) and
[configuration](../reference/configuration.md).

## Conventions and defaults

- **Detected defaults that back off.** atomic sets itself up from what it finds
  around it. An API key in the environment picks that provider; a local model
  server answering on localhost can supply the `basic` or `fast` tier; a GitHub
  remote suggests the GitHub toolset; `ATOMIC_DATABASE_URL` naming a Postgres
  database switches to team storage (atomic never reads `DATABASE_URL`, which
  normally belongs to the application); and running inside Kubernetes picks that
  backend. Whatever is set explicitly takes precedence over detection (M39a).
- **Detection discovers, it never grants.** Managed requirements cap every
  layer. No flag, detected default or project file can raise permissions above
  that cap, and finding an integration gives no credential and no right to
  change anything.
- **Every default is explainable.** `atomic config --explain` lists each
  resolved setting with its value, where it came from (default, detection, a
  file, an environment variable or a flag) and why, with secrets masked.
- **Errors say what to do.** Each error from `atomic check` or the runtime has
  a description and a suggested action. With `--json` the action is its own
  field, so a coding agent can correct its mistakes in a loop.
- **Layered configuration.** From lowest to highest precedence: the committed
  manifest, a gitignored local file for personal keys and models, one file per
  profile (`local`, `ci`, `team`, `prod`), `ATOMIC_*` environment variables, and
  command-line flags (M36a).
- **Names bind by convention.** An atom is named after its file; schema files
  next to it are its schemas; its cases live in `evals/<name>/`. The smallest
  valid atom is a description and instructions, with its schemas next to it.
- **Variants live side by side.** Another atom file for the same task, with its
  own name and the same schemas, is a variant of it. Workflows and evals say
  which variant they use, and `eval --compare` compares variants as it compares
  revisions (M39b). See [variants](../reference/atom-files.md#variants).
- **Safe by convention.** By default the sandbox is read-only with no network;
  an atom writes only if it says so, and then only inside its step's worktree;
  every atom has a budget; a push needs a gate first; and build caches live
  outside worktrees. Projects can relax safety deliberately, but never have to
  ask for it.
- **Admin endpoints.** Next to health and readiness, `atomic serve` reports its
  resolved configuration, the conditions it detected, its runs and its metrics
  (M37).

## Packs

A pack brings a curated set of definitions that are known to work together into
a project under one name. There is no separate mechanism behind it: a pack is a
package, meaning a git source at a pinned revision, namespaced and hashed into
`atomic.lock`, that also knows how to install itself. `atomic add` writes the
`packages:` entry and updates the lock (M43).

| Content | Role in the consuming project |
| --- | --- |
| Atoms and workflows | Used by namespaced name, for example `github:review-pull-request` |
| Components | Imported into project atoms through an `@packs/*` path, which the generated editor configuration resolves |
| Schemas and toolsets | Registered in the pack's namespace |
| Eval cases and fixtures | Run against the pack's atoms; a variant in the project inherits them |
| Agent Skills | Installed for coding agents and versioned along with the pack |
| Configuration defaults and a context schema fragment | Placed under `config.<pack>`, where each atom's context sees them with types; the project's files and profiles can override them |
| Conditions and needs | When the pack switches on, and which secrets, credentials and hosts it requires |

- **`atomic add`** reports what the pack added, what its atoms will be able to
  reach (as the policy lock will record it), and what it is still missing.
  `atomic check` keeps failing with the same hint until those needs are met.
- **Conditions discover, they never grant.** A pack can switch on because of a
  git remote, a configured credential, a service it detects or a profile. A pack
  that is off is still loaded and checked; it simply fires no triggers and
  offers none of its toolsets. A pack that is on can do only what its atoms
  declare and the policy lock allows.
- **Trust.** Packs obey the same rule as other packages. A pack from outside the
  team counts as `external` and may only contain declarative content; pinning its
  hash raises it to `team`, which permits code. First-party packs are signed with
  a key built into the binary.
- **Compatibility.** For each pack the lock stores its source, version,
  revision, hash, its `requires` range and the atomic version that verified it.
  Before writing the lock, `atomic update` shows which exports appeared or
  disappeared, how prompt snapshots and the policy lock change, and which
  configuration defaults moved.
- **Registry.** A public index of packs is planned (M43b), and private indexes
  can be added in user or managed configuration. `--vendor` copies a pack into
  the project for machines without network access. `atomic pack verify` runs a
  pack's checks, evals and render snapshots on their own before `atomic pack
  publish` will accept it.
- **What packs are not.** They are not a plugin API, since they contain no Rust
  code and no runtime hooks; they grant no permissions; and they are not a
  second way to import things.

## Tooling

| Command | Does |
| --- | --- |
| `atomic init [template]` | Creates an embedded or a dedicated project; `--workspace` sets up a monorepo |
| `atomic dev` | Watches the project, reloads changed definitions and re-runs the checks and evals they affect |
| `atomic run <name-or-file>` | Runs a single atom or workflow and prints its typed output; input comes from `--input <file>`, `--input -` or `--data '<json>'` |
| `atomic curl <name> -d '<json>'` | Calls a served atom or workflow over the HTTP API with a JSON body; `--print` shows the same request as a plain `curl` command, token masked (M16) |
| `atomic render <atom> --input <file>` | Shows exactly what prompt a run would send, with no model call. For an atom with code it calls `prepare` first, unless `--prepared <file>` provides those values. `--profile` picks a profile, `--org` an organization overlay, and `--as` sets the principal to a named user, group or service account, so comparing two renders shows every line that identity or tenancy changes |
| `atomic eval <atom>`, `atomic run <workflow>` with `--plan` | Lists the model calls, compiled prompt sizes and a cost range, then stops before calling any model |
| `atomic check`, `atomic types`, `atomic schemas` | Validation, generated types and editor schemas |
| `atomic runs show <id>` | Everything recorded about a run: each step with its input and output, the spend, what people did and the transcripts |
| `atomic runs replay <id> [--from <step>]` | Re-runs from checkpoints using the recorded model calls; can step through, or fork with edited input |
| `atomic stale [--as-of <date>]` | Lists recorded decisions whose evidence or review date has run out |
| `atomic runtimes install`, `atomic runtimes lock`, `atomic deno`, `atomic test` | Install the pinned Deno, write `deno.lock`, call Deno directly, and run code tests that can reach only the local machine (see [runtimes](runtimes.md)) |

A language server for templates and CEL, a documentation site and a gallery of
examples will round this out (M39b).

## Local development loop

Writing atoms by hand happens in three loops, each costlier than the one before.

1. **While typing (no model calls).** Frontmatter and workflow files are
   validated against generated schemas. The editor marks an expression path that
   does not exist, a type error in a condition or validator, and a typed example
   that does not fit its schema. See [VS Code](../guides/vs-code.md).
2. **On save (no model calls by default).** `atomic dev` reloads whatever
   changed and shows, for each atom, the size of its compiled prompt, how its
   schema was lowered and its content hash. It then re-runs checks against
   **cassettes**, which are recorded generations keyed by everything that makes
   up the real request: the compiled messages, the normalized input, the hashes
   of prepared context and artifacts, the generation schema and lowering
   profile, the provider, endpoint, model and settings, the tool definitions
   and fixture versions, and the revision of the seed repository. Editing only
   validation logic re-checks the recorded output with no model call, and the
   result is labelled a historical regression check. Changing any part of the
   request identity throws the recording away. A reused recording is never
   reported as a live pass (identity in M39a, the on-save loop in M39b).
3. **On demand (spends tokens).** `atomic run` against a fixture, `atomic eval
   --live` to record afresh, and `atomic eval --compare <rev>` to see pass rate,
   cost and output differences between two versions. A Codex atom gets a
   disposable worktree cut from a fixture repository, so the checkout the
   developer works in is left alone.

Workflows get extra aids. Gates in dev mode (`--gates auto|prompt|stop`) are
recorded in the run. `--fail-at <step>` fires a crash failpoint from the command
line, so a developer can kill a workflow, resume it and see that nothing runs
twice. `--effects preview` records every external effect without applying any.
OpenAPI toolsets can run against a mock generated from their spec, and MCP
servers can be replaced by recorded responses. A dev budget per session, cheaper
tiers or local models while iterating, and `--plan` keep spending in view. For
debugging there is `runs show`, `runs replay` with `--edit-input`, and `eval
add-from`, which turns a failure into a case.

The user's guide to evals and cassettes is
[test and improve an atom](../guides/testing-and-evals.md).

## Authoring skills

atomic ships Agent Skills that teach coding agents, and atomic itself, how to
write atoms and workflows.

- **The skill teaches, the CLI checks.** Every skill works the same way. The
  agent writes the file and calls `atomic check --json`, corrects whatever comes
  back, then runs the evals and reports. The format reference inside a skill is
  generated from atomic's schemas when atomic is built, so the skill always
  matches the binary that checks its output.
- **The first skill** (M42a) is `atomic-create-atom`, which
  `atomic setup claude` and `atomic setup codex` install; see
  [write atoms with a coding agent](../guides/coding-agents.md). Later skills
  will cover project setup, workflows, components (including spotting the same
  guidance repeated across atoms and pulling it into a shared component), evals,
  debugging runs, OpenAPI toolsets, atom code, importing and planning migrations
  (M42b, M43).
- **Portable format.** A skill is a `SKILL.md` with the standard fields plus a
  `references/` folder. It has no scripts; the only tool it calls is the
  `atomic` CLI. `atomic setup` merges into an agent's configuration without
  losing or reordering anything already there, and undoing it puts the old
  configuration back.
- **Versioned with the binary.** `atomic check` warns when the installed skills
  are older than the atomic that is running.
- **`atomic new "<task>"`** will be a durable workflow built into atomic (M43):
  it drafts an atom with its schemas and evals, loops on check and eval, and
  stops at a gate where a person accepts the result.
- **Atoms can use skills (M43).** Listing skills under `skills:` loads them into
  the atom's Codex thread. A skill that contains scripts is handled like code:
  it must come from an allowed origin and be pinned by hash.
- **Migration as gated work.** `atomic-plan-migration` surveys an existing agent
  project, decides for each piece whether to import it, wrap it (over A2A or
  HTTP) or rewrite it, and files the plan as graph items with dependencies. Each
  item ends at a gate and carries eval cases that feed identical inputs to the
  original agent and to its replacement atom.

## Learning from human decisions

When people correct an atom, the correction is labelled data. atomic will turn
these corrections into eval cases for that atom (M40):

- **Rejected gates, take-over edits and steering.** A single action saves the
  input, the output that was rejected and the person's reason as a case. The
  case fails until the atom is fixed. From the terminal that is `atomic eval
  add-from <run> <step>`; next to a decision it is one button.
- **Any run can be annotated.** Even a run that passed all its gates can be
  marked `wrong`, `right` or `negative` with a note, from `runs show` or from
  the console, and becomes a case the same way.
- **Dismissed findings become negative cases**, so a reviewer that keeps raising
  the same false alarm gets tested against it.
- **Generated red-team cases.** atomic reads an atom's `trust`, `tools` and
  `effects` declarations and builds cases that try injection and overreach: for
  example, external input that asks for exactly the write the atom is allowed to
  make, or a retrieved chunk that tries to widen the atom's scope. These run as
  evals with cassettes on a schedule, and any case that succeeds is reported
  with the prompt that was rendered.

Eval results, gate decisions and steering are emitted as telemetry events too,
so the learning loop can find its cases with a query (see
[observability](observability.md)).

## Importing existing agents

`atomic import` (M41) will turn existing agent definitions into atom files.
Sources include `AGENTS.md` files, skills and agent roles from Codex, skills and
subagents from Claude Code, and kagent agent definitions, and the importer
builds on a configuration-migration crate that Codex already has. Each imported
atom gets inferred schemas flagged for review, and `atomic check` points out
whatever did not convert. Typed prompt formats (Dotprompt, Prompty and
Agent-Flavored Markdown) become lite atoms, schemas included. In the other
direction, `atomic export` writes a lite atom as a Dotprompt or Prompty file
that imports back with the same schemas; anything with no counterpart in the
target format is flattened or reported.
