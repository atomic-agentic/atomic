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
names it has to be a path, so a number, a list, nothing at all, a bool and an
empty string are refused by name instead of raising a Python error. So is a name
with a NUL in it, which no file system can hold.

Settling a path -- following the links above it before it is checked or read --
is done here rather than by `Path.resolve` alone, because resolve answers a
symlink loop differently by Python version, and leaves a name that is not there
yet against whatever its directory links to. So a loop, a path with nothing at
the end, a link to nothing outside the project, a project that is itself a
symlink, a directory component that is one, and a neighbouring project whose
name starts with the project's are each answered the same way on every Python --
refused, or read -- even where the wording differs, because what a path that is
not there settles to depends on the version. None of them ends in a Python error,
and none in a lock taken on trust. A `..` that goes up from a directory the
checker cannot see is refused outright, because putting it back on the settled
path moves the path every caller compares against.

A lock the checker cannot get to is refused as what it is. A route through a
directory it may not enter is a lock that cannot be read, not a lock that is
missing: the manifest is refused as it is read, whether or not an atom ever
reads it, and the atom check refuses its own route the same way. A lockfile
whose text the atom check cannot open is refused as unreadable too, rather than
raising or passing as one that records nothing.

An atom's imports are checked against what the project itself recorded, so a
lockfile the project does not hold must never be read, whatever route leads
there.

The fixtures are inert: JSON, a manifest-shaped text and TypeScript-shaped text
under a temporary directory, and a few directories whose mode shuts the checker
out. Nothing here runs Deno, needs a provider or reaches the network.

    python3 tools/deno_exec_test.py
