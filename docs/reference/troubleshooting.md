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

## An `.atom.mdx` file fails to parse

In the JSX-style form, `<` and `{` are syntax. Text such as `Vec<String>` or
`OUT_DIR/<stage>.log` in prose fails at load with its line and column. Put it in
a code span, or write the atom as `.atom.md`, where angle brackets inside a line
of prose are plain text. Comments are `{/* like this */}`, not `<!-- -->`. See
[the JSX-style form](../guides/template-tags.md#the-same-atom-with-jsx-style-tags).

## A section tag is reported as unclosed

A lowercase tag alone on its line, such as `<rubric>`, opens a section and needs
a matching `</rubric>` on its own line. Check for a misspelled closing tag and
for sections closed in the wrong order. Tags that start with `data-` are
reserved and are rejected wherever you write them. See
[section tags](../guides/template-tags.md#mark-sections-with-xml-style-tags).

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

An edit to an atom's instructions, or to a partial it includes, changed the
prompt for one of its typed examples. Print the new prompt with
`atomic render <atom> --input <file.json>`, confirm the change is the one you
meant, then update the snapshot. The update command is not specified yet.

## A compact schema is rejected

The compact form covers names, types, descriptions, optional fields, lists,
nested objects and enums. A constraint such as `minLength` or `pattern` needs
full JSON Schema. Print what the compact form compiles to with
`atomic schemas expand <atom>`. See
[the compact form](atom-files.md#the-compact-form).

## Code is refused network access

An atom's code, in a `---ts` block or a script, fails with
`Requires net access to "<host>"` when it tries a host that is not in its atom's
`network.allow`. The message names
the host. Add it to the list if the script should reach it. An AWS SDK
addresses an S3 bucket by the bucket's own host name, such as
`acme-reports.s3.us-east-1.amazonaws.com`, and other services by their regional
endpoint. See
[call AWS and other services with an SDK](../guides/scripts-and-cloud-sdks.md).

## Deno cannot be downloaded

Atomic fetches its own Deno the first time a project with atom code is checked
or run. If the machine has no network, or your organization forbids the
download, install it from a file:

```sh
atomic runtimes install --from deno-x86_64-unknown-linux-gnu.zip
```

Use the archive for the version your Atomic release pins; a different or
altered archive fails the checksum and is not installed. A team-mode server
never downloads while running: build the runtime into its image. To use a Deno
you manage yourself, set `runtime.deno.binary` in the manifest. See
[you do not install Deno](../guides/atom-code-runtime.md#you-do-not-install-deno).

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
or commits as setup errors; do not substitute the developer's checkout. The
v0.1 target requires paired `--fixture` / `--fixture-rev` options; use the
[preparation helper](../../examples/02-change-request/README.md) for real inputs.

For the recovery model, read [durable execution](../concepts/durable-execution.md).

## The runtime stopped while a run was waiting

From the same project and state store, inspect `atomic runs show <run-id>` and
continue with `atomic runs resume <run-id>` (v0.1 target). A pending gate keeps
its original deadline, including offline time. A rejected/expired gate, exhausted
budget or `outcome_unknown` cannot be cleared by resume. See
[recovery](../concepts/durable-execution.md#resume-existing-work).
