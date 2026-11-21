#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for the project boundary around the lockfile that tools/deno_exec.py reads.

The offline catalog validator accepts an import only when a lockfile records it.
That lockfile is named by a string in a file: the default beside the manifest,
the one the manifest's runtime.deno names, or the one an atom's runtime.deno
names. So each of the three is tested twice: once kept inside the project, and
once driven out of it by a relative path, by a symlink or by an absolute path.
An atom's imports are checked against what the project itself recorded, so a
lockfile the project does not hold must never be read, whatever route leads
there.

The fixtures are inert: JSON, a manifest-shaped text and TypeScript-shaped text
under a temporary directory. Nothing here runs Deno, needs a provider or
reaches the network.

    python3 tools/deno_exec_test.py
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import deno_exec

# What the atom's script imports, and what a lockfile has to record for it.
TARGET = "npm:jsonc-parser@3.3.1"
HOST = "example.com"
OUTSIDE = "outside"  # the project's name for the directory beside it, holding a lockfile that passes


def lockfile(*targets: str) -> str:
    """A lockfile in the shape the checker reads, recording these specifiers."""
    return json.dumps({"specifiers": {t: t.rsplit("@", 1)[1] for t in targets}}, indent=2) + "\n"


def write(path: Path, text: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    return path


def link(target: Path, at: Path) -> Path:
    at.parent.mkdir(parents=True, exist_ok=True)
    at.symlink_to(target)
    return at


def manifest_text(deno: dict | None) -> str:
    """The manifest as text, for the fixture; the checker is handed the parsed config."""
    lines = ["atomic: 1", "name: lock-boundary"]
    if deno:
        lines += ["runtime:", "  deno:"] + [f"    {key}: {value}" for key, value in deno.items()]
    return "\n".join(lines) + "\n"


def make_project(root: Path, deno: dict | None = None, default_lock: str | None = lockfile(TARGET)) -> Path:
    """A project with a manifest, its default lockfile and one exec atom whose script imports TARGET.

    The manifest's runtime.deno block is `deno`; the default lockfile beside the
    manifest holds `default_lock`, or is left out when that is None. Both checks
    check_examples.py runs for such an atom are made here, so the settings the
    manifest carries are the ones the atom check reads.
    """
    write(root / "atomic.yaml", manifest_text(deno))
    config = {"atomic": 1, "name": "lock-boundary"}
    if deno is not None:
        config["runtime"] = {"deno": deno}
    deno_exec.check_manifest(config, root / "atomic.yaml")
    if default_lock is not None:
        write(root / "deno.lock", default_lock)
    write(root / "src" / "index.ts", f'import {{ parse }} from "{TARGET}"\n')
    return root


def check_atom(project: Path, lock: str | None = None) -> None:
    """The atom check: an exec atom whose script is src/index.ts, naming `lock` or naming none."""
    deno = {"lock": lock} if lock is not None else {}
    fm = {"exec": {"script": "../../src/index.ts"}, "runtime": {"deno": deno},
          "network": {"allow": [HOST]}, "effects": {"external": "read-only"}}
    path = write(project / "atoms/demo/atom.atom.astro", "---\n---\n")
    deno_exec.check_script(path, fm, project)


failures: list[str] = []


def case(what: str, run, refusal: str | None = None) -> None:
    """Runs one case: the check takes it, or refuses it saying `refusal`."""
    try:
        run()
    except ValueError as error:
        if refusal is None:
            failures.append(f"{what}: refused, though it should pass: {error}")
            print(f"FAIL {what}\n     refused: {error}")
        elif refusal in str(error):
            print(f"PASS {what}\n     {error}")
        else:
            failures.append(f"{what}: refused for the wrong reason: {error} (wanted {refusal!r})")
            print(f"FAIL {what}\n     refused for the wrong reason: {error} (wanted {refusal!r})")
    else:
        if refusal is None:
            print(f"PASS {what}")
        else:
            failures.append(f"{what}: accepted; the check let it through")
            print(f"FAIL {what}\n     accepted, though {refusal!r} should stop it")


def run_cases(root: Path) -> None:
    outside = write(root / OUTSIDE / "deno.lock", lockfile(TARGET))

    # The default lockfile beside the manifest.
    in_project = make_project(root / "default")
    case("the default lockfile beside the manifest is read", lambda: check_atom(in_project))
    escaped_default = make_project(root / "default-symlink", default_lock=None)
    link(outside, escaped_default / "deno.lock")
    case("a default lockfile that is a symlink out of the project is refused",
         lambda: check_atom(escaped_default), "outside the project")
    missing = make_project(root / "default-missing", default_lock=None)
    case("a default lockfile that is not there is still reported missing",
         lambda: check_atom(missing), "is missing")
    inside_link = make_project(root / "default-symlink-inside", default_lock=None)
    link(write(inside_link / "vendor/deno.lock", lockfile(TARGET)), inside_link / "deno.lock")
    case("a default lockfile that is a symlink to a file in the project is read",
         lambda: check_atom(inside_link))

    # The lockfile the manifest names.
    named = make_project(root / "manifest", {"lock": "vendor/deno.lock"})
    write(named / "vendor/deno.lock", lockfile(TARGET))
    case("a lockfile the manifest names inside the project is read", lambda: check_atom(named))
    case("a lockfile the manifest names that is not there is still reported missing",
         lambda: check_atom(make_project(root / "manifest-missing", {"lock": "vendor/absent.lock"})), "is missing")
    case("a lockfile the manifest reaches with ../ is refused",
         lambda: check_atom(make_project(root / "manifest-parent", {"lock": f"../{OUTSIDE}/deno.lock"})),
         "outside the project")
    via_link = make_project(root / "manifest-symlink", {"lock": "link.lock"})
    link(outside, via_link / "link.lock")
    case("a lockfile the manifest reaches through a symlink is refused",
         lambda: check_atom(via_link), "outside the project")
    case("a lockfile the manifest names by absolute path outside the project is refused",
         lambda: check_atom(make_project(root / "manifest-absolute", {"lock": str(outside)})),
         "outside the project")

    # The lockfile the atom names.
    atomic = make_project(root / "atom", {}, default_lock=lockfile())
    write(atomic / "atoms/demo/vendor.lock", lockfile(TARGET))
    case("a lockfile the atom names inside the project is read",
         lambda: check_atom(atomic, "vendor.lock"))
    # The atom names its lock relative to itself, so it is two levels down.
    case("a lockfile the atom reaches with ../ is refused",
         lambda: check_atom(make_project(root / "atom-parent", {}, default_lock=lockfile()),
                            f"../../../{OUTSIDE}/deno.lock"),
         "outside the project")
    atom_link = make_project(root / "atom-symlink", {}, default_lock=lockfile())
    link(outside, atom_link / "atoms/demo/link.lock")
    case("a lockfile the atom reaches through a symlink is refused",
         lambda: check_atom(atom_link, "link.lock"), "outside the project")

    # What keeps an in-project lockfile from being a blank cheque.
    thin = make_project(root / "thin", default_lock=lockfile("npm:other@1.0.0"))
    case("an in-project lockfile that does not record the import is refused",
         lambda: check_atom(thin), "is not recorded in")


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="atomic-deno-exec-test-") as tmp:
        run_cases(Path(tmp).resolve())
    if failures:
        print(f"\n{len(failures)} failed:")
        for one in failures:
            print(f"  {one}")
        return 1
    print("\nPASS lockfile boundary: the default, the manifest's and the atom's all stay in the project")
    return 0


if __name__ == "__main__":
    sys.exit(main())
