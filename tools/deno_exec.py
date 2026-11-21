# SPDX-License-Identifier: Apache-2.0
"""Static checks for atom code that runs on the proposed Deno runtime.

Executable code lives in a separate script named by an exec atom (`exec.script`).
Astro frontmatter and expressions are parsed by the shipped compiler.

Not an implementation of atomic, and it does not run Deno. It checks what can be
checked from the files: the code stays inside the project, every package import
is pinned to an exact version that the lockfile records, the atom declares the
hosts it may reach, credentials are secret references and never values, and an
atom that reaches the network says its effect is safe to repeat.

The code itself is exercised by the example's own `deno test`.
"""

from __future__ import annotations

import errno
import json
import os
import re
import stat
from pathlib import Path

CHECKED = []
CODE = "(code)"  # reserved; declarative Astro atoms never contain executable code

DENO_KEYS = {"lock", "sys"}
# Deno's `--allow-sys` kinds that only describe the machine. Nothing here reads
# user data; the AWS SDK uses osRelease for its user agent.
HARMLESS_SYS = {"osRelease", "hostname"}
HOST = re.compile(r"(?=.{1,253}$)([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,}(:\d{1,5})?")
ENV_NAME = re.compile(r"[A-Z][A-Z0-9_]*")
SECRET_NAME = re.compile(r"[a-z][a-z0-9-]*")
SENSITIVE = re.compile(r"KEY|SECRET|TOKEN|PASSWORD|CREDENTIAL")
IMPORT = re.compile(r"""(?:from|import)\s*\(?\s*["']([^"']+)["']""")
PINNED = re.compile(r"(npm|jsr):(@?[^@]+)@(\d+\.\d+\.\d+)(/.*)?")
PROVIDED = ("atomic:", "atoms:")  # modules atomic itself supplies

