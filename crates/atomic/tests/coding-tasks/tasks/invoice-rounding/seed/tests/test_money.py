import unittest

from invoicing import money


class ParseTest(unittest.TestCase):
    def test_plain_numbers(self):
        for text, shown in [("12.50", "12.50"), ("3", "3.00"), (" 7.1 ", "7.10"), ("0", "0.00")]:
            self.assertEqual(money.plain(money.parse_amount(text)), shown)

    def test_rejects_other_forms(self):
        for text in ["", "abc", "1e3", "1,000", "1.", ".5", "-", "12.5.1"]:
            with self.assertRaises(ValueError, msg=text):
                money.parse_amount(text)

    def test_rejects_non_strings(self):
        with self.assertRaises(ValueError):
            money.parse_amount(12.5)

    def test_percent_range(self):
        self.assertEqual(money.format_number(money.parse_percent("12.5")), "12.5")
        with self.assertRaises(ValueError):
            money.parse_percent("100.5")


class FormatTest(unittest.TestCase):
    def test_thousands(self):
        self.assertEqual(money.format_amount(money.parse_amount("1234567.5")), "1,234,567.50")
        self.assertEqual(money.plain(money.parse_amount("1234567.5")), "1234567.50")

    def test_unit_prices(self):
        self.assertEqual(money.format_unit(money.parse_amount("4.1")), "4.10")
        self.assertEqual(money.format_unit(money.parse_amount("4.115")), "4.115")
        self.assertEqual(money.format_unit(money.parse_amount("0.0125")), "0.0125")
        self.assertEqual(money.format_unit(money.parse_amount("85")), "85.00")

    def test_rounding_plain_values(self):
        self.assertEqual(money.plain(money.round_cents(money.parse_amount("1.234"))), "1.23")
        self.assertEqual(money.plain(money.round_cents(money.parse_amount("1.236"))), "1.24")


if __name__ == "__main__":
    unittest.main()
