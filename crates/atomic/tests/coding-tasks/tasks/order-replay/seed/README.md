# orders

Tools for the order service's event logs at Fennick & Daughters, a made-up mail-order shop. The
service writes one JSON line per accepted event. These tools replay a log to rebuild each order's
state, report events the state machine would not allow, and summarise a day's orders.

```
python3 -m orders replay LOG...     # each order's final state, and any invalid transitions
python3 -m orders summary LOG...    # how many orders end in each state
python3 -m orders states            # the state machine, as a table
```

Exit codes: 0 when every event is a valid transition, 1 when `replay` or `summary` found an invalid
one, 2 for bad usage or a log that cannot be read (a bad line, an unknown event kind, conflicting
events).

Run the tests with:

```
python3 -m unittest discover -s tests -t .
```

## Log lines

```json
{"seq": 4107, "ts": "2026-10-04T09:15:02Z", "order": "A-1001", "kind": "pay"}
```

- `seq` is assigned by the order service when it accepts an event. It is unique, and **it is the
  only order that counts**: event 4108 happened after event 4107.
- `ts` is the clock of the host that wrote the line, to the second. Hosts can be a few seconds apart,
  and several events can share a second, so `ts` is for people to read, not for ordering.
- Each host writes its own file, and a day's log is those files concatenated, so lines are not in
  `seq` order. A host that retries a write can write the same line twice: an exact duplicate is
  dropped, but two different lines with one `seq` make the log unreadable.

## The state machine

| From             | Event     | To               |
|------------------|-----------|------------------|
| (new order)      | `create`  | `pending`        |
| `pending`        | `pay`     | `paid`           |
| `paid`           | `ship`    | `shipped`        |
| `shipped`        | `deliver` | `delivered`      |
| `pending`        | `cancel`  | `cancelled`      |
| `paid`           | `cancel`  | `cancelled`      |
| `paid`           | `refund`  | `refunded`       |
| `delivered`      | `return`  | `return_pending` |
| `return_pending` | `refund`  | `refunded`       |

Any other event is an invalid transition. Replay reports it and leaves the order in the state it was
in. An order that has had no `create` is in the state `none`.

## Output

`replay` prints each order's final state, ordered by order id, then each invalid transition in `seq`
order, then a count:

```
A-1001 delivered
A-1002 cancelled
invalid: seq 4120 order A-1002 ship from cancelled
2 orders, 9 events, 1 invalid
```

`summary` prints the number of orders in each state that has any, in the order of the table above,
then the time span the log covers:

```
pending 1
delivered 3
cancelled 1
5 orders, from 2026-10-04T08:00:11Z to 2026-10-04T17:42:09Z
```

## Layout

- `orders/states.py`: the states.
- `orders/transitions.py`: the table above.
- `orders/events.py`: one log line, parsed and checked.
- `orders/logfile.py`: reading log files.
- `orders/replay.py`: rebuilding states from events.
- `orders/report.py`: the output above.
- `orders/timeutil.py`: timestamps.
- `orders/cli.py`: the command line.
