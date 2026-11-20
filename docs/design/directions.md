# Directions

Put together, durable checkpoints, typed atoms, a work graph, gates and evals
replayed from cassettes make some things possible that no one of them offers
by itself. This page collects those directions. The scheduled ones belong to
milestones; the candidates are ideas only, to be weighed when a phase is
planned, and nothing here commits to them. [Milestones](milestones.md) holds
the acceptance tests, and [availability](../reference/availability.md) says
what is built.

## Scheduled

### Earned autonomy (M55)

The tier of an atom's gate will follow how well the atom has done. People write
the policy. One might say that after a given number of approvals in a row with
no revert, AI review alone may approve that atom's changes, and that a single
revert or incident traced back to the atom returns it to human review. Each
move up or down goes into the audit log with its evidence. The policy is also
kept as a recorded decision, so `atomic stale` flags a tier whose history has
stopped backing it.

### Outcome attribution (M55)

What happens after a merge, whether a revert, a bug report, an incident or a
failed deploy, will be linked back to the run that made the change, with its
atom version, model and prompt hash. Each commit an atom makes is already tied
to a durable run; matching on the commit link, a commit trailer and blob hashes
keeps the tie through rebases and squashes. The result is a measure of quality
for each atom and each model, which evals, routing and earned autonomy can use.

### Sensitivity-based routing (M56)

Taint tracking per field and data classification will put labels on content
(M14, M51). The model layer is designed to read those labels before each request
to a provider and pick a model accordingly: content labelled or detected as
sensitive goes only to providers cleared for it, and each choice is audited. See
[sensitivity routing](model-layer.md#sensitivity-routing).

## Candidates

- **Speculative execution past gates.** During a person's review of a gate,
  atomic could already try the step most likely to come next, in a worktree it
  can throw away. Approval then finds the work done; rejection deletes it. This
  is only safe because the [side-effect
  contract](durability.md#side-effect-contract) keeps external effects in steps
  of their own, so speculative work can be discarded.
- **Learned model cascades.** Every item begins on a small local model. A
  decision atom rates how confident that model is and hands the item up a tier
  when confidence is low, and from its own history atomic works out which model
  tends to succeed on which sort of item.
- **Shadow runs and canary atoms.** A new version of an atom runs alongside the
  current one on real work, with its effects stubbed out. Their outputs, cost
  and pass rates are compared, and a gate decides whether the new version takes
  over. Cassettes, forked replays and `eval --compare` are the parts this would
  build on.
- **Signed provenance for agent-written code.** Attach to every commit an atom
  makes a signed, in-toto or SLSA style statement listing its model, its prompt
  hash, the hashes of its inputs, which tests ran, what the gates decided and who
  approved.
- **Estimates from run history.** The planner estimates an epic's cost, time and
  chance of success per tier from earlier runs, so agent work can be approved
  like any other spending. The `--plan` preview is the simple form of this,
  working only from prompt sizes and price tables.
- **A run debugger.** Breakpoints with conditions, such as stopping as soon as
  any atom reports a blocker, then inspecting and editing state and carrying on
  from there, all on top of durable checkpoints.
- **A conflict-predicting merge queue.** Guess from an item's plan and the
  repository map which files it will change, and schedule parallel atoms so that
  those sets do not overlap.
- **Fine-tuning a local model on approved runs.** Runs that passed their gates
  are typed, labelled examples that could adapt a local model to one codebase,
  but only where the provider's terms allow outputs to be used this way.
- **Contract tests between agents.** For an A2A link, each party publishes a
  typed contract and CI checks both against it, so a change in a partner's agent
  cannot break a workflow unnoticed.
