#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The catalog mirror renders captured helpers through atomic, and never itself.

A `.atom.astro` that imports a local `.ts` helper renders only if something runs
that helper. atomic does, in its own helper runtime; the Python catalog mirror
must not, so the questions this suite asks are:

  * does a helper-backed atom's fixture pair actually render, with the helper's
    mark in the prompt (`RendersThroughAtomicTests.test_helper_backed_atom`);
  * does an atom whose helper is broken fail, and fail for that reason rather
    than as a missing file, a closed channel or an empty prompt
    (`RendersThroughAtomicTests.test_a_broken_helper_fails_for_its_own_reason`);
  * does the mirror refuse to run a helper it is asked to run, and refuse an atom
    that needs a helper and a context snapshot at once (`RefusalTests`);
  * does the whole examples checker fail a *variant* whose base renders, which
    is what it used to miss, and does it render that variant at all
    (`VariantFixtureTests`);
  * is the preview's environment one it builds itself, with no way out to a
    provider (`PreviewEnvironmentTests`).

Needs a built atomic and the checker's own libraries (PyYAML, jsonschema,
referencing), because it runs `tools/check_examples.py` and, for the fixture pair
of a helper-backed atom, the real CLI. As in CI's Examples job, nothing here
reaches a model or the network: every render is a preview, the run's `HOME` and
`ATOMIC_HOME` are private temporary directories, and host detection is pointed at
loopback, so a fall-through to an endpoint is a refused connection rather than a
request.

    ATOMIC_BIN=$PWD/target/debug/atomic \
      /cache/examples-venv/bin/python tools/helper_call_mirror_test.py

Where atomic is not built, or where those libraries are missing, this refuses
(exit 2) instead of passing for want of a backend: a suite that cannot reach what
it tests fails.

Controls this suite covers, each of which bites with its half switched off. Run
a row against committed work with
`ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/controls-check.sh <name>`; until
this suite is wired into a cargo test (atomic-vd6h.5.14) these have no row to
run, so each is shown by editing the named line and running this file:

  * switch `astro_atoms.preview`'s render off -- answer nothing, so the CLI is
    never asked -- and `test_helper_backed_atom` fails: with it off, the fixture
    pair of a helper-backed atom cannot render at all.
  * make `astro_atoms._refusal` answer a call with `{"ok": null}` instead of a
    refusal, and the three tests in `RefusalTests` that ask what a call is answered
    with fail: the refusal a render is given (and that no prompt comes of it), the
    refusal a foreign capture is given, and the two structural refusals, in atomic's
    own order.
  * delete the variant pass in `tools/check_examples.py` (the loop over
    `variants`), and both `VariantFixtureTests` fail: with it off the broken
    variant passes the checker, which is the bug atomic-vd6h.5.17 records, and
    the good variant is never rendered, so its fixture pair is missing from the
    count.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))

try:
    import astro_atoms
except ImportError as missing:  # the mirror itself needs no third-party library
    print(f"REFUSED: tools/astro_atoms.py cannot be imported: {missing}", file=sys.stderr)
    raise SystemExit(2)
try:
    import check_examples
except ImportError as missing:  # jsonschema, referencing and PyYAML are the checker's own
    print(f"REFUSED: tools/check_examples.py cannot be imported (it needs its virtualenv; "
          f"see tools/requirements.txt): {missing}", file=sys.stderr)
    raise SystemExit(2)

try:
    ATOMIC = astro_atoms.compiler()
except ValueError as refusal:
    print(f"REFUSED: {refusal}", file=sys.stderr)
    raise SystemExit(2)

# A broken helper says this, and every failure meant to come from it has to say
# it back, so a failure from anywhere else cannot pass for one.
BROKEN = "the helper is broken on purpose"
INPUT = {
    "type": "object",
    "additionalProperties": False,
    "required": ["text"],
    "properties": {"text": {"type": "string", "description": "the text to show"}},
}
OUTPUT = {
    "type": "object",
    "additionalProperties": False,
    "required": ["echo"],
    "properties": {"echo": {"type": "string", "description": "what was shown"}},
}
VALID_HELPER = "export const wrap = (text: string): string => `[bound] ${text}`;\n"
BROKEN_HELPER = f"throw new Error({BROKEN!r});\nexport const wrap = (text: string): string => text;\n"


