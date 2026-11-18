# Order report · v0.1

[The workflow](workflows/order-report.workflow.yaml) computes an order total,
asks a lite atom for a factual note, waits for a local gate, then renders a text
report. Computation and formatting use exec atoms; neither needs a model.
Amounts are integer cents, so no floating-point currency rounding is introduced.

This is a JSON pipeline. It does not rely on the later `atomic:file` type,
DuckDB, spreadsheets, runtime images or `/inputs` and `/outputs` mounts. It does
not place an order, write business data or send the report anywhere.

You can execute the deterministic parts now, from this directory:

```sh
python3 tools/total_order.py < fixtures/total-order/ordinary.input.json
python3 tools/render_report.py < fixtures/render-report/ordinary.input.json
```

Exec atoms use a literal command argument array. The runner supplies one JSON
value on stdin and validates one JSON value on stdout. Errors go to stderr and a
nonzero exit fails the attempt. The pinned project bundle is the working
directory; scripts here do not write files or open network connections. Python 3
is a host prerequisite for this local example. Reproducible runtime images arrive
with M52; declaring one here would make this a later-phase example.

Future atomic command:

```sh
atomic run order-report --input fixtures/total-order/ordinary.input.json
```

The gate accepts a report, not a purchase. If a later workflow sends the report,
that send needs an effect identity and the effect ledger; the gate by itself
does not prevent duplicate delivery after a crash.
