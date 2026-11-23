# Troubleshooting

This is a guide to the intended error-handling experience. Use the matching
release's help when the CLI is available; these commands are still proposed.

## `atomic` is not found

Check [installation and release status](../getting-started/installation.md). This
repository currently has no installable runtime. A directory of atom files is
not itself an installed CLI.

## Atomic cannot find my automation project

Look for `atomic.yaml` or `.atomic/atomic.yaml` in the current directory or its
parents. Having both forms in one directory is an error. For a member project,
check that the parent workspace lists it.

An interactive coding session is intended to work without a manifest; named atoms
and workflows need a discoverable project.

## The model or configuration is unexpected

```sh
atomic config --explain
```

Check which source supplied the value, whether a personal/profile override is
active and whether managed policy restricts the available choices. Confirm that
your provider credentials and endpoint are available without putting secrets into
a prompt or committed atom file.

## An atom fails validation

```sh
atomic check
```

Read the file location and schema path. Check required fields, reference bases,
input mappings and tag paths. A model output can pass provider generation checks
and still fail Atomic's full contract. Bounded repair may help; persistent failure
needs a corrected contract, task or model choice.

## An `.atom.astro` file fails to parse

Astro treats `<` and `{` as syntax. Escape literal prose with entities such as
`&lt;name>` and `&#123;value&#125;`, including inside displayed code fences.
Use local `.astro` components and generated Props imports. The compiler rejects
unsupported executable syntax before a model call; changing the extension to a
removed format does not bypass the check. See [the expression guide](../guides/template-tags.md).

## A prompt section is missing or mismatched

Section tags must balance, in the atom and in each component it uses, inside
`{…}` expressions as well as outside them. The load fails, naming the file,
the line and the tag, when a closing tag names another element than the
innermost one open (`` `</notes>` closes <section>, opened on line 8 ``), when
a closing tag has nothing open to close, and when an element is still open at
the end of the file. Close the element the message names, or remove the stray
closing tag. The load fails too when Astro does not read a closing tag as the
end of the element it is written for, as with a `<slot>` or `<head>` after
leading text, or a closing tag inside a `{…}` expression the element did not
open in; the message names the element, both lines and what to change. A void
HTML element such as `<input>` has no closing tag, so it
cannot be a section's name. Tags that start with `data-` are reserved and are
rejected wherever you write them. See [section tags](../guides/template-tags.md).

## `atomic check` says a diagram is stale

A diagram under an `atomic:diagram` marker is generated from a workflow, and the
workflow has changed since the diagram was committed. Refresh it:

```sh
atomic docs --write
```

