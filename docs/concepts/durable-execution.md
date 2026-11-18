# Durable execution

Atomic includes durable execution in its workflow runtime. A workflow saves the
result of each completed durable step. After a process restart, it can recover
the run and use those saved results rather than repeating completed steps.

On a laptop, this is designed to work with local storage in one executable. You
do not need a separate workflow cluster to use the basic model.

## Think in steps

Consider a workflow that plans a change, waits for approval, implements it and
reviews the candidate.

```mermaid
flowchart LR
  plan["Plan<br/>save result"] --> gate["Wait for approval<br/>save decision"]
  gate --> implement["Implement<br/>save candidate"]
  implement --> review["Review<br/>save result"]
```

If Atomic restarts during review, the saved plan and implementation are available.
The interrupted review may run again. If it restarts while waiting at the gate,
the gate remains waiting and can be answered later.

| Event | Recovery behavior |
| --- | --- |
| A step completed and its checkpoint committed | Return the recorded result |
| A step was interrupted before its checkpoint | Retry it when its policy permits; it may spend tokens again |
| A workflow was waiting for a durable approval | Preserve the pending gate or its recorded decision |
| A person was answering an in-turn question or command approval | The live turn can be lost; the question may need to be asked again |
| An external mutation may have succeeded but its response was lost | Reconcile or park the operation instead of blindly sending it again |

## Durable does not mean every effect happens exactly once

Model calls can repeat for an interrupted step. Arbitrary shell commands can
change systems Atomic does not control. An approval authorizes an action; it
does not prevent that action from being repeated after a crash.

Coding workflow attempts use isolated worktrees based on immutable commits, so a
retry starts from the intended base. For external mutations, Atomic's planned
effect ledger records intent and outcome, uses supported idempotency keys and
parks uncertain operations when safe recovery cannot be established.

When a run is parked as `outcome_unknown`, inspect the external system before
resolving it. For an unkeyed mutation, a negative lookup is insufficient: an old
dispatcher might still send after recovery. Resending requires authoritative proof
that earlier senders can no longer transmit and earlier requests have settled;
otherwise the operation stays parked. Provider idempotency keys also have scope
and retention limits. Starting a new run is not a substitute for resolving it.

The initial mutation contract uses one saved proposal and one deterministic effect
step per logical operation. Intentional identical requests have distinct identities;
retries reuse an identity and cannot change its bound arguments.

An atom's saved candidate and proposed graph edits stay private until checks and
required review accept them. A separate short transaction publishes the accepted
result and whole graph batch. Rejected candidates never close items or publish
follow-ups. Claim ownership and budget/effect journals are separate durable control
records. Recovery between candidate persistence and publication reuses the candidate
without another author model call.

## Keep inputs stable

A run pins its workflow, atom definitions and relevant artifacts. Editing a source
file should affect new runs, not silently change the meaning of an existing run.
Use durable artifacts and immutable commit references between steps rather than
relying on whatever happens to remain in a workspace.

## Resume existing work

In the **v0.1 target**, return to the same project and state store:

```sh
atomic runs show <run-id>
atomic runs resume <run-id>
```

`resume` continues that unfinished run in the foreground using its saved inputs,
pinned definitions and completed checkpoints. It preserves the run ID, cumulative
budget usage, effect records and gate deadline. An interrupted, uncheckpointed
step may retry when policy permits. If another local runtime owns the project,
the command reports that owner rather than starting a competing executor.

Closing a foreground terminal can terminate its runtime. There is no implicit
background handoff: no work executes while the process is stopped. A pending gate
stays saved, but its wall-clock timeout continues. On recovery, an expired gate
applies its timeout policy before downstream work can run. Terminal closure does
not approve or reject the gate.

Alternatively, run `atomic serve` in v0.1 to keep a local headless runtime
recovering eligible unfinished runs. Keep it running under a supervisor for
unattended progress, and use gate commands from another terminal.

Resume cannot bypass a rejected or expired gate, revive a terminal failure or
cancellation, reset a budget, or resolve `outcome_unknown`. Those require their
own authorized resolution. A new `atomic run` starts new work; a recording replay
is a separate inspection/testing operation.

The implementation and its crash-test suite are still planned. See
[availability](../reference/availability.md) for the staged guarantees and
[operations](../operations/running-atomic.md) for local and team operation.

Next: [your first workflow](../getting-started/first-workflow.md) or
[control and approvals](../guides/controlling-agents.md).
