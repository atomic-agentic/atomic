# Availability and delivery stages

These docs sketch Atomic's intended product experience. As of this draft, this
repository contains plans and example definitions, not a released Atomic runtime.
The CLI snippets are proposed product behavior and have not been executed against
an Atomic binary.

The first-release path and the full interactive CLI arrive at different stages.
The documentation leads with the complete product picture so readers can see why
the coding agent and framework belong together.

| Stage | Target experience |
| --- | --- |
| **v0.1 · one developer** | `init`, `run`, `runs resume`, local headless `serve`, `check`, evals and run inspection; file-defined Codex/lite/exec atoms; sequential durable workflows; local gates; local artifacts, budgets and basic spans |
| **v0.5 · a team** | Full interactive CLI attachment, live agent supervision, work graph and messages, Postgres, initial identity/audit controls, full OpenTelemetry integration and optional local trace helper, Slack approvals, OpenAPI tools, triggers, parallel composition and merge queue |
| **v0.7 · models and data** | Planner and code context, broader model routing, TypeScript in atoms on Deno, application data sources, typed files, retrieval, multimodal work and outcome-based learning |
| **v1.0 · platform** | Remote workers/environments, Kubernetes/Substrate execution, extended TUI panes, TypeScript workflows, published MCP workflows, A2A, broader enterprise controls and web console |

Codex's native web search and MCP client already exist upstream; Atomic
configuration and policy integration remain planned; native web search does not
depend on the later RAG milestone. Slack MCP access, inbound Slack triggers and
Slack gate approvals are separate integrations, with Atomic adapters still planned.

The first authoring skill, `atomic-create-atom`, is installed today by
`atomic setup claude` and `atomic setup codex`; the rest of the pack and
higher-level project generation follow later. Workspace member discovery
and shared-schema resolution are built; sharing atoms between members belongs
to M11b/M39b in v0.5.

