import os
import tempfile
import unittest

from logship.collector import Collector
from logship.config import Config
from logship.formatter import Formatter
from logship.record import Record, read_jsonl
from logship.sender import BatchSender


class CollectorTest(unittest.TestCase):
    def test_rules(self):
        collector = Collector(10)
        self.assertIsNone(collector.check(b"short"))
        self.assertIn("too long", collector.check(b"x" * 11))
        self.assertEqual(collector.check(b"a\nb"), "holds a line break")
        self.assertEqual(collector.check(b"\xe6\x97"), "not valid UTF-8")


class SenderTest(unittest.TestCase):
    def test_writes_every_line(self):
        formatter = Formatter(Config(host="h", app="a", max_line_bytes=40))
        records = [(1, Record(time=0, severity="info", message="one")), (2, Record(time=0, severity="info", message="two"))]
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "out.log")
            report = BatchSender(path, formatter).send(records)
            with open(path, "rb") as f:
                lines = f.read().splitlines()
        self.assertEqual(report.sent, 2)
        self.assertEqual(report.rejected, [])
        self.assertEqual(lines, [b"<14>1970-01-01T00:00:00Z h a: one", b"<14>1970-01-01T00:00:00Z h a: two"])

    def test_read_jsonl(self):
        lines = ['{"time": 5, "severity": "info", "message": "a"}\n', "\n", '{"time": 6, "severity": "err", "message": "b", "host": "x"}\n']
        records = list(read_jsonl(lines))
        self.assertEqual([number for number, _ in records], [1, 3])
        self.assertEqual(records[1][1].host, "x")
