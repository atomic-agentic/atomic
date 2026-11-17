---
params:
  strict: { type: boolean }
---
Sort the findings into what must be fixed now and what can follow up.
{% if params.strict %}
A missing test for new behaviour counts as a blocker.
{% /if %}
Name each file that must change by its path, such as src/<module>.py.
{% slot /%}
