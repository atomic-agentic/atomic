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
is done here rather than by `Path.resolve` alone, because resolve has answered a
symlink loop differently across Python versions -- as a missing file, as an
OSError, and as a RuntimeError -- and leaves a name that is not there yet
against whatever its directory links to. So a loop, a path with nothing at the
end, a link to nothing outside the project, a project that is itself a symlink,
a directory component that is one, and a neighbouring project whose name starts
with the project's are each answered the same way on every Python -- refused, or
read -- even where the wording differs, because what a path that is not there
settles to depends on the version. None of them ends in a Python error, and none
in a lock taken on trust. A `..` that goes up from a directory the checker
cannot see, or from a name that is there but is no directory, is refused
outright, because putting it back on the settled path moves the path every
caller compares against.

Whether the checker can see a file at all is asked of the file system directly,
not of `Path.is_file`, which answers some errors as its own judgement of them
rather than as the file system reported them, and has changed which between
versions. Only a name that is not there, and a name whose directory is not a
directory, count as absent. So a lock, an imported module or a named script
behind a directory the checker may not enter is refused as unreadable, by its
own name, and a name that is there but is no file at all -- a lockfile or a
script that is a directory -- is refused too, never opened for its contents.

What a lockfile holds is what every import is answered from, so its shape is
checked before one import is answered from it. A body that is not an object, a
`specifiers` that is not the map from each package to its version, and JSON
nested deeper than this Python can read are each refused saying the lockfile and
what is wrong with it. None is taken as a lock that records everything, which is
what testing a substring of a string amounts to, and none is taken as one that
records nothing; none ends in a Python error either.

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
under a temporary directory, and a few directories and one file whose mode shuts
the checker out. A mode of 000 shuts out everyone but root, and those cases are
about what a checker does when the file system refuses it, so run as root they
show a lock that reads: they are reported as skipped, each by name with why, and
the run still reports everything it did check.
Nothing here runs Deno, needs a provider or reaches the network.

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
# A refusal about how a lock is named, or about where the name leads, closes
# with this, so that whoever is shown it knows what to write instead of what was
# wrong. Three refusals do not carry it, because the name in them is not the
# trouble: that the lockfile is missing, that it cannot be read, and that what
# it holds is not a lockfile's shape.
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
skipped: list[str] = []  # cases whose fixture is a mode, and what stops them here
AS_ROOT = os.geteuid() == 0  # what a mode of 000 does not shut out


def close(directory: Path) -> Path:
    """A directory the checker may not enter, holding what was written there first.

    What is in it stays, so the only thing the case changes is that the way
    through it is gone. The mode goes back in main, because removing a temporary
    directory through a directory that cannot be entered fails.
    """
    directory.chmod(0o000)
    shut.append(directory)
    return directory


def mode_case(what: str, run, refusal: str | None = None) -> None:
    """Runs a case whose fixture is a mode 000, or reports it skipped as root.

    These cases are about what the checker does where the file system refuses
    it, and a mode of 000 refuses everyone but root. Run as root the lock reads,
    the way through the closed directory is open, and the case would be shown
    passing on an answer no one else can get. So it is said to be skipped, by
    name and with why, rather than passed -- or failed, which would say the
    checker is wrong for an answer the file system gave root.
    """
    if AS_ROOT:
        skipped.append(what)
        print(f"SKIP {what}\n     running as root, which a mode of 000 does not shut out")
        return
    case(what, run, refusal)


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


def check_atom(project: Path, lock: object = NO_LOCK, script: str = "../../src/index.ts") -> None:
    """The atom check: an exec atom whose script is src/index.ts, naming `lock` or naming none.

    `NO_LOCK` is an atom that names no lock of its own, which takes the
    manifest's or the default beside it; `None` is an atom that names `lock:`
    with nothing after it, which is a name of its own and not a path. `script`
    is what the atom names for its code, which is two levels down from the
    project root the atom's own directory sits under.
    """
    deno = {} if lock is NO_LOCK else {"lock": lock}
    fm = {"exec": {"script": script}, "runtime": {"deno": deno},
          "network": {"allow": [HOST]}, "effects": {"external": "read-only"}}
    path = write(project / "atoms/demo/atom.atom.astro", "---\n---\n")
    deno_exec.check_script(path, fm, project)


failures: list[str] = []


