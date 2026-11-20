import json
import os
import random
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timezone

from logship.config import Config
from logship.formatter import Formatter
from logship.record import Record

TS0 = "1970-01-01T00:00:00Z"


def fmt(message, host="h", app="a", limit=40, severity="info", time=0):
    formatter = Formatter(Config(host=host, app=app, facility=1, max_line_bytes=limit))
    return formatter.format(Record(time=time, severity=severity, message=message))


def blen(text):
    return len(text.encode("utf-8"))


class Exact(unittest.TestCase):
    def test_cjk_message(self):
        # prefix is 30 bytes; 7 bytes of room hold two 3-byte characters.
        self.assertEqual(fmt("日本語のテキスト"), "<14>" + TS0 + " h a: 日本...")

    def test_emoji_message(self):
        # 7 bytes of room hold one 4-byte emoji and three ASCII letters.
        self.assertEqual(fmt("😀abcdefgh"), "<14>" + TS0 + " h a: 😀abc...")
        self.assertEqual(fmt("ab😀😀😀"), "<14>" + TS0 + " h a: ab😀...")

    def test_combining_marks_are_code_points(self):
        # "e" + U+0301 is two code points, 1 + 2 bytes.
        self.assertEqual(fmt("éééé"), "<14>" + TS0 + " h a: éée...")

    def test_non_ascii_host(self):
        # "hø" is 3 bytes, so the prefix is 32 bytes and 5 are left for the message.
        self.assertEqual(fmt("abcdefghijklmnopqrstuvwxyz", host="hø"), "<14>" + TS0 + " hø a: abcde...")

    def test_non_ascii_app_and_message(self):
        # "ápp" is 4 bytes: prefix 33, room 4 = one "ü" (2) and "b", "c".
        self.assertEqual(fmt("übcdefghijklmn", app="ápp"), "<14>" + TS0 + " h ápp: übc...")

    def test_record_host_override(self):
        formatter = Formatter(Config(host="h", app="a", facility=1, max_line_bytes=40))
        line = formatter.format(Record(time=0, severity="info", message="z" * 30, host="ŝŝ"))
        # "ŝŝ" is 4 bytes: prefix 33, room 4.
        self.assertEqual(line, "<14>" + TS0 + " ŝŝ a: " + "z" * 4 + "...")

    def test_fits_exactly_with_multibyte(self):
        self.assertEqual(fmt("日本語"), "<14>" + TS0 + " h a: 日本語")  # 30 + 9 = 39
        self.assertEqual(fmt("日本語x"), "<14>" + TS0 + " h a: 日本語x")  # 40

    def test_ascii_unchanged(self):
        self.assertEqual(fmt("x" * 9), "<14>" + TS0 + " h a: " + "x" * 9)
        self.assertEqual(fmt("x" * 10), "<14>" + TS0 + " h a: " + "x" * 10)
        self.assertEqual(fmt("x" * 11), "<14>" + TS0 + " h a: " + "x" * 7 + "...")
        self.assertEqual(fmt("ab cdefgh ij"), "<14>" + TS0 + " h a: ab cdef...")

    def test_prefix_that_does_not_fit(self):
        with self.assertRaises(ValueError):
            fmt("日本語のテキスト", host="ホスト名", limit=40)


ALPHABET = list("abc xyz") + ["é", "ß", "ø", "€", "日", "本", "語", "😀", "🙁", "́", "ア"]
HOSTS = ["h", "edge-1", "hø", "brú-edge", "東京"]
APPS = ["a", "api", "ápp", "модуль"]


class Invariants(unittest.TestCase):
    def test_generated(self):
        rng = random.Random(20261108)
        for _ in range(3000):
            message = "".join(rng.choice(ALPHABET) for _ in range(rng.randint(0, 60))).strip(" ")
            host, app = rng.choice(HOSTS), rng.choice(APPS)
            limit = rng.randint(45, 90)
            time = rng.randint(0, 2_000_000_000)
            prefix = "<14>%s %s %s: " % (datetime.fromtimestamp(time, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"), host, app)
            if blen(prefix) + 3 > limit:
                continue
            line = fmt(message, host=host, app=app, limit=limit, time=time)
            case = (message, host, app, limit)
            data = line.encode("utf-8")
            self.assertLessEqual(len(data), limit, case)
            self.assertNotIn("�", line, case)
            self.assertTrue(line.startswith(prefix), case)
            if blen(prefix + message) <= limit:
                self.assertEqual(line, prefix + message, case)
                continue
            self.assertTrue(line.endswith("..."), case)
            cut = line[len(prefix):-3]
            self.assertTrue(message.startswith(cut), case)
            following = message[len(cut)]
            self.assertGreater(blen(prefix) + blen(cut) + blen(following) + 3, limit, case)


class Cli(unittest.TestCase):
    def test_every_line_accepted(self):
        rng = random.Random(7)
        with tempfile.TemporaryDirectory() as directory:
            records = os.path.join(directory, "records.jsonl")
            with open(records, "w", encoding="utf-8") as f:
                for i in range(200):
                    message = "".join(rng.choice(ALPHABET) for _ in range(rng.randint(10, 140)))
                    record = {"time": 1767225600 + i, "severity": "info", "message": message}
                    if i % 3 == 0:
                        record["host"] = rng.choice(HOSTS)
                    if i % 4 == 0:
                        record["app"] = rng.choice(APPS)
                    f.write(json.dumps(record, ensure_ascii=False) + "\n")
            conf = os.path.join(directory, "logship.conf")
            with open(conf, "w", encoding="utf-8") as f:
                f.write("host = edge-07\napp = billing-api\nfacility = 16\nmax_line_bytes = 100\n")
            out = os.path.join(directory, "out.log")
            run = subprocess.run([sys.executable, "-B", "-m", "logship", "--config", conf, "--output", out, records],
                                 capture_output=True, text=True, timeout=30)
            self.assertEqual(run.returncode, 0, run.stderr)
            self.assertIn("sent 200 lines, collector rejected 0", run.stdout)
            with open(out, "rb") as f:
                lines = f.read().split(b"\n")
            self.assertEqual(lines[-1], b"")
            self.assertEqual(len(lines), 201)
            for data in lines[:-1]:
                self.assertLessEqual(len(data), 100)
                self.assertNotIn("�", data.decode("utf-8"))
