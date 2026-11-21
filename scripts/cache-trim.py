#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Trim target/ to what CI's build cache keeps: the dependencies.

    scripts/cache-trim.py                     # in CI: trim the workspace's target directory
    scripts/cache-trim.py --force             # the same outside CI
    scripts/cache-trim.py --metadata FILE     # with `cargo metadata --no-deps` output from FILE

CI saves target/ in its cache so that a job rebuilds only the workspace's own
crates. Those rebuild in every job anyway, since a checkout gives every source
file a new time, so their artifacts, the test binaries above all, would only
make the cache larger. This keeps each profile's deps/, build/ and
.fingerprint/ entries for dependencies and the V8 mirror, and removes the
workspace's own entries (found by the hashes in their .fingerprint/
directories), incremental state, linked binaries and everything else.

Everything else means everything: logs, scripts or notes kept under a local
target/ go too. So it runs only where GITHUB_ACTIONS is set, as on a CI
runner, unless given --force.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# What a profile directory keeps, and what target/ keeps beside the profiles.
PROFILE_KEEPS = {"deps", "build", ".fingerprint"}
TOP_KEEPS = {"v8-mirror", "CACHEDIR.TAG", ".rustc_info.json"}


def remove(path):
    if os.path.isdir(path) and not os.path.islink(path):
        shutil.rmtree(path)
    else:
        os.remove(path)


def is_profile(path):
    return any(os.path.isdir(os.path.join(path, d)) for d in PROFILE_KEEPS)


def trim_profile(profile, members):
    """Remove the workspace's entries from one profile directory."""
    removed = 0
    for name in os.listdir(profile):
        if name not in PROFILE_KEEPS:
            remove(os.path.join(profile, name))
            removed += 1
    # Each unit's .fingerprint/ and build/ directory is <package>-<hash>, and
    # its files in deps/ carry the same hash: <target>-<hash>[.ext].
    hashes = set()
    for sub in ("build", ".fingerprint"):
        directory = os.path.join(profile, sub)
        if not os.path.isdir(directory):
            continue
        for name in os.listdir(directory):
            package, _, digest = name.rpartition("-")
            if package in members:
                if sub == ".fingerprint":
                    hashes.add(digest)
                remove(os.path.join(directory, name))
                removed += 1
    deps = os.path.join(profile, "deps")
    if os.path.isdir(deps):
        for name in os.listdir(deps):
            stem = name.split(".", 1)[0]
            if stem.rpartition("-")[2] in hashes:
                remove(os.path.join(deps, name))
                removed += 1
    return removed


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--metadata", help="read `cargo metadata --no-deps` output from this file")
    parser.add_argument("--force", action="store_true", help="trim outside CI too")
    args = parser.parse_args()
    if not (args.force or os.environ.get("GITHUB_ACTIONS")):
        print("scripts/cache-trim.py: this deletes everything in target/ but the dependencies' "
              "artifacts; it runs only in CI (GITHUB_ACTIONS set) or with --force", file=sys.stderr)
        return 2
    if args.metadata:
        with open(args.metadata, encoding="utf-8") as f:
            meta = json.load(f)
    else:
        out = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
            cwd=ROOT, stdout=subprocess.PIPE, check=True,
        ).stdout
        meta = json.loads(out)
    target = meta["target_directory"]
    members = {p["name"] for p in meta["packages"]}
    if not os.path.isdir(target):
        print(f"scripts/cache-trim.py: no {target}; nothing to trim")
        return 0
    removed = 0
    for name in os.listdir(target):
        path = os.path.join(target, name)
        if name in TOP_KEEPS:
            continue
        if os.path.isdir(path) and is_profile(path):
            removed += trim_profile(path, members)
        else:
            remove(path)
            removed += 1
    print(f"scripts/cache-trim.py: removed {removed} entries from {target}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
