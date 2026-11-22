#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The controls a change touches, which CI's Controls workflow runs.

    scripts/controls-touched.py BASE HEAD                       # the rows to run, one per line
    scripts/controls-touched.py BASE HEAD --shards N --shard I  # those in shard I of N
    scripts/controls-touched.py BASE HEAD --report              # touched rows not run, and deleted rows
    scripts/controls-touched.py --all HEAD [--shards N --shard I]  # every row, for a full sweep

A change touches a row (scripts/controls/<name>.row) when it adds or changes
the row's file; changes the file the row switches off, since a code change
can move the switch's text or stop it biting; changes a file whose text, at
BASE or at HEAD, holds the name of a test the row names (the last `::` part
of each, read as scripts/controls-check.sh reads them), since a change to the
guarding test can stop it failing; or changes anything under the row's
`--test <t>` target, crates/*/tests/<t>.rs or crates/*/tests/<t>/, or any
test code that crate's targets share (crates/<c>/tests/support/, fixtures/
and the like: a directory there that is no one target's), or a shared test
helper outside tests/ that scripts/controls-shared.txt lists for the row (by
its --test target, a feature it turns on or a package it builds). Before
these rules, the change is expanded by one level of literal Rust includers
at the base and head. A literal tests/... path in an integration target owns
that data directory; all named owners are kept, and unowned data retains
the crate-wide shared-test fallback. The rows are read from HEAD's tree, not the working tree, and the changes are git's
diff from BASE to HEAD.

It fails, printing why, rather than leave out a row it cannot read: a row file
that is not one row of five fields named after its file, or a file under
scripts/controls/ that is not a regular `*.row` file. It also fails on a row
file the change deletes while its switch's text is still in a file at HEAD,
any file, that no row switches it in: deleting the row would only stop its
control being checked, and a file renamed with its rows deleted still holds
the text. A deleted row whose text is nowhere at HEAD, or is switched by
another row wherever it is (as renaming a row leaves it), is listed by
--report.

scripts/controls-local.txt lists the rows a CI runner cannot run, each with
its reason. A touched row listed there is left out of the rows to run and
listed by --report instead; a line there naming no row fails.

The shards are deterministic: the rows to run are sorted by their cargo
arguments before `--`, so that rows built the same way share a shard and its
build, then by name, and cut into N runs of nearly equal length.
"""

import argparse
import collections
import functools
import os
import posixpath
import re
import subprocess
import sys

ROWS = "scripts/controls"
LOCAL = "scripts/controls-local.txt"
SHARED = "scripts/controls-shared.txt"


class Refused(Exception):
    pass


def git(*args):
    done = subprocess.run(["git", *args], capture_output=True)
    if done.returncode != 0:
        raise Refused(f"git {' '.join(args)} failed: {done.stderr.decode(errors='replace').strip()}")
    return done.stdout


@functools.lru_cache(maxsize=None)
def exists(commit, path):
    done = subprocess.run(["git", "cat-file", "-e", f"{commit}:{path}"], capture_output=True)
    return done.returncode == 0


def text_of(commit, path):
    try:
        return git("cat-file", "blob", f"{commit}:{path}").decode("utf-8")
    except UnicodeDecodeError:
        raise Refused(f"{path} is not UTF-8 at {commit}")


def parse(path, text):
    """One row: its name, the file it switches, and its five fields."""
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    if len(lines) != 1:
        raise Refused(f"{path} holds {len(lines)} rows, not the one it must")
    fields = lines[0].split("|")
    if len(fields) != 5:
        raise Refused(f"{path} has {len(fields) - 1} '|' separators, not the 4 between its five fields")
    name, file, switch, _, args = fields
    if not name:
        raise Refused(f"{path} names no control")
    if name != posixpath.basename(path)[: -len(".row")]:
        raise Refused(f"{path} names itself '{name}', which is not its file name")
    if not file or not switch:
        raise Refused(f"{path} names no file or no text to switch")
    return {"name": name, "file": posixpath.normpath(file), "switch": switch, "args": args}


def regular(commit, path, mode):
    """The path of the regular file a row file is, following links within the
    tree as scripts/controls-check.sh's `[ -f ]` follows them; a link out of
    the tree, to nothing, to a directory or in a loop is refused."""
    seen = set()
    while mode == "120000":
        if path in seen or len(seen) >= 40:
            raise Refused(f"{path} is a link that does not end")
        seen.add(path)
        target = git("cat-file", "blob", f"{commit}:{path}").decode("utf-8", errors="surrogateescape")
        path = posixpath.normpath(posixpath.join(posixpath.dirname(path), target))
        if posixpath.isabs(target) or path.startswith("../") or path == "..":
            raise Refused(f"{path} is reached by a link out of the repository, so it cannot be read from git")
        listed = git("ls-tree", "-z", commit, "--", path).split(b"\0")[0].decode("utf-8", errors="replace")
        meta, _, name = listed.partition("\t")
        if name != path:
            raise Refused(f"{path}, which a row file links to, is not in the tree")
        mode = meta.split(" ")[0]
    if mode not in ("100644", "100755"):
        raise Refused(f"{path} is not a regular file")
    return path


def rows_at(commit):
    """Every row in the commit's tree, by name."""
    rows = {}
    for entry in filter(None, git("ls-tree", "-z", f"{commit}:{ROWS}/").split(b"\0")):
        meta, _, base = entry.decode("utf-8", errors="replace").partition("\t")
        path = f"{ROWS}/{base}"
        if not base.endswith(".row"):
            raise Refused(f"{path} is not named <name>.row, so its control would never be read")
        row = parse(path, text_of(commit, regular(commit, path, meta.split(" ")[0])))
        rows[row["name"]] = row
    if not rows:
        raise Refused(f"{ROWS} holds no control at {commit}")
    return rows


