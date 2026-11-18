# atomic

atomic is a Rust framework for small, typed, single-purpose agents, called
atoms. An atom is a typed function from an input to an output that runs as one
model turn and returns structured output that fits its JSON Schema. Workflows
compose atoms, and they are durable: a workflow survives a crash without
redoing finished work, and a person signs off at durable gates.

atomic builds on OpenAI's open-source [Codex](https://github.com/openai/codex)
crates for running agents, and gets durable execution from a fork of
[durare](https://github.com/atomic-agentic/durare).

**Status: early development.** Most of what the documentation describes is
designed, not built yet; [availability](docs/reference/availability.md) says
what works today.

Declarative atoms use `.atom.astro`, with schema-generated types, local prompt
components and bounded interpreted expressions. `atomic check` validates them and
`atomic render` previews their developer/user prompt parts without a model call.
See [Astro authoring](docs/reference/atom-files.md).

- [Documentation](docs/README.md), starting with
  [the mental model](docs/concepts/mental-model.md)
- [Examples](examples/README.md), checked offline by `tools/check_examples.py`
- [Developing atomic](docs/development.md): the development container, tests
  and CI
- [Security policy](SECURITY.md)

atomic is licensed under the [Apache License 2.0](LICENSE).
