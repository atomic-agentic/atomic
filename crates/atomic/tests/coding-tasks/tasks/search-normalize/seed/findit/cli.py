"""python3 -m findit [--docs DIR] [--limit N] QUERY"""

import argparse
import sys

from findit.corpus import load
from findit.search import search


def main(argv):
    parser = argparse.ArgumentParser(prog="findit", description="Search a folder of text files.")
    parser.add_argument("--docs", default="docs", help="the folder of .txt files (default: docs)")
    parser.add_argument("--limit", type=int, help="show at most this many results")
    parser.add_argument("query", nargs="+", help="words, \"quoted phrases\" and -exclusions")
    args = parser.parse_args(argv)
    try:
        index = load(args.docs)
    except OSError as error:
        print(f"findit: {error}", file=sys.stderr)
        return 2
    results = search(index, " ".join(args.query), limit=args.limit)
    if not results:
        print("no matches", file=sys.stderr)
        return 1
    for doc_id, score in results:
        print(f"{doc_id}\t{score:.4f}")
    return 0
