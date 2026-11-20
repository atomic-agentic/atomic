"""Writing tickets out as CSV or JSON lines."""

import csv
import json

FIELDS = ("id", "updated_at", "status", "title")
FORMATS = ("csv", "jsonl")


def write_tickets(tickets, out, fmt="csv"):
    """Write tickets to the text stream out. Returns how many were written."""
    if fmt not in FORMATS:
        raise ValueError(f"unknown format {fmt!r}")
    count = 0
    if fmt == "csv":
        writer = csv.writer(out, lineterminator="\n")
        writer.writerow(FIELDS)
        for ticket in tickets:
            writer.writerow([getattr(ticket, name) for name in FIELDS])
            count += 1
    else:
        for ticket in tickets:
            record = {name: getattr(ticket, name) for name in FIELDS}
            out.write(json.dumps(record) + "\n")
            count += 1
    return count