Declarative atoms now use Astro exclusively. The production loader supports local
components, named slots with fallback content, frontmatter locals, template
literals, bounded TypeScript-shaped expressions, typed validators,
[TypeScript helpers](atom-files.md#typescript-helpers), which run in a fresh,
bounded Boa isolate in a short-lived child with the step's recorded clock, and
[literal Markdown imports](atom-files.md#literal-prompt-examples), which bind a
project file's text as authored prompt content.
`atomic check` checks source and generates editor types, including
[types from shared schema files](atom-files.md#types-from-shared-schema-files); `atomic render` previews
prompts without a model for atoms without helpers; helper-backed atoms require
`--preview-helpers`, which makes a non-parity inspection preview. `atomic check`
compares opted-in prompt snapshots
for context-free model atoms without TypeScript helpers. Contextual snapshot
comparison awaits a frozen context manifest; helper snapshots await a recorded
step clock. Markdown and MDX atom frontends have been removed.
The Astro compiler and full JSON Schema gates execute in this repository; broader
workflow scheduling, interactive commands and integrations retain their staged status.

The workflow interpreter is built: a workflow file runs as one durable run, with
no rebuild, through sequential atom steps (Codex, lite and exec atoms), local
gates and `worktree: true` steps, which run a Codex atom in its own worktree at
the step's `base` and record atomic's commit in `steps.<id>.metadata.commit`.
A run keeps the definitions it started with. An atom step's `review` supports
one reviewer atom or a fixed two-reviewer coding panel with `require: all`.
Each reviewer produces a typed verdict in its own durable step. A request for
changes gives the author repair rounds; a rejection or exhausted rounds goes
to a person at a gate. `require: majority`, `ai-then-human` gate policy and
calibrated decision atoms are not built yet.

The local SQLite work graph accepts beads JSONL through
`atomic graph import <file>` and writes it through `atomic graph export`.
Committed import and the human `graph create`, `link`, `claim` and `close`
commands require the enrolled operator credential; their writes verify it in
the graph transaction. Import refuses an unusable file as a unit, and repeated
imports leave matching items unchanged. Import redacts credentials in items,
comments and memories before storage. Native exports preserve success and
failure so conditional dependencies retain their readiness after a round trip.
An ordinary closed Beads item has an unknown outcome. `atomic graph import
--dry-run` and `atomic graph export` open the project store read-only. A
missing database stays absent, and older stores are inspected using a migrated
in-memory snapshot. These commands accept `--json` through the
CLI's global option. `atomic graph create` accepts `--request-id <key>` so a
person can retry a create with the same key and payload and receive the original
item. Without it, each invocation creates a fresh request. `atomic graph pin
<item>` admits an imported open epic
for graph runs after an operator approves the transition. `atomic graph
reconcile-failed <epic> --run <run>` lets that operator release a terminal
failed run's exclusive epic binding when it holds no graph item lease; the
corrected run can then bind without losing already closed children. `atomic graph policy set <item> --require commit` and
`atomic graph policy clear <item>` use the same credential to change a named
item's close rule. `policy show` is read-only and identifies an item rule or an
inherited legacy rule. `--require verified-commit` requires an exact
`--branch refs/heads/<name>`. `graph verify-commit` checks a recorded worktree
attempt against that branch before storing verified evidence. `graph criteria
set <item> --file <path>` accepts a bounded JSON list of `text`, `test`, `cel`
and `http` criteria from the enrolled operator; `criteria show` is read-only
and `criteria clear` removes the list. Imported Beads acceptance prose is
stored separately and is never executed as a criterion. The staged graph work
loop runs each executable item criterion after the author and before reviewers
or publication. Test and HTTP criteria require a runner that has verified
confinement and hides other projects’ scratch directories. A failed criterion
enters a bounded repair round. Diagnostic text is redacted before atomic
checkpoints it or passes it to the repair model.

`atomic goals list` and `atomic goals show` use the same read-only inspection
path as graph export. Graph JSONL preserves a pinned goal item and its child
links. To register it in the destination, an enrolled operator runs
`atomic goals add <name>` with the local `goals/<name>.goal.yaml`. This
activates the restored goal using that definition. Repeating the command for
an already registered goal preserves and reports its current active or paused
state. PostgreSQL
graph storage remains staged; this v0.2 work is not a released runtime.

The staged `atomic delivery start` command reads a committed graph coding
item's accepted candidate, two clean independent reviews, self-review,
control proofs and trial merge before opening an enrolled-operator gate.
`atomic delivery resume` applies the approved push, pull request and queue
requests through an operator-installed adapter in private project state. Each
request has a durable intent and an exact remote receipt; an ambiguous result
parks for reconciliation and is never sent again merely because the process
restarted.

A `coding: true` worktree step now requires a sibling `review.self_review`
turn before each independent review. atomic computes the candidate's patch
from its recorded base and commit with its hardened git runner, checks the
ten-item typed checklist evidence, and gives the reviewer that evidence and
the patch as external data. The host pins the original admitted revision,
reads its decision inventory and control rows, and selects touched controls
from the candidate diff. Missing inventory, omitted rows or decisions, and
uncertain path aliases stop review. The reviewer also receives this host
inventory and verified control results as external data. See
[coding reviews](workflow-files.md#coding-reviews).
Coding review requires a trial merge and checks against protected main.
Before each unfinished self-review or reviewer turn, atomic re-reads that
branch head. If it advanced since the recorded trial, the new turn stops and
the operator starts a new run; completed review turns remain checkpointed.

`atomic init`, `atomic run`, `atomic runs resume`, `atomic runs show`,
`atomic config --explain` and a local foreground `atomic serve` are built.
`--profile` on `config`, `check`, `run`, `eval`, `serve` and `runs resume`
selects a project or workspace member's `atomic.<profile>.yaml`, with
`ATOMIC_PROFILE` as the fallback; invalid names and missing selected files
are refused. A project's profile cannot select the operator's database URL
or API key variable. See [configuration](configuration.md#the-layers).
`atomic init` writes a manifest, a lite atom with its schemas, a one-step
workflow and an eval case: at the top of an empty directory, or under
`.atomic/` in one that already holds code. With only `OPENAI_API_KEY` set, or a
local Ollama running, the new project runs with no configuration. `atomic run` takes an atom or workflow by name, by file or with
`--project`, and input from a file, standard input or `--data`; a run cut short
continues with `atomic runs resume <run-id>`, or under `atomic serve`, from its
checkpoints. A workflow that changes code runs only bound to a fixture
repository and revision (`--fixture`, `--fixture-rev`): its worktrees come
from a snapshot of that commit in the state directory, never from your
checkout. One process at a time executes a project's runs. Errors carry a
description and an action, separately in `--json` output. `atomic gates enroll`,
`list`, `approve` and `reject` decide gates from a second terminal; a gate
decision counts only with the credential its person enrolled. `atomic run` and
`atomic runs resume` prompt at the terminal for each gate their run reaches, and
question gates (`ask` with a typed `answer`, `remember`, and a default when time
runs out) are built; `atomic gates answer` answers one from another terminal.

Codex atoms can run on a ChatGPT sign-in (`atomic model login`, `model status`,
`model logout`), kept in the OS keyring or the opt-in credential file store, on Linux
only, for the developer's own runs: a shared store and `atomic serve` refuse it.
Lite atoms need a model endpoint and say so.

A project's artifact store takes a lock in its private state while it is
written or swept. A run that finds it held by another process, such as a sweep,
as it pins its definitions or gives an exec atom's command the artifacts its
input references, says so (a pin once a run, an exec step once each time it
waits), waits and tries again for a bounded time (about 7.4 minutes with the
store's 30-second wait: 11 tries and the pauses between them), then stops,
recording nothing and running nothing, and is run again. A process that is
stopping waits out no more pauses between tries, though a try already
waiting on the store's lock runs to the end of that lock's wait. Such stops never count toward dead-lettering the
run, however many there are; each spends one of durare's recoveries of the
run, and once they run out the run waits for `atomic runs resume`. The files
a command hands over are stored after it has run: a store held then is
waited out a few times, and past that the step's outcome is unknown, as
after a crash there.

`atomic run` keeps the content its input names from a sweep until the input
is recorded. Before the run starts, it holds each artifact the input names
and marks it as just written, under the store's lock. Holding and marking
share one wait for the lock, the store's 30 seconds: a sweep holding it is
said once and waited for, and if it holds the lock longer the run is refused
(`store.locked`); Ctrl-C while it waits stops the run (`run.stopped`). A field
the workflow's input contract types as `atomic:artifact` that names content
the store does not hold, or content a sweep removed after the store was
listed, refuses the run (`run.input_missing`), and one naming content the
contract does not allow there refuses it too (`input.invalid`); the first
such field is the one named. A reference in free text is held if stored and
otherwise left alone. Either way nothing is recorded.

A sweep also keeps the content a gate's decision names, in a question's answer
or an approval's comment, from the moment it is decided, before the run that
asked wakes and records it, and anything else a gate or an entry of the
feedback log names, such as a question's default, for every project's gates
in the store, not only the project that sweeps. `atomic gates answer` and
the other deciding commands hold what the decision names, and mark it as just
written, while the decision is recorded, so a sweep that read the gate store
just before keeps it too; a decision naming content a sweep removed meanwhile
is refused, with nothing recorded. A gate store the sweep cannot read fails
the sweep, and nothing is removed.

`atomic serve` also runs on PostgreSQL (`ATOMIC_DATABASE_URL`) when the process
has an executor identity (`ATOMIC_EXECUTOR_ID`). `atomic runner` and
`--runner process` run exec atoms' commands in a separate process with an
exit-code contract; Codex turns and their shell still run in the control
plane's process, and a runner reached over the network is not built yet.

Every atom run goes through the execution backend: once its prompt is
rendered, which runs the atom's helpers, it opens a workspace there before any
model call or command, and closes it however the run ends, including a panic or
a cancelled run, and only once a command running in it has ended. A backend
with no room for a workspace makes the run wait, recording nothing and saying
on stderr that it waits, and the run goes on once there is room. atomic's own
writes for a step that find the disk or a quota full, making its run's build
caches or the directory its worktree goes in, wait the same way. A checkout git
cannot finish, for a full disk or anything else, fails the step with what git
said, and the run ends failed; once space is freed, start it again. Each of
these waits, and the wait for the backend, keeps its own clock, so a worktree
step may wait up to three times its limit: for its caches, its worktree's
directory and its workspace. Without a limit, a step whose own writes find the
disk full waits for as long as it takes, saying so on stderr every ten minutes.
With the
`backend_wait` setting (`--backend-wait <seconds>`, `ATOMIC_BACKEND_WAIT`) it
waits at most that long: `atomic run` and
`atomic runs resume` then stop with exit code 75, the run unfinished and nothing
recorded, for `atomic runs resume` to continue, and `atomic serve` sets the
step aside and keeps trying, at most 30 seconds apart, so the run goes on once
there is room. A signal ends a wait at once, and a workspace that opens as the
process stops is closed again before anything runs in it. The
local backend is the only one with a Codex shell, and
it states that its workspaces are durable. A backend that states anything else,
or a value atomic does not recognize, is treated as ephemeral, and a workflow
with a `worktree: true` step is refused on it before anything runs.

A command an atom runs, whether an exec atom's or one a model runs in a Codex
turn, gets the platform's core variables and what the atom declares, never
atomic's own environment or its credentials. Nor does it get the descriptors
whatever started atomic left open, such as a CI runner's pipes or a shell's,
an editor's or an agent's files and sockets: on Unix, atomic marks every one
past the standard three close-on-exec as it starts, and so does Codex's Linux
sandbox helper before it starts a command, so a command holds only what atomic
hands it. That includes descriptors handed to atomic on purpose, such as
systemd's socket activation (`LISTEN_FDS`) or a `make` jobserver: none reaches
a command, or another program atomic starts. On Linux, atomic uses
`close_range`, or, on a kernel before 5.11, the list in `/proc/self/fd`; where
it has neither, atomic does not start, and the helper does not start the
command. Codex's process launcher also marks, as far as it can, what a turn's
commands would inherit close-on-exec. On macOS atomic relies on
the list in `/dev/fd`, which is not verified. Windows, where a process
inherits handles rather than descriptors, is not covered. A turn's commands run with no
login shell; a `workspace-write` turn's `HOME` (and `ZDOTDIR`) is a private
scratch directory of the attempt's own, a `read-only` turn's `HOME` is
`/nonexistent`, where nothing may be (the turn does not start if anything is
there, and neither does any git atomic runs itself, so every run stops on
such a machine), so its shell starts without a word about a home it cannot read, and
their `TMPDIR` is a directory inside the scratch directory, so no shell
startup file runs and no other turn, of the project or another project of the
same atomic home, can see, add, change or remove what a turn keeps in its
`HOME` or temporary directory: every project's scratch directories, those of
`atomic run`, `atomic serve` and `atomic eval` and those of `atomic debug
codex-prompt` and `atomic debug probe`, are kept in the atomic home's
`scratch/`, which is denied to a turn's commands whole, apart from a
`workspace-write` turn's own directory. A `read-only` turn, which may not
write its own, cannot reach it at all, not even to list it, nor its `TMPDIR`
or `CARGO_HOME` there. Apart from macOS, whose own sandbox denies them, only
Codex's Linux sandbox helper, which the `atomic` binary provides, can deny
them, so a process without it refuses the turn instead of running it. On macOS
none of this is verified. An exec atom's command, which has a scratch
directory of its own too, is kept out of the others in the same way, its own
apart, wherever its command is confined, on Linux; where exec atoms' commands
run unconfined, it is not. (Codex's sandbox keeps its registry of the mount
points it makes in a directory every turn of the project shares, which no
turn's command may write.) Tools found through `HOME` are therefore not found: cargo's registry and tools, pyenv, nvm,
Maven's `~/.m2` and the like. Every Codex turn's `CARGO_HOME` is `.cargo` in the
attempt's own scratch directory. A worktree step's turn, and an exec atom's command in a run, is also pointed at
its run's own build caches, removed when the run ends, finished or failed, with
a Cargo target directory for each worktree and each exec step, and at your rustup
toolchains, read-only, through `RUSTUP_HOME` (see
[worktree steps](workflow-files.md#atom-steps)). Otherwise, as for a Codex atom
outside a worktree or an exec atom in an eval, an atom names what it needs in
its `env`, as in `env: { RUSTUP_HOME: '/home/me/.rustup' }`. An
exec atom's command gets its own scratch directory as `HOME` and `TMPDIR`.
Scratch directories live in the atomic home's `scratch/`, one directory per
project, where no sandboxed turn may write; atomic refuses to run when that
would be under `/tmp`, `$TMPDIR` or the project. The scratch directories an
older atomic kept in a project's state directory are denied to every turn too,
until atomic next starts in the project and removes them: what attempts of
processes that no longer run left there goes then, and a directory that still
holds a live attempt, as of an older atomic still running, stays, and stays
denied, until a later start. A turn's shell always runs on the machine its backend
names; `CODEX_EXEC_SERVER_URL` and an `environments.toml` in Codex's home have
no effect.

`atomic run` and `atomic serve` do not read public context settings yet, so they
refuse a step whose atom declares a context.

On Linux, atomic has verified that no atom's commands can reach the gate store
or the credential: a Codex turn's commands and an exec atom's alike run under
Codex's sandbox, which denies them the atomic home's `private/` directory, where
every project keeps its database, its execution lock and its context
checkpoints, and every project's
Codex records; the credential and key directories; the default atomic home's
data and configuration when `ATOMIC_HOME` points elsewhere; your own atomic
configuration directory; the places in your home directory where credentials,
shell startup files and history are kept (under both `$HOME` and the home
directory the system records for your user, and wherever the variables that
move them, such as `XDG_CONFIG_HOME`, `GNUPGHOME`, `CARGO_HOME` or
`KUBECONFIG`, put them); workload credentials (the files AWS's, Azure's and
Google's credential variables name, the token files, certificates and keys
Google's credentials and certificate configurations and AWS's profiles name,
with the whole volume when such a file, or what a link to it leads to, is in
a Kubernetes projected volume, so a rotated token stays denied, and
`/var/run/secrets`, `/run/secrets` and systemd's `/run/credentials` whole); and what a link among them points at, up to four
directory levels inside a denied directory (except a device, anything under
the system's program, library and device directories, and, for a link found
inside a credential place, a program or a directory someone else owns);
the kernel keyrings; the network, unless an exec atom declares the hosts it
reaches, and then all but those, which it reaches through atomic's proxy, at
most 64 connections at once unless `exec.connections_per_command` says
otherwise (see [network access](atom-files.md#network-access)); and
sockets. Cargo's own configuration is
not among them, so a registry token kept there rather than in
`credentials.toml` is not protected. A command does not start when a denial
would cover its working directory or a directory it may write, apart from its
own scratch directory, which a denied scratch root covers and which is opened
to it again, or when a
denied place or a linked secret lies two or more levels inside a directory it
may write, or when a denied place is itself a link inside a directory it may
write, which a command could remove to read what it points at; a place that
has become such a link since the list was built stops the next command too.
What Codex records of a project's threads, its state databases and
its logs, is kept with the project's private state, in the atomic home's
`private/`, which every project's commands are denied; its Codex home keeps
no thread records, and its helper links are the only part a turn's shell
reads. Records an older atomic left in a
project's Codex home move there when atomic next starts in that project, so
until then another project's commands can still read them. atomic makes the list when a
turn or an exec step starts: a place on it that appears while a command runs
is denied only to the commands that start after it, and so is what a link
made after the list inside a denied place points at: each command looks for
such links again as it starts, and one made while a command runs is followed
from the next. Credentials kept
anywhere else are not covered. An
exec atom's command may write only its scratch directory and its run's own
build caches, which no other run's command may write, though one can still read them by their path, and which go when the run ends, and only read the artifacts it is given there; `--runner process` confines it the same way. It runs in a process
namespace of its own, so it can neither see nor signal atomic, its runner or
any other process outside, and in a session of its own, with no controlling
terminal and a `/dev` and `/proc` of its own, so it cannot type into, read or
draw on the terminal atomic runs in, whether through `/dev/tty`, the
terminal's path in `/dev/pts` or another process's descriptors. A step that runs commands
needs bubblewrap and unprivileged user namespaces there; a bubblewrap older
than 0.9, without `--argv0`, as Ubuntu 22.04 ships, works too, since Codex
runs the sandbox helper through a link atomic keeps in its own state, by its
path, which no command can write. Where the sandbox
cannot start, `atomic run` and `atomic runs resume` stop, saying what it needs,
before the step runs and without recording an outcome, and the run continues
when resumed where the sandbox can start. `atomic serve` instead sets that run
aside, unfinished, says so, and goes on serving the others; it tries the
sandbox again, first after a few seconds and then at most a minute apart, and
the run goes on once the sandbox starts. A sandbox that fails in a way that may pass, such as
on a full disk, makes the step wait, as a backend with no room does. Atoms that run no commands run as usual. When atomic
stops with commands still running, on Linux it ends every process they
started, inside their sandboxes too, before it exits and frees the project's
execution lock. On macOS it kills each command's process group and does not wait, so a
process that left its group may outlive atomic and the lock. Standalone exec
commands run unconfined on macOS. On Windows, atomic refuses standalone exec
commands. Both platforms refuse credentialed gate decisions.

An exec atom's command hands large results over by reference: it writes a
file to its outputs directory and names it in its output, and the next step's
command reads it from its inputs directory, a read-only copy (see
[atom files](atom-files.md#large-results-go-by-reference)). A file may be 1 GiB
and a step's files 4 GiB, unless `ATOMIC_EXEC_MAX_FILE_BYTES` and
`ATOMIC_EXEC_MAX_STEP_BYTES` say otherwise. Only exec atoms hand files over;
a Codex atom's turn does not yet.

A `workspace-write` atom cannot write `/tmp`, `/var/tmp` or `$TMPDIR` as atomic
found them: its commands' `TMPDIR` is the turn's own one above. A tool
that ignores `TMPDIR` and writes to `/tmp` directly, such as a JVM's default
`java.io.tmpdir`, fails there; point it at `$TMPDIR` (for the JVM,
`-Djava.io.tmpdir=$TMPDIR`).

Several authoring aids arrive in stages. Eval cases, in YAML and in JSONL files,
are loaded and checked, and the eval runner reports pass rates against each
atom's threshold and scorer means against their minimums and a baseline; the
`atomic eval` command runs it from the terminal, from cassettes or `--live`, and
fails CI below an atom's threshold. It has no judge to run scorers with yet, so an
atom with scorers fails unless `--allow-unscored` is given, and `--baseline` is
refused. Its live calls are metered in the usage ledger under each atom's budget.
Until fixture worktrees exist it refuses an atom that could write: a Codex atom
with a `workspace-write` sandbox, and an exec atom that does not declare
`effects: { external: 'read-only' }`, whether it declares `idempotent` or
nothing. It runs Codex atoms only live, with `--no-cassettes`. It lists what it
refuses, runs the rest, and fails unless `--skip-refused` is given. A
recorded workflow-file run can be replayed from its checkpoints as a new run:
each atom step is served the result it published, with no model call and no
command run, while gates open again, titled as a replay's, and wait. A replay
never runs an atom live: a step with no published result, or whose input differs
from the one it was recorded with, fails it, and a run recorded under another
layout, or whose results make writes, is refused. The served results do not take
over the recorded run's model threads. A step that maps a gate's `decision` cannot
be replayed past that gate: the new decision's time, decider and comment differ
from the recorded one's, so the step's input does too. The `atomic runs replay` command that
starts one is not built yet. Compact schemas, variants, the `--plan` cost preview, declared effect caps
and the staged `--effects preview` mode are planned for v0.5. The `file` tag
follows with code context in v0.7. The committed `atomic.policy.lock`, and
`atomic import` and `atomic export`, are planned for v1.0.

The v0.2 host controls proof, criteria checkpoints and graph lane model choices
change the durable pipeline layout from `atomic.pipeline.7` to
`atomic.pipeline.10`. An unfinished run recorded under `.7` or the staged `.8`
or `.9` layouts is not resumed or replayed by a `.10` binary. Finish it with
the matching older binary, or inspect its completed effects and start a new run
under `.10`.

Mermaid diagrams written by hand in docs need nothing from Atomic. The
[diagram exporters](../guides/diagrams.md) for workflows, the work graph and
runs, their `--view risk` and `--view cost` views and the stale-diagram check are
planned for v0.5; literal `mermaid` fences in atom instructions are built. Rendering in the web
console follows at v1.0.

[Recorded decisions](manifest.md#recorded-decisions) in the manifest and the
`atomic stale` command that evaluates them are planned for v0.7.

Running an atom by the path to its file, and passing input with `--data` or on
standard input, are part of the v0.1 `atomic run`. `atomic curl` arrives with
the HTTP API in v0.5. [Code on the Deno runtime](../guides/scripts-and-cloud-sdks.md) is planned
for v0.7: separate SDK scripts, `.atom.ts` code atoms and
`exec.script`, with `env` and `runtime.deno`, the manifest's `runtime.deno`
settings, named credentials, `atomic test`, and the Deno that Atomic downloads
and manages for them. A plain
`exec.command` is v0.1.
TypeScript workflows follow in v1.0.

Budgets are partly built: the `budget` field on atoms and workflows, its
configured defaults and price table, and the durable usage ledger that reserves
each model call before it is dispatched, and each exec atom's command, by its
active time, before it starts, settles each once, and parks a run that reaches
a cap. A released park admits the call again against the caps as stored.
`atomic parks list` and `atomic parks release` let the local operator, with
their enrolled credential, raise a cap and release a park, recorded with who did
it. Each park is recorded with the project whose run parked, and on a
PostgreSQL store several projects share, both commands see only the current
project's parks: releasing another project's park is refused as a park that
does not exist. A project is told apart there by its manifest's name (its
directory's name, with no manifest) together with an id that comes from its
path. Two projects are the same project only when both match, so a project
renamed, or checked out at another path, is another project, whose parks the
old one's commands no longer list or release. Two different projects that
share both a name and a path, such as the same `/app` in two containers, are
one project there: give them different names. A manifest that is there but
cannot be read or is not valid stops these commands, rather than have them
use another key: so does an `atomic.yaml` that is a directory, a link to
itself, to nothing or through a file, or one atomic may not look at, which
is not taken for no manifest. A local SQLite store is the project's own, so a park recorded
there before atomic kept parks' projects, or under an older name of the
project's at the same path, becomes the project's when atomic opens the store:
on one, a rename strands no parked run. On a shared PostgreSQL store
such a park is no project's: no atomic command lists or releases it, its run
stays parked with its record kept in the store, and the work goes on only as a
new run, started again with `atomic run`. Stop every atomic process that uses a
shared store before upgrading it, so that none goes on recording parks without
their project.
Runs and their gates are kept apart the same way. Each run is recorded with its
project as it starts, and on a shared store `atomic gates list`, `approve`,
`reject` and `answer` see only the current project's runs' gates, and `atomic
runs show` and `atomic runs resume` only its runs: another project's gate or
run is refused as one that does not exist. Run ids stay unique across the whole
store, so `atomic run` refuses an id another project has taken, saying only
that it is taken. On a local SQLite store, runs are adopted as parks are: one
recorded before atomic kept runs' projects, or under an older name of the
project's, becomes the project's, so a rename strands no run at its gate. A run
started on a shared store before atomic kept runs' projects is no project's:
none of these commands shows it or its gates, so it cannot get past a gate it
waits at; start it again as a new run with `atomic run`. A broken manifest stops
these commands too, and leaves the runs and gates where they are until it is
mended. A gate decision still comes only from a person's enrolled credential.
The caps and prices in the person's own configuration are ceilings: a project's
files, a variable and a written `budget` block may only bring a cap down to them.
The commands that show budgets and spend come with run inspection.

The v0.2 Windows AMD64 release job exercises the extracted CLI with a scripted
Codex provider. Its acceptance test requires a write in the fixture worktree
and a denied read from private Atomic state. Standalone exec commands on Windows
return an unsupported-confinement error.

## Observability stages

The local baseline is saved run inspection and basic spans in v0.1 (M12).
Full OpenTelemetry correlation/export and the optional `atomic dev --otel` local
viewer helper belong to M44, assigned to v0.5.
Datadog and other compatible backends remain optional. See [observability](../operations/observability.md).

## Read examples by stage

Start with [the first atom](../getting-started/first-atom.md) and
[first workflow](../getting-started/first-workflow.md) for the v0.1 target. The
[CLI quickstart](../getting-started/quickstart.md) describes the interactive target.
Each larger project in the [example catalog](../examples/README.md) identifies
its intended stage and any unresolved integration contracts.

## What has been checked in this repository

The existing example catalog includes offline schema/fixture checks and executable
Python transforms. One example's TypeScript also runs today under Deno, against
a local stand-in for S3. None of this establishes live-model quality, command compatibility
or crash recovery in an Atomic runtime. Those need the implementation and phase
acceptance tests.

The release workflow builds, packages and Sigstore-signs the Linux and macOS
archives with their SBOMs ([verifying a download](../getting-started/installation.md#verifying-a-download)),
but no release has been published yet.

Installation packaging, standalone-session behavior and some control commands
remain open decisions, tracked with the rest of the work in the project's issue
tracker. No feature in this table should be read as already shipped.
