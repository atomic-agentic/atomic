# Risks and open questions

Each risk below is a technical one that the design has to absorb, listed with
what atomic does about it. The biggest is that Codex's internal crates move
fast: they are all versioned `0.0.0`, and nothing in them is promised to stay
stable.

## Risks

### Foundations

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Codex crate APIs shift with every bump | The workspace no longer compiles | One pinned revision; only `atomic-core` imports Codex; bumps happen on a schedule and must pass the current phase's milestone tests |
| durare is a young project | Durability stalls, or atomic inherits bugs it has to fix | The M0 spike exercises recovery, `recv`, queues and transaction steps before anything depends on them; all access goes through the `DurableRuntime` trait, behind which a Postgres-only engine could replace it |
| atomic's durare fork falls behind upstream | Fixes are missed and rebases hurt | The sqlx 0.9 port stays small, is offered upstream, and sits behind the trait |
| Codex moves or re-patches sqlx | The durare fork stops sharing Codex's SQLite stack | Move both pins together, and build the fork against each candidate Codex revision before bumping |
| The copied `[patch.crates-io]` block diverges | Builds or behaviour differ in ways that are hard to see | `scripts/codex-pin.sh check` regenerates the expected block and fails CI on any difference |
| Everything shares one process | A turn that aborts takes every workflow down with it | Each atom runs in its own tokio task, durare resumes unfinished runs on restart, and an outside supervisor restarts the process |
| The SQLite and Postgres stores behave differently | Bugs show up only in team deployments | Each store has one trait and one conformance suite, run against both backends in CI |
| It is not yet clear how durare identifies executors across workers | Two workers recover one run and repeat its effects | Settle this before M19, adding executor IDs to the fork if needed; ownership epochs fence a stale owner regardless |
| Workflow code changes under runs that are still going | A replay no longer matches its history | Runs carry an application version, and old runs finish on the binary that started them (see [upgrades](operating.md#upgrades-and-format-stability)) |

### Work graph and messages

| Risk | Impact | Mitigation |
| --- | --- | --- |
| The graph grows toward everything beads does | Time goes into commands the work loop never calls | The target is beads' portable core; integrations and UI are out of scope |
| Claims on different machines are optimistic | Two machines work one item until they next sync | A project that needs exclusivity names one node as lease authority; the step that loses is cancelled |
| Messages crowd the graph | `ready` slows and atoms see noisy context | Chatter is ephemeral with a TTL and is compacted; message items never appear in `ready` |
| beads changes its JSONL format | Import and export stop working | The importer is versioned and tested differentially against a pinned beads release |

### CLI and people in the loop

| Risk | Impact | Mitigation |
| --- | --- | --- |
| One app-server instance might be unable to handle its in-process client and a socket listener at the same time | Clients cannot attach to atom threads | M9 tests this first; the fallback is for atomic to talk to its own server over the socket, as any client would |
| A TUI fork would have to track a large, fast-moving crate | Rebasing eats time | The first CLI release uses the stock TUI and needs no fork; the extended TUI keeps atomic's panes in their own modules, reached through two hook points |
| Experimental app-server methods change | Live updates or approval routing break after a bump | Clients can fall back to polling the admin tools, and the tests cover both paths |
| An in-turn approval lives only in memory | After a restart, someone answers a prompt that is gone | Any decision that must outlast a restart is a durable gate between steps; in-turn prompts are kept for sandbox escalations |
| A chat approval is forged or credited to the wrong person | The wrong person decides a gate | Platform signatures are verified, chat users are mapped to SSO identities, email links work once, and every decision is audited |

### Code, packages and interop

| Risk | Impact | Mitigation |
| --- | --- | --- |
| v0.7 adds a Deno that atomic downloads and manages for code atoms and scripts (atomic embeds the native Astro compiler and a V8 helper runtime) | A first-use download that some networks block, and an executable fetched at run time | The managed Deno will be one version per release, checked against a hash built into atomic, installable offline from an archive, never fetched by a server at run time, and never fetched by a project with no atom code; managed requirements can forbid the download or point it at a mirror |
| TypeScript workflow code that is not deterministic | A replay takes a different path | Workflow code gets no permissions, a seeded random source and a recorded clock; `atomic check` flags an `await` outside a context call |
| Packs turn into a second supply chain | A pack ships a prompt that leaks context, or a new version reaches further | Packs are hash-pinned in the lock; first-party packs are signed; other packs are declarative only unless pinned at `team`; every permission lands in the policy lock, and `atomic update` shows prompt and permission diffs |
| Atom code may import any npm package | A compromised dependency acts with the atom's credentials | The lockfile is frozen and pins each version exactly; packages never run install scripts; a release must reach a minimum age before it can be locked; the network reaches only declared hosts; and code loads only from trusted origins |
| Publishing over MCP and A2A exposes more entry points | Outside callers start expensive or risky work | Nothing is published unless chosen; each entry point has its own auth; inbound calls are tagged external and still meet budgets and gates |
| Rust A2A libraries are immature | Gaps in features, or wire-format bugs | Use the SDK's types first, test against the official A2A command-line client, and keep the transport layer thin |
| kagent's harness contract is internal and moves with kagent | A kagent upgrade breaks the bridge | The bridge stays a thin harness on kagent's own SDK, separate from atomic's A2A server, with a conformance test for each kagent release |

### Runners and Substrate

| Risk | Impact | Mitigation |
| --- | --- | --- |
| A suspend or a router restart cuts an exec-server session | A step fails partway through a command when only execution runs in the actor | Actors are suspended only between steps; a dropped session counts as a failed attempt that starts again from its base commit; harness mode is the alternative |
| Codex's sandbox and gVisor do not fit together | The runner refuses to start, or commands run without a sandbox | A runner template grants the capabilities Substrate permits and uses a Codex profile with networking on; failing that, the sandbox is turned off by managed requirement and the change in trust is written down |
| An asynchronous snapshot is lost | The workspace state of the latest step is gone | Commits are pushed before the checkpoint, durare remains the source of truth, and long waits use a synchronous suspend |
| A snapshot holds whatever was in the runner's memory | Sensitive data rests in object storage | The bucket uses customer-managed keys, the default placement keeps credentials and transcripts out of runners, and snapshots are retained only briefly |

### Data, security and models

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Data synced through git history is hard to erase | A regulated tenant's deletion request cannot be met | Such tenants use Postgres with git sync off, sensitive fields are encrypted under per-subject keys that can be destroyed, and nothing sensitive goes into git |
| A FIPS build has not been verified | Deployments that require FIPS cannot use atomic | Check the `aws-lc-rs` FIPS mode across the full dependency tree before offering such a build |
| Models from other providers do worse as Codex atoms | Fewer real provider choices than intended | M34 will publish a cross-provider eval report; lite atoms remain the path that works across providers |
| The claim that the work graph lifts small models has never been measured | The benchmark may find little or no effect | The M34a harness runs early on new long-horizon tasks and its result is published whatever it shows; nothing in the core design rests on the claim |
| The OpenTelemetry GenAI conventions are still moving | Dashboards and backend mappings break when they change | atomic pins one version, maps it in one module, and the Collector can rename older attributes during a transition |

## Open questions

- Should the sqlx 0.9 port go upstream into durare, or stay a fork for good?
  That depends on durare being willing to leave sqlx 0.8.
- Should graph sync use one git ref per repository, or one per project that
  spans several repositories?
- Should ephemeral messages expire after one fixed default, or after a TTL set
  per scope?
- Should lite atoms call models through Codex's API crates or through a client
  of atomic's own?
- If the managed Deno is ever replaced by Deno linked into the binary, how
  would Deno's own SQLite and V8 requirements line up with the versions Codex
  and the helper isolates already pin?
