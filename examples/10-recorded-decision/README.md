# A decision recorded with its evidence · v0.7

A release-note writer with one recorded decision. [The atom](atoms/write-release-note.atom.astro)
is `kind: lite`: it reads the change's title and description and has no access to
the repository. That is a choice, and it holds only while the atom keeps passing
its evals. [The manifest](atomic.yaml) says so:

```yaml
decisions:
  release-notes-without-repo-access:
    chose: kind lite for atom write-release-note, so it reads the change description and not the repository
    rests_on:
      - "evals['write-release-note'].pass_rate >= 0.9"   # CEL over stored eval results
    evidence_max_age: 30d
    review_by: 2027-02-01
```

- `chose` states the decision in words, for the person who reads it later.
- `rests_on` lists conditions over Atomic's own evidence, here the pass rate of
  [the atom's eval cases](evals/write-release-note/cases.jsonl).
- `evidence_max_age` is how old that evidence may be before it no longer counts.
- `review_by` is the date to look at the decision again whatever the evidence says.

Future commands, from this directory:

```sh
atomic check
atomic eval write-release-note
atomic stale
atomic stale --as-of 2027-03-01
```

`atomic stale` is planned to list each decision that needs another look, with the
reason: its review date has passed, its evidence is older than the limit, the
evidence was produced on a different model version or prompt than the one now in
use, or a condition no longer holds. With `--as-of 2027-03-01` this decision is
listed because its review date has passed. The wording of the report is not
settled.

`atomic check` never reads the clock, so checking an unchanged repository always
gives the same answer. Staleness is a separate command, meant to run on a
schedule.

The catalog checker validates the block's shape, that each condition names an
atom with eval cases, and that the review date is a real date. It compares that
date only with dates passed to it. It does not evaluate the CEL condition or
measure a pass rate; the JSONL outputs are illustrative, not a recording of a
model. The block's keys follow the plan's sketch and may change.
