# Typed A2A translation · v1.0

[The atom](atoms/translate.atom.astro) delegates a translation through a pinned and
approved remote Agent Card, validates the typed result and checks that the target
language is unchanged. Its name, version and schema locations follow convention.
It overrides kind and bounds the remote wait to five minutes.

The `.invalid` URL is a deliberate placeholder. `translator-a2a-token` is a
secret reference; no credential belongs in this directory. To deploy, replace
the card URL, pin the verified card and skill through the operator's configuration,
configure outbound policy and supply the secret through the control plane. The
card-pinning configuration is not yet specified in the plan.

The greeting fixture is a mocked contract pair, not a live remote-agent result.
The local checker sends no request and uses no secret. A real A2A run must retain
the remote task ID and reconcile an uncertain send before repeating it. Do not
assume an A2A message ID alone guarantees server-side deduplication.

External-origin remote outputs remain tagged external. Consumers need suitable
trust policy. The example is for approved public text; a remote handoff is not a
way to bypass local-only sensitivity rules.