def case(what: str, run, refusal: str | None = None) -> None:
    """Runs one case: the check takes it, or refuses it saying `refusal`.

    A check that raises TypeError, AttributeError, OSError or RuntimeError where
    it should refuse is a failure and not a refusal: it says nothing about the
    file or the key at fault. AttributeError is what asking a list or a nothing
    for its keys comes to, RecursionError -- a RuntimeError -- is what json
    answers a document nested deeper than this Python reads, and RuntimeError is
    one of the ways a path library reports a symlink loop, so a check that stops
    turning any of these into a refusal fails here rather than ending the run.
    """
    try:
        run()
    except (ValueError, TypeError, AttributeError, OSError, RuntimeError) as error:
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
    mode_case("a manifest that reaches up through a directory it may not enter is refused, though no atom uses it",
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
    mode_case("a manifest whose lock sits in a directory it may not enter is refused as unreadable, though no atom uses it",
              lambda: check_manifest_only(root / "manifest-shut", {"lock": "shut/deno.lock"}),
              "lockfile deno.lock cannot be read")

    # The settling of a link that is not there settles links of its own, so a
    # path library that reports a loop there is answered with a loop, and not
    # with a Python error. Pathlib's answer about that settling has been the
    # settled path on some versions and a RuntimeError on others, so the branch
    # is reached the same way whichever version this runs on: by handing it the
    # answers of a path library that reports one, for one file name, and running
    # the manifest check over the link that really is there. Without the
    # conversion a RuntimeError walks out of the check and `case` reports it as a
    # failure.
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
    mode_case("an atom that reaches up through a directory it may not enter is refused",
              lambda: check_atom(up_shut_atom, f"shut/../../../../{OUTSIDE}/deno.lock"),
              "goes up from a directory that is not there")
    back_atom = make_project(root / "atom-up-inside", {}, default_lock=None)
    write(back_atom / "atoms/demo/here.lock", lockfile(TARGET))  # a lockfile that records the import
    write(back_atom / "atoms/demo/shut/keep.txt", "shut\n")
    close(back_atom / "atoms/demo/shut")
    mode_case("an atom that reaches its own lockfile up through a directory it may not enter is refused",
              lambda: check_atom(back_atom, "shut/../../atoms/demo/here.lock"),
              "goes up from a directory that is not there")

    # A directory the checker may not enter is not a lockfile that is missing,
    # and a lockfile it may not open is not one that records nothing.
    shut_atom = make_project(root / "atom-shut", {}, default_lock=lockfile())
    write(shut_atom / "atoms/demo/shut/deno.lock", lockfile(TARGET))
    close(shut_atom / "atoms/demo/shut")
    mode_case("an atom whose lock sits in a directory it may not enter is refused as unreadable",
              lambda: check_atom(shut_atom, "shut/deno.lock"), "lockfile deno.lock cannot be read")
    shut_lock_atom = make_project(root / "atom-unreadable", {}, default_lock=None)
    secret = write(shut_lock_atom / "atoms/demo/secret.lock", lockfile(TARGET))
    secret.chmod(0o000)  # the lock is there, and records the import; opening it is refused
    mode_case("an atom whose lock it may not open is refused as unreadable, not as one that records nothing",
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

    # What a lockfile is allowed to hold. Its contents are what every import is
    # answered from, so a body that is not an object, and a specifiers that is
    # not the map from each package to its version, are refused saying this
    # lockfile and what is wrong with it. None is answered as a lock that
    # records everything -- which is what asking whether an import is a
    # substring of a string comes to -- and none ends in a Python error about a
    # list or a nothing having no keys. A record whose version is nothing still
    # records the package, and JSON nested deeper than this Python reads is
    # refused rather than raising.
    bodies = [
        ("[]", "lockfile deno.lock is a list, not a JSON object"),
        ("null", "lockfile deno.lock is nothing, not a JSON object"),
        (f'"{TARGET}"', "lockfile deno.lock is a string, not a JSON object"),
        (f'["{TARGET}"]', "lockfile deno.lock is a list, not a JSON object"),
        ("42", "lockfile deno.lock is a number, not a JSON object"),
        ("true", "lockfile deno.lock is true or false, not a JSON object"),
        ("not JSON at all", "lockfile deno.lock cannot be read as JSON"),
        ("[" * 200_000, "lockfile deno.lock cannot be read as JSON"),
        ('{"specifiers": null}', "records specifiers as nothing, not as the map"),
        (f'{{"specifiers": "{TARGET}"}}', "records specifiers as a string, not as the map"),
        ('{"specifiers": "npm:"}', "records specifiers as a string, not as the map"),
        (f'{{"specifiers": ["{TARGET}"]}}', "records specifiers as a list, not as the map"),
        ('{"specifiers": true}', "records specifiers as true or false, not as the map"),
        ('{"specifiers": []}', "records specifiers as a list, not as the map"),
        (f'{{"specifiers": {{"{TARGET}": null}}}}', None),  # the package is recorded; no version is read
        ('{"version": 4, "remote": {}}', "is not recorded in deno.lock"),  # a lock that records no specifiers
    ]
    for n, (body, refusal) in enumerate(bodies):
        shown = body if len(body) <= 40 else f"{body[:20]}… ({len(body)} characters)"
        shaped = make_project(root / f"body-{n}", default_lock=f"{body}\n")
        answer = "is read" if refusal is None else "is refused saying the file and what is wrong with it"
        case(f"a lockfile holding {shown!r} {answer}",
             lambda shaped=shaped: check_atom(shaped), refusal)

    # What the checker reads and what it may not. A lock, an imported module and
    # a named script are asked the same way: is there a regular file at the
    # settled path, and where the file system will not say, what did it say. A
    # name behind a directory the checker may not enter is refused by that name,
    # not as missing and not as a Python error; a queue where a lockfile should
    # be is refused as no regular file, because opening it would wait for a
    # writer; and a lockfile or a script that is a directory is refused, never
    # opened. A name the kernel will not look through -- a file with something
    # under it -- leaves nothing at the end, which is what those refusals say,
    # and a `..` that reaches back up through one is refused where the manifest
    # is read, saying that the name is no directory. A script that is not there
    # says it is not there, which is not what one that is no script says; and a
    # script outside the project is refused as outside it whoever holds it.
    closed_import = make_project(root / "import-shut", default_lock=lockfile())
    module = write(closed_import / "src/module.ts", f'import {{ parse }} from "{TARGET}"\n')
    write(closed_import / "src/index.ts", 'import "./module.ts"\n')
    module.chmod(0o000)  # index.ts imports it; opening it is refused
    mode_case("a script whose import the checker may not open is refused by that import's name",
              lambda: check_atom(closed_import), "import module.ts cannot be read")
    # A named script the same way: the route is what is shut, so the refusal
    # names the script rather than saying it is not there.
    closed_script = make_project(root / "script-shut", default_lock=lockfile())
    (closed_script / "src/index.ts").unlink()
    write(closed_script / "src/closed/index.ts", f'import {{ parse }} from "{TARGET}"\n')
    close(closed_script / "src/closed")
    mode_case("a script the checker may not open is refused by its name, not as missing",
              lambda: check_atom(closed_script, script="../../src/closed/index.ts"),
              "exec.script index.ts cannot be read")
    # The same refusal where the way to the script is open and the script is
    # not: its name reads as a script, so what is refused is the contents, and
    # a script read as none is a script that imports nothing.
    unreadable_script = make_project(root / "script-unreadable", default_lock=lockfile())
    (unreadable_script / "src/index.ts").chmod(0o000)
    mode_case("a script whose contents the checker may not have is refused as unreadable, not as one that imports nothing",
              lambda: check_atom(unreadable_script), "exec.script index.ts cannot be read")
    through_file = make_project(root / "import-through-file", default_lock=lockfile())
    write(through_file / "src/module.ts", f'import {{ parse }} from "{TARGET}"\n')
    write(through_file / "src/index.ts", 'import "./module.ts/not.ts"\n')
    case("a script whose import goes through a module that holds no directory is refused",
         lambda: check_atom(through_file), "import ./module.ts/not.ts is missing or leaves the project")
    lock_used_as_dir = make_project(root / "lock-through-file", default_lock=lockfile(TARGET))
    write(lock_used_as_dir / "atoms/demo/deno.lock", lockfile(TARGET))  # and one where the atom looks
    case("an atom whose lock goes through a lockfile that holds no directory is refused as missing",
         lambda: check_atom(lock_used_as_dir, "deno.lock/deno.lock"), "lockfile deno.lock is missing")
    case("a manifest that reaches back up through its own lockfile is refused, and told it is no directory",
         lambda: deno_exec.check_manifest(manifest_config({"lock": "deno.lock/../deno.lock"}),
                                          lock_used_as_dir / "atomic.yaml"),
         "goes up from a directory that is not there, cannot be read, or is not a directory")
    lock_dir = make_project(root / "lock-is-dir", default_lock=None)
    (lock_dir / "deno.lock").mkdir()
    case("a default lockfile that is a directory is refused as missing, not opened",
         lambda: check_atom(lock_dir), "lockfile deno.lock is missing")
    manifest_lock_dir = root / "manifest-lock-dir"
    (manifest_lock_dir / "deno.lock").mkdir(parents=True)
    case("a manifest's own default lockfile that is a directory is refused as no regular file, though no atom uses it",
         lambda: check_manifest_only(manifest_lock_dir, {}), "lockfile deno.lock is not a regular file")
    # Neither of these reads anything off the lock: the manifest is refused as
    # it is read, on its own name, before any atom that needs the lock is
    # checked. And a queue is refused without being opened, because opening one
    # waits for a writer that never comes.
    manifest_lock_fifo = root / "manifest-lock-fifo"
    manifest_lock_fifo.mkdir(parents=True)
    os.mkfifo(manifest_lock_fifo / "deno.lock")
    case("a manifest's own default lockfile that is a queue is refused as no regular file, and is not opened",
         lambda: check_manifest_only(manifest_lock_fifo, {}), "lockfile deno.lock is not a regular file")
    lock_fifo = make_project(root / "lock-is-fifo", default_lock=None)
    os.mkfifo(lock_fifo / "deno.lock")  # opening this waits for a writer, so nothing opens it
    case("a default lockfile that is a queue is refused as no regular file, and is not opened",
         lambda: check_atom(lock_fifo), "lockfile deno.lock is not a regular file")
    # A lock whose bytes are no text is refused as one that cannot be read,
    # which is not what one that records nothing says, and not a codec error
    # escaping the check.
    not_text = make_project(root / "lock-not-text", default_lock=None)
    (not_text / "deno.lock").write_bytes(b"\xff\xfe not a lockfile\n")
    case("a lockfile that is no text is refused as unreadable, not as one that records nothing",
         lambda: check_atom(not_text), "lockfile deno.lock cannot be read")

    absent_script = make_project(root / "script-absent", default_lock=lockfile())
    case("a script that is not there says so, which is not what a script that is no script says",
         lambda: check_atom(absent_script, script="../../src/gone.ts"),
         "exec.script '../../src/gone.ts' is missing or cannot be read")
    dir_script = make_project(root / "script-is-dir", default_lock=lockfile())
    (dir_script / "src/whatever.ts").mkdir()
    case("a script that is a directory is refused as not there to read",
         lambda: check_atom(dir_script, script="../../src/whatever.ts"),
         "exec.script '../../src/whatever.ts' is missing or cannot be read")
    write(dir_script / "src/index.js", f'import {{ parse }} from "{TARGET}"\n')
    case("a script that is no TypeScript says what it is not",
         lambda: check_atom(dir_script, script="../../src/index.js"), "is not a TypeScript file")
    # The settled script is compared to the project as it settled, whoever the
    # file system will let the checker ask.
    outside_script = make_project(root / "script-outside", default_lock=lockfile())
    write(root / "elsewhere" / "index.ts", f'import {{ parse }} from "{TARGET}"\n')
    case("a script outside the project is refused as outside it, whoever holds it",
         lambda: check_atom(outside_script, script="../../../elsewhere/index.ts"), "is outside the project")


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="atomic-deno-exec-test-") as tmp:
        try:
            run_cases(Path(tmp).resolve())
        finally:
            for directory in shut:  # a closed directory is opened again before the tree goes away
                directory.chmod(0o700)
    # A skipped case is said by name, and counted on the way out, so that a run
    # which checked less than it knows how to is never mistaken for one that
    # checked all of it.
    if skipped:
        print(f"\n{len(skipped)} skipped, each one a case whose fixture is a mode 000:")
        for one in skipped:
            print(f"  {one}")
    if failures:
        print(f"\n{len(failures)} failed:")
        for one in failures:
            print(f"  {one}")
        return 1
    print("\nPASS lockfile boundary: the default, the manifest's and the atom's all stay in the project"
          + (f", {len(skipped)} mode-000 cases skipped" if skipped else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
