import os
import tempfile
import unittest

from pipeline import csvio

COLUMNS = ("a", "b")


class CsvFiles(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)

    def path(self, text):
        path = os.path.join(self.dir.name, "in.csv")
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
        return path

    def test_rows_are_stripped(self):
        rows = list(csvio.read_rows(self.path("a,b\n 1 , two\n\n3,4\n"), COLUMNS))
        self.assertEqual(rows, [{"a": "1", "b": "two"}, {"a": "3", "b": "4"}])

    def test_a_short_row_is_skipped_and_logged(self):
        with self.assertLogs("pipeline.csvio", "ERROR") as logged:
            rows = list(csvio.read_rows(self.path("a,b\n1\n2,3\n"), COLUMNS))
        self.assertEqual(rows, [{"a": "2", "b": "3"}])
        self.assertIn(":2: expected 2 fields, got 1", logged.output[0])

    def test_a_wrong_header(self):
        with self.assertLogs("pipeline.csvio", "ERROR"):
            self.assertEqual(list(csvio.read_rows(self.path("b,a\n1,2\n"), COLUMNS)), [])

    def test_round_trip(self):
        out = os.path.join(self.dir.name, "sub", "out.csv")
        rows = [{"a": "x, y", "b": "1"}, {"a": "z", "b": ""}]
        self.assertEqual(csvio.write_rows(out, COLUMNS, rows), 2)
        self.assertEqual(list(csvio.read_rows(out, COLUMNS)), rows)


if __name__ == "__main__":
    unittest.main()
