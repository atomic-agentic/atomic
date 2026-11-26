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
user turn, with the same JSON output schema. The local Radium requests used
temperature 0, with 2,048 output tokens for triage and 8,192 for review. The
frontier request envelopes specified those values, but Claude Code did not
expose flags to verify their realized settings. No external shell, web or
repository tools were enabled for the frontier calls.

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

On two `review-evidence` inputs, each model saw five versions: plain baseline,
heading only, record link only, italic wording only, and bold wording only.
Within a case, each treatment changed one factor from the baseline. The
rendered developer and user turns matched byte for byte across models; their
JSON Schema validation rules matched after removing only the unsupported
`$schema` draft URI from the Sonnet CLI request. The owner approved that exact
normalized set of ten Sonnet requests. The first attempt with the draft URI
failed in the CLI before any model call.

| Model | Expected decision | Valid schema | Host row fields | Current-pass citations | Unavailable citation omitted |
| --- | ---: | ---: | ---: | ---: | ---: |
| Radium | 10/10 | 10/10 | 15/15 | 10/10 | 1/5 |
| Sonnet | 10/10 | 10/10 | 15/15 | 10/10 | 0/5 |

The row score checks criterion, requirement, state, record ID, finding count,
claim and claim IDs, credit, and outcome; citation and free-text detail are
scored separately. The current-pass case had two required rows in each
variant. Both models approved all five, including the valid pass that the
author had omitted, so neither falsely rejected that evidence. The
unavailable-scan case had one required row in each variant. Both models
escalated all five, credited no unavailable scan as a pass, and returned no
false CLEAN. They identified the row and its unassessable outcome; all ten
summary counts per model matched the host scorer. They often put the
unavailable record ID in `citations`; the host scorer permits that ID in
`record` for provenance but requires an empty citation list. Radium omitted it
only with bold wording; Sonnet cited it in every variant. Sonnet had cited the
unavailable case correctly in the earlier paired pilot. That pilot used a
different prompt and request/CLI settings, so the observed difference is not
a controlled run-to-run regression or evidence that bold is a reliable fix.
There were no required-row omissions in these ten cases; the earlier paired
review cases still show that correct overall decisions can hide row errors.

| Treatment, both cases | Rendered characters | Radium input / output tokens | Sonnet input / output tokens |
| --- | ---: | ---: | ---: |
| Plain baseline | 18,964 | 9,170 / 1,828 | 12,708 / 1,104 |
| Heading only | 18,982 | 9,162 / 1,717 | 12,770 / 1,117 |
| Record link only | 19,021 | 9,181 / 1,710 | 12,758 / 1,238 |
| Italic only | 18,972 | 9,164 / 1,591 | 12,707 / 1,346 |
| Bold only | 18,980 | 9,141 / 1,629 | 12,748 / 1,169 |
| **All ten calls** | **94,919** | **45,818 / 8,475** | **63,691 / 5,974** |

Rendered characters count the developer and user turns, without the schema or
CLI framing. Input tokens sum the provider's ordinary, cache-created and
cache-read input counts. The Sonnet calls cost $0.2481 in total; the local
server reported no price. Claude Code's framing and the two tokenizers make
absolute cross-model counts incomparable. The Sonnet baseline also created
more cache than later calls, so per-variant billed cost is affected by call
order. One response per treatment and case provides no confidence interval or
causal emphasis estimate.

The new frontier records report canonical model `claude-sonnet-5-5` through
Claude Code 2.1.332; the provider did not report a source revision. The
request envelope specified temperature 0, 8,192 maximum output tokens, no
streaming and a strict JSON schema. Claude Code exposed the schema and model
controls but no temperature or maximum-output flag, so the realized values of
those two settings are unverified. No external shell, web or repository tools
were enabled. Schema structured output used Claude Code's internal
`stop_reason: tool_use`; all ten replies exited successfully and reported zero
web requests.

## Authoring guidance and limits

Use the bullet triage layout when it fits the findings: it used the fewest
input tokens here with the same observed decisions and attribution. Preserve
explicit record and dependency IDs in review prompts, and check every returned
row and citation against host data before a gate. The results do not support
choosing headings, links, bold or italics as a correctness control.

This pilot has only six triage and ten review fixtures, plus two one-factor
review cases, one call per condition, and no confidence interval. It measures
blocker decision and required-file attribution for triage, not exhaustive
finding coverage; triage has no citation field. The review scorer counts exact
rows and citations independently of the
overall decision. The paired source prompts are the rendered snapshots and
fixtures in this catalog. The exact prompt turns, parsed and raw replies, case
labels, token usage and model metadata are recorded in the
[`pilot-data/`](pilot-data/) JSONL files: `radium-layout`, `radium-review`,
`radium-factor`, `sonnet-layout`, `sonnet-review`, and
[`sonnet-factor`](pilot-data/sonnet-factor.jsonl). The `sonnet-factor` records
hold the complete normalized request, raw and parsed answer, reported usage
and cost, case and variant labels, and the approved request-set SHA-256
`76aebe5a53fb9932344e9c0052eee20c3a6a95fa9f9714ad572e4513ea4486b6`.
These records include the model's generated text as data; they are not
instructions for this repository.
