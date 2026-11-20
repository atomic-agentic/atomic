"""Reading and writing the pipeline's CSV files."""

import csv
import logging
import os

log = logging.getLogger("pipeline.csvio")


def read_rows(path, columns):
    """Yield each data row of a CSV file as a dict, with surrounding spaces stripped.

    The header must be exactly `columns`. A row with the wrong number of fields is logged as an error, with
    its line number, and skipped. Blank lines are skipped.
    """
    with open(path, newline="", encoding="utf-8") as f:
        reader = csv.reader(f)
        try:
            header = [name.strip() for name in next(reader)]
        except StopIteration:
            log.error("%s: empty file", path)
            return
        if header != list(columns):
            log.error("%s: header is %s, expected %s", path, ",".join(header), ",".join(columns))
            return
        for fields in reader:
            if not fields or all(not field.strip() for field in fields):
                continue
            if len(fields) != len(columns):
                log.error("%s:%d: expected %d fields, got %d", path, reader.line_num, len(columns), len(fields))
                continue
            yield {name: value.strip() for name, value in zip(columns, fields)}


def write_rows(path, columns, rows):
    """Write rows (dicts) under a header, creating the directory. Returns the number of rows."""
    directory = os.path.dirname(path)
    if directory:
        os.makedirs(directory, exist_ok=True)
    count = 0
    with open(path, "w", newline="", encoding="utf-8") as f:
        writer = csv.writer(f, lineterminator="\n")
        writer.writerow(columns)
        for row in rows:
            writer.writerow([row[name] for name in columns])
            count += 1
    return count
