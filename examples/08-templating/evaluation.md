# Prompt layout pilot

This pilot compares the three `triage-findings` layouts in this example and
the two review atoms in [example 14](../14-review-authoring/README.md). It is a
small paired fixture study, not a general model benchmark. The prompt snapshots
and fixture labels in the examples are the inputs; every call used the same
output contract and accessible evidence for its pair.

## Setup

The local endpoint reported `radium`; its deployed artifact was
`own-1m.rad` (modified 2026-11-06), served by `cfbe7e0-dirty` (2026-11-08).
The endpoint did not report a model source revision. The frontier endpoint
reported `claude-sonnet-5-5` through Claude Code 2.1.330. Both received the
rendered developer text as instructions and the bound external values as the
user turn, with the same JSON output schema. Temperature was zero. The triage
limit was 2,048 output tokens; the review limit was 8,192. The frontier calls
used no tools or repository access.

The six triage fixtures were `blocker`, `no-findings`, `interleaved`,
`hard-cases`, `interleaved-duplicates`, and `adjacent-duplicates`. Each was
rendered in bullet, labeled-record and table form. The review pilot used six
`review-evidence` and four `review-plan` cases. There was one call per
fixture, layout and model. The review cases shared the same host records,
schemas and expected decision across models.

## Paired observations

| Model and layout | Decision | Exact schema | File attribution | Input tokens | Output tokens |
| --- | ---: | ---: | ---: | ---: | ---: |
| Radium, bullets | 6/6 | 6/6 | 5/5 nonempty | 5,554 | 2,352 |
| Radium, labeled records | 6/6 | 5/6 | 5/5 | 7,375 | 2,705 |
| Radium, table | 6/6 | 6/6 | 5/5 | 10,732 | 2,961 |
| Sonnet, bullets | 6/6 | 6/6 | 5/5 | 10,078 | 1,093 |
| Sonnet, labeled records | 6/6 | 6/6 | 5/5 | 12,321 | 888 |
| Sonnet, table | 6/6 | 6/6 | 5/5 | 16,326 | 658 |

The required attribution was `pricing.py` in every nonempty fixture. The
bullets were shortest in rendered characters and actual input tokens. The
table gave no observed decision or attribution gain. The one Radium schema
miss was an extra property on the interleaved labeled-record response.

| Review atom and model | Decision | Schema | Exact host rows | Correct citations |
| --- | ---: | ---: | ---: | ---: |
| `review-evidence`, Radium | 6/6 | 6/6 | 11/13 | 12/13 |
| `review-evidence`, Sonnet | 6/6 | 6/6 | 13/13 | 13/13 |
| `review-plan`, Radium | 4/4 | 4/4 | 0/8 | 7/8 |
| `review-plan`, Sonnet | 4/4 | 4/4 | 6/8 | 8/8 |

All 28 overall decisions per model matched the fixture expectations. In
particular, neither model returned a false CLEAN on the missing or stale
evidence cases, or rejected the valid current-pass case. Those decision scores
did not imply exact evidence: Radium credited one unsupported host record,
cited an unavailable scan, and substituted descriptions for IDs in plan rows.
Sonnet used wrong dependency IDs in two plan rows. The host scorer and JSON
Schema remain the authority for row coverage and citation validity.

The Sonnet run reported $0.5053 total for its 28 matched calls. The local
server reported token usage but no price. Claude Code added its own prompt
framing and occasional support calls, so the two providers' absolute token
totals are not directly comparable. Within each model, the input-token counts
above are comparable across layouts.

## Markdown one-factor check

On two `review-evidence` inputs, the local model saw five versions differing
only in one treatment: plain baseline, heading, record link, italic wording,
or bold wording. All ten replies had the expected decision and valid schema.
All five versions reproduced both current-pass rows and citations. For the
unavailable-scan case, baseline, heading, link and italic cited the unavailable
record; bold did not. One response per treatment gives no reliable emphasis
effect or improvement claim.

## Authoring guidance and limits

Use the bullet triage layout when it fits the findings: it used the fewest
input tokens here with the same observed decisions and attribution. Preserve
explicit record and dependency IDs in review prompts, and check every returned
row and citation against host data before a gate. The results do not support
choosing headings, links, bold or italics as a correctness control.

This pilot has only six triage and ten review fixtures, one call per condition,
and no confidence interval. It measures blocker decision and required-file
attribution for triage, not exhaustive finding coverage; triage has no citation
field. The review scorer counts exact rows and citations independently of the
overall decision. The paired source prompts are the rendered snapshots and
fixtures in this catalog. The exact prompt turns, parsed and raw replies, case
labels, token usage and model metadata are recorded in the
[`pilot-data/`](pilot-data/) JSONL files: `radium-layout`, `radium-review`,
`radium-factor`, `sonnet-layout`, and `sonnet-review`. These records include the
model's generated text as data; they are not instructions for this repository.
