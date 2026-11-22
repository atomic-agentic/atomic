#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Offline checks for the proposed examples, not an implementation of atomic."""

from __future__ import annotations

import ast
import datetime
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile

import yaml
from jsonschema import Draft202012Validator

import astro_atoms
import decisions
import deno_exec
import workflow_diagram
import yaml_scalar
from referencing import Registry, Resource
from referencing.jsonschema import DRAFT202012


ROOT = Path(__file__).resolve().parents[1]
EXAMPLES = ROOT / "examples"


class UniqueKeysLoader(yaml.SafeLoader):
    pass


# The shapes a plain scalar can be, and the two rules the key path below asks by
# name, all of them a port of a rule in crates/atomic-schema/src/yaml.rs. That
# loader classifies a plain scalar by its own text, and `yaml_scalar` is the same
# classifier: `core_scalar` answers what it reads a text as or why it refuses it,
# `schema_scalar` puts the tag the library resolved beside that answer for a value,
# and the key path below asks the same rules for a key's text. The module needs no
# third-party library, so `yaml_scalar_test.py` can check it where the library that
# reads the catalog is not installed.
#
# PyYAML, which reads the catalog here, resolves scalars the way YAML 1.1 does, so
# nothing here can depend on what PyYAML constructs: `0o17` is the string "0o17" to
# PyYAML and the number 15 to the loader, and `12:30` is the integer 750 to PyYAML
# and a string to the loader. Both are refused as keys, as the loader refuses them,
# and read or refused as values the way the loader reads or refuses them.


def mapping_key(loader, key_node, deep):
    """One mapping key, read the way atomic's loader reads it.

    The loader takes a plain key that is a canonical decimal integer -- `0` or
    `-?[1-9][0-9]*` -- as its own text, so an OpenAPI spec's `200:` is the
    property name "200" and one key with `"200":`. Every other plain integer
    spelling is refused there, because the key would be renamed to the number
    some version reads, and quoting is the fix. That is judged from the key's
    own text and not from what PyYAML constructs, since PyYAML follows YAML 1.1:
    it reads `200:` as the integer 200, `0200:` as an octal, 128, `08:` as a
    string, `0o17:` as a string and `12:30:` as the integer 750.

    A key the loader reads as neither a string nor a number -- `true:`, `null:` --
    is constructed as written and comes back a bool or a null: `strict_scalar`
    makes it so in a schema file, where it answers every plain scalar, and PyYAML's
    own constructor elsewhere. It is `json_model` that reports it here, as no key a
    JSON document can hold; atomic's loader refuses such a key itself. A float,
    like `1.5:` or `1e3:`, is refused with the integers: the loader reads a number
    and refuses the key, and PyYAML reads `1e3` as the string "1e3" and would let
    it through.
    """
    spelling = key_node.value
    if key_node.style is None:
        line = key_node.start_mark.line + 1
        reason = None
        if spelling.lower() in yaml_scalar.YAML_11_BOOLS:
            reason = "is a boolean in YAML 1.1 and a string in YAML 1.2"
        elif yaml_scalar.INF_OR_NAN.match(spelling):
            reason = "cannot be written in JSON"
        elif yaml_scalar.canonical_integer_key(spelling):
            reason = None
        elif yaml_scalar.SIGN_ON_A_BASE_MARKER.match(spelling):
            reason = "puts a sign on the wrong side of its base marker"
        elif yaml_scalar.LEADING_ZERO_INT.match(spelling):
            reason = "has a leading zero, which YAML 1.1 reads as an octal"
        elif yaml_scalar.CORE_INT.match(spelling) or yaml_scalar.CORE_FLOAT.match(spelling):
            reason = "is a number, so the key would be renamed to it"
        elif yaml_scalar.one_version_integer(spelling):
            reason = "is a number in YAML 1.1 and a string in YAML 1.2 and JSON"
        require(
            reason is None,
            f"{spelling!r} key at line {line} {reason}; atomic reads a plain key as its own "
            f"text only when it is a canonical decimal integer; quote it (\"{spelling}\")",
        )
    key = loader.construct_object(key_node, deep=deep)
    if not isinstance(key, int) or isinstance(key, bool):
        return key
    line = key_node.start_mark.line + 1
    require(
        str(key) == spelling,
        f"{spelling!r} key at line {line} is not a plain decimal integer as atomic reads it; "
        f"quote it (\"{spelling}\")",
    )
    return str(key)


def unique_mapping(loader, node, deep=False):
    result = {}
    for key_node, value_node in node.value:
        key = mapping_key(loader, key_node, deep)
        if key in result:
            raise ValueError(f"duplicate YAML key {key!r} at line {key_node.start_mark.line + 1}")
        result[key] = loader.construct_object(value_node, deep=deep)
    return result


UniqueKeysLoader.add_constructor(
    yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, unique_mapping
)


def read_yaml(text):
    return yaml.load(text, Loader=UniqueKeysLoader)


class SchemaLoader(UniqueKeysLoader):
    """YAML restricted to what JSON can express, for schema files."""


def strict_scalar(loader, node):
    """One plain value, read the way atomic's loader reads it.

    PyYAML resolves a plain scalar the way YAML 1.1 does and then constructs from
    what it resolved, so left alone it reads `0200` as the integer 128 that YAML
    1.1 reads where the loader refuses the spelling, `-0x1F`, `1_000` and `12:30`
    as the integers YAML 1.1 reads where the loader reads strings, `.inf` as an
    infinity JSON has no form for, and `0o17` and `1e3` as strings where the
    loader reads 15 and 1000.0. So how the scalar was written, the tag PyYAML
    resolved and the text are asked together, of `yaml_scalar.schema_scalar` -- the
    module that holds the loader's rules and that the key rule above reads from
    too: a spelling the loader refuses is refused here with its reason and its fix,
    and what the core schema reads is what comes back. Only `true` and `false` are
    booleans, as the loader has it, because `yes`, `no`, `on` and `off` are among
    what it refuses.

    A quoted scalar is its own text to every version, so `schema_scalar` answers it
    before it asks anything. A plain one PyYAML resolved to a tag whose value this
    has no answer for -- a `2026-11-21`, which YAML 1.1 resolves as a timestamp and
    the core schema reads as a string -- never reaches here: SafeLoader's
    constructor for the timestamp tag is popped off this loader, so PyYAML raises
    that it has no constructor for the tag rather than build a date no JSON
    document can hold. A scalar that does reach this with a tag outside the five is
    refused by `schema_scalar` the same way, named by the tag.
    """
    kind, answer = yaml_scalar.schema_scalar(node.style, node.tag, node.value)
    if kind == yaml_scalar.REFUSED:
        raise ValueError(
            f"{node.value!r} value at line {node.start_mark.line + 1} is not one atomic's "
            f"loader reads: {answer.as_value()}"
        )
    return answer


