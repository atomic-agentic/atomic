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
subdirectories included: it must hold exactly the files atomic installs there,
each with the content atomic installs, and every directory leading to it must
be a plain directory. A file or directory atomic does not write, a symbolic
link, and a file holding other content are each named in a refusal, and while
any of them remains nothing is installed and no instruction points the agent at
the directory. Delete what the refusal names, or the whole directory, and run
setup again.

So replacing an old skill is a deliberate step: the files an older atomic wrote
hold content this one does not, and setup says so instead of installing beside
them. Delete `.claude/skills/atomic-create-atom`, or its `.agents/skills/`
counterpart, and run setup again to get the skill that matches the new binary.
Where nothing has changed, running setup again writes the same files and adds
nothing to the instructions file.

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
