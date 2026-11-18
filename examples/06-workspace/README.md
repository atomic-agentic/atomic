# Shared contracts in a monorepo · v0.5 target

[The root manifest](atomic.yaml) lists `shared` and `services/*` members. The
[billing member](services/billing/atomic.yaml) contains a lite atom whose output
sidecar references the shared schema's absolute `$id`. The schema identifier is
a registry key, not a URL to fetch. No registry host or network is required.

The atom does not copy the shared contract, repeat collection globs, override
its generated directory or add a package dependency for a workspace member.
The default kind must be overridden to lite because no coding harness is needed.

Future commands, from `services/billing/`:

```sh
atomic check
atomic eval assess-change
atomic run assess-change --input fixtures/assess-change/tax-rounding.input.json
```

Discovery must load the parent workspace and its shared schema before resolving
the member's output contract. The catalog checker does so offline; runtime
workspace discovery/shared-schema resolution are assigned to M11b/M39b in v0.5.
Schema sharing alone does not grant
access to another team's graph, secrets or artifacts.
