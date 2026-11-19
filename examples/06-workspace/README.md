# Shared contracts in a monorepo · v0.5 target

[The root manifest](atomic.yaml) lists `shared` and `services/*` members. The
[billing member](services/billing/atomic.yaml) contains a lite atom whose output
sidecar references the shared schema's absolute `$id`. The schema identifier is
a registry key, not a URL to fetch. No registry host or network is required.

The atom does not copy the shared contract, repeat collection globs, override
its generated directory or add a package dependency for a workspace member.
The default kind must be overridden to lite because no coding harness is needed.

From `services/billing/`, or naming it from anywhere:

```sh
atomic check                      # works today
atomic eval assess-change         # later
atomic run assess-change --input fixtures/assess-change/tax-rounding.input.json   # later
```

`atomic check` finds the billing manifest, then the workspace above it that
lists `services/*`, and loads the schemas the other members share before
resolving the member's output contract, offline. The member's own schemas win
over a shared one with the same ID; two members sharing an ID is an error.
Schema sharing alone does not grant access to another team's graph, secrets or
artifacts, and atoms are not shared between members yet.
