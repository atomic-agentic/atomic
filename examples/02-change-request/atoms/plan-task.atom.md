---
atomic: 1
description: Plan one bounded change against its acceptance criteria.
---

Inspect the repository at the supplied base. Produce at most five steps for this
     one task, with tests that demonstrate each acceptance criterion. Return a plan;
     do not edit files or create graph items.

     Task: {% $input.task.item.title %}
     Criteria: {% $input.task.item.criteria %}
     Base: {% $input.task.base %}
