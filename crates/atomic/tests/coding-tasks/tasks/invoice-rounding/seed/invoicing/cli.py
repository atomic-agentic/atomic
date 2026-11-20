"""The command line: render, totals and check."""

import json
import sys

from . import loader, render, totals

USAGE = """usage: python3 -m invoicing render FILE
       python3 -m invoicing totals FILE
       python3 -m invoicing check FILE...

render   print the invoice as text
totals   print its amounts as JSON
check    check invoices, printing "FILE: ok" for each good one

Exit codes: 0 when every file is a valid invoice, 1 when one is not, 2 for bad usage."""


def main(argv, out=None, err=None):
    out = out or sys.stdout
    err = err or sys.stderr
    if not argv or argv[0] in ("-h", "--help"):
        print(USAGE, file=out if argv else err)
        return 0 if argv else 2
    command, files = argv[0], argv[1:]
    if command in ("render", "totals") and len(files) != 1:
        print(f"{command} takes one FILE\n\n{USAGE}", file=err)
        return 2
    if command == "check" and not files:
        print(f"check takes at least one FILE\n\n{USAGE}", file=err)
        return 2
    if command not in ("render", "totals", "check"):
        print(f"unknown command {command!r}\n\n{USAGE}", file=err)
        return 2

    status = 0
    for path in files:
        try:
            invoice = loader.load(path)
        except ValueError as e:
            print(f"{path}: {e}", file=err)
            status = 1
            continue
        if command == "check":
            print(f"{path}: ok", file=out)
            continue
        amounts = totals.compute_totals(invoice)
        if command == "render":
            out.write(render.render(invoice, amounts))
        else:
            print(json.dumps(totals.as_dict(invoice, amounts)), file=out)
    return status
