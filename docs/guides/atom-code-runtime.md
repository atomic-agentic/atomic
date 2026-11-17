# How Atomic runs and confines atom code

An atom can carry TypeScript: a block at the top of the atom file, or a script
run by an exec atom. [Call AWS and other services with an SDK](scripts-and-cloud-sdks.md)
shows how to write it. This page covers what happens when it runs: the
permissions it gets, the defaults you can tighten, why the runtime is Deno, and
where that runtime comes from.

In short: the code starts with nothing, and gets only the hosts, the
environment and the credential its atom declares. This is planned for v0.7 and
its names are proposals; see [availability](../reference/availability.md).

## How Atomic runs it

You never write Deno's permission flags. Atomic builds the command from the
atom's declaration. For the write script in
[the S3 example](../../examples/11-aws-s3/atoms/put-digest.atom.md):

```sh
deno run --no-prompt --lock=deno.lock --frozen --cached-only \
  --allow-net=acme-reports.s3.us-east-1.amazonaws.com \
  --allow-env=AWS_REGION,REPORTS_BUCKET,AWS_ACCESS_KEY_ID,AWS_SECRET_ACCESS_KEY,AWS_SESSION_TOKEN,HOME \
  --ignore-env --no-remote --allow-sys=osRelease \
  --allow-read=<project> --allow-write=<scratch> \
  tools/put_object.ts
```

| Flag | Why |
| --- | --- |
| `--allow-net=<hosts>` | From `network.allow`. A request to any other host fails, and the error names the host |
| `--no-prompt` | A missing permission is an error, never a question |
| `--lock`, `--frozen` | The run fails if the lockfile would have to change |
| `--cached-only` | Packages were fetched by an earlier setup step, whose network reaches only the package registry |
| `--allow-env=<names>` | The names in `env` and the ones a credential provides, plus `HOME`, which Atomic points at the step's scratch directory |
| `--ignore-env` | A lookup of any other variable returns nothing. An SDK probes for many settings it was not given |
| `--no-remote` | No modules from a URL |
| `--allow-read=<project>` | The project's files, read-only |
| `--allow-write=<scratch>` | The step's scratch directory, and nowhere else |
| `--allow-sys=osRelease` | From `runtime.deno.sys` |

