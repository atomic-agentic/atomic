import os
import tempfile
import unittest

from logship.config import Config, ConfigError, build, load, parse


class ConfigTest(unittest.TestCase):
    def test_defaults(self):
        self.assertEqual(load(), Config())
        self.assertEqual(load().max_line_bytes, 120)

    def test_parse_skips_comments_and_blanks(self):
        text = "# a comment\n\nhost = web-1\n  app=api  \n"
        self.assertEqual(parse(text), {"host": "web-1", "app": "api"})

    def test_unknown_key(self):
        with self.assertRaises(ConfigError):
            parse("colour = blue\n")

    def test_line_without_equals(self):
        with self.assertRaises(ConfigError):
            parse("host web-1\n")

    def test_integers(self):
        config = build({"facility": "16", "max_line_bytes": "200"})
        self.assertEqual((config.facility, config.max_line_bytes), (16, 200))
        with self.assertRaises(ConfigError):
            build({"max_line_bytes": "lots"})
        with self.assertRaises(ConfigError):
            build({"facility": "24"})
        with self.assertRaises(ConfigError):
            build({"max_line_bytes": "0"})

    def test_file_and_overrides(self):
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "logship.conf")
            with open(path, "w", encoding="utf-8") as f:
                f.write("host = web-2\noutput = a.log\n")
            config = load(path, {"output": "b.log", "app": None})
        self.assertEqual((config.host, config.app, config.output), ("web-2", "logship", "b.log"))
