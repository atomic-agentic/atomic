#!/usr/bin/env python3
"""Stage 3: total each customer's orders in euros, into OUT_DIR/totals.csv.

    python3 transform.py DATA_DIR OUT_DIR
"""

import sys

from pipeline import csvio, schema, stage
from pipeline.stage import run_stage
from pipeline.totals import TOTAL_COLUMNS, compute_totals


def transform(data_dir, out_dir, log):
    customers = list(csvio.read_rows(stage.staged_file(out_dir, schema.CUSTOMERS), schema.CUSTOMER_COLUMNS))
    orders = list(csvio.read_rows(stage.staged_file(out_dir, schema.ORDERS), schema.ORDER_COLUMNS))
    rows = compute_totals(orders, customers)
    written = csvio.write_rows(stage.totals_file(out_dir), TOTAL_COLUMNS, rows)
    log.info("totals for %d customer(s)", written)


if __name__ == "__main__":
    sys.exit(run_stage("transform", transform))