# The tags PyYAML's resolver can give a plain scalar that the core schema reads a
# null, a boolean, a number or a string by. A constructor on the default tag alone
# would never see the ones PyYAML resolved on its own, which is exactly the set
# this has to read: `0200`, `1_000` and `12:30` as integers, `.inf` as a float,
# `yes` as a boolean and `~` as a null. The default scalar tag is
# `tag:yaml.org,2002:str` itself, which is what a plain scalar nothing resolved
# gets -- `0o17` and `1e3` -- and what every quoted scalar gets.
#
# Three more tags the resolver can give a plain scalar are left unregistered, and
# what happens to each is a refusal: the timestamp, which is what a plain
# `2026-11-21` gets, and which SafeLoader would otherwise construct a
# `datetime.date` for; and the merge (`<<`) and value (`=`) keys, which PyYAML
# resolves only where they are written plainly. All three are strings to the core
# schema and to atomic's loader, so refusing them is stricter than the loader: the
# catalog has no use for a plain date, and `a: =` and `a: <<` are refused here with
# PyYAML's own "could not determine a constructor" error, which names the tag.
if yaml.resolver.BaseResolver.DEFAULT_SCALAR_TAG != yaml_scalar.STR_TAG:
    raise ValueError(
        f"PyYAML's default scalar tag is "
        f"{yaml.resolver.BaseResolver.DEFAULT_SCALAR_TAG!r}, which is not what yaml_scalar "
        f"reads a plain string as; the constructor below would never see a plain scalar"
    )
for _tag in yaml_scalar.SCALAR_TAGS:
    SchemaLoader.add_constructor(_tag, strict_scalar)
# SafeLoader has a constructor for the timestamp tag: it builds a `datetime.date`
# for a plain `2026-11-21`, which atomic's loader has no type for at all. Drop
# that one constructor here, so a plain date is refused by name instead of
# reaching `json_model` as an object no JSON document can hold. A subclass shares
# its bases' constructors by reference only until it adds one of its own, and the
# loop just above added five, so the dict popped below is already this loader's
# own: SafeLoader and the loader above -- which read the manifests and the eval
# cases -- have kept their dates since that loop, with or without the copy here,
# which makes the `dict(...)` a harmless restatement of what the pop edits rather
# than what keeps the dates.
SchemaLoader.yaml_constructors = dict(SchemaLoader.yaml_constructors)
SchemaLoader.yaml_constructors.pop(yaml_scalar.TIMESTAMP_TAG, None)


def json_model(value, path):
    """Fails on anything a JSON document could not hold.

    An integer key is a string by the time it gets here: mapping_key has read
    it the way atomic's loader does, and refused any other spelling.
    """
    if isinstance(value, dict):
        for key, child in value.items():
            require(isinstance(key, str), f"{path}: non-string key {key!r}")
            json_model(child, path)
    elif isinstance(value, list):
        for child in value:
            json_model(child, path)
    else:
        require(
            value is None or isinstance(value, (str, int, float, bool)),
            f"{path}: value {value!r} has no JSON equivalent",
        )


SCHEMA_SUFFIXES = (".schema.yaml", ".schema.json")


def read_schema_text(text, where):
    """A schema as written, read the way atomic's loader would read it.

    `where` is what a refusal names -- the file, or the stand-in name a mirror
    case gives -- because a rule that refuses an example without saying which is
    of no use to whoever has to fix it.
    """
    schema = yaml.load(text, Loader=SchemaLoader)
    json_model(schema, where)
    return schema


def read_schema(path):
    if path.name.endswith(".schema.json"):
        return read_json(path)
    return read_schema_text(path.read_text(), path)


def sidecar(atom_path, stem, role):
    """The atom's adjacent schema: YAML by default, JSON accepted, never both."""
    found = [
        candidate
        for suffix in SCHEMA_SUFFIXES
        if (candidate := atom_path.with_name(f"{stem}.{role}{suffix}")).is_file()
    ]
    require(found, f"{atom_path}: missing {role} schema sidecar")
    require(len(found) == 1, f"{atom_path}: {role} schema exists in both YAML and JSON")
    return found[0]