For a block, Atomic splits the code from the atom file, generates a small
module that calls `prepare` with a `ctx` that saves each step, and runs that
module the same way. `atomic check` type-checks it. The setup step that fetches
packages is described under
[lock the packages](scripts-and-cloud-sdks.md#lock-the-packages).

The code gets no permission to start processes or load native code. Deno's
checks sit inside Atomic's own sandbox; they are a second layer, not a
replacement.

Because authority is written once, in the atom, it also appears in the
[risk view](diagrams.md#the-risk-view) of any workflow that uses the atom and,
later, in the [policy lock](security-and-permissions.md#review-permission-changes-as-a-diff).

## Security defaults and settings

A package from npm is someone else's code. Installed with npm and run on Node,
it can run a script at install time and then reach everything your user can:
files, the network, every environment variable. On Deno, Atomic starts from
nothing and adds only what the atom declares.

| What the code could do | Default | To change it |
| --- | --- | --- |
| Reach the network | No hosts | List hosts in the atom's `network.allow` |
| Read environment variables | Only the names in the atom's `env`, the keys its credential provides, and `HOME`, which points at the scratch directory | Add to `env` |
| Read facts about the machine | None | `runtime.deno.sys` in the atom, from a short harmless list |
| Read and write files | Read the project; write the step's scratch directory | Fixed |
| Start processes or load native code | Never | Fixed |
| Import modules | `npm:` and `jsr:` packages pinned to an exact version and recorded in the lockfile; nothing from a URL | Narrow with `allow_packages` |
| Run a package's install scripts | Never | Allow package by package with `install_scripts` |
| Use a release published moments ago | Not until it is a day old | `minimum_dependency_age` |
| Be asked for a permission while running | Never; a missing permission is an error | Fixed |
| Leave a record of what it used | No record | `audit_permissions` |
| Run on a Deno of unknown origin | Never: Atomic's own pinned, checksum-verified copy | `binary` |

The project-wide settings go in the manifest. Every one has a default, so the
block is optional. This shows the defaults; `allow_packages` is unset unless
you add it:

```yaml
# atomic.yaml
runtime:
  deno:
    binary: managed               # Atomic's own pinned Deno; or system, or a path
    lock: deno.lock               # the lockfile every run must match
    minimum_dependency_age: 1d    # how old a release must be before it can be locked
    install_scripts: []           # packages allowed to run install scripts
    # allow_packages: ["npm:@aws-sdk/*"]   # unset by default: any pinned package
    audit_permissions: false      # record each permission the code uses in the step's trace
```

| Setting | Default | Use it to |
| --- | --- | --- |
| `minimum_dependency_age` | `1d` | Wait longer, such as `7d`, so a bad release is caught by others first |
| `allow_packages` | Any pinned package | Limit imports, for example to `npm:@aws-sdk/*`. It covers what atom code imports by name; the packages those depend on are fixed by the lockfile |
| `install_scripts` | None | Allow the rare package that must build something at install time, named one by one |
| `audit_permissions` | `false` | See exactly which hosts, variables and files a new SDK touches before you settle its declaration |
| `lock` | `deno.lock` | Keep the lockfile somewhere else |
| `binary` | `managed` | Use the `deno` on your `PATH` (`system`) or one at a given path, instead of the copy Atomic downloads and verifies |

An organization's managed policy can tighten any of these and can limit which
hosts and system facts an atom may declare. No setting grants the fixed rows:
atom code cannot start a process or load native code.

Deno's checks are not the only barrier. The code also runs inside Atomic's own
sandbox, and a model never sees its credentials.

## Why Deno

| Runtime | Packages | Limits on the code |
| --- | --- | --- |
| Deno | `npm:` imports, a lockfile, no install step, no install scripts | Per-host network, per-path files, no subprocesses unless granted |
| Node | `package.json`, an install step, `node_modules` | A permission model whose network switch is all or nothing; its documentation says it does not protect against malicious code |
| Bun | `package.json`, an install step, `node_modules` | No permission model |

Deno is the only one of the three that can take an atom's list of hosts and
enforce it. Node, Bun and Python scripts remain possible as a plain
`exec.command` with a pinned image; they rely on Atomic's sandbox alone.

### Deno and Node, point by point

Node has a permission model too, so the comparison is worth making exactly.
The Node column describes Node 26 with npm 12, from their documentation as of
November 2026.

| | Deno, as Atomic runs it | Node and npm |
| --- | --- | --- |
| With no flags at all | No network, files, environment or subprocesses | Everything is allowed. The permission model applies only when `--permission` is passed |
| Network | Allowed host by host | On or off with `--allow-net`. No host list is documented, and the flag is marked as under active development |
| Environment variables | Allowed name by name; any other lookup returns nothing | Not covered by the permission model |
| Facts about the machine | Allowed kind by kind | Not covered by the permission model |
| Files | Allowed path by path | Allowed path by path, with `--allow-fs-read` and `--allow-fs-write` |
| Subprocesses and native code | Denied, and Atomic never grants them | Denied under `--permission` unless allowed |
| A package's install scripts | Not run unless the package is named | Run by npm unless `ignore-scripts` is set |
| A release published today | Refused until it is a day old | Installed. npm's `min-release-age` exists and is off by default |
| The lockfile | Checked on every run. An import it does not record stops the run | Checked when packages are installed with `npm ci`, not when the code runs |
| How far to rely on it | Atomic treats it as a second layer, inside its own sandbox | Node's documentation says: "This feature does not protect against malicious code" |

Node is level with Deno on files and on subprocesses. The differences that
matter for code an atom carries are the list of hosts, the environment, and
what happens when a package is installed. Sources:
[Node permissions](https://nodejs.org/api/permissions.html),
[npm configuration](https://docs.npmjs.com/cli/v12/using-npm/config).

## You do not install Deno

Atomic brings its own. Each Atomic release pins one Deno version. The first
time a project with atom code is checked or run, Atomic downloads that
version's official release archive, verifies it against a checksum built into
Atomic, and keeps it in its own data directory, outside your project. A
project with no atom code never downloads it.

- **Every machine runs the same version**, the one your Atomic release pins.
- **Your own Deno is left alone.** Atomic's copy has its own package cache and
  is not on your `PATH`.
- **Fetch it ahead of time** for CI, an image build or a flight:

  ```sh
  atomic runtimes install
  atomic runtimes install --from deno-x86_64-unknown-linux-gnu.zip
  ```

  The second form installs from a file you carried in, for a machine with no
  network. The checksum is verified either way.
- **A server never downloads while running.** Build the runtime into its image
  with the command above; a team-mode `atomic serve` without it fails at load
  and says what to run.
- **Run it yourself** with `atomic deno`, for example to format the code or
  see which version you have. Use `atomic test` for tests:

  ```sh
  atomic deno fmt tools test
  atomic deno --version
  ```

To use a Deno you manage instead, set `binary` in the manifest to `system` (the
`deno` on your `PATH`) or to a path. It must be a version your Atomic release
supports. An organization can forbid the download or serve it from an internal
mirror.

## Packaging and embedding research

The current proposal downloads a managed executable. A follow-up packaging
study recommends including that
pinned executable inside Atomic's release file and materializing it privately,
so first use needs no runtime download. It also evaluates linking Deno's Rust
crates and identifies a SQLite compatibility gate. This is a recommendation;
the download behavior above remains the current plan until a packaging prototype
passes the stated checks.

## What is still open

- **Names.** `runtime.deno` and its settings, and the `atomic runtimes install`
  and `atomic deno` commands, follow the plan and are not a tested contract.
- **What the example's tests cover.** They exercise the network, environment,
  file, lockfile and no-remote rules. `install_scripts`, `audit_permissions`
  and a non-default `minimum_dependency_age` map to Deno options that exist in
  Deno 2.9 but are not exercised by the example.
- **The Node comparison is a snapshot.** It describes Node 26 and npm 12 as
  documented in November 2026.

Next: [call AWS and other services with an SDK](scripts-and-cloud-sdks.md),
[security and permissions](security-and-permissions.md) or
[the S3 example](../../examples/11-aws-s3/README.md).
