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


def unique_mapping(loader, node, deep=False):
    result = {}
    for key_node, value_node in node.value:
        key = loader.construct_object(key_node, deep=deep)
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
    """Fails on anything a JSON document could not hold."""
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
