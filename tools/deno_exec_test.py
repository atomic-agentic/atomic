#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for the project boundary around the lockfile that tools/deno_exec.py reads.

The offline catalog validator accepts an import only when a lockfile records it.
That lockfile is named by a string in a file: the default beside the manifest,
the one the manifest's runtime.deno names, or the one an atom's runtime.deno
names. So each of the three is tested twice: once kept inside the project, and
once driven out of it by a relative path, by a symlink or by an absolute path.
The manifest's own choice is checked when the manifest is read, so a lockfile
outside the project is refused even in a project with no atom in it; and what
names it has to be a path, so a number, a list, a bool, an empty string or
nothing at all is refused by name instead of raising a Python error.

Settling a path -- following the links above it before it is checked or read --
is done here rather than by `Path.resolve` alone, because resolve answers a
symlink loop differently by Python version, and leaves a name that is not there
yet against whatever its directory links to. So a loop, a path with nothing at
the end, a link to nothing outside the project, a project that is itself a
symlink, a directory component that is one, and a neighbouring project whose
name starts with the project's are each answered the same way on every Python.

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
# Every refusal about a lock closes with this, so that whoever is shown it knows
# what to write instead of what was wrong.
FIX = "name a lockfile inside the project, such as deno.lock"


def lockfile(*targets: str) -> str:
    """A lockfile in the shape the checker reads, recording these specifiers."""
    return json.dumps({"specifiers": {t: t.rsplit("@", 1)[1] for t in targets}}, indent=2) + "\n"


