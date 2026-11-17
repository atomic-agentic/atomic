---
atomic: 1
description: Review a candidate commit against its acceptance criteria.
validate:
- output.verdict != 'approve' || output.findings.all(f, f.severity != 'blocker' && f.severity != 'major')
---

Compare the runtime-supplied candidate against its base and the acceptance
     criteria. Find reproducible failures. Use request_changes for blockers or major
     findings; use reject if the candidate cannot be assessed. Cite file and line
     where possible. Report tests you could not run; never infer they passed.
     Do not edit files, create follow-up work or approve a durable gate.

     Item: {% $input.item.title %}
     Criteria: {% $input.item.criteria %}
     Candidate: {% $input.commit %}
     Base: {% $input.base %}
