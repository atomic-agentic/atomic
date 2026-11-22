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
from referencing import Registry, Resource
from referencing.jsonschema import DRAFT202012


ROOT = Path(__file__).resolve().parents[1]
EXAMPLES = ROOT / "examples"


class UniqueKeysLoader(yaml.SafeLoader):
    pass


# Plain scalars atomic's loader reads as a number, or refuses for it. Each is a
# port of a rule in crates/atomic-schema/src/yaml.rs, which classifies a plain
# scalar by its own text. PyYAML, which reads the catalog here, resolves
# scalars the way YAML 1.1 does, so nothing here can depend on what PyYAML
# constructs: `0o17:` is the string "0o17" to PyYAML and the number 15 to the
# loader, and `12:30:` is the integer 750 to PyYAML and a string to the loader.
# Both are refused as keys, as the loader refuses them.
CORE_INT = re.compile(r"^[-+]?(?:0|[1-9][0-9]*|0o[0-7]+|0x[0-9a-fA-F]+)$")
CORE_FLOAT = re.compile(r"^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$")
CANONICAL_INT = re.compile(r"^(?:0|-?[1-9][0-9]*)$")
LEADING_ZERO_INT = re.compile(r"^[-+]?0[0-9]+$")
# A sign on the wrong side of the base marker: `-0x1F`, which YAML 1.1 reads as
# a number where 1.2 reads a string, and `0x-1F`, which no version reads as a
# number at all. Both are refused, and quoting is the fix either way.
SIGN_ON_A_BASE_MARKER = re.compile(r"^(?:[-+]0[ox][0-9a-fA-F]+|0[ox][-+][0-9a-fA-F]+)$")
INF_OR_NAN = re.compile(r"^(?:[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))$")
YAML_11_BOOLS = {"yes", "no", "on", "off"}
# YAML 1.1's integer rules exactly as PyYAML resolves them, in the order its
# resolver tries them. An underscore counts anywhere after the first digit, so
# `1000_` and `0x_1F` are integers to it, and a sexagesimal starts at 1 and
# counts its groups in base 60. `0o` is absent because YAML 1.1 writes an octal
# `0777` or `o777`, never `0o777`: PyYAML reads `0o7_7` as a string, and the
# loader refuses that spelling only because 1.1's own type definition has an
# `o` marker an underscore may sit in.
YAML_11_BINARY = re.compile(r"0b[01_]+$")
YAML_11_SEXAGESIMAL = re.compile(r"[1-9][0-9_]*(?::[0-5]?[0-9])+$")
YAML_11_OTHER = re.compile(r"(?:0x[0-9a-fA-F_]+|0[0-7_]+|0|[1-9][0-9_]*)$")


def one_version_integer(spelling):
    """Whether YAML 1.1 alone reads this plain scalar as an integer.

    Binary (`0b11`), a whole number with an underscore in it (`1_000`,
    `0x_1F`, `07_7`) and the sexagesimal (`12:30`) are integers to YAML 1.1 and
    strings to the core schema and JSON. A spelling both read as an integer is
    left out: the loader has already read it as one or refused it for its own
    reason. `yaml_11_int` in the loader is the same rule.
    """
    if CORE_INT.match(spelling) or LEADING_ZERO_INT.match(spelling) or SIGN_ON_A_BASE_MARKER.match(spelling):
        return False
    digits = re.sub(r"^[-+]", "", spelling)
    if YAML_11_BINARY.match(digits):
        return True
    if ":" in digits:
        return YAML_11_SEXAGESIMAL.match(digits) is not None
    return "_" in digits and YAML_11_OTHER.match(digits) is not None


def canonical_integer_key(spelling):
    """Whether the loader reads this plain scalar as a key that is its own text.

    That is `0` or `-?[1-9][0-9]*`, and only while a 64-bit integer can hold it:
    the loader reads any decimal as a number, however many digits, and keeps a
    key only while the number is one whose digits are the key, so a decimal past
    `18446744073709551615` is refused there. `CORE_INT` below reports that
    refusal; PyYAML, with Python's unbounded integers, reads either as the
    integer and would let the key through.
    """
    if not CANONICAL_INT.match(spelling):
        return False
    return -(2**63) <= int(spelling) < 2**64


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

    A key the loader reads as neither a string nor a number -- `true:`, `null:`
    -- is left as PyYAML constructs it, because `json_model` is what reports
    those here. A float, like `1.5:` or `1e3:`, is refused with the integers: the
    loader reads a number and refuses the key, and PyYAML reads `1e3` as the
    string "1e3" and would let it through.
    """
    spelling = key_node.value
    if key_node.style is None:
        line = key_node.start_mark.line + 1
        reason = None
        if spelling.lower() in YAML_11_BOOLS:
            reason = "is a boolean in YAML 1.1 and a string in YAML 1.2"
        elif INF_OR_NAN.match(spelling):
            reason = "cannot be written in JSON"
        elif canonical_integer_key(spelling):
            reason = None
        elif SIGN_ON_A_BASE_MARKER.match(spelling):
            reason = "puts a sign on the wrong side of its base marker"
        elif LEADING_ZERO_INT.match(spelling):
            reason = "has a leading zero, which YAML 1.1 reads as an octal"
        elif CORE_INT.match(spelling) or CORE_FLOAT.match(spelling):
            reason = "is a number, so the key would be renamed to it"
        elif one_version_integer(spelling):
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


def strict_bool(loader, node):
    # PyYAML follows YAML 1.1, where yes/no/on/off are booleans. The proposed
    # loader uses the YAML 1.2 core schema, so only true/false are accepted here.
    require(
        node.value in {"true", "false"},
        f"ambiguous YAML boolean {node.value!r} at line {node.start_mark.line + 1}; quote it or write true/false",
    )
    return node.value == "true"


SchemaLoader.add_constructor("tag:yaml.org,2002:bool", strict_bool)


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


def read_schema(path):
    if path.name.endswith(".schema.json"):
        return read_json(path)
    schema = yaml.load(path.read_text(), Loader=SchemaLoader)
    json_model(schema, path)
    return schema


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
        # Past what a 64-bit integer holds the loader reads a float, where
        # PyYAML, with Python's integers, reads the digits as an integer.
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
