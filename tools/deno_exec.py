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

import json
import re

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
    CREDENTIALS[manifest.parent.resolve()] = credentials
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
    SETTINGS[manifest.parent.resolve()] = deno


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
            local = (origin.parent / target).resolve()
            _require(local.is_file() and local.is_relative_to(project), f"{origin}: import {target} is missing or leaves the project")
            if local not in seen:
                seen.add(local)
                found |= _specifiers(local.read_text(), local, project, seen)
        else:
            found.add((origin, target))
    return found


def _declaration(path, fm, project, specifiers):
    """The parts of an atom that say what its code may do."""
    deno = (fm.get("runtime") or {}).get("deno", {})
    _require(isinstance(deno, dict) and deno.keys() <= DENO_KEYS, f"{path}: runtime.deno takes only {sorted(DENO_KEYS)}")
    _require(set(deno.get("sys", [])) <= HARMLESS_SYS, f"{path}: runtime.deno.sys may hold only {sorted(HARMLESS_SYS)}")
    # The lockfile beside the manifest is the default; the manifest may name
    # another and an atom a third. Whichever is chosen, its real path is settled
    # before it is checked or read: a relative path or a symlink must not lead
    # the checker out of the project, where a lockfile written elsewhere would
    # vouch for specifiers the project never recorded.
    settings = SETTINGS.get(project, {})
    declared = deno.get("lock", settings.get("lock", "deno.lock"))
    lock_path = ((path.parent if "lock" in deno else project) / declared).resolve()
    _require(lock_path.is_relative_to(project),
             f"{path}: lockfile {declared} is {lock_path}, which is outside the project")
    for source, target in sorted(specifiers):
        _require(PINNED.fullmatch(target), f"{source}: import {target!r} must be an npm: or jsr: package pinned to an exact version")
        _require("allow_packages" not in settings or _allowed(target, settings["allow_packages"]),
                 f"{source}: {target} is not in the manifest's runtime.deno.allow_packages")
        _require(lock_path.is_file(), f"{path}: lockfile {lock_path.name} is missing")
        _require(target in json.loads(lock_path.read_text()).get("specifiers", {}), f"{source}: {target} is not recorded in {lock_path.name}")

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
    project = project.resolve()
    _require(fm.get("exec", {}).keys() == {"script"}, f"{path}: with a script, exec takes only `script`")
    _require(not path.read_text().split("\n---\n", 1)[1].strip(), f"{path}: an exec atom has no instructions")
    _require("deno" in (fm.get("runtime") or {}), f"{path}: a script needs runtime.deno")
    # References in an atom file are relative to that file.
    script = (path.parent / fm["exec"]["script"]).resolve()
    _require(script.is_file() and script.suffix == ".ts", f"{path}: script {fm['exec']['script']} is not a TypeScript file")
    _require(script.is_relative_to(project), f"{path}: script {fm['exec']['script']} is outside the project")
    specifiers = _specifiers(script.read_text(), script, project, {script})
    _require(specifiers, f"{path}: {fm['exec']['script']} imports no package; a plain exec command would do")
    _require("network" in fm, f"{path}: declare the hosts the script may reach in network.allow")
    CHECKED.append((path, "script", *_declaration(path, fm, project, specifiers)))
