import unittest

from logship.config import Config
from logship.formatter import Formatter
from logship.record import Record, RecordError


class FormatterTest(unittest.TestCase):
    def setUp(self):
        self.formatter = Formatter(Config(host="web-1", app="api", facility=1, max_line_bytes=60))

    def test_short_line(self):
        record = Record(time=0, severity="info", message="hello")
        self.assertEqual(self.formatter.format(record), "<14>1970-01-01T00:00:00Z web-1 api: hello")

    def test_priority(self):
        self.assertEqual(self.formatter.priority(Record(time=0, severity="emerg", message="")), 8)
        self.assertEqual(self.formatter.priority(Record(time=0, severity="debug", message="")), 15)

    def test_record_overrides_host_and_app(self):
        record = Record(time=86400, severity="err", message="x", host="db 2", app="")
        self.assertEqual(self.formatter.format(record), "<11>1970-01-02T00:00:00Z db-2 -: x")

    def test_control_characters(self):
        record = Record(time=0, severity="info", message="a\nb\t\tc ")
        self.assertTrue(self.formatter.format(record).endswith(": a b c"))

    def test_long_ascii_line_is_cut(self):
        record = Record(time=0, severity="info", message="x" * 100)
        line = self.formatter.format(record)
        self.assertEqual(len(line), 60)
        self.assertEqual(line, "<14>1970-01-01T00:00:00Z web-1 api: " + "x" * 21 + "...")

    def test_exactly_at_the_limit(self):
        message = "y" * 24
        line = self.formatter.format(Record(time=0, severity="info", message=message))
        self.assertEqual(line, "<14>1970-01-01T00:00:00Z web-1 api: " + message)

    def test_prefix_too_long(self):
        formatter = Formatter(Config(host="web-1", app="api", max_line_bytes=30))
        with self.assertRaises(ValueError):
            formatter.format(Record(time=0, severity="info", message="hello there"))

    def test_bad_record(self):
        with self.assertRaises(RecordError):
            Record(time="soon", severity="info", message="x")
        with self.assertRaises(RecordError):
            Record(time=0, severity="loud", message="x")