# Project-wide settings in the manifest, under runtime.deno. Each has a default,
# and each can only tighten what atom code may do.
MANIFEST_KEYS = {"binary", "lock", "minimum_dependency_age", "install_scripts", "allow_packages", "audit_permissions"}
# What code that reaches the network says about itself. `read-only` may be run by
# evals and previews; `idempotent` writes, and is only ever run by a real step.
EFFECTS = {"read-only", "idempotent"}
ROLE = re.compile(r"arn:aws:iam::\d{12}:role/[\w+=,.@/-]+")
DURATION = re.compile(r"(1[5-9]|[2-9]\d|\d{3,})m|[1-9]\d*h")
# Variables a credential provides. An atom never sets them itself.
PROVIDED_BY = {"aws_role": ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN")}
CREDENTIALS = {}  # project directory -> its named credentials
AGE = re.compile(r"[1-9]\d*[hd]")
PACKAGE_PATTERN = re.compile(r"(npm|jsr):(@[a-z0-9][a-z0-9._-]*/)?([a-z0-9][a-z0-9._-]*|\*)")
SETTINGS = {}  # project directory -> its runtime.deno settings


def _require(condition, message):
    if not condition:
        raise ValueError(message)


def _shape(value):
    """What a JSON value read from a file is, in the words a refusal shows.

    `json.loads` returns only these six kinds, so every value it can hand back
    is named: a refusal never says `a str` and never has to guess.
    """
    if value is None:
        return "nothing"
    if isinstance(value, bool):
        return "true or false"
    if isinstance(value, str):
        return "a string"
    if isinstance(value, list):
        return "a list"
    if isinstance(value, dict):
        return "an object"
    return "a number"


LOCK_FIX = "name a lockfile inside the project, such as deno.lock"


def _unresolvable(where, fix, error):
    """Refuses a path whose symlinks cannot be followed, naming the path and the fix.

    An OSError carries a symlink loop as ELOOP; older pathlibs raise a plain
    RuntimeError for one, which is the only thing strict resolving can raise
    that is not an OSError. Anything else is said as the system told it.
    """
    loop = error.errno == errno.ELOOP if isinstance(error, OSError) else isinstance(error, RuntimeError)
    reason = "is a symlink loop" if loop else f"cannot be resolved ({error})"
    raise ValueError(f"{where} {reason}; {fix}") from error


def _settled(path, where, fix):
    """The real place `path` names, with every symlink above it followed.

    A lockfile, a script or an imported module is read only once its path has
    been shown to sit inside the project, and the project directory is what
    every one of those is compared against, so each is settled first.
    `Path.resolve` is not enough on its own. It leaves a name that does not
    exist yet unresolved against whatever its directory is a link to, so a
    path with nothing at the end can settle outside the project before
    anything is read; and it reports a symlink loop differently by Python
    version -- as a missing file, as an OSError, or as a RuntimeError -- none
    of which says what to fix. So the part of the path that exists is settled
    strictly, links followed, and the part that does not exist is put back
    unchanged -- except a `..` among it, which is refused: put back, it moves
    the settled path, and every caller compares that path against the project
    without resolving it again. Every refusal says which path is wrong and
    what to write instead.
    """
    tail = path if path.is_absolute() else Path.cwd() / path
    missing = []
    # The root exists wherever this runs, so stopping at it only keeps a path
    # that exists nowhere from walking past the top of the tree forever.
    while not os.path.lexists(tail) and tail != tail.parent:
        missing.append(tail.name)
        tail = tail.parent
    try:
        settled = tail.resolve(strict=True)
    except FileNotFoundError:
        # A link to a file that is not there settles as far as it goes, so
        # that the caller still reports the file itself as missing. That
        # settling follows links of its own, so it meets a symlink loop too:
        # a link that points round has nothing at its end either, and is
        # refused as a loop rather than raising out of the check.
        try:
            settled = tail.resolve(strict=False)
        except (OSError, RuntimeError) as error:
            _unresolvable(where, fix, error)
    except (OSError, RuntimeError) as error:
        _unresolvable(where, fix, error)
    # What is not there goes back as it was written, so a `..` among it would
    # take the settled path with it, out of the project and past every
    # comparison the caller makes -- each caller compares the settled path
    # against the project without resolving it again. The walk only peels a
    # component it cannot see, so this is reached by a directory that is
    # missing, by one the checker may not enter, and by a name that is there
    # but is not a directory at all -- a lockfile used as a directory, say.
    # None of the three has a place a `..` could go up from.
    _require(".." not in missing, f"{where} goes up from a directory that is not there, cannot be read, or is not a directory; {fix}")
    for name in reversed(missing):
        settled = settled / name
    return settled


def _project(manifest):
    """The settled directory a manifest stands in, which holds the project."""
    return _settled(manifest.parent, f"{manifest}: the project directory", "keep the manifest in a project directory that exists")


def check_manifest(config, manifest):
    """Validates the manifest's credentials and runtime.deno blocks and remembers them for the atom checks."""
    credentials = config.get("credentials", {})
    _require(isinstance(credentials, dict), f"{manifest}: credentials maps a name to a credential")
    for name, credential in credentials.items():
        _require(isinstance(name, str) and SECRET_NAME.fullmatch(name), f"{manifest}: invalid credential name {name!r}")
        _require(isinstance(credential, dict) and credential.keys() <= {"aws_role", "duration"} and "aws_role" in credential,
                 f"{manifest}: credential {name} takes aws_role and an optional duration")
        _require(isinstance(credential["aws_role"], str) and ROLE.fullmatch(credential["aws_role"]), f"{manifest}: credential {name}: aws_role is a role ARN")
        _require("duration" not in credential or (isinstance(credential["duration"], str) and DURATION.fullmatch(credential["duration"])),
                 f"{manifest}: credential {name}: duration is at least 15m, such as 15m or 1h")
    project = _project(manifest)
    CREDENTIALS[project] = credentials
    runtime = config.get("runtime")
    if runtime is None:
        return
    _require(isinstance(runtime, dict) and set(runtime) == {"deno"}, f"{manifest}: runtime takes only deno in this catalog")
    deno = runtime["deno"]
    _require(isinstance(deno, dict) and deno.keys() <= MANIFEST_KEYS, f"{manifest}: runtime.deno takes only {sorted(MANIFEST_KEYS)}")
    if "minimum_dependency_age" in deno:
        _require(isinstance(deno["minimum_dependency_age"], str) and AGE.fullmatch(deno["minimum_dependency_age"]),
                 f"{manifest}: minimum_dependency_age is a number of hours or days, such as 12h or 7d")
    for key in ("install_scripts", "allow_packages"):
        if key in deno:
            values = deno[key]
            _require(isinstance(values, list) and all(isinstance(v, str) and PACKAGE_PATTERN.fullmatch(v) for v in values),
                     f"{manifest}: {key} lists packages such as npm:@aws-sdk/* or jsr:@std/assert")
    _require(isinstance(deno.get("audit_permissions", False), bool), f"{manifest}: audit_permissions is true or false")
    binary = deno.get("binary", "managed")
    _require(isinstance(binary, str) and (binary in ("managed", "system") or binary.startswith("/")),
             f"{manifest}: binary is managed, system or an absolute path")
    _require(not any(v.endswith("*") for v in deno.get("install_scripts", [])), f"{manifest}: install_scripts names packages one by one")
    # The lock is the one every run must match, so it is checked as the
    # manifest is read, against the project directory the manifest stands in,
    # whether or not an atom ever reads it.
    declared = deno.get("lock", "deno.lock")
    # A NUL is not a name anything can open, so it is refused as the wrong name
    # for the key rather than reaching the file system and raising there.
    _require(isinstance(declared, str) and declared and "\x00" not in declared,
             f"{manifest}: runtime.deno.lock is {deno.get('lock')!r}, not a path to a lockfile; {LOCK_FIX}")
    lock = _settled(project / declared, f"{manifest}: runtime.deno.lock {declared!r}", LOCK_FIX)
    _require(lock.is_relative_to(project),
             f"{manifest}: runtime.deno.lock {declared!r} is {lock}, which is outside the project; {LOCK_FIX}")
    # What the manifest names is the manifest's own fault to put right, so a
    # route to the lock that the checker cannot walk is refused here, whether
    # or not an atom ever reads it. So is a name that leads to something that is
    # no lockfile at all -- a directory, a queue, a device -- which is asked
    # only where something is there: that the lockfile is not there yet is left
    # to the atom that needs it, which is the check that says which import a
    # missing lock cannot vouch for.
    mode = _seen(lock, manifest, "lockfile")
    _require(mode is None or stat.S_ISREG(mode), f"{manifest}: lockfile {lock.name} is not a regular file")
    SETTINGS[project] = deno


def _allowed(target, patterns):
    """Whether an import such as npm:@aws-sdk/client-s3@3.1169.0 matches an allowed package pattern."""
    name = PINNED.fullmatch(target)
    package = f"{name[1]}:{name[2]}"
    return any(package == pattern or (pattern.endswith("/*") and package.startswith(pattern[:-1])) for pattern in patterns)


# --- shared checks ------------------------------------------------------------------

def _specifiers(text, origin, project, seen):
    """Package specifiers imported by a piece of code and by the local modules it imports."""
    found = set()
    for target in IMPORT.findall(text):
        if target.startswith(PROVIDED):
            continue
        if target.startswith("."):
            local = _settled(origin.parent / target, f"{origin}: import {target}", "import a module inside the project")
            # A module behind a directory the checker may not enter is refused
            # by that route, which is not what a module that is not there says.
            # What is there is compared as it settled, as every caller compares
            # what it settled, and is opened once the way to it is settled: a
            # module whose contents the checker may not have is refused as that
            # too, and never read as one that imports nothing.
            _require(_can_see(local, origin, "import"), f"{origin}: import {target} is missing or leaves the project")
            _require(local.is_relative_to(project), f"{origin}: import {target} is missing or leaves the project")
            if local not in seen:
                seen.add(local)
                found |= _specifiers(_read(local, origin, "import"), local, project, seen)
        else:
            found.add((origin, target))
    return found


# What the file system says when a path has nothing at it: the name is not
# there, or a name above it is not a directory to look in. Everything else it
# reports -- a directory the checker may not enter, a link that points round --
# is a path it cannot see, not a path with nothing at it.
ABSENT = (errno.ENOENT, errno.ENOTDIR)


def _seen(path, where, what):
    """What the file system says the settled `path` holds, refusing what it will not say.

    `where` and `what` name the thing the caller is looking for -- a lockfile,
    an import, a named script -- so that a refusal says which one, at which
    path, as the file system told it. What comes back is `stat`'s own mode, or
    None where there is nothing to look at.

    `Path.is_file` cannot carry this. It answers some errors as its own
    judgement of them rather than as the file system reported them, and which
    errors those are has changed between Python versions, so a directory above
    the file that the checker may not enter reads as no file at all on one
    version and raises on the next. What is asked here is what the inode is.

    Only `ABSENT` is answered None, which leaves it to the caller whether the
    project needed the file. Anything else the file system reports -- a
    directory it may not enter, a link that points round -- becomes a refusal
    naming the path, because a file the checker cannot see cannot vouch for an
    import either, and an error escaping the check says nothing about the file
    at fault.
    """
    try:
        return path.stat().st_mode
    except OSError as error:
        if error.errno in ABSENT:
            return None
        raise ValueError(f"{where}: {what} {path.name} cannot be read ({error})") from error


def _can_see(path, where, what):
    """Whether a regular file sits at the settled `path`, with a directory answered as no file.

    A name that holds a directory is no file, and is answered as the caller
    answers a missing one, because a lockfile or a script that is a directory is
    a name to put right. A name holding anything else a file system can hold --
    a queue, a socket, a device -- is refused here instead: what the caller does
    next is open it, and opening a queue waits for a writer that will never
    come.
    """
    mode = _seen(path, where, what)
    if mode is None or stat.S_ISDIR(mode):
        return False
    _require(stat.S_ISREG(mode), f"{where}: {what} {path.name} is not a regular file")
    return True


def _read(path, where, what):
    """The text of a file the checker settled on, refused by name where it cannot be opened.

    Being a regular file is not being readable, so a file whose contents the
    checker may not have is refused as that -- as a lock whose contents it may
    not have is -- and not as an error escaping the check. Text that is not text
    is refused the same way, since nothing else can be said about it.
    """
    try:
        return path.read_text()
    except (OSError, UnicodeDecodeError) as error:
        raise ValueError(f"{where}: {what} {path.name} cannot be read ({error})") from error


def _lock(path, deno, settings, project):
    """The settled lockfile an atom's imports are checked against.

    The lockfile beside the manifest is the default; the manifest may name
    another and an atom a third. Whichever is chosen, its real path is settled
    before it is checked or read: a relative path or a symlink must not lead
    the checker out of the project, where a lockfile written elsewhere would
    vouch for specifiers the project never recorded.
    """
    if "lock" in deno:
        where, declared, base = f"{path}: runtime.deno.lock", deno["lock"], path.parent
    elif "lock" in settings:
        where, declared, base = f"{path}: the manifest's runtime.deno.lock", settings["lock"], project
    else:
        where, declared, base = f"{path}: the default lockfile beside the manifest", "deno.lock", project
    _require(isinstance(declared, str) and declared and "\x00" not in declared,
             f"{where} is {declared!r}, not a path to a lockfile; {LOCK_FIX}")
    lock_path = _settled(base / declared, where, LOCK_FIX)
    _require(lock_path.is_relative_to(project), f"{where} is {lock_path}, which is outside the project; {LOCK_FIX}")
    return lock_path


def _declaration(path, fm, project, specifiers):
    """The parts of an atom that say what its code may do."""
    deno = (fm.get("runtime") or {}).get("deno", {})
    _require(isinstance(deno, dict) and deno.keys() <= DENO_KEYS, f"{path}: runtime.deno takes only {sorted(DENO_KEYS)}")
    _require(set(deno.get("sys", [])) <= HARMLESS_SYS, f"{path}: runtime.deno.sys may hold only {sorted(HARMLESS_SYS)}")
    settings = SETTINGS.get(project, {})
    lock_path = _lock(path, deno, settings, project)
    _require(_can_see(lock_path, path, "lockfile"), f"{path}: lockfile {lock_path.name} is missing")
    # The lock is read once, for every import. Settled and shown to be a file,
    # it is still one the checker may not be able to open, and a lock that
    # cannot be opened or is no JSON this Python can read -- JSON nested deeper
    # than this Python reads is that -- vouches for nothing and is refused as
    # what it is rather than raising. What it holds is taken as what it records,
    # so its shape is checked before one import is answered from it: a body that
    # is not an object, and a `specifiers` that is not the map from each package
    # to its version, say nothing about any import -- and a string answers every
    # import that is a substring of it.
    text = _read(lock_path, path, "lockfile")
    try:
        body = json.loads(text)
    except (ValueError, RecursionError) as error:
        raise ValueError(f"{path}: lockfile {lock_path.name} cannot be read as JSON ({error})") from error
    _require(isinstance(body, dict),
             f"{path}: lockfile {lock_path.name} is {_shape(body)}, not a JSON object holding what the project recorded")
    recorded = body.get("specifiers", {})
    _require(isinstance(recorded, dict),
             f"{path}: lockfile {lock_path.name} records specifiers as {_shape(recorded)}, not as the map from each package to its version")
    for source, target in sorted(specifiers):
        _require(PINNED.fullmatch(target), f"{source}: import {target!r} must be an npm: or jsr: package pinned to an exact version")
        _require("allow_packages" not in settings or _allowed(target, settings["allow_packages"]),
                 f"{source}: {target} is not in the manifest's runtime.deno.allow_packages")
        _require(target in recorded, f"{source}: {target} is not recorded in {lock_path.name}")

    hosts = []
    if "network" in fm:
        network = fm["network"]
        _require(isinstance(network, dict) and set(network) == {"allow"}, f"{path}: declare the hosts the code may reach in network.allow")
        hosts = network["allow"]
        _require(isinstance(hosts, list) and hosts, f"{path}: network.allow is empty")
        for host in hosts:
            _require(isinstance(host, str) and HOST.fullmatch(host), f"{path}: {host!r} is not a plain host name; wildcards and URLs are not accepted")
        # atomic cannot observe what the code sends, so the atom has to say that
        # a repeated attempt is safe. Without that a crashed attempt parks.
        effects = fm.get("effects")
        _require(isinstance(effects, dict) and set(effects) == {"external"} and effects["external"] in EFFECTS,
                 f"{path}: code that reaches the network declares effects.external as read-only or idempotent")

    # Named credentials are defined in the manifest and resolved at dispatch.
    names = fm.get("credentials", [])
    known = CREDENTIALS.get(project, {})
    _require(isinstance(names, list) and all(isinstance(n, str) for n in names), f"{path}: credentials is a list of names")
    provided = set()
    for name in names:
        _require(name in known, f"{path}: credential {name!r} is not defined in the manifest")
        provided |= {variable for kind, variables in PROVIDED_BY.items() if kind in known[name] for variable in variables}
    _require(len(names) <= 1, f"{path}: one credential per atom keeps its access narrow")
    clash = provided & set(fm.get("env") or {})
    _require(not clash, f"{path}: {sorted(clash)} come from the credential and must not be set in env")

    secrets = 0
    for name, value in (fm.get("env") or {}).items():
        _require(ENV_NAME.fullmatch(name), f"{path}: {name!r} is not an environment variable name")
        if isinstance(value, dict):
            _require(set(value) == {"secret"} and isinstance(value["secret"], str) and SECRET_NAME.fullmatch(value["secret"]),
                     f"{path}: {name} must be a literal or {{ secret: <name> }}")
            secrets += 1
        else:
            _require(isinstance(value, str), f"{path}: {name} must be a string or a secret reference")
            _require(not SENSITIVE.search(name), f"{path}: {name} looks like a credential; reference a secret instead of writing a value")
    return len(hosts), secrets + len(names)


def check_script(path, fm, project):
    """An exec atom that names a script for the Deno runtime."""
    project = _settled(project, f"{path}: the project directory", "keep the atom inside the project the manifest names")
    _require(fm.get("exec", {}).keys() == {"script"}, f"{path}: with a script, exec takes only `script`")
    _require(not path.read_text().split("\n---\n", 1)[1].strip(), f"{path}: an exec atom has no instructions")
    _require("deno" in (fm.get("runtime") or {}), f"{path}: a script needs runtime.deno")
    # References in an atom file are relative to that file.
    script = _settled(path.parent / fm["exec"]["script"], f"{path}: exec.script {fm['exec']['script']!r}",
                      "name a TypeScript file inside the project")
    # A script the checker may not open is refused as that, which is not what a
    # script that is not there says; either way it is not read. What is there
    # is compared as it settled, as every caller compares what it settled.
    _require(_can_see(script, path, "exec.script"),
             f"{path}: exec.script {fm['exec']['script']!r} is missing or cannot be read")
    _require(script.suffix == ".ts", f"{path}: script {fm['exec']['script']} is not a TypeScript file")
    _require(script.is_relative_to(project), f"{path}: script {fm['exec']['script']} is outside the project")
    specifiers = _specifiers(_read(script, path, "exec.script"), script, project, {script})
    _require(specifiers, f"{path}: {fm['exec']['script']} imports no package; a plain exec command would do")
    _require("network" in fm, f"{path}: declare the hosts the script may reach in network.allow")
    CHECKED.append((path, "script", *_declaration(path, fm, project, specifiers)))
