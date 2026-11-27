# deno_core and deno_v8, carried on Codex's V8

atomic runs its Astro compiler worker and captured helpers on V8 through
`deno_core`. Codex pins one V8, the `v8` crate 152.2.0 from
<https://github.com/openai-oss-forks/rusty_v8> (see the patch block in the
workspace `Cargo.toml`). The latest `deno_core` release reaches V8 through
`deno_v8` 0.5.0, which requires `v8` 150.4.0, so the published crates would
put a second V8 in the build. These two directories are those releases with
the change that moves them to `v8` 152.2.0, so `deno_core` builds on the one
V8 Codex pins.

| Crate | Version | crates.io SHA-256 |
| --- | --- | --- |
| `deno_core` | 0.413.0 | `7ad7d5294f640fd99db08c455e2453c9860b1fa728b76fbdacf0eed3e308d48a` |
| `deno_v8` | 0.5.0 | `3dfb3151dcae150420c2fe784928b0dbce8897c6d8bc4bac7b54a5565e06b288` |

Both were published from <https://github.com/denoland/deno> at commit
`fe7ed012d92c642fd8e5607ac5a45d517538b5c5` (`libs/core` and `libs/deno_v8`).
They are MIT licensed, Copyright 2018-2026 the Deno authors
([LICENSE.md](LICENSE.md), also in each crate's directory).

## The change

[`pr-36665.diff`](pr-36665.diff) is the whole difference between the
published crates and these directories. It is the `libs/core` and
`libs/deno_v8` part of denoland/deno pull request 36665
(<https://github.com/denoland/deno/pull/36665>), which bumps the `v8` crate
to 152.2.0 and was not merged when this was vendored:

- `deno_v8` requires `v8` 152.2.0 and adds the two type aliases V8 152 needs
  (`PropertyInterceptorReturnValue`, `OwnedMicrotaskQueue`);
- `deno_core` follows the V8 152 API: `Global::open` becomes
  `v8::Local::new`, the property setter and definer interceptors take
  `PropertyInterceptorReturnValue`, and it no longer passes the
  `--no-validate-asm` V8 flag;
- two of `deno_core`'s own tests get a 20 MiB heap instead of 5 MiB.

One hand-port: the pull request edits 13 sites in `modules/map.rs` that the
0.413.0 release had already moved to `modules/map/dynamic.rs`; they are
applied there with the same rewrite. `serde_v8` and `deno_ops` need no
change and come from crates.io.

To check it, unpack the two `.crate` files from crates.io (their SHA-256 is
above) and apply the diff:

```sh
patch -p1 < pr-36665.diff   # in a directory holding deno_core/ and deno_v8/
```

The result is byte-for-byte these directories, except for the `LICENSE.md`
files atomic adds.

`deno_core`'s own library tests pass on Codex's V8 with these changes, except
`tests::located_script_name`, which expects the source path
`libs/core/lib.rs` of the Deno repository and sees this directory's layout
instead.

## How it is used

The workspace `Cargo.toml` patches `deno_core` and `deno_v8` to these paths
in its `[patch.crates-io]` block (scripts/codex-pin.sh writes those lines
after Codex's own) and excludes `third_party` from the workspace.
scripts/lock-check.sh fails if the lockfile holds more than one built V8.

## Dropping it

When a `deno_core` release reaches `v8` 152.2.0 (or whatever Codex then
pins) on its own, delete this directory, remove the two lines from
scripts/codex-pin.sh, require that release in the workspace `Cargo.toml`,
and run `scripts/codex-pin.sh check` and `scripts/lock-check.sh`. If Codex
moves to a newer V8 before that, the same change has to be carried again
for the new version.
