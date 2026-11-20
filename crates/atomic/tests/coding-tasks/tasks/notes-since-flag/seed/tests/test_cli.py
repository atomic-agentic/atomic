import contextlib
import csv
import io
import json
import os
import tempfile
import unittest

from notes import storage
from notes.cli import main
from tests.fixtures import sample_notes


class Cli(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.dir.name, "notes.json")
        storage.save(self.path, sample_notes())

    def tearDown(self):
        self.dir.cleanup()

    def run_cli(self, *argv, environ=None):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = main(["--file", self.path, *argv], environ=environ or {})
            except SystemExit as exit:
                code = exit.code
        return code, out.getvalue(), err.getvalue()

    def test_list(self):
        code, out, _ = self.run_cli("list", "--tag", "home")
        self.assertEqual(code, 0)
        self.assertEqual(out, "   1  2026-03-01  Buy paint  #home\n   2  2026-03-02  Call the plumber  #home #urgent\n")

    def test_list_limit(self):
        code, out, _ = self.run_cli("list", "--limit", "2")
        self.assertEqual(code, 0)
        self.assertEqual([line.split()[0] for line in out.splitlines()], ["1", "3"])

    def test_bad_limit_is_usage(self):
        code, _, err = self.run_cli("list", "--limit", "0")
        self.assertEqual(code, 2)
        self.assertIn("--limit", err)

    def test_add(self):
        code, out, _ = self.run_cli("add", "Water plants", "--tag", "home", environ={"NOTES_NOW": "2026-03-05T12:00:00+01:00"})
        self.assertEqual((code, out), (0, "6\n"))
        self.assertEqual(storage.find(storage.load(self.path), 6).created, "2026-03-05T11:00:00Z")

    def test_show_missing(self):
        code, _, err = self.run_cli("show", "42")
        self.assertEqual(code, 1)
        self.assertIn("no note 42", err)

    def test_export_json(self):
        code, out, _ = self.run_cli("export", "--text", "plan")
        self.assertEqual(code, 0)
        self.assertEqual([n["id"] for n in json.loads(out)], [3, 5])

    def test_export_csv_to_file(self):
        target = os.path.join(self.dir.name, "out.csv")
        code, out, _ = self.run_cli("export", "--format", "csv", "--tag", "work", "--output", target)
        self.assertEqual((code, out), (0, ""))
        with open(target, newline="") as f:
            rows = list(csv.reader(f))
        self.assertEqual(rows[0], ["id", "created", "title", "tags", "body"])
        self.assertEqual([r[0] for r in rows[1:]], ["3", "5"])

    def test_tags(self):
        code, out, _ = self.run_cli("tags")
        self.assertEqual(code, 0)
        self.assertEqual(out.splitlines()[0].split(), ["2", "home"])


if __name__ == "__main__":
    unittest.main()