def frontmatter(kind, description, imports):
    """An atom's frontmatter, in the catalog's own order: imports, then props, then config."""
    return "\n".join([
        "---",
        f"import type {{ Props, Input, Output }} from '../.atomic/types/{kind}';",
        "import type { AtomConfig } from '../.atomic/types/authoring';",
        *imports,
        "const { input }: Props = Astro.props;",
        "export const config: AtomConfig<Input, Output> = {",
        f'  "description": "{description}",',
        f'  "input": {json.dumps(INPUT)},',
        f'  "output": {json.dumps(OUTPUT)},',
        '  "validate": [({input, output}) => output.echo.length >= input.text.length]',
        "};",
        "---",
    ])


class Catalog(unittest.TestCase):
    """A catalog example of one shape: a base atom, one variant, one fixture pair.

    Each atom imports a captured helper of its own -- the base `layout.ts`, the
    variant `loud.ts`, both exporting `wrap` -- so one can be broken while the
    other renders, which is the pair of facts the variant pass is for.
    """

    def setUp(self):
        # Every render, here and inside the checker, runs on one built atomic.
        os.environ["ATOMIC_BIN"] = str(ATOMIC)
        self.home = Path(tempfile.mkdtemp(prefix="atomic-helper-mirror-"))
        self.addCleanup(shutil.rmtree, self.home, ignore_errors=True)
        astro_atoms.CACHE.clear()
        self.addCleanup(astro_atoms.CACHE.clear)
        self.root = self.home / "catalog"
        self.root.mkdir()
        (self.root / "atomic.yaml").write_text("atomic: 1\nname: helper-call\n")
        (self.root / "atoms").mkdir(parents=True)

    def write(self, relative, text):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def component(self):
        """A typed component, so a render needs the component graph as well."""
        return self.write("atoms/Wrapped.astro", "\n".join([
            "---",
            "interface Props { text: string }",
            "const { text }: Props = Astro.props;",
            "---",
            "wrapped: {text}",
        ]) + "\n")

    def atom(self, stem, helper, helper_name, description, said):
        self.write(f"atoms/{helper_name}.ts", helper)
        self.component()
        return self.write(f"atoms/{stem}.atom.astro", "\n".join([
            frontmatter(stem, description, [
                "import Wrapped from './Wrapped.astro';",
                f"import {{ wrap }} from './{helper_name}';",
            ]),
            "Text: {input.text}",
            said,
            "<Wrapped text={wrap(input.text)} />",
        ]) + "\n")

    def base(self, helper=VALID_HELPER):
        return self.atom("show", helper, "layout", "Show one text through the captured helper.",
                         "{wrap(input.text)}")

    def variant(self, helper=VALID_HELPER):
        return self.atom("show.loud", helper, "loud",
                         "The same text, said twice, through a captured helper.",
                         "{wrap(input.text)} {wrap(input.text)}")

    def case(self):
        """The eval case and fixture pair the checker finds for the base atom."""
        self.write("evals/show/plain.yaml",
                   "input:\n  text: hello\nexpect:\n  - output.echo == 'hello'\n")
        self.write("fixtures/show/plain.input.json", json.dumps({"text": "hello"}))
        self.write("fixtures/show/plain.output.json", json.dumps({"echo": "hello"}))

    def fixture(self, atom, text="hello"):
        """Ask the mirror for one fixture pair, exactly as the checker does."""
        return astro_atoms.fixture(Path(atom), {"text": text}, {"echo": text})


class RendersThroughAtomicTests(Catalog):
    def test_helper_backed_atom(self):
        atom = self.base()
        self.assertTrue(astro_atoms.helpers(atom), "the atom captured no helper")
        prompt = self.fixture(atom)
        self.assertIn("[bound] hello", prompt["user"], json.dumps(prompt))
        self.assertIn("wrapped:", prompt["developer"], "the component did not render")
        self.assertNotIn(BROKEN, json.dumps(prompt))

    def test_a_broken_helper_fails_for_its_own_reason(self):
        atom = self.base(BROKEN_HELPER)
        # The atom still loads: a capture is transpiled source, and nothing runs
        # it until a render asks. So only a render can catch this, which is what
        # the catalog checker now does.
        _, config = astro_atoms.load(atom)
        self.assertIn("Show one text", config["description"])
        self.assertTrue(astro_atoms.helpers(atom), "the broken helper was not captured")
        with self.assertRaises(ValueError) as caught:
            self.fixture(atom)
        said = str(caught.exception)
        self.assertIn(BROKEN, said, f"a broken helper must name what it threw: {said}")
        self.assertIn("wrap", said, f"a broken helper must name the call that failed: {said}")

    def test_a_preview_writes_nothing_in_the_example(self):
        atom = self.base()
        before = sorted(p.relative_to(self.root).as_posix() for p in self.root.rglob("*"))
        home = self.home / "preview-home"
        home.mkdir()
        prompt = astro_atoms.preview(atom, {"text": "hello"}, home)
        self.assertEqual(sorted(prompt), ["developer", "user"], "a preview is its two turns")
        self.assertIn("[bound] hello", prompt["user"])
        self.assertEqual(sorted(p.relative_to(self.root).as_posix() for p in self.root.rglob("*")),
                         before, "a preview render wrote into the example")
        # Its state is the run's own, and the input file it was handed is gone.
        self.assertTrue(any((home / "home").rglob("*")), "the preview kept no state of its own")
        self.assertEqual([p.name for p in home.iterdir()], ["home"],
                         f"the preview left its input behind: {list(home.iterdir())}")

    def test_a_relative_atomic_bin_still_previews_from_the_project(self):
        atom = self.base()
        relative = os.path.relpath(ATOMIC, Path.cwd())
        with patch.dict(os.environ, {"ATOMIC_BIN": relative}):
            prompt = self.fixture(atom)
        self.assertIn("[bound] hello", prompt["user"])


