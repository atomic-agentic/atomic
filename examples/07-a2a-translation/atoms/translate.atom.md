---
atomic: 1
description: Translate supplied public text through an approved remote agent.
kind: a2a
remote:
  card: https://translator.example.invalid/.well-known/agent-card.json
  skill: translate
auth:
  secret: translator-a2a-token
budget:
  minutes: 5
validate:
- output.target == input.target
---

Return the translated public text and the unchanged target language.
