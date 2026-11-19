# Typed public execution context

[context-reply](atoms/context-reply.atom.astro) reads generated readonly profile,
configuration, environment and dispatch metadata. Its typed validator uses the
configured reply-length limit. The full context schema is a sidecar, alongside
input/output schemas.

`context.settings.json` declares base configuration, explicit profile patches and
an allowlist of public environment names. `context.fixture.json` is synthetic host
input for the offline catalog checker; undeclared env values never enter a prompt.
Secrets belong to the credential channel.

From this directory, with the built Atomic binary on PATH:

```sh
atomic check
SUPPORT_REGION=eu-west atomic render atoms/context-reply.atom.astro \
  --input fixtures/context-reply/ordinary.input.json \
  --context-settings context.settings.json --profile test --run-id context-example-1
```

The checkpoint lives in the external project state directory. Repeating the run ID
restores its original public context without reading settings or environment;
`--step-id review` binds a new trusted dispatch step. Use another run ID to choose
new settings. A changed compiled source/schema version is refused on restore.

This exercises production direct invocation. Inside a workflow the same atom's
context is resolved once per run and step and recorded by the durable runtime,
not the file store, so a step retried after a crash renders the context it
recorded even if settings or the environment changed.