def listed(commit):
    """The names scripts/controls-local.txt lists at a commit, read as
    leniently as a list that may name rows since deleted must be."""
    if commit is None or not exists(commit, LOCAL):
        return set()
    names = set()
    for line in git("cat-file", "blob", f"{commit}:{LOCAL}").decode("utf-8", errors="replace").splitlines():
        line = line.strip()
        if line and not line.startswith("#") and ":" in line:
            names.add(line.partition(":")[0].strip())
    return names


def local_rows(commit, rows):
    """The rows a CI runner cannot run, by name, with each reason."""
    if not exists(commit, LOCAL):
        return {}
    local = {}
    for n, line in enumerate(text_of(commit, LOCAL).splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name, colon, reason = line.partition(":")
        name, reason = name.strip(), reason.strip()
        if not colon or not name or not reason:
            raise Refused(f"{LOCAL}:{n}: '{line}' is not '<row>: <why a CI runner cannot run it>'")
        if name not in rows:
            raise Refused(f"{LOCAL}:{n}: names '{name}', which is not a row")
        if name in local:
            raise Refused(f"{LOCAL}:{n}: names '{name}' a second time")
        local[name] = reason
    return local


def holding(head, text):
    """The files in the head's tree, rows aside, whose text holds `text`."""
    done = subprocess.run(
        ["git", "grep", "-z", "-l", "-F", "-I", "-e", text, head, "--", f":(exclude){ROWS}/"],
        capture_output=True,
    )
    if done.returncode == 1:
        return []
    if done.returncode != 0:
        raise Refused(f"git grep for a switch's text failed: {done.stderr.decode(errors='replace').strip()}")
    prefix = f"{head}:"
    files = []
    for entry in filter(None, done.stdout.split(b"\0")):
        path = entry.decode("utf-8", errors="surrogateescape")
        files.append(posixpath.normpath(path[len(prefix) :] if path.startswith(prefix) else path))
    return sorted(files)


def deleted(base, head, changed, rows):
    """Each row file the change deletes, with why that is allowed.

    A deleted row is allowed only when no file in the head's tree holds its
    switch's text, or each one that does has a row switching that text there:
    a file renamed with its rows deleted still holds the text, so it is
    refused."""
    held = {(row["file"], row["switch"]): row["name"] for row in rows.values()}
    report = []
    for path in sorted(changed):
        if posixpath.dirname(path) != ROWS or not path.endswith(".row"):
            continue
        if exists(head, path) or not exists(base, path):
            continue
        mode = git("ls-tree", "-z", base, "--", path).decode("utf-8", errors="replace").split(" ")[0]
        gone = parse(path, text_of(base, regular(base, path, mode)))
        files = holding(head, gone["switch"])
        holders = [(f, held[(f, gone["switch"])]) for f in files if (f, gone["switch"]) in held]
        unheld = [f for f in files if (f, gone["switch"]) not in held]
        if unheld:
            raise Refused(
                f"{path} is deleted, but its switch's text is still in {', '.join(unheld)} and no row "
                "switches it there; a control whose code remains keeps its row (update its file and switch)"
            )
        if holders:
            why = "; ".join(f"{name} switches the same text in {f}" for f, name in holders)
        else:
            why = "its text is in no file at the head"
        report.append((gone["name"], why))
    return report


def named_tests(args):
    """The tests a row's cargo arguments name, as scripts/controls-check.sh
    reads them: every positional argument, and after `--` every argument but
    a flag."""
    names, after, skip = [], False, False
    for a in args.split():
        if skip:
            skip = False
            continue
        if after:
            if not a.startswith("-"):
                names.append(a)
            continue
        if a == "--":
            after = True
        elif a in ("-p", "--package", "--features", "--test", "--bin", "--example"):
            skip = True
        elif not a.startswith("-"):
            names.append(a)
    return names


def test_targets(args):
    """The integration test targets a row's arguments name with --test."""
    words = args.split()
    before = words[: words.index("--")] if "--" in words else words
    return [before[i + 1] for i, a in enumerate(before[:-1]) if a == "--test"]


def rust_literals(path, text):
    """Literal spans outside Rust comments and character literals.

    This is a lexical scan, not a Rust parser: only literal include paths and
    literal tests/... data paths are followed. Dynamic paths remain sweep-only.
    Comments are blanked without changing offsets, so a comment cannot claim
    ownership and comments between a macro and its argument are harmless.
    """
    code, literals, i = list(text), [], 0
    character = re.compile(r"'(?:\\(?:u\{[0-9A-Fa-f_]+\}|x[0-9A-Fa-f]{2}|.)|[^'\\\n])'")
    while i < len(text):
        start = i
        if text.startswith("//", i):
            i = text.find("\n", i)
            if i < 0:
                i = len(text)
            code[start:i] = " " * (i - start)
            continue
        if text.startswith("/*", i):
            depth, i = 1, i + 2
            while i < len(text) and depth:
                if text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
            if depth:
                raise Refused(f"{path} has an unterminated Rust comment")
            code[start:i] = " " * (i - start)
            continue
        if text[i] == "'":
            matched = character.match(text, i)
            if matched:
                i = matched.end()
                code[start:i] = " " * (i - start)
                continue
        if text[i] == "r":
            quote = i + 1
            while quote < len(text) and text[quote] == "#":
                quote += 1
            if quote < len(text) and text[quote] == '"':
                hashes = text[i + 1 : quote]
                end = text.find('"' + hashes, quote + 1)
                if end < 0:
                    raise Refused(f"{path} has an unterminated raw Rust string")
                i = end + 1 + len(hashes)
                literals.append((start, i, text[quote + 1 : end], True))
                continue
        if text[i] == '"':
            i += 1
            while i < len(text) and text[i] != '"':
                i += 2 if text[i] == "\\" else 1
            if i >= len(text):
                raise Refused(f"{path} has an unterminated Rust string")
            literals.append((start, i + 1, text[start + 1 : i], False))
            i += 1
            continue
        i += 1
    return "".join(code), literals


def literal_path(source, text, raw, classification=False):
    """Decode a Rust literal; selected paths are never guessed.

    Classification admits bytes and NUL so unrelated byte/data literals can
    be ruled out. An actual path is decoded again under the strict rules.
    """
    if raw:
        value = text
    else:
        chars, i = [], 0
        escapes = {"n": "\n", "r": "\r", "t": "\t", "0": "\0", "\\": "\\", '"': '"', "'": "'"}
        while i < len(text):
            if text[i] != "\\":
                chars.append(text[i])
                i += 1
                continue
            i += 1
            if i >= len(text):
                raise Refused(f"{source} has an incomplete Rust path escape")
            escape = text[i]
            i += 1
            if escape in escapes:
                chars.append(escapes[escape])
            elif escape == "x" and re.fullmatch(r"[0-9A-Fa-f]{2}" if classification else r"[0-7][0-9A-Fa-f]", text[i : i + 2]):
                chars.append(chr(int(text[i : i + 2], 16)))
                i += 2
            elif escape == "u" and i < len(text) and text[i] == "{":
                end = text.find("}", i + 1)
                digits = text[i + 1 : end].replace("_", "") if end >= 0 else ""
                if not re.fullmatch(r"[0-9A-Fa-f]{1,6}", digits):
                    raise Refused(f"{source} has an invalid Unicode Rust path escape")
                point = int(digits, 16)
                if point > 0x10FFFF or 0xD800 <= point <= 0xDFFF:
                    raise Refused(f"{source} has an invalid Unicode Rust path escape")
                chars.append(chr(point))
                i = end + 1
            elif escape in "\r\n":
                while i < len(text) and text[i] in " \t\r\n":
                    i += 1
            else:
                raise Refused(f"{source} has an unsupported Rust path escape")
        value = "".join(chars)
    if "\0" in value and not classification:
        raise Refused(f"{source} has a NUL in a Rust path")
    return value


def in_tree(path):
    return not posixpath.isabs(path) and path != ".." and not path.startswith("../")


def included_paths(commit, source, path, modes, allow_directory=False):
    """Every tracked link and final input a literal include depends on.

    Walk the committed tree, not the host filesystem. Resolve links before
    parent components, retaining link names so retargeting a directory link
    is a change to the includer too. A missing input name remains a dependency
    (including when the change removes it); it never means no dependency.
    Runtime readers may consume directories as well as regular files.
    """
    if posixpath.isabs(path):
        raise Refused(f"{source} includes a path out of the repository")
    pending = collections.deque(path.split("/"))
    resolved, names, seen, hops = [], set(), set(), 0
    while pending:
        part = pending.popleft()
        if part in ("", "."):
            continue
        if part == "..":
            if not resolved:
                raise Refused(f"{source} includes a path out of the repository")
            resolved.pop()
            continue
        resolved.append(part)
        current = "/".join(resolved)
        mode = modes.get(current)
        if mode is None:
            # There can be no tracked descendant below a missing component.
            # Preserve its logical final name for additions/removals instead
            # of treating a dangling include as an empty set of readers.
            missing = posixpath.normpath("/".join([*resolved, *pending]))
            if not in_tree(missing):
                raise Refused(f"{source} includes a path out of the repository")
            names.add(missing)
            return names
        if mode == "120000":
            state = (current, tuple(pending))
            if state in seen or hops >= 40:
                raise Refused(f"{current} is an included link that does not end")
            seen.add(state)
            hops += 1
            names.add(current)
            target = git("cat-file", "blob", f"{commit}:{current}").decode("utf-8", errors="surrogateescape")
            if not target or "\0" in target:
                raise Refused(f"{current} is an included link with an unreadable target")
            if posixpath.isabs(target):
                raise Refused(f"{current} is an included link out of the repository")
            resolved.pop()
            pending.extendleft(reversed(target.split("/")))
            continue
        if pending and mode != "040000":
            raise Refused(f"{current}, on an included path, is not a directory")
        if not pending:
            if mode not in ("100644", "100755") and not (allow_directory and mode == "040000"):
                raise Refused(f"{current}, which {source} includes, is not a regular file")
            names.add(current)
    final = "/".join(resolved)
    if modes.get(final) not in ("100644", "100755") and not (allow_directory and modes.get(final) == "040000"):
        raise Refused(f"{final}, which {source} includes, is not a regular file")
    names.add(final)
    return names


@functools.lru_cache(maxsize=None)
def source_references(commit):
    """One level of literal includers and integration targets naming data.

    Resolve includes and top-level #[path] attributes from the source file's
    own directory, not a basename match elsewhere in the tree. A named tests/... data directory is owned
    by every integration target that names it; ambiguous owners are all kept.
    """
    included, data, sources, readers = {}, [], set(), []
    entries = []
    for entry in filter(None, git("ls-tree", "-r", "-t", "-z", commit).split(b"\0")):
        meta, _, name = entry.decode("utf-8", errors="surrogateescape").partition("\t")
        entries.append((meta.split(" ")[0], name))
    modes = dict((name, mode) for mode, name in entries)
    directories = {name for mode, name in entries if mode == "040000"}
    macro = re.compile(r"\binclude(?:_str|_bytes)?\s*!\s*[({\[]\s*$")
    attribute = re.compile(r"#\s*\[\s*path\s*=\s*$")
    for mode, name in entries:
        if mode == "040000" or not name.endswith(".rs"):
            continue
        source = text_of(commit, regular(commit, name, mode))
        sources.add(name)
        parts = name.split("/")
        target = (
            len(parts) == 4 and parts[0] == "crates" and parts[2] == "tests"
        ) or (
            len(parts) == 5 and parts[0] == "crates" and parts[2] == "tests" and parts[4] == "main.rs"
        )
        # Every target must be lexed: its test path can be a bare root or
        # use Rust escapes. Other sources need literal include syntax.
        candidate = "include" in source or ("#" in source and "path" in source)
        if not candidate and not target:
            continue
        code, literals = rust_literals(name, source)
        previous, depth = 0, 0
        for start, end, literal, raw in literals:
            # Disjoint prefixes keep this linear even in large generated
            # source files with many literals. No include starts inside a
            # preceding literal.
            before, previous = code[previous:start], end
            depth += before.count("{") - before.count("}")
            # Inline-module #[path] bases differ from the source directory.
            # Do not guess their owner and accidentally suppress the shared
            # data fallback. Those paths remain covered by the sweep.
            top_level_path = depth == 0 and attribute.search(before)
            if macro.search(before) or top_level_path:
                path = posixpath.join(posixpath.dirname(name), literal_path(name, literal, raw))
                for dependency in included_paths(commit, name, path, modes):
                    included.setdefault(dependency, set()).add(name)
            if target:
                decoded = literal_path(name, literal, raw, classification=True)
                if posixpath.isabs(decoded):
                    continue
                runtime = posixpath.normpath(decoded)
                first = next((part for part in decoded.split("/") if part not in ("", ".")), "")
                if runtime != "tests" and not runtime.startswith("tests/") and first != "tests":
                    continue
                # Preserve components until committed links are resolved:
                # tests/alias/../word need not mean tests/word.
                path = posixpath.join(*parts[:2], literal_path(name, literal, raw))
                dependencies = included_paths(commit, name, path, modes, allow_directory=True)
                for dependency in dependencies:
                    readers.append((dependency, name))
                # Only a concrete directory below this crate's tests root
                # can narrow the old shared-data rule. A root-wide read, a
                # runtime filename or an unknown path keeps that fallback.
                tests = "/".join(parts[:3]) + "/"
                for dependency in dependencies:
                    if dependency.startswith(tests) and dependency in directories:
                        data.append((dependency, name))
    return included, data, sources, readers


def references_to(path, references, owners_only=False):
    included, data, _, readers = references
    owners = set(included.get(path, ()))
    # A broad directory owner cannot make exact-file or root-wide readers
    # disappear. All readers select rows; only proven directory owners can
    # narrow the old shared-data fallback.
    for directory, target in data + ([] if owners_only else readers):
        if path == directory or path.startswith(directory + "/"):
            owners.add(target)
    return owners


def shared_test_crates(base, head, changed, references):
    """The crates whose shared test code the change touches: a file under
    crates/<c>/tests/ that belongs to no one test target, such as one in
    tests/support/ or tests/fixtures/, or a file there that is not Rust. A
    directory tests/<d>/ belongs to the target <d> when tests/<d>.rs or
    tests/<d>/main.rs is there, at the base or at the head. Literal include
    and data references give a path to its named readers first."""
    crates = set()
    for path in changed:
        parts = path.split("/")
        if len(parts) < 4 or parts[0] != "crates" or parts[2] != "tests":
            continue
        tests = "/".join(parts[:3])
        if len(parts) > 4:
            directory = f"{tests}/{parts[3]}"
            owned = any(
                f"{directory}.rs" in refs[2] or f"{directory}/main.rs" in refs[2]
                for refs in references
            )
            if owned:
                continue
            # A shared Rust module may also be named as runtime data. That
            # does not make its other compiled users disappear.
            compiled = any(f"{directory}/mod.rs" in refs[2] for refs in references)
            if compiled:
                crates.add(parts[1])
                continue
        owners = set().union(*(references_to(path, refs, owners_only=True) for refs in references))
        # We expanded only one level. A reader added by that expansion can
        # itself have readers not in this change: keep the broad fallback
        # rather than pretend the second dependency level was followed.
        if owners and owners <= changed:
            continue
        if len(parts) > 4 or not parts[3].endswith(".rs"):
            crates.add(parts[1])
    return crates


def shared_helpers(head):
    """The test helpers outside crates/<c>/tests/ that scripts/controls-shared.txt
    lists, each with the rows it serves: `--test` for every row with a --test
    target, `feature=<name>` for every row whose features turn <name> on
    (--all-features turns every one on), `package=<name>` for every row that
    builds that package (--workspace, or no package named, builds them all). A
    line the script cannot read, or a helper that is not there, is refused:
    a stale line would select nothing."""
    if not exists(head, SHARED):
        return {}
    helpers = {}
    for n, line in enumerate(text_of(head, SHARED).splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        path, colon, serves = line.partition(":")
        path, words = posixpath.normpath(path.strip()), serves.split()
        if not colon or not path or not words:
            raise Refused(f"{SHARED}:{n}: '{line}' is not '<path>: <--test, feature=<name> or package=<name>> ...'")
        for word in words:
            kind, equals, value = word.partition("=")
            if word != "--test" and not (equals and kind in ("feature", "package") and value):
                raise Refused(f"{SHARED}:{n}: '{word}' is none of --test, feature=<name> and package=<name>")
        if not exists(head, path):
            raise Refused(f"{SHARED}:{n}: names {path}, which is not there")
        helpers.setdefault(path, set()).update(words)
    return helpers


def features(args):
    """The features a row's cargo arguments turn on, each by its own name,
    and whether they turn on every feature (--all-features)."""
    words = args.split()
    before = words[: words.index("--")] if "--" in words else words
    named, every = set(), False
    for i, a in enumerate(before):
        value = None
        if a in ("--features", "-F") and i + 1 < len(before):
            value = before[i + 1]
        elif a.startswith("--features="):
            value = a[len("--features=") :]
        elif a.startswith("-F") and a != "-F":
            value = a[2:].lstrip("=")
        elif a == "--all-features":
            every = True
        for feature in (value or "").replace(",", " ").split():
            named.add(feature.rsplit("/", 1)[-1])
    return named, every


def packages(args):
    """The packages a row's cargo arguments build, or None for any of them
    (--workspace, or no package named, which is the workspace's own)."""
    words = args.split()
    before = words[: words.index("--")] if "--" in words else words
    named = set()
    for i, a in enumerate(before):
        if a == "--workspace" or a == "--all":
            return None
        if a in ("-p", "--package") and i + 1 < len(before):
            named.add(before[i + 1])
        elif a.startswith("--package="):
            named.add(a[len("--package=") :])
        elif a.startswith("-p") and a != "-p":
            named.add(a[2:].lstrip("="))
    return named or None


def serves(row, words):
    """Whether a helper serving `words` serves the row."""
    if "--test" in words and test_targets(row["args"]):
        return True
    turned_on, every = features(row["args"])
    for word in words:
        if word.startswith("feature=") and (every or word[len("feature=") :] in turned_on):
            return True
    built = packages(row["args"])
    for word in words:
        if word.startswith("package=") and (built is None or word[len("package=") :] in built):
            return True
    return False


def selects(row, changed, contents, shared, head):
    """Whether a change touches the row: its row file, its file, a file that
    names one of its tests (at the base or the head), its --test target, or
    the shared test code of a crate that holds that target."""
    if f"{ROWS}/{row['name']}.row" in changed or row["file"] in changed:
        return True
    tests = {name.rsplit("::", 1)[-1] for name in named_tests(row["args"])}
    if any(test and test in text for test in tests for text in contents):
        return True
    for target in test_targets(row["args"]):
        for path in changed:
            parts = path.split("/")
            if len(parts) >= 4 and parts[0] == "crates" and parts[2] == "tests":
                if parts[3] == f"{target}.rs" or (parts[3] == target and len(parts) > 4):
                    return True
        for crate in shared:
            if exists(head, f"crates/{crate}/tests/{target}.rs") or exists(head, f"crates/{crate}/tests/{target}"):
                return True
    return False


def touched(base, head, every=False):
    """The rows to run, the touched rows CI does not run, and the deleted rows."""
    rows = rows_at(head)
    local = local_rows(head, rows)
    if every:
        names = sorted(rows)
        gone = []
    else:
        changed = {
            posixpath.normpath(path.decode("utf-8", errors="surrogateescape"))
            for path in git("diff", "--no-renames", "--name-only", "-z", base, head).split(b"\0")
            if path
        }
        gone = deleted(base, head, changed, rows)
        references = [source_references(commit) for commit in (base, head)] if changed else []
        # Expand exactly one level from the original change, at both ends.
        # A removed include still belongs to the source that formerly read it.
        included = {owner for path in changed for refs in references for owner in references_to(path, refs)}
        changed |= included
        # Each changed file's text at the base and at the head, so a change to a
        # guarding test, or its removal, selects the row that names it.
        contents = []
        for path in sorted(changed):
            for commit in (base, head):
                if exists(commit, path):
                    contents.append(git("cat-file", "blob", f"{commit}:{path}").decode("utf-8", errors="replace"))
        shared = shared_test_crates(base, head, changed, references)
        helpers = [words for path, words in shared_helpers(head).items() if path in changed]
        # A row taken off the list of rows CI does not run is run.
        released = listed(base) - set(local)
        names = sorted(
            name
            for name, row in rows.items()
            if name in released
            or selects(row, changed, contents, shared, head)
            or any(serves(row, words) for words in helpers)
        )
    run = [rows[n] for n in names if n not in local]
    return run, [(n, local[n]) for n in names if n in local], gone


def build_key(row):
    args = row["args"].split()
    return " ".join(args[: args.index("--")] if "--" in args else args)


def shard(rows, shards, index):
    ordered = sorted(rows, key=lambda row: (build_key(row), row["name"]))
    n = len(ordered)
    return ordered[(index - 1) * n // shards : index * n // shards]


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("revs", nargs="+", metavar="BASE HEAD", help="the change's base and head; with --all, only the head")
    parser.add_argument("--all", action="store_true", help="every row, not only the touched ones")
    parser.add_argument("--shards", type=int, help="cut the rows to run into this many shards")
    parser.add_argument("--shard", type=int, help="print only this shard, from 1")
    parser.add_argument("--report", action="store_true", help="list touched rows not run, and deleted rows")
    args = parser.parse_args()
    if len(args.revs) != (1 if args.all else 2):
        parser.error("give BASE and HEAD, or --all and HEAD")
    base, head = (None, args.revs[0]) if args.all else args.revs
    if (args.shards is None) != (args.shard is None):
        parser.error("--shards and --shard go together")
    if args.shards is not None and not (args.shards >= 1 and 1 <= args.shard <= args.shards):
        parser.error("--shards must be at least 1, and --shard between 1 and --shards")
    if args.report and args.shards is not None:
        parser.error("--report takes no shard")
    try:
        run, not_run, gone = touched(base, head, every=args.all)
    except Refused as refused:
        print(f"scripts/controls-touched.py: {refused}", file=sys.stderr)
        return 1
    if args.report:
        for name, reason in not_run:
            print(f"not run in CI {name}: {reason}")
        for name, why in gone:
            print(f"deleted {name}: {why}")
        return 0
    if args.shards is not None:
        run = shard(run, args.shards, args.shard)
    for row in run:
        print(row["name"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