"""

from __future__ import annotations

import contextlib
import errno
import json
import os
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import deno_exec

# What the atom's script imports, and what a lockfile has to record for it.
TARGET = "npm:jsonc-parser@3.3.1"
HOST = "example.com"
OUTSIDE = "outside"  # the project's name for the directory beside it, holding a lockfile that passes
# A refusal about how a lock is named closes with this, so that whoever is shown
# it knows what to write instead of what was wrong. Two refusals do not carry it,
# because the name in them is not the trouble: that the lockfile is missing, and
# that it cannot be read.
FIX = "name a lockfile inside the project, such as deno.lock"
NO_LOCK = object()  # an atom that names no lock at all, which is not the same as naming none


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


shut: list[Path] = []  # directories a case closed, opened again before the tree is removed


def close(directory: Path) -> Path:
    """A directory the checker may not enter, holding what was written there first.

    What is in it stays, so the only thing the case changes is that the way
    through it is gone. The mode goes back in main, because removing a temporary
    directory through a directory that cannot be entered fails.
    """
    directory.chmod(0o000)
    shut.append(directory)
    return directory


@contextlib.contextmanager
def settling_as_loop(name: str):
    """`Path.resolve` answering about `name` the way another Python's path library does.

    A strict resolve says the link is not there, and the settling it falls back
    to says the link points round. This Python answers the second with the
    settled path rather than saying so, which is why the loop below cannot be
    reached by a file on disk here. Every other path resolves as it really
    does, and what runs inside is the check every manifest goes through.
    """
    real = Path.resolve

    def resolve(self, strict=False):
        if self.name == name:
            if strict:
                raise FileNotFoundError(errno.ENOENT, os.strerror(errno.ENOENT), str(self))
            raise RuntimeError(f"Symlink loop from {str(self)!r}")
        return real(self, strict=strict)

    Path.resolve = resolve  # type: ignore[method-assign]
    try:
        yield
    finally:
        Path.resolve = real  # type: ignore[method-assign]


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


def check_atom(project: Path, lock: object = NO_LOCK) -> None:
    """The atom check: an exec atom whose script is src/index.ts, naming `lock` or naming none.

    `NO_LOCK` is an atom that names no lock of its own, which takes the
    manifest's or the default beside it; `None` is an atom that names `lock:`
    with nothing after it, which is a name of its own and not a path.
    """
    deno = {} if lock is NO_LOCK else {"lock": lock}
    fm = {"exec": {"script": "../../src/index.ts"}, "runtime": {"deno": deno},
          "network": {"allow": [HOST]}, "effects": {"external": "read-only"}}
    path = write(project / "atoms/demo/atom.atom.astro", "---\n---\n")
    deno_exec.check_script(path, fm, project)


failures: list[str] = []


def case(what: str, run, refusal: str | None = None) -> None:
    """Runs one case: the check takes it, or refuses it saying `refusal`.

    A check that raises TypeError, OSError or RuntimeError where it should refuse
    is a failure and not a refusal: it says nothing about the file or the key at
    fault. RuntimeError is one of the ways a path library reports a symlink loop,
    so a settling that stops converting it to a refusal fails here rather than
    ending the run.
    """
    try:
        run()
    except (ValueError, TypeError, OSError, RuntimeError) as error:
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
    # What else cannot name a lockfile. YAML reads `lock:` and `lock: []` as
    # nothing and as a list, and no file system holds a NUL in a name.
    case("a manifest whose lock is nothing at all is refused by name",
         lambda: check_manifest_only(root / "manifest-nothing", {"lock": None}), "runtime.deno.lock is None")
    case("a manifest whose lock is a list is refused by name",
         lambda: check_manifest_only(root / "manifest-list", {"lock": ["deno.lock"]}),
         "runtime.deno.lock is ['deno.lock']")
    case("a manifest whose lock is an empty string is refused by name",
         lambda: check_manifest_only(root / "manifest-empty", {"lock": ""}), "runtime.deno.lock is ''")
    case("a manifest whose lock holds a NUL is refused by name, which no file system can hold",
         lambda: check_manifest_only(root / "manifest-nul", {"lock": "deno\x00.lock"}),
         "runtime.deno.lock is 'deno\\x00.lock'")

    # A `..` is settled by the kernel on the way through the directories named
    # before it, so one that goes up from a directory the checker cannot see is
    # refused. Put back on the settled path, it moves that path, and the moved
    # path still reads as inside the project to the comparison made next.
    case("a manifest that reaches up from a directory that is not there is refused, though no atom uses it",
         lambda: check_manifest_only(root / "manifest-up-gone", {"lock": f"gone/../../{OUTSIDE}/deno.lock"}),
         "goes up from a directory that is not there")
    up_shut = root / "manifest-up-shut"
    (up_shut / "shut").mkdir(parents=True)
    close(up_shut / "shut")
    case("a manifest that reaches up through a directory it may not enter is refused, though no atom uses it",
         lambda: check_manifest_only(up_shut, {"lock": f"shut/../../{OUTSIDE}/deno.lock"}),
         "goes up from a directory that is not there")
    # The same answer about the project directory itself, which is where a name
    # like this has to be put right. The manifest is there; the way to it that
    # the caller handed over is not.
    here = root / "manifest-through-gone"
    write(here / "atomic.yaml", manifest_text({"lock": "deno.lock"}))
    reached_through = root / "gone" / ".." / "manifest-through-gone" / "atomic.yaml"
    case("a manifest reached through a directory that is not there is refused as the project",
         lambda: deno_exec.check_manifest(manifest_config({"lock": "deno.lock"}), reached_through),
         "the project directory goes up from a directory that is not there")

    # A directory the checker may not enter is not a lockfile that is missing.
    # The refusal says the lockfile cannot be read, which is not what the `..`
    # above says and not what a lockfile that is not there says.
    shut_lock = root / "manifest-shut" / "shut"
    write(shut_lock / "deno.lock", lockfile(TARGET))
    close(shut_lock)
    case("a manifest whose lock sits in a directory it may not enter is refused as unreadable, though no atom uses it",
         lambda: check_manifest_only(root / "manifest-shut", {"lock": "shut/deno.lock"}),
         "lockfile deno.lock cannot be read")

    # The settling of a link that is not there settles links of its own, so a
    # path library that reports a loop there is answered with a loop, and not
    # with a Python error. This Python answers it with the settled path, so that
    # branch is reached by handing it the answers of a path library that does
    # report one, for one file name, and running the manifest check over the
    # link that really is there. Without the conversion a RuntimeError walks out
    # of the check and `case` reports it as a failure.
    tail_loop = root / "manifest-loop-tail"
    link("loop.lock", tail_loop / "loop.lock")
    link("gone/../loop.lock", tail_loop / "dangling.lock")
    with settling_as_loop("dangling.lock"):
        case("a manifest whose lock is a link the settling finds looping is refused as a loop, though no atom uses it",
             lambda: check_manifest_only(tail_loop, {"lock": "dangling.lock"}), "symlink loop")

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
    # The same names are refused when an atom writes them: nothing, a list, an
    # empty string, and a NUL no file system can hold.
    case("an atom whose lock is nothing at all is refused by name",
         lambda: check_atom(bad_lock, None), "runtime.deno.lock is None")
    case("an atom whose lock is a list is refused by name",
         lambda: check_atom(bad_lock, ["deno.lock"]), "runtime.deno.lock is ['deno.lock']")
    case("an atom whose lock is an empty string is refused by name",
         lambda: check_atom(bad_lock, ""), "runtime.deno.lock is ''")
    case("an atom whose lock holds a NUL is refused by name, which no file system can hold",
         lambda: check_atom(bad_lock, "deno\x00.lock"), "runtime.deno.lock is 'deno\\x00.lock'")

    # A `..` that goes up from a directory the checker cannot see is refused,
    # from an atom as from the manifest. Without it the settled path moves and
    # the next comparison still reads it as inside the project, so an escape
    # and a lockfile of the project's own that would pass are both taken.
    case("an atom that reaches up from a directory that is not there is refused",
         lambda: check_atom(make_project(root / "atom-up-gone", {}, default_lock=lockfile()),
                            f"gone/../../../../{OUTSIDE}/deno.lock"),
         "goes up from a directory that is not there")
    up_shut_atom = make_project(root / "atom-up-shut", {}, default_lock=lockfile())
    write(up_shut_atom / "atoms/demo/shut/keep.txt", "shut\n")
    close(up_shut_atom / "atoms/demo/shut")
    case("an atom that reaches up through a directory it may not enter is refused",
         lambda: check_atom(up_shut_atom, f"shut/../../../../{OUTSIDE}/deno.lock"),
         "goes up from a directory that is not there")
    back_atom = make_project(root / "atom-up-inside", {}, default_lock=None)
    write(back_atom / "atoms/demo/here.lock", lockfile(TARGET))  # a lockfile that records the import
    write(back_atom / "atoms/demo/shut/keep.txt", "shut\n")
    close(back_atom / "atoms/demo/shut")
    case("an atom that reaches its own lockfile up through a directory it may not enter is refused",
         lambda: check_atom(back_atom, "shut/../../atoms/demo/here.lock"),
         "goes up from a directory that is not there")

    # A directory the checker may not enter is not a lockfile that is missing,
    # and a lockfile it may not open is not one that records nothing.
    shut_atom = make_project(root / "atom-shut", {}, default_lock=lockfile())
    write(shut_atom / "atoms/demo/shut/deno.lock", lockfile(TARGET))
    close(shut_atom / "atoms/demo/shut")
    case("an atom whose lock sits in a directory it may not enter is refused as unreadable",
         lambda: check_atom(shut_atom, "shut/deno.lock"), "lockfile deno.lock cannot be read")
    shut_lock_atom = make_project(root / "atom-unreadable", {}, default_lock=None)
    secret = write(shut_lock_atom / "atoms/demo/secret.lock", lockfile(TARGET))
    secret.chmod(0o000)  # the lock is there, and records the import; opening it is refused
    case("an atom whose lock it may not open is refused as unreadable, not as one that records nothing",
         lambda: check_atom(shut_lock_atom, "secret.lock"), "lockfile secret.lock cannot be read")

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
    # An atom names its lock two levels down, and its neighbour here is the
    # project's own name with one more character, so the two paths share a
    # prefix. A comparison that asks whether one path written as text starts
    # with the other takes that neighbour for the project itself.
    write(root / "proj-atom2/deno.lock", lockfile(TARGET))
    neighbour = make_project(root / "proj-atom", {}, default_lock=lockfile())
    case("an atom that reaches a neighbour whose project's name starts with its own is refused",
         lambda: check_atom(neighbour, "../../../proj-atom2/deno.lock"), "outside the project")
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
        try:
            run_cases(Path(tmp).resolve())
        finally:
            for directory in shut:  # a closed directory is opened again before the tree goes away
                directory.chmod(0o700)
    if failures:
        print(f"\n{len(failures)} failed:")
        for one in failures:
            print(f"  {one}")
        return 1
    print("\nPASS lockfile boundary: the default, the manifest's and the atom's all stay in the project")
    return 0


if __name__ == "__main__":
    sys.exit(main())
