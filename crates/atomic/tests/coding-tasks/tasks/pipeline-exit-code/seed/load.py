#!/usr/bin/env python3
"""Stage 4: write the nightly report, OUT_DIR/report.txt, from OUT_DIR/totals.csv.

    python3 load.py DATA_DIR OUT_DIR
"""

import os
import sys
from decimal import Decimal

from pipeline import csvio, stage
from pipeline.stage import run_stage
from pipeline.totals import TOTAL_COLUMNS


def report_lines(rows):
    width = max([len(row["name"]) for row in rows] + [len("Customer")])
    lines = ["Nightly totals (EUR)", "", f"{'Customer':<{width}}  Orders  {'Total':>9}"]
    grand = Decimal("0")
    for row in rows:
        lines.append(f"{row['name']:<{width}}  {row['orders']:>6}  {row['total_eur']:>9}")
        grand += Decimal(row["total_eur"])
    lines += ["", f"Grand total: {grand:.2f}"]
    return lines


def load(data_dir, out_dir, log):
    source = stage.totals_file(out_dir)
    if not os.path.isfile(source):
        log.error("no totals.csv: run transform first")
        return
    rows = list(csvio.read_rows(source, TOTAL_COLUMNS))
    if not rows:
        log.error("totals.csv has no rows")
        return
    with open(stage.report_file(out_dir), "w", encoding="utf-8") as f:
        f.write("\n".join(report_lines(rows)) + "\n")
    log.info("report for %d customer(s)", len(rows))


if __name__ == "__main__":
    sys.exit(run_stage("load", load))