class PreviewEnvironmentTests(unittest.TestCase):
    def test_the_preview_environment_is_built_here_and_reaches_no_provider(self):
        with patch.dict(os.environ, {
            "PATH": "/untrusted/path",
            "ATOMIC_DETECT_OLLAMA_ADDR": "provider.example:11434",
            "AWS_SECRET_ACCESS_KEY": "do-not-inherit",
        }):
            environment = astro_atoms.preview_environment("/opt/atomic/bin/atomic", "/run/preview")
        self.assertEqual(set(environment), {"PATH", "HOME", "ATOMIC_HOME", "ATOMIC_DETECT_OLLAMA_ADDR"})
        self.assertEqual(environment["PATH"], os.pathsep.join(["/opt/atomic/bin", os.defpath]))
        self.assertEqual(environment["HOME"], "/run/preview/home")
        self.assertEqual(environment["ATOMIC_HOME"], "/run/preview/home/.local/share/atomic")
        # Detection on port 9 refuses, so a fall-through is a closed connection.
        self.assertTrue(environment["ATOMIC_DETECT_OLLAMA_ADDR"].startswith("127.0.0.1:"))
        self.assertNotIn("AWS_SECRET_ACCESS_KEY", environment)


class PreviewProcessTests(unittest.TestCase):
    def test_stdout_is_bounded_and_the_child_is_reaped(self):
        with patch.object(astro_atoms, "PREVIEW_STDOUT_LIMIT", 1024):
            with self.assertRaisesRegex(ValueError, "stdout limit"):
                astro_atoms._run_preview(
                    [sys.executable, "-c", "import sys; sys.stdout.write('x' * 4096)"],
                    Path.cwd(), os.environ.copy())

    def test_timeout_stops_the_whole_process_group(self):
        with tempfile.TemporaryDirectory(prefix="atomic-preview-process-") as directory:
            ready = Path(directory) / "ready"
            late = Path(directory) / "late"
            parent = "\n".join([
                "import pathlib, subprocess, sys, time",
                "child = 'import pathlib, sys, time; time.sleep(0.8); pathlib.Path(sys.argv[1]).write_text(\"late\")'",
                "subprocess.Popen([sys.executable, '-c', child, sys.argv[2]])",
                "pathlib.Path(sys.argv[1]).write_text('ready')",
                "time.sleep(5)",
            ])
            with patch.object(astro_atoms, "PREVIEW_SECONDS", 0.4):
                with self.assertRaisesRegex(ValueError, "exceeded 0.4 seconds"):
                    astro_atoms._run_preview(
                        [sys.executable, "-c", parent, str(ready), str(late)],
                        Path.cwd(), os.environ.copy())
            self.assertTrue(ready.exists(), "the grandchild was never started")
            time.sleep(0.6)
            self.assertFalse(late.exists(), "a preview timeout left a descendant running")


