"""Run `pricer batch` in this process and count how often each .ini file is opened.

    python3 hidden_count.py JOBS COUNTS_JSON
"""

import builtins
import collections
import contextlib
import io
import json
import os
import sys

counts = collections.Counter()
real_open = builtins.open
real_io_open = io.open


def counted(opener):
    def wrapper(file, *args, **kwargs):
        if isinstance(file, (str, bytes, os.PathLike)):
            name = os.fsdecode(file)
            if name.endswith(".ini"):
                counts[os.path.basename(name)] += 1
        return opener(file, *args, **kwargs)

    return wrapper


builtins.open = counted(real_open)
io.open = counted(real_io_open)

from pricer import cli  # noqa: E402

out = io.StringIO()
with contextlib.redirect_stdout(out):
    status = cli.main(["--config", "config", "batch", sys.argv[1]])
builtins.open = real_open
io.open = real_io_open
with real_open(sys.argv[2], "w") as f:
    json.dump({"status": status, "counts": dict(counts), "stdout": out.getvalue()}, f)
