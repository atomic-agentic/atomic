#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Prepare isolated real Git inputs for the proposed Atomic coding example.

This prepares a fixture; it does not run Atomic or a model. Requires Python and Git.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess

SOURCE = Path(__file__).resolve().parents[1]
# The repository holding the examples, which a destination must stay outside.
WORKSPACE = next((parent for parent in SOURCE.parents
                  if (parent / "Cargo.toml").is_file() and (parent / "examples").is_dir()), SOURCE)


def prepare(destination: Path) -> dict:
    destination = destination.resolve()
    if destination == WORKSPACE or destination.is_relative_to(WORKSPACE):
        raise ValueError("choose a destination outside the source workspace")
    if destination.exists():
        raise ValueError(f"destination already exists: {destination}")
    destination.mkdir(parents=True)
    project = destination / "project"
    repository = destination / "repository"
    shutil.copytree(SOURCE, project, ignore=shutil.ignore_patterns(".git", "__pycache__", "*.pyc"))
    shutil.copytree(SOURCE / "fixtures" / "repo", repository,
                    ignore=shutil.ignore_patterns(".git", "__pycache__", "*.pyc"))

    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update({
        "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_AUTHOR_NAME": "Atomic Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
        "GIT_COMMITTER_NAME": "Atomic Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
        "GIT_AUTHOR_DATE": "2000-01-01T00:00:00+00:00",
        "GIT_COMMITTER_DATE": "2000-01-01T00:00:00+00:00",
    })

    def git(*args: str) -> str:
        result = subprocess.run(
            ["git", "-c", f"core.hooksPath={os.devnull}", "-c", "commit.gpgsign=false",
             "-c", "core.autocrlf=false", *args], cwd=repository, env=env,
            capture_output=True, text=True, check=True,
        )
        return result.stdout.strip()

    git("init", "--quiet", "--template=", "--object-format=sha1", "--initial-branch=fixture")
    git("add", "--", "pricing.py", "tests")
    git("commit", "--quiet", "-m", "Curated baseline with quantity bug")
    base = git("rev-parse", "HEAD")
    git("tag", "baseline", base)
    pricing = repository / "pricing.py"
    pricing.write_text(pricing.read_text().replace(
        "return unit_cents", "return unit_cents  # Quantity handling is still incomplete."))
    git("add", "--", "pricing.py")
    git("commit", "--quiet", "-m", "Deliberately incomplete candidate for review")
    candidate = git("rev-parse", "HEAD")
    git("tag", "candidate", candidate)
    git("checkout", "--quiet", "--detach", base)

    # Explicit setup replaces schema-only values in the generated copy. Atomic
    # itself has no placeholder substitution rule.
    for folder, pattern in (("fixtures", "*.input.json"), ("evals", "*.yaml")):
        for path in (project / folder).rglob(pattern):
            value = path.read_text().replace("1" * 40, base).replace("2" * 40, candidate)
            path.write_text(value)
    metadata = {"project": str(project), "repository": str(repository),
                "base": base, "candidate": candidate}
    (destination / "fixture.json").write_text(json.dumps(metadata, indent=2) + "\n")
    return metadata


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path, help="new directory outside the source workspace")
    args = parser.parse_args()
    try:
        metadata = prepare(args.destination)
    except (OSError, ValueError, subprocess.CalledProcessError) as exc:
        parser.exit(1, f"fixture setup failed: {exc}\n")
    print(json.dumps(metadata, indent=2))


if __name__ == "__main__":
    main()
