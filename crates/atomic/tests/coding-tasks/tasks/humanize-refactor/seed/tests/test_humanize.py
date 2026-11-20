import unittest

from duscan.humanize import format_bytes, format_rate, format_share, format_si


class Bytes(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(format_bytes(0), "0 B")
        self.assertEqual(format_bytes(512), "512 B")
        self.assertEqual(format_bytes(1536), "1.5 KiB")
        self.assertEqual(format_bytes(5 * 1024 ** 3), "5.0 GiB")
        self.assertEqual(format_bytes(1536, precision=2), "1.50 KiB")

    def test_negative(self):
        self.assertEqual(format_bytes(-2048), "-2.0 KiB")

    def test_not_a_number(self):
        with self.assertRaises(TypeError):
            format_bytes("12")
        with self.assertRaises(TypeError):
            format_bytes(True)


class Counts(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(format_si(0), "0")
        self.assertEqual(format_si(42), "42")
        self.assertEqual(format_si(1500), "1.5k")
        self.assertEqual(format_si(2_000_000), "2.0M")

    def test_not_a_number(self):
        with self.assertRaises(TypeError):
            format_si(None)


class Rates(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(format_rate(0), "0 B/s")
        self.assertEqual(format_rate(2_500_000), "2.5 MB/s")
        self.assertEqual(format_rate(12), "12.0 B/s")

    def test_negative(self):
        with self.assertRaises(ValueError):
            format_rate(-1)


class Share(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(format_share(1, 8), "12.5%")
        self.assertEqual(format_share(0, 0), "0.0%")


if __name__ == "__main__":
    unittest.main()
