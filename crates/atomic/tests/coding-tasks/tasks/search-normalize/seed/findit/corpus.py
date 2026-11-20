"""Reading a folder of .txt files into an index."""

import os

from findit.index import Index


def load(folder):
    index = Index()
    for name in sorted(os.listdir(folder)):
        if not name.endswith(".txt"):
            continue
        with open(os.path.join(folder, name), encoding="utf-8") as f:
            index.add(name[: -len(".txt")], f.read())
    return index
