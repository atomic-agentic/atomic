# Write atoms with a coding agent

Claude Code, Codex and other coding agents can write atoms for you. Atomic ships
the know-how as an [Agent Skill](https://agentskills.io), `atomic-create-atom`,
and `atomic setup` installs it where the agent looks:

```sh
atomic setup claude    # .claude/skills/atomic-create-atom and a CLAUDE.md section
atomic setup codex     # .agents/skills/atomic-create-atom and an AGENTS.md section
```

Run it at the root of the repository, or pass `--path <dir>`. Codex reads the
cross-tool `.agents/skills/` directory, and so does the Codex extension for VS
Code; Claude Code reads its own `.claude/skills/`. The command adds a short
section between marker lines to `CLAUDE.md` or `AGENTS.md`, creating the file
when there is none, and leaves the rest of the file alone. Skills installed
beside atomic's are left as they are.

The skill's own directory belongs to atomic, and the agent reads everything in
it, so setup checks that directory before it writes anything, hidden files and
subdirectories included. It may hold only what atomic wrote there. Setup records
the files it writes, with a hash of each, in `.atomic-pack.json` beside them, and
a later setup writes over only what that record still vouches for. A file or
directory atomic did not write, a file atomic wrote that someone has since
edited, and a symbolic link are each named in a refusal, and while any of them
remains nothing is installed and no instruction points the agent at the
directory. Remove what the refusal names and run setup again.

Commit `.atomic-pack.json` with the skill. It is how a later setup tells atomic's
own files from yours, and what follows is only as good as that record: keep it
where a change to it shows.

Nothing of yours is deleted, as long as the record is the one atomic wrote: what
setup removes is only what the record says atomic wrote and nothing has touched
since, and — when it is the only thing in the way — an empty directory atomic did
not write, named in the refusal that removes it. A file an older release wrote
that this one no longer ships does come out on that basis, and a directory that
only held such files goes with them.

The way to that directory matters as much as what is in it: it must run through
plain directories from the repository root. A `.claude` or `.agents` that is
really a symbolic link somewhere else is refused where the link stands, because
atomic does not write through links: remove the link, not your directory. The
instructions file may be a link only to the other agent's instructions file,
which is how one file can serve both agents.

An upgrade needs no clean-up. The references are generated from the binary that
installs them, so their bytes change between releases, and setup writes the new
ones over the old precisely because its record says those bytes are its own and
untouched. That is why a refusal says an older version of atomic may be the cause:
what atomic cannot account for is refused rather than installed beside.

One case is still a deliberate step. A directory written by a version of atomic
that kept no record cannot be checked, so setup accepts it only while it holds
nothing but the current pack's files, each with the current pack's bytes.
Upgrading from
such a version, delete `.claude/skills/atomic-create-atom`, or its
`.agents/skills/` counterpart, and run setup again; from then on an upgrade takes
nothing away. Where nothing has changed, running setup again writes the same files
and adds nothing to the instructions file.

Each file is written to a temporary beside it, named `.<file>.<random>.atomic-writing`,
and then renamed into place. A setup that was killed partway can leave one of
those behind; in a skill directory it is refused like any other file atomic did
not write, so remove it and run setup again.

Then describe the task in a sentence, for example "write an atom that pulls the
action items, with their owners, out of meeting notes".

## What the skill does

The skill teaches; the CLI checks. The agent writes the schemas first, then the
`.atom.astro` file with a typed example, then eval cases, and runs
`atomic check` until it passes. It previews the prompt with `atomic render` and
reports what it wrote. The eval runner is not available yet, so the agent
writes eval cases but cannot run them.

The skill's `references/` folder is generated from the definitions the binary
checks atoms with: the configuration fields the loader accepts and refuses, the
typed `AtomConfig` declaration, the output-schema generation profile, and the
project layout. Its prompt-syntax and CEL examples, and the mistakes it warns
about, are compiled and evaluated by atomic's tests. An installed skill
therefore describes the binary that installed it.

The repository keeps a copy of the pack under `skills/`. When a change to
atomic changes the generated references, a test fails until the copy is
regenerated:

```sh
ATOMIC_TEST_BLESS=1 scripts/dev.sh cargo test -p atomic-agentic skills
```

With both agents installed and signed in, `scripts/agent-skills-check.sh`
runs `atomic setup` in fresh directories and confirms that each agent lists
the skill.

See [atom files](../reference/atom-files.md) for the format the skill teaches.
