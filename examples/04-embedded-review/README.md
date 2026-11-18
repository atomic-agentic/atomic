# Add a diff reviewer to an existing repository · v0.1

The manifest and atom live under [.atomic/](.atomic/atomic.yaml). Run discovery
from this directory or below it; the application repository need not become an
agent-only project. Runtime state remains outside the source tree.

[The atom](.atomic/atoms/review-diff.atom.astro) discovers adjacent schema files and
includes `ReviewRules.astro` relative to itself. The manifest does not repeat the
default collections. The atom explicitly selects the Codex kind, with a read-only sandbox and
no optional tools.

The fixture supplies patch text and acceptance criteria. It deliberately does
not put `fixtures/change.patch` in a field whose type is a commit SHA. A review
may return `insufficient_context`; patch text is not evidence that tests passed.

Future commands, from this directory:

```sh
atomic check
atomic eval review-diff
atomic run review-diff --input .atomic/fixtures/review-diff/quantity-missing.input.json
```

Paths passed as CLI arguments are relative to the caller's directory. Paths in
the manifest are relative to `.atomic/`; a partial reference is relative to its
atom file. Public PR input needs v0.5's external-origin controls; these are
team-curated local fixtures.