Review the difference before committing. In the risk view it can mean that a
step gained write or network access. See
[keeping a committed diagram current](../guides/diagrams.md#keep-a-committed-diagram-current).

## `atomic check` says a prompt snapshot changed

An edit to an atom's instructions, or to a component it imports, changed the
prompt for one of its typed examples. Print the new prompt with
`atomic render <atom> --input <file.json>`, confirm the change is the one you
meant, then update the snapshot. The update command is not specified yet.

## A compact schema is rejected

The compact form has no constraints: a field such as `when: date-time` or
`name: string(minLength 3)` fails with its file and path. Write that schema as
full JSON Schema, which can say `format: date-time` or `minLength: 3`. A compact
schema also cannot name a top-level field after a structural JSON Schema
keyword, such as `type`, `enum` or `items`: a schema with one is read as full
JSON Schema, and atomic refuses it if its other fields read as compact ones
(`type: string` beside `message: string`), rather than ignore them. A field
named after a constraint or annotation keyword, such as `description` or
`format`, stays a field while its value is `type, description`.
`atomic schemas expand <atom>` prints what a compact schema compiles to. See
[compact schemas](atom-files.md#compact-schemas).

## Code is refused network access

An exec atom's separate script fails with
`Requires net access to "<host>"` when it tries a host that is not in its atom's
`network.allow`. The message names
the host. Add it to the list if the script should reach it. An AWS SDK
addresses an S3 bucket by the bucket's own host name, such as
`acme-reports.s3.us-east-1.amazonaws.com`, and other services by their regional
endpoint. See
[call AWS and other services with an SDK](../guides/scripts-and-cloud-sdks.md).

## The Astro compiler fails or runs out of time or memory

`atomic check` and `atomic render` run Atomic's bundled Astro compiler in a
child process of Atomic itself, on its embedded V8; no Deno is needed. A compile
that runs past 30 seconds is stopped with `the Astro compiler exceeded 30
seconds`, and one that uses 32 seconds of CPU time with `the Astro compiler
exceeded its CPU time limit`. The compiler has a 1 GiB JavaScript heap and a
2.5 GiB process memory budget (twice the heap to allow unwinding, plus 512 MiB
for V8, WebAssembly memory, and buffers). Linux enforces the process budget on
writable allocations. On macOS the parent stops the compiler when a reading of
its physical footprint exceeds the budget. Short spikes can fall between
readings. A memory failure usually reports `the Astro compiler exceeded its
memory limit`. Checking the whole
example catalog needs about 800 MiB. A smaller component graph or simpler types
bring an atom within these limits. A compiler that crashes is reported with what
it wrote to stderr, and leaves no core file; report it as a bug.

## Deno cannot be downloaded

From v0.7, Atomic fetches its own Deno the first time a project with atom code
is checked or run. If the machine has no network, or your organization forbids the
download, install it from a file:

```sh
atomic runtimes install --from deno-x86_64-unknown-linux-gnu.zip
```

Use the archive for the version your Atomic release pins; a different or
altered archive fails the checksum and is not installed. A team-mode server
never downloads while running: build the runtime into its image. To use a Deno
you manage yourself, set `runtime.deno.binary` in the manifest. See
[you do not install Deno](../guides/atom-code-runtime.md).

## A package is not in the lockfile

If a check or a run reports that the lockfile is out of date, an import was
added or its version changed. Run `atomic runtimes lock`, review the difference
in `deno.lock` and commit it. Deno also refuses a package version published within the last day
by default; pin a slightly older version or wait.

## `atomic eval` refuses an atom

An atom whose code writes to an outside system is never run by an eval, a
render or a preview, because running it would make the change. That covers
code that declares `effects.external: idempotent` and code that reaches the
network without saying it is read-only. Test that code with `atomic test`.
If the code only reads, declare `effects.external: read-only`.

## A workflow is waiting

```sh
atomic runs show <run-id>
atomic gates list
```

A waiting gate is saved work. Inspect its proposal and identity requirements before
deciding it. In a team deployment, an authorized approver may need to act.

## A run is parked as `outcome_unknown`

An external effect may have happened without a recorded response. Inspect the
external system and resolve the existing operation through the authorized control
surface. Blindly retrying or starting a new run can duplicate the action.

## A coding fixture does not run

A commit hash needs a repository that contains it. Treat absent fixture sources
or commits as setup errors; do not substitute the developer's checkout. A
workflow that changes code needs the pair `--fixture` / `--fixture-rev`, and
the failure's kind (`fixture.required`, `fixture.path`, `fixture.unreadable`,
`fixture.revision`, `fixture.unsupported`, `fixture.schema`, `fixture.input` or
`fixture.snapshot`) says which part is wrong; the
[CLI reference](cli.md#coding-fixture-selection) lists them. Use
the [preparation helper](../../examples/02-change-request/README.md) for real
inputs.

For the recovery model, read [durable execution](../concepts/durable-execution.md).

## The runtime stopped while a run was waiting

From the same project and state store, inspect `atomic runs show <run-id>` and
continue with `atomic runs resume <run-id>` (v0.1 target). A pending gate keeps
its original deadline, including offline time. A rejected/expired gate, exhausted
budget or `outcome_unknown` cannot be cleared by resume. See
[recovery](../concepts/durable-execution.md#resume-existing-work).
