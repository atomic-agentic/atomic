#!/usr/bin/env python3
"""Stage 1: copy the day's export into OUT_DIR/staged, cleaned up.

    python3 extract.py DATA_DIR OUT_DIR
"""

import os
import sys

from pipeline import csvio, schema, stage
from pipeline.stage import run_stage


def extract(data_dir, out_dir, log):
    for name, columns in schema.INPUTS:
        source = stage.data_file(data_dir, name)
        if not os.path.isfile(source):
            log.error("missing input %s in %s", name, data_dir)
            continue
        rows = list(csvio.read_rows(source, columns))
        written = csvio.write_rows(stage.staged_file(out_dir, name), columns, rows)
        log.info("extracted %d row(s) from %s", written, name)


if __name__ == "__main__":
    sys.exit(run_stage("extract", extract))
