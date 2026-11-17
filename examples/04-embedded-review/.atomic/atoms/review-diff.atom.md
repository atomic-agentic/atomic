---
atomic: 1
description: Review supplied patch text without changing the repository.
validate:
- output.verdict != 'approve' || output.findings.size() == 0
---

{% partial file="_review-rules.md" /%}

     Criteria: {% $input.criteria %}
     Patch: {% $input.patch %}
