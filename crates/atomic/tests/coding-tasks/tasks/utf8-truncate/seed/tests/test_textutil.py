import unittest

from logship.textutil import clean, token, truncate_bytes, utf8_len


class TextutilTest(unittest.TestCase):
    def test_clean(self):
        self.assertEqual(clean("one\ntwo\t\tthree"), "one two three")
        self.assertEqual(clean("  padded \r\n"), "padded")
        self.assertEqual(clean(""), "")

    def test_token(self):
        self.assertEqual(token("web 1"), "web-1")
        self.assertEqual(token("  "), "-")
        self.assertEqual(token("api"), "api")

    def test_utf8_len(self):
        self.assertEqual(utf8_len("abc"), 3)
        self.assertEqual(utf8_len(""), 0)

    def test_truncate_ascii(self):
        self.assertEqual(truncate_bytes("abcdef", 4), "abcd")
        self.assertEqual(truncate_bytes("abc", 10), "abc")
        self.assertEqual(truncate_bytes("abc", 3), "abc")
        self.assertEqual(truncate_bytes("abc", 0), "")
