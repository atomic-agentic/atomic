# Security policy

## Reporting a vulnerability

Please report security problems privately, through GitHub's private
vulnerability reporting: open the repository's **Security** tab and choose
**Report a vulnerability**. Do not open a public issue for a vulnerability.

A useful report says what is affected (a commit or release), how to reproduce
the problem, and what an attacker gains. You will get an acknowledgement, and
the fix and advisory are coordinated with you before anything is published.

## Supported versions

atomic is in early development and has no stable release yet. Fixes land on
`main` only.

## Scope

atomic runs model-driven agents against code, so these are in scope among
others:

- escaping an atom's sandbox, or a step reaching the network or files its
  declaration does not allow;
- a model, a tool call or an atom's shell producing a gate decision or an
  admin action, which only a human credential may do;
- code running from a declarative file (atom, workflow, schema or manifest);
- git operations on an atom's worktree running hooks, drivers or other code
  from that worktree;
- secrets reaching model input, logs, artifacts or the source tree.

Vulnerabilities in Codex or other dependencies belong with their own
projects; report them there, and tell us if atomic's use of them is affected.
