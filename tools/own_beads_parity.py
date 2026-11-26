#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Compare this repository's bd ready IDs with a throwaway atomic import.

The bd project is read-only. All atomic state, credentials and imported issue
data live under a private temporary directory that is removed on exit.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import pty
import re
import sqlite3
import subprocess
import sys
import tempfile
import time


def command(argv: list[str], *, cwd: pathlib.Path, env: dict[str, str] | None = None,
            terminal_stdin: bool = False) -> str:
    master = slave = None
    try:
        if terminal_stdin:
            master, slave = pty.openpty()
        result = subprocess.run(
            argv,
            cwd=cwd,
            env=env,
            stdin=slave if terminal_stdin else subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
    finally:
        if master is not None:
            os.close(master)
        if slave is not None:
            os.close(slave)
    if result.returncode:
        raise RuntimeError(
            f"{pathlib.Path(argv[0]).name} {argv[1:3]} failed "
            f"(exit {result.returncode}); inspect this command in the private test environment"
        )
    return result.stdout


def ready_ids(bd: str, project: pathlib.Path) -> list[str]:
    data = json.loads(command(
        [bd, "--readonly", "-C", str(project), "ready", "--limit", "0", "--json"],
        cwd=project,
    ))
    ids = [item["id"] for item in data]
    if len(ids) != len(set(ids)):
        raise RuntimeError("bd ready returned duplicate IDs")
    return sorted(ids)


def export(bd: str, project: pathlib.Path, path: pathlib.Path) -> tuple[str, int]:
    command(
        [bd, "--readonly", "-C", str(project), "export", "--output", str(path)],
        cwd=project,
    )
    data = path.read_bytes()
    lines = [json.loads(line) for line in data.splitlines() if line.strip()]
    ids = [line["id"] for line in lines if line.get("_type", "issue") == "issue"]
    if not ids or len(ids) != len(set(ids)):
        raise RuntimeError("bd export is empty or has duplicate issue IDs")
    return hashlib.sha256(data).hexdigest(), len(ids)


def atomic_environment(root: pathlib.Path) -> dict[str, str]:
    env = os.environ.copy()
    for name in (
        "ATOMIC_DATABASE_URL", "ATOMIC_DATABASE_SCHEMA", "ATOMIC_PROFILE",
        "ATOMIC_MODEL_BASE_URL", "ATOMIC_MODEL_API_KEY_ENV", "ATOMIC_MODEL",
        "OPENAI_API_KEY", "OPENAI_BASE_URL", "CODEX_HOME", "XDG_DATA_HOME",
    ):
        env.pop(name, None)
    env.update({
        "HOME": str(root / "home"),
        "ATOMIC_HOME": str(root / "state"),
        "XDG_CONFIG_HOME": str(root / "config"),
        "ATOMIC_CREDENTIAL_STORE": "file",
        "ATOMIC_DETECT_OLLAMA_ADDR": "127.0.0.1:9",
        "USER": "atomic-parity",
    })
    return env


def graph_ready_ids(database: pathlib.Path, source: pathlib.Path) -> tuple[list[str], int, int]:
    rust = source.read_text()
    match = re.search(r'const READY_ITEMS_SQL: &str = "([^"]+)";', rust)
    if not match or match.group(1).count("?") != 4:
        raise RuntimeError("the graph ready query changed; update this probe")
    now_ms = int(time.time() * 1000)
    with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as db:
        leases = db.execute("SELECT COUNT(*) FROM atomic_graph_leases").fetchone()[0]
        if leases:
            raise RuntimeError("throwaway import unexpectedly has live graph leases")
        ids = [row[0] for row in db.execute(
            # This exact source constant is called by SqliteStore::ready with
            # include_unheld_in_progress=false. Import creates no leases.
            match.group(1).replace("SELECT i.*", "SELECT i.id"),
            (now_ms, 0, now_ms, 2**31 - 1),
        )]
        items = db.execute("SELECT COUNT(*) FROM atomic_graph_items").fetchone()[0]
        in_progress = db.execute(
            "SELECT COUNT(*) FROM atomic_graph_items WHERE status='in_progress'"
        ).fetchone()[0]
    if len(ids) != len(set(ids)):
        raise RuntimeError("atomic ready query returned duplicate IDs")
    return sorted(ids), items, in_progress


def run(args: argparse.Namespace) -> int:
    project = args.bd_project.resolve(strict=True)
    binary = args.atomic_bin.resolve(strict=True)
    source = args.source.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError("--atomic-bin is not executable")
    with tempfile.TemporaryDirectory(prefix="atomic-own-beads-parity-") as temp:
        root = pathlib.Path(temp)
        root.chmod(0o700)
        for name in ("home", "config", "project"):
            (root / name).mkdir(mode=0o700)
        (root / "project" / "atomic.yaml").write_text(
            "atomic: 1\nname: own-beads-parity\n"
        )
        first = ready_ids(args.bd, project)
        export_hash, exported = export(args.bd, project, root / "issues.jsonl")
        second = ready_ids(args.bd, project)
        second_hash, second_count = export(args.bd, project, root / "issues-again.jsonl")
        if first != second or export_hash != second_hash or exported != second_count:
            raise RuntimeError("bd changed during the two read-only snapshots; retry in a quiet window")
        env = atomic_environment(root)
        def atomic(parts: list[str], *, terminal_stdin: bool = False) -> str:
            return command(
                [str(binary), *parts], cwd=root / "project", env=env,
                terminal_stdin=terminal_stdin,
            )

        atomic(["gates", "enroll"], terminal_stdin=True)
        report = json.loads(atomic(["--json", "graph", "import", str(root / "issues.jsonl")]))
        repeated = json.loads(atomic(["--json", "graph", "import", str(root / "issues.jsonl")]))
        if report["items"] != exported or report["left_as_is"]:
            raise RuntimeError("atomic import did not accept the complete bd export")
        if repeated["added"] or repeated["left_as_is"]:
            raise RuntimeError("a second import changed the throwaway graph")
        databases = list((root / "state" / "private").glob("*/atomic.db"))
        if len(databases) != 1:
            raise RuntimeError(f"expected one throwaway atomic database, got {len(databases)}")
        atomic_ready, imported, in_progress = graph_ready_ids(databases[0], source)
        missing = sorted(set(first) - set(atomic_ready))
        unexpected = sorted(set(atomic_ready) - set(first))
        missing_by_status: dict[str, int] = {}
        missing_blocker_kinds: dict[str, int] = {}
        if missing:
            placeholders = ",".join("?" for _ in missing)
            with sqlite3.connect(f"file:{databases[0]}?mode=ro", uri=True) as db:
                missing_by_status = dict(db.execute(
                    f"SELECT status, COUNT(*) FROM atomic_graph_items WHERE id IN ({placeholders}) "
                    "GROUP BY status", missing,
                ))
                missing_blocker_kinds = dict(db.execute(
                    "SELECT e.kind, COUNT(DISTINCT b.source) FROM atomic_graph_blocked b "
                    "JOIN atomic_graph_edges e ON e.source=b.source AND e.target=b.target "
                    f"WHERE b.source IN ({placeholders}) GROUP BY e.kind", missing,
                ))
        summary = {
            "bd_export_sha256": export_hash,
            "exported_issues": exported,
            "imported_issues": imported,
            "imported_in_progress_without_leases": in_progress,
            "bd_ready": len(first),
            "bd_ready_ids": first,
            "atomic_ready": len(atomic_ready),
            "atomic_ready_ids": atomic_ready,
            "missing_from_atomic": missing,
            "missing_by_status": missing_by_status,
            "missing_blocker_kinds": missing_blocker_kinds,
            "unexpected_in_atomic": unexpected,
            "parity": not missing and not unexpected and imported == exported,
        }
        print(json.dumps(summary, indent=2, sort_keys=True))
        return 0 if summary["parity"] else 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bd-project", type=pathlib.Path, required=True,
                        help="live repository checkout whose bd data is read only")
    parser.add_argument("--atomic-bin", type=pathlib.Path, required=True,
                        help="current atomic executable built from the source under test")
    parser.add_argument("--bd", default="bd", help="bd executable (default: bd)")
    parser.add_argument("--source", type=pathlib.Path,
                        default=pathlib.Path(__file__).resolve().parents[1]
                        / "crates/atomic-graph/src/sqlite.rs")
    args = parser.parse_args()
    try:
        return run(args)
    except (OSError, ValueError, KeyError, RuntimeError, sqlite3.Error) as error:
        print(f"own-beads parity refused: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
