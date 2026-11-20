#!/usr/bin/env python3
"""Stage 2: check every staged customer and order against the rules in README.md.

    python3 validate.py DATA_DIR OUT_DIR

Logs one error per problem, and checks every row, so a single run lists everything to fix.
"""

import os
import sys

from pipeline import csvio, schema, stage
from pipeline.stage import run_stage


def load_staged(out_dir, name, columns, log):
    path = stage.staged_file(out_dir, name)
    if not os.path.isfile(path):
        log.error("no staged %s: run extract first", name)
        return []
    return list(csvio.read_rows(path, columns))


def validate(data_dir, out_dir, log):
    customers = load_staged(out_dir, schema.CUSTOMERS, schema.CUSTOMER_COLUMNS, log)
    orders = load_staged(out_dir, schema.ORDERS, schema.ORDER_COLUMNS, log)
    customer_ids = set()
    for row in customers:
        for problem in schema.customer_problems(row):
            log.error(problem)
        customer_ids.add(row["customer_id"])
    seen = set()
    bad = 0
    for row in orders:
        problems = schema.order_problems(row, customer_ids, seen)
        for problem in problems:
            log.error(problem)
        bad += bool(problems)
    log.info("checked %d customer(s) and %d order(s): %d bad order(s)", len(customers), len(orders), bad)


if __name__ == "__main__":
    sys.exit(run_stage("validate", validate))
