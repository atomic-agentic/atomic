# Run locally or with a team

Begin locally. Atomic's basic workflow runtime is designed to use local storage
without a service deployment, and the full interactive client can start its local
runtime when needed.

See [availability](../reference/availability.md) for the implementation stages.

## Local work

| Entry point | Intended use |
| --- | --- |
| `atomic` · v0.5 | Interactive coding and live supervision |
| `atomic run <name> --input <file.json>` | A named reusable task or workflow |
| `atomic runs resume <run-id>` · v0.1 | Recover an existing unfinished local run in the foreground |
| `atomic serve` · v0.1 local | Run a headless runtime; a supervisor keeps it independent of your terminal |

Source files stay in your project. Local databases, transcripts, artifacts and
attempt worktrees belong in the platform's user-data directories, separate from
the application checkout.

A durable workflow survives loss of the process through saved state and recovery.
It does not keep executing while no runtime process is running. For unattended
progress or long waits, keep `atomic serve` running under your usual supervisor.

## Inspect local work before adding a backend

`atomic runs show <run-id>` is the planned built-in inspection path for steps,
outputs, usage, failures and human actions. It needs no observability service.
Atomic also includes basic tracing initially and a fuller OpenTelemetry integration
later. The optional `atomic dev --otel` helper is intended to bring up a local
Jaeger or Langfuse setup; it is assigned to the later observability milestone.

For team telemetry, export OTLP through a Collector to Datadog or other compatible
backends. Export and content capture are separate choices; prompt/completion/tool
content stays out of telemetry by default. See
[local observability and telemetry](observability.md).

## A client and a runtime have different lifetimes

In v0.1, `atomic run`, `atomic runs resume` and `atomic serve` are foreground
processes. Closing their terminal can terminate execution. Saved work remains;
recover it with `atomic runs resume <run-id>` or restart local `atomic serve`.
Gate deadlines continue while offline. There is no automatic detach or daemon.

In v0.5, a client attached to an existing service can close while that service
continues. Background handoff for an all-in-one interactive session remains
unspecified; use a separately supervised runtime for unattended work.

In a team environment, keep the runtime separate from individual terminal sessions.
The durable run is the unit you recover and inspect; a live thread can be recreated.

## A team on one server

The team stage adds Postgres, identity, audit records, transport protection and
shared approvals. Configure `ATOMIC_DATABASE_URL` from the deployment's secret
source, then run `atomic serve`. Keep Atomic's state in its own database/schema
and role, separate from application data sources.

Give each `atomic serve` process an executor identity with `ATOMIC_EXECUTOR_ID`:
a name unique to that process that stays the same when it restarts, such as a
host or StatefulSet member name, never a random container or pod name. On
PostgreSQL `atomic serve` refuses to start without one. A restarted process
recovers the unfinished runs it owns and leaves other processes' runs alone.
Atomic takes the identity only from its own setting and refuses to start when
the durable engine's own variables (`DBOS__VMID`, `DBOS__APPVERSION`) are set.

The execution lock that keeps two processes from running the same project's
runs covers one host. On PostgreSQL, `atomic runs resume` therefore refuses a
run that another executor owns, which may still be running it. Once that
executor has stopped for good, pass `--owner-stopped` to take the run over;
passing it while the owner still runs lets two processes run the same steps.

Atomic currently plans API keys, approved enterprise providers or workload identity
as its defaults for unattended model access. Eligible documented account-auth
automation, including Enterprise token paths, requires explicit deployment policy
and supported identity/credential handling; it is not enabled implicitly by a
local login. See
[subscriptions and authentication](../getting-started/authentication.md).

Remote clients attach through the configured, authenticated endpoint:

```sh
atomic --remote <url>
```

Exact identity and deployment setup will accompany the team release. The CLI
quickstart does not require this server configuration.

## Remote execution later

The control plane coordinates runs and policy. Runners execute the permitted shell
and filesystem work in disposable environments. Separate workers, Kubernetes and
Agent Substrate are later backends; they do not change the basic atom/workflow
contract.

Shared queues require ownership and recovery controls. Several processes sharing
Postgres is a platform feature, not a shortcut to enable by copying the local
server command onto several hosts.

Backups, retention, encryption, deletion and rollout compatibility need explicit
operating policies before production deployment; they are tracked as later
milestones.
