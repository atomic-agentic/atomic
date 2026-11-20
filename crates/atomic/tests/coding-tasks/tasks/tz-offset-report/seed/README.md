# daylog

Daily sales reports for our sites from their event logs. Each site keeps its books by its own local
calendar day, so an event belongs to the day it happened on at the site, not the day in UTC.

## Usage

```
python3 -m daylog report --site harbour data/events-harbour.jsonl
python3 -m daylog report --offset +05:30 data/events-delta.jsonl    any offset, without a site
python3 -m daylog sites                                             every site and its offset
python3 -m daylog check data/events-harbour.jsonl                   report bad lines only
```

`--sites FILE` picks another sites file (default: `sites.json`). Write a negative offset with an
equals sign, as in `--offset=-03:30`, so that it is not taken for an option.

## Event logs

One JSON object a line:

```
{"ts": "2026-03-02T03:10:00Z", "kind": "sale", "amount": "12.50"}
```

- `ts` is in UTC: it ends in `Z` or `+00:00`, and has seconds.
- `kind` is `sale` or `refund`.
- `amount` is a string with at most two decimal places, never negative.

Blank lines are skipped. A bad line is reported on stderr as `LOG:N: message` and left out of the
report, and the exit status is then 1.

## Sites

`sites.json` maps a site's name to its UTC offset and a display name:

```
{"harbour": {"offset": "-03:30", "name": "Harbour Street"}}
```

An offset is written `+HH:MM` or `-HH:MM`: a sign, two-digit hours, a colon and two-digit minutes.
Minutes run from 00 to 59. Offsets run from `-12:00` to `+14:00`, both included. `-00:30` is half an
hour behind UTC. Anything else, such as `+5:30`, `05:30`, `+0530`, `+24:00`, `+14:30` or `-12:45`, is
rejected with `ValueError`.

## Report

```
Harbour Street (UTC-03:30): 5 events
day         events      sales    refunds        net
2026-03-01       3      41.50       0.00      41.50
2026-03-02       2      10.00       2.25       7.75
```

One row for each local day that has events, oldest first. `net` is sales minus refunds.

## Tests

```
python3 -m unittest discover -s tests -t .
```
