# nightly-totals

The nightly job that turns the day's order export into per-customer totals in euros. It runs as four
stages, one Python script each, driven by `run.sh`:

| Stage | Script | Reads | Writes |
|---|---|---|---|
| extract | `extract.py` | `DATA_DIR/customers.csv`, `DATA_DIR/orders.csv` | `OUT_DIR/staged/*.csv` |
| validate | `validate.py` | `OUT_DIR/staged/*.csv` | nothing |
| transform | `transform.py` | `OUT_DIR/staged/*.csv` | `OUT_DIR/totals.csv` |
| load | `load.py` | `OUT_DIR/totals.csv` | `OUT_DIR/report.txt` |

## Running it

    sh run.sh DATA_DIR [OUT_DIR]

`OUT_DIR` defaults to `out`. Each stage can also be run on its own, with the same two arguments, as in
`python3 validate.py data/good out`, as long as the stages before it have run.

`data/good` is a small export that goes through cleanly. `data/bad` holds orders that break the rules
below, for trying the error path.

## Rules

- **Logs.** Every stage logs to stderr as `LEVEL logger: message`, through a child of the `pipeline`
  logger (`pipeline.extract`, `pipeline.validate`, ...). The shared modules in `pipeline/` log through
  their own children of it (`pipeline.csvio`, `pipeline.totals`, ...). `run.sh` keeps each stage's output,
  stdout and stderr together, in `OUT_DIR/<stage>.log`, and shows it on the terminal too.
- **A stage fails when anything it ran logged an ERROR**, whether the stage itself or one of the shared
  modules. It still finishes its work, logging every problem it finds, and then exits with status 1. A stage
  that logged no error exits 0. A stage that crashes logs the exception as an error and exits 1.
- **`run.sh` stops at the first stage that fails.** It prints `pipeline failed: <stage>` to stderr and
  exits 1, and the later stages do not run, so they leave no log. When every stage passes it prints
  `pipeline ok` and exits 0. Logs from an earlier run in the same `OUT_DIR` are removed first.
- `run.sh` without a data directory prints its usage and exits 2.

## Data

`customers.csv`: `customer_id,name,region`. `orders.csv`: `order_id,customer_id,date,amount,currency`.

An order is valid when its `order_id` is a positive integer and unique, its customer exists, its `date` is
a real date written `YYYY-MM-DD`, its `amount` is a decimal number of at least 0 with at most two decimal
places, and its `currency` is three capital letters. A row with the wrong number of fields is reported by
extract and left out. Transform converts each amount to euros with the table in `pipeline/rates.py`; a
currency that is not in it is an error there.

## Tests

    python3 -m unittest discover -s tests -t .