class RefusalTests(Catalog):
    def test_the_mirror_refuses_to_run_a_helper_it_was_asked_to_run(self):
        atom = self.base()
        request, _ = astro_atoms.load(atom)
        # The compiler child on its own: no CLI, and nothing to answer the call.
        with self.assertRaises(ValueError) as caught:
            astro_atoms.invoke(request | {"action": "render", "input": {"text": "hello"},
                                          "options": {"runId": "catalog", "origin": "external"}},
                               astro_atoms.helper_refusal(astro_atoms.helpers(atom)))
        said = str(caught.exception)
        self.assertIn("does not run captured helpers", said, said)
        self.assertIn("wrap", said, f"the refusal must name the helper: {said}")
        self.assertNotIn("[bound] hello", said, "a refused render produced a prompt")

    def test_an_atom_that_needs_a_helper_and_a_context_is_refused(self):
        atom = self.base()
        # Settings alone do not declare public context for this atom. The helper
        # preview still works until the atom's own sidecar declares a contract.
        (self.root / "context.settings.json").write_text("{}\n")
        self.assertIn("[bound] hello", self.fixture(atom)["user"])
        self.write("atoms/show.context.schema.json", json.dumps({"type": "object"}))
        # The preview that answers helper calls carries no context snapshot, so a
        # helper-backed contextual atom must not be rendered as though it did.
        with self.assertRaises(ValueError) as caught:
            self.fixture(atom)
        said = str(caught.exception)
        self.assertIn("carries no context snapshot", said, said)
        self.assertIn(atom.name, said, f"the refusal must name the atom: {said}")
        self.assertNotIn("[bound] hello", said, "a refused render produced a prompt")

    def test_a_call_naming_a_capture_the_atom_never_loaded_is_named_as_that(self):
        foreign = astro_atoms.helper_refusal(["a" * 64])({"capture": "b" * 64, "export": "wrap"})
        self.assertEqual(foreign["error"]["kind"], "refused")
        self.assertEqual(foreign["error"]["message"],
                         "the helper is not in the atom's captured helpers")
        ours = astro_atoms.helper_refusal(["a" * 64])({"capture": "a" * 64, "export": "wrap"})
        self.assertIn("does not run captured helpers", ours["error"]["message"])

    def test_a_malformed_call_is_refused_in_atomic_s_own_order(self):
        known = {"capture": "a" * 64}
        # The capture is asked about first, as it is for atomic: whatever else a
        # call holds, a capture this atom never loaded is refused as that.
        for call in ({}, {"capture": None}, {"capture": 1}, {"export": "wrap"}, None, "call"):
            with self.subTest(call=call):
                refusal = astro_atoms.helper_refusal(["a" * 64])(call)
                self.assertEqual(refusal["error"]["message"],
                                 "the helper is not in the atom's captured helpers")
        for call in (dict(known), dict(known, export=""), dict(known, export=None), dict(known, export=1)):
            with self.subTest(call=call):
                refusal = astro_atoms.helper_refusal(["a" * 64])(call)
                self.assertEqual(refusal["error"]["message"], "a helper call names its export")


class VariantFixtureTests(Catalog):
    def test_the_checker_renders_a_variant_and_counts_its_fixture_pair(self):
        self.base()
        self.variant()
        self.case()
        atoms, pairs, _exec, workflows, diagrams = check_examples.check_project(self.root)
        self.assertEqual((atoms, pairs, workflows, diagrams), (1, 2, 0, 0),
                         "the base and its variant should each render the one pair")

    def test_a_variant_the_old_checker_let_through_now_fails_the_checker(self):
        self.base()
        variant = self.variant(BROKEN_HELPER)
        self.case()
        # The base renders, so nothing but the variant pass can catch this.
        self.assertIn("[bound] hello", self.fixture(self.root / "atoms/show.atom.astro")["user"])
        astro_atoms.CACHE.clear()
        with self.assertRaises(ValueError) as caught:
            check_examples.check_project(self.root)
        said = str(caught.exception)
        self.assertIn(BROKEN, said, f"the variant must fail for what its helper threw: {said}")
        self.assertIn(variant.name, said, f"the failure must name the variant file: {said}")


class CatalogMirrorTests(unittest.TestCase):
    """Every example, with the prompt-layout example's three shapes rendered."""

    def setUp(self):
        os.environ["ATOMIC_BIN"] = str(ATOMIC)
        astro_atoms.CACHE.clear()
        self.addCleanup(astro_atoms.CACHE.clear)

    def test_every_example_passes_the_checker(self):
        check_examples.main()


def main() -> int:
    print(f"# atomic under test: {ATOMIC}")
    suite = unittest.TestSuite(
        unittest.defaultTestLoader.loadTestsFromTestCase(case)
        for case in (PreviewEnvironmentTests, PreviewProcessTests, RendersThroughAtomicTests,
                     RefusalTests, VariantFixtureTests, CatalogMirrorTests)
    )
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    print(f"RESULT: {result.testsRun - len(result.failures) - len(result.errors)} passed, "
          f"{len(result.failures)} failed, {len(result.errors)} errored, {len(result.skipped)} skipped")
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
