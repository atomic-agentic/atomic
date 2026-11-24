# SPDX-License-Identifier: Apache-2.0
"""The catalog mirror follows atomic's bounded Astro and helper source graph."""

from pathlib import Path
import tempfile
import unittest

import astro_atoms


class SourceGraphTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="atomic-astro-mirror-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "atomic.yaml").write_text("atomic: 1\nname: mirror-fixture\n")
        (self.root / "atoms").mkdir()

    def write(self, name, text):
        path = self.root / "atoms" / name
        path.write_text(text)
        return path

    def test_local_ts_helper_is_loaded_but_declaration_and_js_are_refused(self):
        helper = self.write("helper.ts", "export function f(x: string) { return x; }\n")
        canonical, text = astro_atoms.read_source(helper, self.root)
        self.assertEqual(canonical, str(helper.resolve()))
        self.assertIn("export function f", text)
        for suffix in (".d.ts", ".js"):
            denied = self.write("bad" + suffix, "export const f = 1;\n")
            with self.subTest(suffix=suffix), self.assertRaisesRegex(ValueError, "Astro component or a .ts helper"):
                astro_atoms.read_source(denied, self.root)

    def test_missing_outside_root_and_symlink_loop_refuse(self):
        with self.assertRaises(FileNotFoundError):
            astro_atoms.read_source(self.root / "atoms" / "missing.ts", self.root)
        outside = self.root.parent / (self.root.name + "-outside.ts")
        outside.write_text("export const f = 1;\n")
        self.addCleanup(outside.unlink)
        with self.assertRaisesRegex(ValueError, "inside the project root"):
            astro_atoms.read_source(outside, self.root)
        escape = self.root / "atoms" / "escape.ts"
        escape.symlink_to(outside)
        with self.assertRaisesRegex(ValueError, "inside the project root"):
            astro_atoms.read_source(escape, self.root)
        link = self.root / "atoms" / "loop.ts"
        link.symlink_to(link)
        with self.assertRaises((RuntimeError, OSError)):
            astro_atoms.read_source(link, self.root)

    def test_forbidden_runtime_import_is_refused_by_embedded_compiler(self):
        self.write("helper.ts", "import fs from 'node:fs';\nexport function f(x: string) { return x; }\n")
        atom = self.write("demo.atom.astro", "---\nimport { f } from './helper.ts';\n---\n{f('x')}\n")
        with self.assertRaisesRegex(ValueError, "runtime import node:fs is not a local .ts helper module"):
            astro_atoms.load(atom)

    def test_oversized_helper_is_refused_after_bounded_read(self):
        helper = self.write("large.ts", "x" * 262_145)
        with self.assertRaisesRegex(ValueError, "source size limit exceeded"):
            astro_atoms.read_source(helper, self.root)


if __name__ == "__main__":
    unittest.main()