def read_json(path):
    def unique_pairs(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate JSON key {key!r} in {path}")
            result[key] = value
        return result

    return json.loads(path.read_text(), object_pairs_hook=unique_pairs)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def frontmatter(path):
    _, config = astro_atoms.load(path)
    return config, path.read_text().split("\n---\n", 1)[-1]


def validator(uri, registry):
    # No retrieval callback: unregistered references fail instead of using the network.
    return Draft202012Validator({"$ref": uri}, registry=registry)


def resolve_root(schema, base_uri, registry):
    seen = set()
    resolver = registry.resolver(base_uri)
    while "$ref" in schema:
        ref = schema["$ref"]
        require((base_uri, ref) not in seen, f"reference-only cycle at {ref}")
        seen.add((base_uri, ref))
        resolved = resolver.lookup(ref)
        resolver, schema = resolved.resolver, resolved.contents
        base_uri = ref
    return schema


def check_schema_refs(schema, base_uri, registry):
    # Resolves references even in optional fields that fixtures don't exercise.
    def visit(value, resolver):
        if isinstance(value, dict):
            if "$id" in value:
                resolver = resolver.in_subresource(
                    Resource.from_contents(value, default_specification=DRAFT202012)
                )
            if "$ref" in value:
                resolver.lookup(value["$ref"])
            for child in value.values():
                visit(child, resolver)
        elif isinstance(value, list):
            for child in value:
                visit(child, resolver)

    visit(schema, registry.resolver(base_uri))


def check_input_path(path, schema, base_uri, registry):
    current = schema
    for part in path.split("."):
        current = resolve_root(current, base_uri, registry)
        require(part in current.get("properties", {}), f"unknown input path {path}")
        current = current["properties"][part]


def check_markdown_links(path):
    for target in re.findall(r"\]\(([^)]+)\)", path.read_text()):
        if "://" in target or target.startswith("#"):
            continue
        target = target.split("#", 1)[0]
        require((path.parent / target).exists(), f"{path}: broken local link {target}")


ATOM_SUFFIXES = (".atom.astro",)



def atom_stem(path):
    return next(path.name.removesuffix(s) for s in ATOM_SUFFIXES if path.name.endswith(s))


def atom_schema(path, stem, role, fm, schemas, registry):
    """The adjacent sidecar, an explicit `$ref` to a schema file, or an inline schema.

    An inline schema may be full JSON Schema or the compact form. It is compiled,
    checked and registered under a name of its own. Returns (key, registry).
    """
    declared = fm.get(role)
    if declared is None:
        return sidecar(path, stem, role), registry
    for suffix in SCHEMA_SUFFIXES:
        require(
            not path.with_name(f"{stem}.{role}{suffix}").exists(),
            f"{path}: {role} is declared in frontmatter and also has a sidecar",
        )
    if isinstance(declared, dict) and set(declared) == {"$ref"}:
        require("#" not in declared["$ref"], f"{path}: {role} $ref must name a whole schema file")
        target = (path.parent / declared["$ref"]).resolve()
        require(target in schemas, f"{path}: {role} $ref does not name a schema file in this example")
        return target, registry
    schema = declared
    json_model(schema, path)
    Draft202012Validator.check_schema(schema)
    key = path.with_name(f"{stem}.{role}.inline")
    schemas[key] = schema
    registry = registry.with_resource(key.as_uri(), Resource.from_contents(schema, default_specification=DRAFT202012))
    return key, registry


def check_project(top):
    manifests = sorted(top.rglob("atomic.yaml"))
    require(manifests, f"{top}: no manifest")
    registry = Registry()
    schemas = {}
    schema_ids = {}
    schema_paths = sorted(p for suffix in SCHEMA_SUFFIXES for p in top.rglob("*" + suffix))
    for path in schema_paths:
        require(
            path.name.endswith(".schema.yaml"),
            f"{path}: the catalog demonstrates the YAML schema default",
        )
        schema = read_schema(path)
        Draft202012Validator.check_schema(schema)
        schemas[path] = schema
        resource = Resource.from_contents(schema)
        registry = registry.with_resource(path.as_uri(), resource)
        if "$id" in schema:
            sid = schema["$id"]
            require(sid not in schema_ids, f"duplicate schema ID {sid}")
            schema_ids[sid] = path
            registry = registry.with_resource(sid, resource)
    for path, schema in schemas.items():
        check_schema_refs(schema, path.as_uri(), registry)

    atom_count = fixture_count = exec_count = workflow_count = diagram_count = 0
    for manifest in manifests:
        project = manifest.parent
        config = read_yaml(manifest.read_text())
        require(config.get("atomic") == 1, f"{manifest}: wrong format")
        require(isinstance(config.get("name"), str), f"{manifest}: no project name")
        deno_exec.check_manifest(config, manifest)
        # Nearest manifest defines the project; a parent's globs do not ingest members twice.
        def belongs(path):
            return not any(
                parent != project and (parent / "atomic.yaml").is_file()
                for parent in path.parents
                if parent.is_relative_to(project)
            )

        atoms = {}
        templates = {}
        variants = {}
        prepared_schemas = {}  # atom name -> the shape its prepare() returns, from a committed sample
        for path in sorted(p for suffix in ATOM_SUFFIXES for p in project.rglob("*" + suffix)):
            if not belongs(path):
                continue
            fm, body = frontmatter(path)
            stem = atom_stem(path)
            if "." in stem:
                # `<name>.<variant>.atom.astro`: same schemas and evals as the base atom,
                # different instructions and settings.
                base, variant = stem.split(".", 1)
                require(base in atoms, f"{path}: variant of {base}, which is not an atom in this project")
                require(re.fullmatch(r"[a-z][a-z0-9-]*", variant), f"{path}: invalid variant name {variant!r}")
                require(fm.get("atomic") == 1, f"{path}: wrong format")
                _, inp, out, inputs, outputs = atoms[base]
                astro_atoms.check(path, schemas[inputs], schemas[outputs], registry, inputs.as_uri(), outputs.as_uri())
                require(variant not in variants.setdefault(base, set()), f"{path}: duplicate variant {variant}")
                variants[base].add(variant)
                continue
            name = fm.get("name", stem)
            require(name not in atoms, f"{path}: duplicate atom name")
            require(fm.get("atomic") == 1, f"{path}: wrong format")
            require(isinstance(fm.get("description"), str), f"{path}: no description")
            require(fm.get("version") != 1, f"{path}: redundant default version")
            require(fm.get("sandbox") != "read-only", f"{path}: redundant sandbox default")
            require("evals" not in fm, f"{path}: catalog should demonstrate default eval discovery")
            require("tools" not in fm, f"{path}: no catalog atom needs optional tools")
            require(fm.get("kind", "codex") in {"codex", "lite", "exec", "a2a"}, f"{path}: unknown kind")
            inputs, registry = atom_schema(path, stem, "input", fm, schemas, registry)
            outputs, registry = atom_schema(path, stem, "output", fm, schemas, registry)
            require(inputs in schemas and outputs in schemas, f"{path}: missing sidecars")
            inp, out = validator(inputs.as_uri(), registry), validator(outputs.as_uri(), registry)
            atoms[name] = (fm, inp, out, inputs, outputs)
            atom_count += 1
            if fm.get("kind") != "exec":
                require(body.strip(), f"{path}: missing instructions")
            elif "script" in fm.get("exec", {}):
                # A script on the Deno runtime. Checked statically; it reaches a
                # service, so the catalog does not run it.
                deno_exec.check_script(path, fm, project)
            else:
                command = fm.get("exec", {}).get("command")
                require(isinstance(command, list) and all(isinstance(x, str) for x in command), f"{path}: command must be an argv list")
                require(command and command[0] == "python3", f"{path}: unsupported catalog command")
                require(len(command) == 2 and (project / command[1]).is_file(), f"{path}: missing exec script")
                ast.parse((project / command[1]).read_text())
            astro_atoms.check(path, schemas[inputs], schemas[outputs], registry, inputs.as_uri(), outputs.as_uri())
            for example in fm.get("examples", []):
                inp.validate(example["input"])
                out.validate(example["output"])
                astro_atoms.fixture(path, example["input"], example["output"])
            evals = project / "evals" / name
            cases = sorted(evals.glob("*.yaml"))
            case_files = sorted(evals.glob("*.jsonl"))
            if (fm.get("effects") or {}).get("external") == "idempotent":
                # Code that writes to an outside system is never run by an eval.
                # It has code tests instead, and its fixtures are still contract samples.
                require(not cases and not case_files, f"{path}: an atom whose code writes has tests, not eval cases")
                script = Path(fm["exec"]["script"]).name
                tests = sorted((project / "test").glob("*_test.ts"))
                require(any(script in test.read_text() for test in tests), f"{path}: no test in test/ runs {script}")
                samples = sorted((project / "fixtures" / name).glob("*.input.json"))
                require(samples, f"{path}: no fixture pair for the tests")
                for sample in samples:
                    inp.validate(read_json(sample))
                    out.validate(read_json(Path(str(sample).removesuffix(".input.json") + ".output.json")))
                    fixture_count += 1
                continue
            require(cases or case_files, f"{path}: no evals")
            for case_file in case_files:
                # Many cases to a file: one JSON object per line, same two keys as a YAML case.
                rows = [json.loads(line) for line in case_file.read_text().splitlines() if line.strip()]
                require(rows, f"{case_file}: no cases")
                golden_path = project / "fixtures" / name / (case_file.stem + ".outputs.jsonl")
                golden = [json.loads(line) for line in golden_path.read_text().splitlines() if line.strip()]
                require(len(golden) == len(rows), f"{golden_path}: one golden output per case is required")
                for number, (row, expected) in enumerate(zip(rows, golden), 1):
                    require(isinstance(row, dict) and set(row) == {"input", "expect"}, f"{case_file}:{number}: unknown eval field")
                    require(isinstance(row["expect"], list) and row["expect"] and all(isinstance(x, str) for x in row["expect"]), f"{case_file}:{number}: missing CEL assertions")
                    inp.validate(row["input"])
                    out.validate(expected)
                    astro_atoms.fixture(path, row["input"], expected)
                    fixture_count += 1
            for eval_path in cases:
                ev = read_yaml(eval_path.read_text())
                # A case for an atom with code may supply `prepared`: the values
                # prepare() would return. The eval then does not run prepare().
                has_code = deno_exec.CODE in fm
                require(set(ev) - {"prepared", "origin"} == {"input", "expect"}, f"{eval_path}: unknown eval field")
                require(ev.get("origin", "external") in ("external", "agent", "team"), f"{eval_path}: `origin` is external, agent or team")
                require(("prepared" in ev) == has_code, f"{eval_path}: catalog cases give `prepared` exactly when the atom has code")
                if has_code:
                    sample = Path(str(project / "fixtures" / name / eval_path.stem) + ".prepared.json")
                    require(sample.is_file() and read_json(sample) == ev["prepared"], f"{eval_path}: eval and fixture prepared values drift")
                require(isinstance(ev["expect"], list) and ev["expect"] and all(isinstance(x, str) for x in ev["expect"]), f"{eval_path}: missing CEL assertions")
                prefix = project / "fixtures" / name / eval_path.stem
                fixture_input = read_json(Path(str(prefix) + ".input.json"))
                fixture_output = read_json(Path(str(prefix) + ".output.json"))
                require(fixture_input == ev["input"], f"{eval_path}: eval and fixture input drift")
                inp.validate(fixture_input)
                out.validate(fixture_output)
                astro_atoms.fixture(path, fixture_input, fixture_output)
                fixture_count += 1
                if fm.get("kind") == "exec" and "command" in fm["exec"]:
                    result = subprocess.run(fm["exec"]["command"], cwd=project, input=json.dumps(fixture_input), text=True, capture_output=True, timeout=5)
                    require(result.returncode == 0, f"{eval_path}: exec failed: {result.stderr}")
                    actual = json.loads(result.stdout)
                    out.validate(actual)
                    require(actual == fixture_output, f"{eval_path}: exec result differs from golden")
                    exec_count += 1

        if "decisions" in config:
            # Every catalog atom has eval cases, so any atom can be named as evidence.
            decisions.check(config["decisions"], set(atoms), manifest)

        workflows = {}
        for path in sorted(project.rglob("*.workflow.yaml")):
            if not belongs(path):
                continue
            workflow = read_yaml(path.read_text())
            workflows[path.name.removesuffix(".workflow.yaml")] = workflow
            require(workflow.get("atomic") == 1, f"{path}: wrong format")
            require("name" not in workflow and "version" not in workflow, f"{path}: use identity conventions")
            require({"input", "output", "steps", "returns"} <= workflow.keys(), f"{path}: missing workflow fields")
            for boundary in ("input", "output"):
                Draft202012Validator.check_schema(workflow[boundary])
                check_schema_refs(workflow[boundary], path.as_uri(), registry)
            steps = workflow["steps"]
            require(isinstance(steps, dict) and steps, f"{path}: empty steps")
            ancestors = {}

            def visit(name, visiting):
                require(name in steps, f"{path}: unknown step {name}")
                require(name not in visiting, f"{path}: dependency cycle at {name}")
                if name not in ancestors:
                    deps = set(steps[name].get("after", []))
                    inherited = set()
                    for dep in deps:
                        inherited |= visit(dep, visiting | {name})
                    ancestors[name] = deps | inherited
                return ancestors[name]

            for name, step in steps.items():
                visit(name, set())
                require(("atom" in step) != ("gate" in step), f"{path}: ambiguous step {name}")
                if "atom" in step:
                    require(step["atom"] in atoms, f"{path}: unknown atom {step['atom']}")
                    _, _, _, input_path, _ = atoms[step["atom"]]
                    input_schema = resolve_root(schemas[input_path], input_path.as_uri(), registry)
                    mapping = step.get("with", {})
                    require(set(input_schema.get("required", [])) <= mapping.keys(), f"{path}: missing input binding in {name}")
                    require(mapping.keys() <= input_schema.get("properties", {}).keys(), f"{path}: extra input binding in {name}")
                    require(all(isinstance(value, str) for value in mapping.values()), f"{path}: binding values must be CEL strings")
                else:
                    require(step["gate"].get("on_timeout") == "reject", f"{path}: catalog gates must fail closed")
                    require("show" in step["gate"], f"{path}: gate must show evidence")
                for referenced in re.findall(r"steps\.([\w-]+)\.", json.dumps(step)):
                    require(referenced in ancestors[name], f"{path}: {name} reads {referenced} without an after dependency")
                # A workflow may read what a step's code prepared, as steps.<id>.prepared.<path>.
                for referenced, prepared_path in re.findall(r"steps\.([\w-]+)\.prepared((?:\.\w+)+)", json.dumps(step)):
                    source = steps[referenced].get("atom")
                    require(source in prepared_schemas, f"{path}: {name} reads prepared values of {referenced}, whose atom has no code")
                    current = prepared_schemas[source]
                    for part in prepared_path[1:].split("."):
                        require(part in current.get("properties", {}), f"{path}: {name} reads unknown prepared path {referenced}{prepared_path}")
                        current = current["properties"][part]
            for referenced in re.findall(r"steps\.([\w-]+)\.", workflow["returns"]):
                require(referenced in steps, f"{path}: unknown returns step")
            workflow_count += 1

        # A diagram committed under an `atomic:diagram` marker is generated output.
        # Regenerate it from the workflow and fail when the committed copy differs.
        for path in sorted(project.rglob("*.md")):
            if not belongs(path) or path.name.endswith(".atom.astro"):
                continue
            text = path.read_text()
            blocks = list(workflow_diagram.marked_blocks(text))
            require(len(blocks) == text.count("<!-- atomic:diagram"), f"{path}: a diagram marker must sit directly above a mermaid block")
            for attrs, committed in blocks:
                require(attrs.keys() <= {"workflow", "view"} and "workflow" in attrs, f"{path}: unknown diagram marker {attrs}")
                require(attrs["workflow"] in workflows, f"{path}: diagram of unknown workflow {attrs['workflow']}")
                view = attrs.get("view", "default")
                require(view in {"default", "risk"}, f"{path}: the catalog generates only the default and risk views")
                generated = workflow_diagram.render(
                    workflows[attrs["workflow"]], {name: entry[0] for name, entry in atoms.items()}, view
                )
                require(committed == generated, f"{path}: the {view} diagram of {attrs['workflow']} is stale; regenerate it")
                diagram_count += 1
    for path in top.rglob("*.md"):
        check_markdown_links(path)
    return atom_count, fixture_count, exec_count, workflow_count, diagram_count


def check_yaml_loader_mirror():
    """The loader's key rules, checked on text rather than on the catalog.

    No catalog file needs an integer key, so the mirror of atomic's integer
    keys would never run without this. Each case says what it must be read as
    or how it must be refused, so a mirror that stops refusing reads as a
    failure and not as a smaller count.
    """
    checks = 0

    def refused(text, why):
        """One key that must be refused, and the reason its refusal must say.

        A refusal of a key spelling also has to name the fix, which is quoting:
        a message that only says the text is odd is not one an author can act
        on. The exception is a duplicate, where the fix is to remove a key.
        """
        try:
            read_yaml(text)
        except ValueError as exc:
            require(why in str(exc), f"{text!r}: refused for {exc}, which does not say {why!r}")
            if not why.startswith("duplicate"):
                require(
                    "quote it" in str(exc),
                    f"{text!r}: refused for {exc}, which does not say to quote it",
                )
            return 1
        raise ValueError(f"{text!r} was read as a key; it must be refused as {why}")

    # A plain canonical integer key is read as its own text, so a schema's
    # property name is a string and the two spellings are one key. A decimal past
    # what a 64-bit integer holds is read as a number and refused below. These are
    # the same keys the loader's own table holds, in
    # crates/atomic-schema/src/yaml/tests.rs, which is what keeps the two halves
    # of the mirror from drifting apart.
    for spelling in (
        "200",
        "0",
        "-5",
        "9223372036854775807",
        "9223372036854775808",
        "18446744073709551615",
    ):
        require(
            read_yaml(f"{spelling}: ok\n") == {spelling: "ok"},
            f"the plain integer key {spelling} is not read as its own text",
        )
        require(
            read_yaml(f"{spelling}: ok\n") == read_yaml(f"\"{spelling}\": ok\n"),
            f"the plain integer key {spelling} and its quoted form are not one key",
        )
        checks += 2
    for text, why in [
        ("200: a\n\"200\": b\n", "duplicate YAML key '200'"),
        # A leading zero is a decimal in 1.2 and an octal in 1.1, or a string
        # there when a digit is no octal digit. PyYAML reads `08` as a string
        # and `0200` as the integer 128, so only the text says which it is.
        ("0200: a\n", "has a leading zero"),
        ("007: a\n", "has a leading zero"),
        ("08: a\n", "has a leading zero"),
        ("09: a\n", "has a leading zero"),
        ("0089: a\n", "has a leading zero"),
        ("00: a\n", "has a leading zero"),
        ("-00: a\n", "has a leading zero"),
        # A number the core schema reads is no key either: the key would be
        # renamed to the number and would no longer match its quoted form. This
        # is where the loader's own message says what the key reads as.
        ("+2: a\n", "is a number, so the key would be renamed to it"),
        ("-0: a\n", "is a number, so the key would be renamed to it"),
        ("0x1F: a\n", "is a number, so the key would be renamed to it"),
        ("0o17: a\n", "is a number, so the key would be renamed to it"),
        ("1.5: a\n", "is a number, so the key would be renamed to it"),
        # 1e3 is a float to JSON and to the core schema and a string to PyYAML.
        ("1e3: a\n", "is a number, so the key would be renamed to it"),
        # Past what a 64-bit integer holds the loader still reads the digits as a
        # number, and refuses the key because a key must be a number whose digits
        # are its own text; PyYAML reads the same digits as an integer, so the key
        # would come through as one.
        (
            "99999999999999999999: a\n",
            "is a number, so the key would be renamed to it",
        ),
        (
            "18446744073709551616: a\n",
            "is a number, so the key would be renamed to it",
        ),
        # A sign on the wrong side of the base marker: `-0x1F` is -31 to 1.1 and
        # a string to 1.2, and `0x-1F` is no number to either.
        ("-0x1F: a\n", "puts a sign on the wrong side of its base marker"),
        ("0x-1F: a\n", "puts a sign on the wrong side of its base marker"),
        # Only YAML 1.1 reads these as numbers: binary, an underscore in a whole
        # number, and the sexagesimal. PyYAML reads all three as integers, so a
        # mirror that only looked at what PyYAML constructs would call `0o17` a
        # string and `12:30` the integer 750 and let both through.
        ("0b11: a\n", "is a number in YAML 1.1 and a string in YAML 1.2 and JSON"),
        ("1_000: a\n", "is a number in YAML 1.1 and a string in YAML 1.2 and JSON"),
        ("12:30: a\n", "is a number in YAML 1.1 and a string in YAML 1.2 and JSON"),
        # And the boolean words YAML 1.1 resolves, and what JSON cannot hold.
        ("yes: a\n", "is a boolean in YAML 1.1 and a string in YAML 1.2"),
        (".inf: a\n", "cannot be written in JSON"),
    ]:
        checks += refused(text, why)
    # Quoted, every one of those spellings is the string it is read as, and
    # quoting is the fix the message gives.
    for spelling in (
        "0200", "08", "00", "-00", "0x1F", "0o17", "1e3", "-0x1F", "0x-1F", "0b11", "1_000",
        "12:30", "yes", "99999999999999999999",
    ):
        require(
            read_yaml(f"\"{spelling}\": ok\n") == {spelling: "ok"},
            f"the quoted key {spelling} is not its own text",
        )
        checks += 1
    # What PyYAML reads as a boolean or a null is no key in JSON either, and
    # `json_model` is what says so here; the loader refuses these keys itself.
    for text in ("true: a\n", "null: a\n"):
        try:
            json_model(read_yaml(text), "sidecar.schema.yaml")
        except ValueError as exc:
            require("non-string key" in str(exc), f"{text!r}: {exc}")
        else:
            raise ValueError(f"{text!r} reached JSON as a key; it must be refused")
        checks += 1
    return checks


def check_yaml_value_mirror():
    """The loader's plain-value rules, checked on text rather than on the catalog.

    `strict_scalar` answers every plain value in a schema file, and PyYAML would
    otherwise answer them the way YAML 1.1 reads them. Each case says what a value
    must be read as -- the null, boolean or number the core schema reads -- or how
    it must be refused, so a rule that stops refusing reads as a failure and not
    as a smaller count. The type is part of every read case, because `0` and `0.0`
    compare equal and are not the same answer for a schema's `maximum`. Quoting is
    the other half: quoted, the same text is the string, and no rule here touches
    a quoted scalar.

    These are the spellings atomic's loader pins for values in
    crates/atomic-schema/src/yaml/tests.rs, which is what keeps the two halves of
    the mirror from drifting apart. No catalog file spells any shape the rule
    refuses or re-reads, so nothing but this would ever run it.
    """
    checks = 0
    where = "sidecar.input.schema.yaml"

    def refused(text, *why):
        """One value that must be refused, and each clause its refusal must say.

        A value's refusal says what the versions read differently and both things
        the author could write instead -- the number, and the quoted string --
        because an author who wrote `0200` meant one of them.
        """
        try:
            read_schema_text(f"a: {text}\n", where)
        except ValueError as exc:
            said = str(exc)
            for clause in why:
                require(
                    clause in said,
                    f"{text!r} value: refused for {said}, which does not say {clause!r}",
                )
            require(
                "quote it" in said,
                f"{text!r} value: refused for {said}, which does not say to quote it",
            )
            return 1
        raise ValueError(f"a: {text} was read as a value; it must be refused")

    def read_as(text, want):
        """One value, and exactly what it must be read as."""
        got = read_schema_text(f"a: {text}\n", where)["a"]
        require(
            type(got) is type(want) and got == want,
            f"a: {text} was read as {got!r}, which must be {want!r}",
        )
        return 1

    # What the core schema reads and PyYAML resolves differently. `0o17` and `1e3`
    # are strings to PyYAML and numbers to the loader, so a checker that read what
    # PyYAML built would let a schema say the text where atomic reads a number.
    for text, want in (
        ("200", 200),
        ("-3", -3),
        ("0", 0),
        ("-0", 0),
        ("18446744073709551615", 18446744073709551615),
        ("true", True),
        ("False", False),
        ("null", None),
        ("~", None),
        ("", None),
        ("0o17", 15),
        ("0x1F", 31),
        ("0o777", 511),
        ("1e3", 1000.0),
        ("1.5e3", 1500.0),
        (".5", 0.5),
        ("5.", 5.0),
        # A float's leading zero is no integer's leading zero.
        ("00.5", 0.5),
        ("01.5", 1.5),
        # Text no rule reads: the loader keeps it as the string it is.
        ("1.2.3", "1.2.3"),
        ("1_000.5", "1_000.5"),
        ("0o7_7", "0o7_7"),
        ("0b2", "0b2"),
        ("1:70", "1:70"),
    ):
        checks += read_as(text, want)
    for text, *why in [
        # A leading zero is a decimal in 1.2 and an octal in 1.1, or a string
        # there when a digit is no octal digit. PyYAML reads `0200` as the integer
        # 128 and `08` as a string, so only the text says which it is.
        ("0200", "has a leading zero", "YAML 1.1 reads as 128", "write 200, or 0o200 for octal"),
        ("010", "has a leading zero", "YAML 1.1 reads as 8", "write 10"),
        ("007", "has a leading zero", "YAML 1.1 reads as 7"),
        ("-0200", "has a leading zero", "YAML 1.1 reads as -128", "write -200, or -0o200"),
        ("08", "has a leading zero", "YAML 1.1 reads as a string", "write 8, or quote it"),
        ("06008", "has a leading zero", "YAML 1.1 reads as a string"),
        # A sign on either side of the base marker: `-0x1F` is -31 to YAML 1.1 and
        # a string to 1.2, and `0x-1F` is no number to either.
        (
            "-0x1F",
            "puts a sign before its hexadecimal prefix",
            "YAML 1.1 reads as -31",
            "write -31 for the number",
        ),
        ("-0o17", "puts a sign before its octal prefix", "YAML 1.1 reads as -15"),
        (
            "0x-1F",
            "puts a sign after its 0x",
            "no number in either YAML version",
            "write -31 for the number",
        ),
        # A base integer past what a signed 64-bit number holds, which the loader
        # has no number to read.
        (
            "0xFFFFFFFFFFFFFFFF",
            "is a hexadecimal integer past signed 64 bits",
            "write it in decimal",
        ),
        # Only YAML 1.1 reads these as numbers, and PyYAML constructs all three
        # kinds as integers, so a checker reading what PyYAML built would see the
        # number 750 where atomic reads the string "12:30".
        (
            "0b11",
            "is a number in YAML 1.1 and a string in YAML 1.2 and JSON",
            "write 3 for the number",
        ),
        (
            "1_000",
            "is a number in YAML 1.1 and a string in YAML 1.2 and JSON",
            "write 1000 for the number",
        ),
        (
            "12:30",
            "is a number in YAML 1.1 and a string in YAML 1.2 and JSON",
            "write 750 for the number",
        ),
        (
            "0x_1F",
            "is a number in YAML 1.1 and a string in YAML 1.2 and JSON",
            "write 31 for the number",
        ),
        # The words YAML 1.1 resolves as booleans, in any letter case.
        ("yes", "is a boolean in YAML 1.1 and a string in YAML 1.2", "write true or false"),
        ("OFF", "is a boolean in YAML 1.1 and a string in YAML 1.2"),
        # What JSON has no form for. PyYAML builds an infinity and a not-a-number
        # for these, and both are Python floats, which is how they used to reach a
        # schema unnoticed. `1e999` is the same infinity spelled as an exponent.
        (".inf", "cannot be written in JSON"),
        ("-.Inf", "cannot be written in JSON"),
        (".nan", "cannot be written in JSON"),
        ("1e999", "cannot be written in JSON"),
    ]:
        checks += refused(text, *why)
    # A quoted value is its own text to every version, so none of this touches
    # it: the same spellings that were refused above are the strings here.
    for spelling in (
        "0200", "08", "0x1F", "0o17", "1e3", "-0x1F", "0x-1F", "0b11", "1_000", "12:30",
        "yes", "off", ".inf", ".nan", "2026-11-21", "0xFFFFFFFFFFFFFFFF", "1e999",
    ):
        read_as(f'"{spelling}"', spelling)
        checks += 1
    # A plain date -- what YAML 1.1 calls a timestamp and the core schema reads as
    # a string -- is refused rather than read as a string here while the library
    # makes a date of it. SafeLoader's constructor for that tag is popped off the
    # schema loader, so nothing is left to construct it: PyYAML's own error names
    # the tag it could not construct, and that is what the case asserts, not merely
    # that some YAML error came out. That nothing is registered for any tag no rule
    # here reads is asserted first, so a refusal that came from somewhere else reads
    # as a failure.
    for tag in yaml_scalar.UNREAD_TAGS:
        require(
            tag not in SchemaLoader.yaml_constructors,
            f"a schema file has a constructor for {tag}, so a scalar the resolver gives that "
            f"tag would be read as whatever the library makes of it",
        )
        checks += 1
    # And popped from a copy: the loaders that read the manifests and the eval cases
    # read a plain date as a date still, which is what `check_decision_dates` below
    # compares.
    for reads, where_read in ((yaml.safe_load, "SafeLoader"), (read_yaml, "the catalog loader")):
        date = reads("a: 2026-11-21\n")["a"]
        require(
            isinstance(date, datetime.date),
            f"a plain date read with {where_read} is {date!r}, which must still be a date",
        )
        checks += 1
    no_constructor = f"could not determine a constructor for the tag '{yaml_scalar.TIMESTAMP_TAG}'"
    try:
        read_schema_text("a: 2026-11-21\n", where)
    except yaml.YAMLError as exc:
        said = str(exc)
        require(
            no_constructor in said,
            f"a plain date was refused for {said!r}, which does not say {no_constructor!r}",
        )
        checks += 1
    else:
        raise ValueError(f"{where}: a: 2026-11-21 was read; a plain date must be refused")
    # The two tags no rule here reads besides the timestamp -- the value (`=`) and
    # the merge (`<<`) keys -- are refused the same way, on the text each is
    # resolved from. atomic's loader reads both texts as strings, so this is
    # stricter than the loader, and the catalog writes neither.
    for spelling in ("=", "<<"):
        try:
            read_schema_text(f"a: {spelling}\n", where)
        except yaml.YAMLError as exc:
            said = str(exc)
            require(
                "could not determine a constructor for the tag" in said,
                f"a plain {spelling} was refused for {said!r}, which does not say it could "
                f"not construct the tag",
            )
            checks += 1
        else:
            raise ValueError(f"{where}: a: {spelling} was read; it must be refused")
    return checks


def check_exec_edges():
    project = EXAMPLES / "03-order-report"
    def run(script, data):
        return subprocess.run([sys.executable, str(project / "tools" / script)], input=json.dumps(data), text=True, capture_output=True, timeout=5)

    valid = {"order_id": "注文-1", "lines": [{"sku": "A", "quantity": 10000, "unit_cents": 10000000}]}
    result = run("total_order.py", valid)
    require(result.returncode == 0 and json.loads(result.stdout)["total_cents"] == 100000000000, "large integer total failed")
    checks = 1
    for field, value in (("quantity", 0), ("quantity", -1), ("quantity", True), ("quantity", 10001), ("unit_cents", -1), ("unit_cents", 1.5)):
        bad = {"order_id": "bad", "lines": [{"sku": "A", "quantity": 1, "unit_cents": 5, field: value}]}
        result = run("total_order.py", bad)
        require(result.returncode != 0 and result.stderr and not result.stdout, f"invalid {field}={value!r} was accepted")
        checks += 1
    data = {"total": {"order_id": "注文-1", "total_cents": 25, "item_count": 1}, "assessment": {"order_id": "注文-1", "total_cents": 25, "note": "確認済み"}}
    result = run("render_report.py", data)
    require(result.returncode == 0 and "確認済み" in json.loads(result.stdout)["report"], "Unicode rendering failed")
    checks += 1
    for key, value in (("order_id", "other-order"), ("total_cents", 26)):
        bad = {"total": data["total"], "assessment": {**data["assessment"], key: value}}
        result = run("render_report.py", bad)
        require(result.returncode != 0 and result.stderr and not result.stdout, f"mismatched {key} was accepted")
        checks += 1
    return checks


def check_coding_fixture():
    project = EXAMPLES / "02-change-request"
    helper = project / "tools" / "prepare_fixture.py"
    original = {p.relative_to(project): p.read_bytes() for p in project.rglob("*") if p.is_file()}
    with tempfile.TemporaryDirectory(prefix="atomic-fixture-check-") as directory:
        sandbox = Path(directory)
        prepared = []
        for name in ("first", "second"):
            result = subprocess.run(
                [sys.executable, str(helper), str(sandbox / name)], cwd=sandbox,
                text=True, capture_output=True, timeout=20,
            )
            require(result.returncode == 0, f"fixture preparation failed: {result.stderr}")
            metadata = json.loads(result.stdout)
            prepared.append(metadata)
            repository = Path(metadata["repository"])
            generated = Path(metadata["project"])
            for commit in (metadata["base"], metadata["candidate"]):
                found = subprocess.run(
                    ["git", "cat-file", "-t", commit], cwd=repository,
                    text=True, capture_output=True, timeout=5,
                )
                require(found.returncode == 0 and found.stdout.strip() == "commit", "fixture commit is missing")
            require(metadata["base"] != metadata["candidate"], "candidate must be a distinct commit")
            for path in (generated / "evals").rglob("*.yaml"):
                case = read_yaml(path.read_text())
                golden = generated / "fixtures" / path.parent.name / (path.stem + ".input.json")
                require(case["input"] == read_json(golden), "prepared eval/input drift")
                schema = read_schema(generated / "atoms" / (path.parent.name + ".input.schema.yaml"))
                Draft202012Validator(schema).validate(case["input"])
                def check_commits(value):
                    if isinstance(value, dict):
                        for key, child in value.items():
                            if key == "base":
                                require(child == metadata["base"], "base was not bound to the real fixture")
                            elif key == "commit":
                                require(child == metadata["candidate"], "candidate was not bound to the real fixture")
                            check_commits(child)
                    elif isinstance(value, list):
                        for child in value:
                            check_commits(child)
                check_commits(case["input"])
        require(prepared[0]["base"] == prepared[1]["base"], "fixture baseline is not reproducible")
        require(prepared[0]["candidate"] == prepared[1]["candidate"], "fixture candidate is not reproducible")
        for destination in (sandbox / "first", project / "must-not-exist"):
            result = subprocess.run(
                [sys.executable, str(helper), str(destination)], cwd=sandbox,
                text=True, capture_output=True, timeout=5,
            )
            require(result.returncode != 0, "unsafe/existing fixture destination was accepted")
    current = {p.relative_to(project): p.read_bytes() for p in project.rglob("*") if p.is_file()}
    require(current == original, "fixture preparation changed the source project")


def check_decision_dates():
    """A review date is compared with a date the caller supplies, never with the clock."""
    manifest = EXAMPLES / "10-recorded-decision" / "atomic.yaml"
    block = read_yaml(manifest.read_text())["decisions"]
    name, decision = next(iter(block.items()))
    due = decision["review_by"]
    require(decisions.past_review(block, due) == [], f"{manifest}: {name} is not past review on its review date")
    require(decisions.past_review(block, due + datetime.timedelta(days=1)) == [name], f"{manifest}: {name} is past review the day after")
    return len(block)


def main():
    # Atom declarations come from this mirror, so it must agree with atomic's.
    print(f"PASS TypeScript projection mirror: {astro_atoms.check_projection()} cases match atomic's generator")
    print(f"PASS YAML mirror: {check_yaml_loader_mirror()} key rules match atomic's loader")
    print(f"PASS YAML mirror: {check_yaml_value_mirror()} value rules match atomic's loader")
    total = [0, 0, 0, 0, 0]
    projects = sorted(path for path in EXAMPLES.iterdir() if path.is_dir())
    for project in projects:
        counts = check_project(project)
        total = [a + b for a, b in zip(total, counts)]
        print(f"PASS {project.name}: {counts[0]} atoms, {counts[1]} fixture pairs, {counts[3]} workflows")
    edge_count = check_exec_edges()
    check_coding_fixture()
    print("PASS coding fixture: reproducible commits, real input bindings, isolated setup and rejected unsafe destinations")
    if deno_exec.CHECKED:
        blocks = sum(entry[1] == "block" for entry in deno_exec.CHECKED)
        hosts = sum(entry[2] for entry in deno_exec.CHECKED)
        secrets = sum(entry[3] for entry in deno_exec.CHECKED)
        print(f"PASS Deno code: {blocks} atom code blocks and {len(deno_exec.CHECKED) - blocks} scripts checked statically (pinned imports in the lockfile, {hosts} declared hosts, {secrets} credentials or secret references); not run here")
    decision_count = check_decision_dates()
    print(f"PASS decisions: {decision_count} recorded; review dates checked against supplied dates only")
    check_markdown_links(EXAMPLES / "README.md")
    check_markdown_links(EXAMPLES / "recipes.md")
    print(f"PASS {len(projects)} examples; {total[0]} atoms; {total[1]} fixture pairs; {total[3]} workflows; {total[4]} generated diagrams; {total[2]} exec fixtures + {edge_count} exec edge cases")
    print("Scope: offline contracts and Python execution; shipped Astro parsing, TypeScript checks, rendering and typed validators executed; workflow CEL, models and workflow recovery were not executed.")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, yaml.YAMLError) as exc:
        print(f"FAIL {exc}", file=sys.stderr)
        sys.exit(1)