def write(path: Path, text: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    return path


def link(target: Path | str, at: Path) -> Path:
    at.parent.mkdir(parents=True, exist_ok=True)
    at.symlink_to(target)
    return at


def manifest_text(deno: dict | None) -> str:
    """The manifest as text, for the fixture; the checker is handed the parsed config."""
    lines = ["atomic: 1", "name: lock-boundary"]
    if deno:
        written = {key: f"[{', '.join(map(str, value))}]" if isinstance(value, list) else value
                   for key, value in deno.items()}
        lines += ["runtime:", "  deno:"] + [f"    {key}: {value}" for key, value in written.items()]
    return "\n".join(lines) + "\n"


def manifest_config(deno: dict) -> dict:
    return {"atomic": 1, "name": "lock-boundary", "runtime": {"deno": deno}}


def check_manifest_only(root: Path, deno: dict) -> None:
    """The manifest check on its own, on a project with no atom and no script in it.

    This is the route check_examples.py takes for every manifest before it looks
    at a single atom, so a manifest that fails it never reaches an atom.
    """
    write(root / "atomic.yaml", manifest_text(deno))
    deno_exec.check_manifest(manifest_config(deno), root / "atomic.yaml")


def make_project(root: Path, deno: dict | None = None, default_lock: str | None = lockfile(TARGET)) -> Path:
    """A project with a manifest, its default lockfile and one exec atom whose script imports TARGET.

    The manifest's runtime.deno block is `deno`; the default lockfile beside the
    manifest holds `default_lock`, or is left out when that is None. Both checks
    check_examples.py runs for such an atom are made here, so the settings the
    manifest carries are the ones the atom check reads.
    """
    write(root / "atomic.yaml", manifest_text(deno))
    if deno is not None:
        deno_exec.check_manifest(manifest_config(deno), root / "atomic.yaml")
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
    """Runs one case: the check takes it, or refuses it saying `refusal`.

    A check that raises TypeError or OSError where it should refuse is a failure
    and not a refusal: it says nothing about the file or the key at fault.
    """
    try:
        run()
    except (ValueError, TypeError, OSError) as error:
        if not isinstance(error, ValueError):
            failures.append(f"{what}: raised {type(error).__name__} where it should have refused: {error}")
            print(f"FAIL {what}\n     raised {type(error).__name__} instead of refusing: {error}")
        elif refusal is None:
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
    default_loop = make_project(root / "default-loop", default_lock=None)
    link(default_loop / "other.lock", default_loop / "deno.lock")
    link(default_loop / "deno.lock", default_loop / "other.lock")
    case("a default lockfile that is a symlink loop is refused as a loop",
         lambda: check_atom(default_loop), "symlink loop")
    dir_loop = make_project(root / "default-dir-loop", default_lock=None)
    link("vendor", dir_loop / "atoms/demo/vendor")  # a directory that is a link to itself
    case("a lockfile under a directory that is a link to itself is refused as a loop",
         lambda: check_atom(dir_loop, "vendor/deno.lock"), "symlink loop")

    # The lockfile the manifest names. The manifest is checked as it is read, so
    # these are refused in a project that holds no atom to use the lock.
    named = make_project(root / "manifest", {"lock": "vendor/deno.lock"})
    write(named / "vendor/deno.lock", lockfile(TARGET))
    case("a lockfile the manifest names inside the project is read", lambda: check_atom(named))
    case("a lockfile the manifest names that is not there is still reported missing",
         lambda: check_atom(make_project(root / "manifest-missing", {"lock": "vendor/absent.lock"})), "is missing")
    manifest_link = make_project(root / "manifest-link", {"lock": "link.lock"}, default_lock=None)
    link(manifest_link / "absent.lock", manifest_link / "link.lock")
    case("a lockfile the manifest links to, with nothing at the end of the link, is reported missing",
         lambda: check_atom(manifest_link), "is missing")
    case("a lockfile the manifest reaches with ../ is refused, though no atom uses it",
         lambda: check_manifest_only(root / "manifest-parent", {"lock": f"../{OUTSIDE}/deno.lock"}),
         "outside the project")
    case("a lockfile the manifest names by absolute path outside the project is refused, though no atom uses it",
         lambda: check_manifest_only(root / "manifest-absolute", {"lock": str(outside)}),
         "outside the project")
    escaping = root / "manifest-escaping-link"
    link(f"../{OUTSIDE}/absent.lock", escaping / "link.lock")
    case("a manifest whose lock is a link out of the project is refused, though nothing is at the end of the link",
         lambda: check_manifest_only(escaping, {"lock": "link.lock"}), "outside the project")
    looping = root / "manifest-loop"
    link("loop.lock", looping / "loop.lock")
    case("a manifest whose lock is a symlink loop is refused as a loop, though no atom uses it",
         lambda: check_manifest_only(looping, {"lock": "loop.lock"}), "symlink loop")
    case("a manifest whose lock is not a path names the key it read wrong",
         lambda: check_manifest_only(root / "manifest-bool", {"lock": True}), "runtime.deno.lock")
    case("a manifest whose lock is not a path says what to write instead",
         lambda: check_manifest_only(root / "manifest-advice", {"lock": 5}), FIX)

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
    atom_dangling = make_project(root / "atom-dangling", {}, default_lock=lockfile())
    link(f"../../../{OUTSIDE}/absent.lock", atom_dangling / "atoms/demo/link.lock")
    case("a lockfile the atom links to outside the project is refused, though nothing is at the end of the link",
         lambda: check_atom(atom_dangling, "link.lock"), "outside the project")
    atom_loop = make_project(root / "atom-loop", {}, default_lock=lockfile())
    link("loop.lock", atom_loop / "atoms/demo/loop.lock")
    case("a lockfile the atom names that is a symlink loop is refused as a loop",
         lambda: check_atom(atom_loop, "loop.lock"), "symlink loop")
    bad_lock = make_project(root / "atom-not-a-path", default_lock=None)
    case("an atom whose lock is not a path names the key it read wrong",
         lambda: check_atom(bad_lock, 5), "runtime.deno.lock")

    # How the project is found when the way to it is not a plain directory.
    aliased = link(make_project(root / "real-project"), root / "aliased-project")
    case("a project that is itself a symlink is one project, seen from either side",
         lambda: check_atom(aliased))
    # Both checks are handed the path the project was reached by, so a project
    # reached through a link is one project to both: its manifest's settings
    # still apply to its atoms, and its own lockfile is still inside it. The
    # refusal below only appears when the atom check finds those settings;
    # without them it would take the import.
    def reached_through_a_link() -> None:
        real = root / "settings-project"
        write(real / "atomic.yaml", manifest_text({"allow_packages": ["npm:other"]}))
        write(real / "deno.lock", lockfile(TARGET))
        write(real / "src" / "index.ts", f'import {{ parse }} from "{TARGET}"\n')
        seen = link(real, root / "settings-project-link")
        deno_exec.check_manifest(manifest_config({"allow_packages": ["npm:other"]}), seen / "atomic.yaml")
        check_atom(seen)

    case("a project reached through a symlink is one project for both checks", reached_through_a_link,
         "is not in the manifest's runtime.deno.allow_packages")
    aliased_escape = make_project(root / "aliased-escape", default_lock=lockfile())
    link(outside, aliased_escape / "atoms/demo/link.lock")
    aliased_out = link(aliased_escape, root / "aliased-escape-link")
    case("a lock outside a project that is itself a symlink is refused from either side",
         lambda: check_atom(aliased_out, "link.lock"), "outside the project")
    write(root / "proj2/deno.lock", lockfile(TARGET))  # a name that starts with the other project's name
    case("a manifest that reaches a neighbouring project whose name starts with the project's is refused",
         lambda: check_manifest_only(root / "proj", {"lock": "../proj2/deno.lock"}), "outside the project")
    neighbour = make_project(root / "proj-atom", {}, default_lock=lockfile())
    case("an atom that reaches that neighbour is refused",
         lambda: check_atom(neighbour, "../../../proj2/deno.lock"), "outside the project")
    own = make_project(root / "proj2", default_lock=None)
    case("that neighbour reads its own lockfile, held at the same path", lambda: check_atom(own))
    under_link = make_project(root / "nested", {"lock": "packages/locks/deno.lock"}, default_lock=None)
    link(under_link / "deep/vendor", under_link / "packages")
    write(under_link / "deep/vendor/locks/deno.lock", lockfile(TARGET))
    case("a lockfile under a directory component that is a symlink inside the project is read",
         lambda: check_atom(under_link))
    out_link = make_project(root / "nested-out", {"lock": "escape/deno.lock"}, default_lock=None)
    link(out_link.parent / OUTSIDE, out_link / "escape")
    case("a lockfile under a directory component that is a symlink out of the project is refused",
         lambda: check_atom(out_link), "outside the project")

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
