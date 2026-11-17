---
atomic: 1
description: Implement one approved plan and run its tests.
sandbox: workspace-write
---

Implement only the approved plan in this attempt's worktree. Run the tests and
     report their actual commands. A failing test must fail the attempt; never report
     an unrun test as passing. Do not push or merge, and do not claim a commit SHA.
     The runtime creates and records the candidate commit after validation.

     Task: {% $input.task.item.title %}
     Criteria: {% $input.task.item.criteria %}
     Plan: {% $input.plan.steps %}
     Base: {% $input.base %}
