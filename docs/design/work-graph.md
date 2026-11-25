# Work graph

atomic will carry its own graph of work items that knows about dependencies.
Atoms take an item that is ready, do it and close it with evidence; anything
they discover along the way becomes a new linked item rather than extra scope.
The model is taken from [beads](https://github.com/gastownhall/beads) (MIT) and
written again in Rust as the `atomic-graph` crate, with credit in the
third-party notices. The aim is beads' portable core of commands, not every
command beads has, and atomic never ships a binary called `bd`.

The graph is part of v0.5 (M5 to M7); syncing it between machines comes in v1.0
(M8). For the user's view, see [the work graph connects the terminal and
framework](../concepts/architecture.md#the-work-graph-connects-the-terminal-and-framework).

## Items and edges

- **IDs** are short hashes, extended with a suffix for children (`at-a1b2`,
  `at-a1b2.1`).
- **Statuses** are `open`, `in_progress`, `blocked`, `deferred`, `closed` and
  `pinned`.
- **Edges that decide readiness:** four kinds can hold an item back: `blocks`,
  `parent-child`, `conditional-blocks` and `waits-for`.
- **Links that never block:** `related`, `discovered-from`, `relates-to`,
  `duplicates`, `supersedes` and `replies-to`.
- **Time.** An item may have a `due_at`. It is ready once everything blocking it
  is closed and its due time, if any, has arrived. The trigger scheduler turns a
  due item into one event keyed by the item's ID (M16), so the item fires once
  even if the service was down when it fell due.
- **Memories** (`remember`) are short notes that a person wrote or approved.
  Old closed work can be compacted.

Readiness is computed by a recursive query over the edges that block. Whether
an item is blocked is also stored as a projection, which keeps the query
cheap.

## Claims and leases

Claiming is a single SQL transaction that only succeeds when nobody holds the
item or the holder's lease has run out. Within one store, then, two claimants
can never both win.

- **Each claim bumps the item's claim epoch.** Everything the claimant does
  afterwards (heartbeats, closing, filing follow-ups) is checked against that
  epoch, so someone whose lease lapsed cannot close an item another claimant
  has taken since.
- **Liveness comes from heartbeats,** not from a connection staying open.
- **A lapsed lease that no one has taken over still works** for the original
  holder, because nothing else has been written.
- **Lapsed items go back to the queue as part of the claim query,** with no
  separate timer. A claim can come back empty even though items remain, if
  other claimants have those rows locked at that moment; a caller with nothing
  to do simply tries again later.
- **Creating an item or a note carries a request ID,** so repeating the call is
  harmless.
- **The claim comes before the work.** Orchestration commits the claim together
  with its transaction-step checkpoint and then hands the atom an opaque token
  naming the item, the claimant, the epoch and the acquisition ID. A turn starts
  only for the claim that won. On recovery the same acquisition is reused and
  ownership is checked again; if someone else has taken the item over, the step
  parks instead of quietly claiming again.

Claiming, heartbeats and releasing are done by orchestration. Models never get
them as tools.

## Who wrote it

Every piece of stored text says where it came from: `human`, `approved`,
`agent` or `external`. Only text written or approved by a person is placed in a
prompt without being asked for. Agents cannot edit that text, and nothing
written by an agent or brought in by an import can promote itself to a higher
level (M14). Memories that agents wrote are returned only when requested, and
then as untrusted data. See [trust boundaries](security.md#trust-boundaries).

## Sync between machines

From v1.0 (M8), each write is recorded as an operation carrying a hybrid
logical clock stamp and the ID of the actor that made it. The operations travel
on their own git ref, `refs/atomic/graph`, written with `gix`, which Codex
already depends on. Merging is deterministic. For each field the write with the
higher stamp wins; notes and labels are combined as sets; a delete leaves a
tombstone. Property tests check that syncing the same operations in any order
ends in the same state.

Claims made on different machines are optimistic. Two replicas may each claim
an item before they have synced; the higher stamp wins, and the other atom's
step is cancelled. A project that cannot accept this names one node as the
authority for leases. With Postgres every worker sees the same claims anyway,
and git sync is only needed for offline replicas.

## Import and export

`atomic graph import` and `export` speak beads' JSONL format, so a beads project
can come into atomic or leave it. Importing one export twice leaves the graph
unchanged, and differential tests compare the `ready` results with those of a
pinned `bd` release.

## How workflows use the graph

| Graph concept | What atomic does with it |
| --- | --- |
| Ready and claim | A durable transaction step that runs before the atom; because the acquisition ID is stable, recovery gets back the same claim token |
| Lease and heartbeat | Heartbeats come from a durable timer, and the lease is set longer than the step's timeout |
| `blocks`, `parent-child` | Ordering: an epic turns into a workflow and its children into queued steps |
| `waits-for` | A fan-out gate, open until the children created at run time are all closed |
| `conditional-blocks` | Compensation that only runs when the blocking item fails |
| `gate` item | A read-only copy of a gate-store entry, so gates show up in readiness and blocking queries |
| `discovered-from` | An atom files follow-up work through a tool and keeps its own scope |
| Close with reason | The step's summary of its result, plus the commit SHA its worktree step produced |
| Handoff on release | When a step gives an item back or fails it, it writes down what it attempted, where it went wrong and what it found out; whoever claims the item next gets that note, with each file flagged as changed or untouched since |
| Memories | A context contributor adds a short brief to every turn: the item that was claimed, its criteria, the latest handoff and the approved memories. The brief has a fixed size, and each stored field is placed on its own labelled line so that stored text cannot pretend to be structure |

The M6 graph work loop starts with `atomic graph create --kind epic --pinned`
and runs a one-worktree-step workflow with
`atomic run <workflow> --graph-epic <id> --fixture <repo> --fixture-rev <rev>`.
The workflow's input contract
declares an optional `item` object, but the operator omits it: atomic selects a
ready direct child, including an imported Beads `acceptance_criteria` string
when present, validates the host-supplied item and mapped atom input,
then claims it in the same short transaction. Each child gets separately named
turn, review, publication and close checkpoints. Publication and close fence
the full lease token. The close transaction links the host-generated worktree
commit and refreshes blocked edges, so a newly unblocked child is selectable
immediately. A pinned child or a goal/message child is outside this work loop;
an ordinary blocked or deferred child keeps it pending. When all work children
are closed, the exclusive epic-to-run binding is marked done. The epic stays
pinned as its durable container, and a different run ID cannot silently work it
again. Pending work records a releasable park. After changing a blocker or due
date, the operator releases that park; the resumed run uses a fresh claim step
to select from the graph again.
Each graph reviewer and repair model turn renews and checks its own bounded
claim. Before a human review gate waits, a durable sibling step records an
agent-trust handoff, releases the claim and bumps its epoch. An approval is
followed by a durable re-claim step; if another worker took the item while the
gate waited, the reviewed candidate stays private and the original run parks.
The new lease fences publication and close. The three-item M6 pilot also runs
without review.

Atoms use the graph through typed tools: `graph_ready`, `graph_show`,
`graph_create`, `graph_close`, `graph_dep_add` and `graph_remember`. The read
tools show what has been committed. The write tools do not commit; they add to
the private candidate batch of the current attempt, and `graph_show` within
that attempt shows those proposals on top, marked as provisional. A batch that
is accepted is published by the
[publication transaction](durability.md#candidates-and-publication); one that
is rejected never reaches the graph. The CLI for people offers committed
operations separately, with its own authorization.

## Messages

atomic has no message board of its own. Whatever has to outlast an atom is
stored as a graph item:

| Need | How the graph does it |
| --- | --- |
| A message | An item whose type is `message` |
| Threads | `replies-to` edges |
| The subject | A `relates-to` edge pointing at the work item, which keeps review comments next to the work |
| Channels | A label, or a pinned item that serves as the thread's root |
| Chatter versus record | Chatter is flagged `ephemeral` with a TTL and compacted once it expires; decisions are kept |

Messages are never returned by `ready`. When sub-agents inside a single atom
talk to each other they use Codex's multi-agent messaging, which is not durable;
that is acceptable because a crash re-runs the whole atom anyway. Codex's own
message board is not used. Readers cannot see its ordering key, it is not part
of the graph's sync, and it would mean a fourth store and a second kind of
cursor.

Three tools handle messages: `graph_post`, `graph_read_thread` and
`graph_inbox`. When a message arrives, workflows subscribed to its scope are
woken by an in-process event, or by `LISTEN` and `NOTIFY` on Postgres. Missing a
wake-up does no harm, since readers always pull from their cursor.

## Reading messages without gaps or repeats

Messages are delivered from a local event stream that is only ever appended to.
Creating a message, each accepted edit and a deletion all add a new event that
never changes afterwards; a deletion is a tombstone. Sync drops events it has
already seen, by their stable ID, and numbers each new arrival in local order.
Clock stamps decide how messages are displayed, not the order they are
delivered in. On Postgres the numbers are handed out under a lock that follows
commit order, so a committed event can never sit above a lower number that has
not committed yet. Each cursor belongs to one stream and one scope, and a cursor
presented to a different stream is rejected.

A consumer makes each round out of three sibling steps:

1. **Select a batch.** A transaction step records references to immutable event
   versions, their content hashes and the highest sequence number included. It
   also pins those bytes until the consumer is done and for as long as the run
   must be recoverable. It never just stores item IDs to look up again later.
2. **Run the atom** on that recorded content, through the normal candidate and
   publication path.
3. **Advance the cursor** with a compare-and-set from the batch's lower bound to
   its upper bound, saving the result and releasing the pin in the same commit.

If the process dies after selecting, the same batch comes back on replay, and
anything that arrived in between is left for the next round. If it dies after
the atom, the saved result is reused for exactly the messages it covered. If it
dies after advancing, the next round begins beyond the bound. So no message is
missed, and none ends up in a recorded result twice. Editing or expiring a
message cannot alter a batch that was already selected; those changes show up
as new events later. When a deletion policy requires pinned content to be
erased early, the runs depending on it are invalidated or parked, never left to
summarize bytes that changed underneath them. Doing all three things in a
single step would put one step inside another, which the engine forbids, and
recovery could then combine an old result with a newer, larger read.
