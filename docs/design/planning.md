# Planning, code context and verification

The [work graph](work-graph.md) pays off when work is cut into pieces a model
can finish, when each piece can locate the code it touches, and when each piece
can show that it is done. This page covers those three needs, and how an agent
that pursues a goal decides what to do next. Most of it belongs to v0.7 (M47,
M48). Goal items arrive with the graph in v0.5 (M5), and the triggers that wake
them with M16.

The intended shape is a capable model planning and reviewing at the gates, and
cheaper models doing the work in between.

## Planner

atomic will ship a `plan` step that breaks a goal into graph items. Every item
it creates carries acceptance criteria, its dependencies and a size limit chosen
for the tier that will execute it: for a 30B-class executor, one item has to fit
in one step. Nobody claims work from a plan until it clears an AI-reviewed gate,
plus a human gate where the workflow asks for one. The planner is evaluated
like any atom. Its evals judge the criteria it writes and, for goals, whether
it proposes something new instead of repeating finished work.

## Executable acceptance criteria

Criteria need not be prose. Besides text, a criterion may be a test command, a
CEL expression over what the step returned, or an HTTP probe. atomic will run
the executable ones before any reviewer looks at the result, so a gate stands
on objective checks first and on model judgment second. A step whose test fails
can never reach a reviewer marked as passing. Small authors gain the most from
this order.

People can also set a close policy that demands evidence before an item may
close, for example a commit that atomic checked on a branch on its own. Only a
person can set such a policy.

## Code context

On claim, atomic will attach two things to the item besides the usual brief: a
symbol map of the repository built with tree-sitter, and the files most likely
to matter for that item. Locating the right code is the hardest part for small
models, so the small-model benchmark (M34) runs with and without the map to
find out what it is worth (M48).

Authors can point at code themselves. A `file` construct places one symbol, or
a range of lines, from the step's worktree at its pinned commit into the user
turn. It arrives labelled as data and is capped in size. It shares the map's
extraction, so map and construct always agree on where a symbol starts and
ends. If the symbol has gone, the step fails and names the path and symbol.

For coding atoms, search and the repository map are expected to beat
embeddings; see [retrieval](data.md#retrieval).

## Planning modes

Because every atom states the type it accepts and the type it returns, a planner
can read those types as preconditions and effects.

- **Deterministic type planner.** An A\* search looks for a sequence of atoms
  that leads from what is known now to the type a goal asks for, and searches
  again after every step. It works beside the model planner rather than
  replacing it: the model divides work into items, while the type planner links
  atoms inside one item. The chosen sequence is checkpointed, so a replay makes
  the same choice, and small models never have to plan at all.
- **Three execution modes.** In *focused* mode a named workflow runs. In
  *closed* mode a trigger or message is classified, cheaply by a decision atom,
  and handed to one known workflow. In *open* mode the type planner assembles
  atoms from the library toward a goal. Open mode still runs nothing but
  declared atoms, and a goal-approval policy, which people write the way they
  write gate policy, bounds which goals may be pursued.
- **Utility selection.** Some events have no fixed goal; triaging a fresh issue
  is one. There a trigger can score each candidate atom and run whichever is
  worth most, so nobody has to write a workflow for every kind of event (M16).

## Stuck detection

atomic will mark a run as stuck when it keeps retrying one step, or when a set
number of steps goes by with no new progress in the graph. Rather than burn its
budget, a stuck run goes to a person. Repeats are recognised by comparing
failure fingerprints, which are the failing line with paths, numbers and hashes
removed. Small models fall into these loops often.

## Goals

A goal is written in `goals/<name>.goal.yaml`. Loading it creates a pinned graph
item of kind `goal`, which holds:

- a title, and optionally a **measure**: read-only tool calls by name, or CEL
  over their results, sampled once per wake and stored as a history;
- a **budget** for each period, the atoms and toolsets its tasks may draw on,
  its approvers, and its **triggers** under `wake:` (schedules, webhooks with
  dedupe keys, events), stored and fired the same way as any other trigger;
- a **feedback log**: dated entries from people, such as gate rejections,
  answers to question gates, steering text and plain notes;
- an **ignore list**: dedupe keys and candidate fingerprints the agent chose to
  leave alone, each expiring at a set time.

On every wake the `plan` step adds the goal's tasks as ordinary child items, so
`atomic graph ready` doubles as the goal's to-do list. The goal item itself is
never ready; only its children are. Each plan step receives a brief from the
context contributor: the measure, the newest feedback and ignore entries, and
the children already closed. A topic someone rejected therefore shapes what
gets planned next, not just the draft in front of them. Feedback recorded in
one run reaches the next run's brief, and an ignore entry stops applying once it
expires.

The `atomic goals add|list|show|feedback` commands manage goals. `show` reports
the measure, what is left of the period's budget, the latest wakes with their
outcome and reason, and the open children. `feedback` records a dated entry
from the local operator.

Goals reuse the graph instead of introducing a new store. The design already has
pinned items, parent and child edges, `due_at` and memories in the graph; gates
that reach people where they work and outlast a restart; and triggers that drop
duplicates. A `goal` kind, a measure, a question gate and a `consider` step
build on all of that and keep each atom shaped like a task, which is what lets
its tools and effects be declared up front. Should a goal's history grow too
large for one graph item, that choice will be reopened.

## Deciding not to act

Waking up need not create work. Before anything else, a goal's trigger will run
a `consider` step on the cheapest tier, giving it the goal's brief and the
event. The step answers `act` with its reason, or `no_action` with its reason
and, if it chooses, an entry for the ignore list.

`no_action` is a real, recorded result that changes nothing outside atomic. It
uses one unit of the goal's run budget, shows up in `atomic goals show` and in
the run list, and stores its ignore entry. A question nobody answered, or an
advisory with nothing new to say, therefore stays away at the next heartbeat.
When an event arrives with a dedupe key on a live ignore list, the scheduler
discards it before creating any run and keeps a record of having done so.

## Tool unfolding

A whole tool set, an MCP server or an OpenAPI toolset, can be offered to the
model as a single placeholder. Calling the placeholder reveals the tools it
stands for. Small models do best with few tools in view (M48). Large OpenAPI
specs stay small in a different way, by loading operations only when they are
needed; see [OpenAPI toolsets](schemas-and-apis.md#openapi-toolsets).
