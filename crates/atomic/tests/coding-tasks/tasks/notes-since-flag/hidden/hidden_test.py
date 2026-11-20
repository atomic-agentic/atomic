import csv
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from datetime import date

from notes.model import Note
from notes.query import filter_notes

ROOT = os.path.dirname(os.path.abspath(__file__))

# Synthetic. Notes 6 and 7 sit on either side of midnight UTC, with offsets that put their local day on the
# other side.
NOTES = [
    {"id": 1, "title": "Prune the hedge", "body": "", "tags": ["garden"], "created": "2026-03-01T10:00:00Z"},
    {"id": 2, "title": "Plan the sprint", "body": "carry over two items", "tags": ["work"], "created": "2026-03-03T09:00:00Z"},
    {"id": 3, "title": "Fix the gate latch", "body": "", "tags": ["garden", "urgent"], "created": "2026-03-04T00:00:00Z"},
    {"id": 4, "title": "Send the plan draft", "body": "", "tags": ["work"], "created": "2026-03-04T23:59:59Z"},
    {"id": 5, "title": "Return the drill", "body": "plan a time", "tags": ["errand"], "created": "2026-03-06T08:00:00Z"},
    {"id": 6, "title": "Late call notes", "body": "", "tags": ["work", "urgent"], "created": "2026-03-04T23:30:00-02:00"},
    {"id": 7, "title": "Early market run", "body": "plan the list", "tags": ["errand"], "created": "2026-03-05T01:00:00+03:00"},
    {"id": 8, "title": "Oil the mower", "body": "", "tags": ["garden"], "created": "2026-03-05T00:00:00Z"},
]


def ids(notes):
    return [n.id for n in notes]


class Query(unittest.TestCase):
    def setUp(self):
        self.notes = [Note.from_dict(n) for n in NOTES]

    def test_since_none_is_everything(self):
        self.assertEqual(ids(filter_notes(self.notes, since=None)), [1, 2, 3, 7, 4, 8, 6, 5])

    def test_since_is_inclusive_and_by_utc_day(self):
        self.assertEqual(ids(filter_notes(self.notes, since=date(2026, 3, 4))), [3, 7, 4, 8, 6, 5])
        self.assertEqual(ids(filter_notes(self.notes, since=date(2026, 3, 5))), [8, 6, 5])
        self.assertEqual(ids(filter_notes(self.notes, since=date(2026, 3, 7))), [])

    def test_since_with_other_filters(self):
        self.assertEqual(ids(filter_notes(self.notes, tags=["URGENT"], since=date(2026, 3, 5))), [6])
        self.assertEqual(ids(filter_notes(self.notes, text="plan", since=date(2026, 3, 4))), [7, 4, 5])
        self.assertEqual(ids(filter_notes(self.notes, since=date(2026, 3, 4), limit=2)), [3, 7])
        self.assertEqual(ids(filter_notes(self.notes, ("garden",), None, None, since=date(2026, 3, 2))), [3, 8])


class Cli(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.dir.name, "book.json")
        with open(self.path, "w") as f:
            json.dump({"version": 1, "notes": NOTES}, f)

    def tearDown(self):
        self.dir.cleanup()

    def run_cli(self, *argv):
        env = {k: v for k, v in os.environ.items() if k not in ("NOTES_FILE", "NOTES_NOW")}
        done = subprocess.run([sys.executable, "-B", "-m", "notes", "--file", self.path, *argv], cwd=ROOT, env=env,
                              capture_output=True, text=True, timeout=20)
        return done.returncode, done.stdout, done.stderr

    def text_ids(self, out):
        return [int(line.split()[0]) for line in out.splitlines() if line.strip()]

    def test_list_since(self):
        code, out, err = self.run_cli("list", "--since", "2026-03-05")
        self.assertEqual(code, 0, err)
        self.assertEqual(self.text_ids(out), [8, 6, 5])

    def test_list_since_with_filters_and_limit(self):
        code, out, err = self.run_cli("list", "--tag", "garden", "--since", "2026-03-04")
        self.assertEqual((code, self.text_ids(out)), (0, [3, 8]), err)
        code, out, err = self.run_cli("list", "--since", "2026-03-04", "--limit", "3")
        self.assertEqual((code, self.text_ids(out)), (0, [3, 7, 4]), err)
        code, out, err = self.run_cli("list", "--text", "PLAN", "--since", "2026-03-04", "--limit", "1")
        self.assertEqual((code, self.text_ids(out)), (0, [7]), err)

    def test_list_without_since_unchanged(self):
        code, out, err = self.run_cli("list", "--limit", "2")
        self.assertEqual(code, 0, err)
        self.assertEqual(out, "   1  2026-03-01  Prune the hedge  #garden\n   2  2026-03-03  Plan the sprint  #work\n")

    def test_export_since_each_format(self):
        code, out, err = self.run_cli("export", "--since", "2026-03-05")
        self.assertEqual(code, 0, err)
        self.assertEqual([n["id"] for n in json.loads(out)], [8, 6, 5])
        code, out, err = self.run_cli("export", "--format", "csv", "--since", "2026-03-04", "--tag", "work")
        self.assertEqual(code, 0, err)
        rows = list(csv.reader(io.StringIO(out)))
        self.assertEqual(rows[0], ["id", "created", "title", "tags", "body"])
        self.assertEqual([r[0] for r in rows[1:]], ["4", "6"])
        code, out, err = self.run_cli("export", "--format", "text", "--text", "plan", "--since", "2026-03-06")
        self.assertEqual((code, self.text_ids(out)), (0, [5]), err)

    def test_export_since_to_file(self):
        target = os.path.join(self.dir.name, "out.json")
        code, out, err = self.run_cli("export", "--since", "2026-03-06", "--output", target)
        self.assertEqual((code, out), (0, ""), err)
        with open(target) as f:
            self.assertEqual([n["id"] for n in json.load(f)], [5])

    def test_bad_dates_are_usage_errors(self):
        for command in ("list", "export"):
            for bad in ("2026-13-01", "2026-02-30", "05/03/2026", "yesterday", ""):
                code, out, err = self.run_cli(command, "--since", bad)
                self.assertEqual(code, 2, f"{command} --since {bad!r}: {err}")
                self.assertIn("--since", err, f"{command} --since {bad!r}")
                self.assertEqual(out, "")

    def test_help(self):
        for command in ("list", "export"):
            code, out, err = self.run_cli(command, "--help")
            self.assertEqual(code, 0, err)
            self.assertIn("--since", out, command)


class Readme(unittest.TestCase):
    def test_usage_documents_since(self):
        with open(os.path.join(ROOT, "README.md"), encoding="utf-8") as f:
            text = f.read()
        self.assertIn("## Usage", text)
        usage = text.split("## Usage", 1)[1].split("\n## ", 1)[0]
        self.assertIn("--since", usage)


if __name__ == "__main__":
    unittest.main()
