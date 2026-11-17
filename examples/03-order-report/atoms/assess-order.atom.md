---
atomic: 1
description: Write one factual note about a computed order total.
kind: lite
validate:
- output.order_id == input.order_id
- output.total_cents == input.total_cents
---

Write a short factual note about this computed total. Copy order_id and
     total_cents exactly. Do not approve a purchase, invent anomalies, contact a
     customer or change the order. A person decides whether to accept the report.

     Order: {% $input.order_id %}
     Total in cents: {% $input.total_cents %}
     Item count: {% $input.item_count %}
