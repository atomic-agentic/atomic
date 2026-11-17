# Configuration

atomic reads its configuration in layers. Each layer overrides the one below
it, setting by setting:

| Layer | Source | Required |
| --- | --- | --- |
| 1 | Built-in defaults | always present |
| 2 | `atomic.yaml` | no |
| 3 | `atomic.local.yaml`, for personal settings you do not commit | no |
| 4 | `atomic.<profile>.yaml`, when a profile is selected | yes, if selected |
| 5 | `ATOMIC_*` environment variables | no |
| 6 | Command-line flags | no |

Files are YAML, read with atomic's restricted loader (no anchors, tags or
duplicate keys; quote `yes`, `no`, `on` and `off`). Mappings merge key by key;
any other value replaces the one below. A setting atomic does not know is an
error that names the file and line, so a typo is never silently ignored.

An environment variable names a setting by its path: `model.base_url` is
`ATOMIC_MODEL_BASE_URL`. Matching ignores case and the separators `_`, `-` and
`.`, so `ATOMIC_MODEL_BASEURL` works too. Values take the setting's type: `true`
or `false` (or `1` and `0`) for switches, numbers for numbers, JSON for lists
and sections.
