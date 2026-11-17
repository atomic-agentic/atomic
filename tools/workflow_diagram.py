# SPDX-License-Identifier: Apache-2.0
"""Draws a workflow file as Mermaid, for the example checker.

Not an implementation of atomic. It produces the shape the proposed
`atomic workflows show <name> --format mermaid [--view risk]` exporter is meant to
print, so that a diagram committed in an example can be checked against its source.

Two views of the same workflow:
    default  each step, its atom or gate, and the `after` dependencies
    risk     the same structure, with what each step is allowed to do

Kind is carried by shape (a box for an atom, a diamond for a gate, a rounded box for
the result) and by the label. In the risk view a step that can change files or reach
the network also gets a heavier border. Nothing depends on colour. Labels are data:
node IDs are generated, and label text is escaped.
"""

from __future__ import annotations

import re


def escape(text):
    """Mermaid entity codes, so label text cannot add nodes, edges or markup."""
    return str(text).replace("#", "#35;").replace('"', "#quot;").replace("<", "#lt;").replace(">", "#gt;")


def literal(cel):
    """A CEL string literal reads better without its quotes; anything else is shown as written."""
    match = re.fullmatch(r"'([^']*)'", str(cel).strip())
    return match[1] if match else str(cel).strip()


def atom_authority(fm, step):
    """What an atom may do, from its frontmatter and the defaults: (label lines, elevated).

    `elevated` is true when the step can change files or reach the network. The risk
    view marks those steps with a heavier border as well as saying so in the label.
    """
    kind = fm.get("kind", "codex")
    if kind not in ("codex", "lite", "exec"):
        raise ValueError(f"the catalog's risk view does not cover kind {kind!r}")
    if fm.get("tools"):
        raise ValueError("the catalog's risk view does not cover declared tools")
    if kind == "lite":
        workspace = "no workspace"
        elevated = False
    elif kind == "exec":
        runtime = fm.get("runtime") or {}
        workspace = "runtime: deno" if "deno" in runtime else "host command"
        elevated = False
    else:
        sandbox = fm.get("sandbox", "read-only")
        workspace = f"sandbox: {sandbox}" + (", own worktree" if step.get("worktree") else "")
        elevated = sandbox != "read-only"
    network = fm.get("network")
    effects = fm.get("effects")
    # "(code)" marks an atom whose file starts with a TypeScript block.
    lines = [f"{kind}, {workspace}" + (", code on deno" if "(code)" in fm else "")]
    if not network and not effects:
        lines.append("no network, no tools, no effects")
    else:
        hosts = network["allow"] if isinstance(network, dict) else [str(network)]
        lines.append("network: " + ", ".join(hosts) if network else "no network")
        described = {"read-only": "reads an outside system", "idempotent": "writes outside, safe to repeat"}
        kind_of_effect = (effects or {}).get("external")
        if effects is not None and (set(effects) != {"external"} or kind_of_effect not in described):
            raise ValueError("the catalog's risk view covers effects.external: read-only or idempotent")
        lines.append("no tools, " + (described[kind_of_effect] if effects else "no effects"))
    credentials = fm.get("credentials") or []
    if credentials:
        lines.append("credential: " + ", ".join(credentials) + ", short-lived")
    secrets = sum(isinstance(value, dict) for value in (fm.get("env") or {}).values())
    if secrets:
        lines.append(f"{secrets} secrets, not shown to a model")
    trust = fm.get("trust")
    if trust:
        lines.append(f"accepts {trust} input")
    return lines, elevated or bool(network)


def render(workflow, atoms, view="default"):
    """`atoms` maps an atom name to its frontmatter."""
    if view not in ("default", "risk"):
        raise ValueError(f"unknown view {view!r}")
    steps = workflow["steps"]
    ids = {name: f"n{index}" for index, name in enumerate(steps, 1)}
    lines = ["flowchart TD"]
    any_elevated = False
    for name, step in steps.items():
        if "gate" in step:
            gate = step["gate"]
            label = [escape(name), "gate: " + escape(literal(gate.get("title", name)))]
            if view == "risk":
                approvers = gate.get("approvers")
                label.append("approvers: " + escape(", ".join(approvers)) if approvers else "approver: local operator")
                if "timeout" in gate:
                    label.append(escape(f"after {gate['timeout']}: {gate.get('on_timeout', 'reject')}"))
            lines.append(f'  {ids[name]}{{"{"<br/>".join(label)}"}}')
        else:
            fm = atoms[step["atom"]]
            label = [escape(name), "atom: " + escape(step["atom"])]
            marker = ""
            if view == "risk":
                authority, elevated = atom_authority(fm, step)
                label += [escape(line) for line in authority]
                if elevated:
                    marker = ":::elevated"
                    any_elevated = True
            lines.append(f'  {ids[name]}["{"<br/>".join(label)}"]{marker}')
    result = f"n{len(steps) + 1}"
    lines.append(f'  {result}(["returns {escape(workflow["returns"])}"])')
    depended_on = set()
    for name, step in steps.items():
        for dependency in step.get("after", []):
            lines.append(f"  {ids[dependency]} --> {ids[name]}")
            depended_on.add(dependency)
    for name, step in steps.items():
        if name not in depended_on:
            arrow = '-- "approved" -->' if "gate" in step else "-->"
            lines.append(f"  {ids[name]} {arrow} {result}")
    if any_elevated:
        lines.append("  classDef elevated stroke-width:3px")
    return "\n".join(lines)


MARKED_BLOCK = re.compile(r"<!-- atomic:diagram ([^>]*?) -->\n```mermaid\n(.*?)\n```", re.S)


def marked_blocks(markdown):
    """Yields (attributes, diagram) for each generated block committed in a Markdown file."""
    for match in MARKED_BLOCK.finditer(markdown):
        attrs = dict(pair.split("=", 1) for pair in match[1].split())
        yield attrs, match[2]
