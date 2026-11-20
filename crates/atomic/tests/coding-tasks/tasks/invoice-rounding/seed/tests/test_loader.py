import copy
import unittest

from invoicing import loader

GOOD = {
    "number": "INV-0001",
    "date": "2026-01-02",
    "currency": "EUR",
    "customer": {"id": "C-1", "name": "Test Customer"},
    "lines": [{"sku": "A", "description": "Thing", "quantity": "2", "unit_price": "1.50", "tax": "std"}],
}


def changed(**fields):
    data = copy.deepcopy(GOOD)
    data.update(fields)
    return data


def with_line(**fields):
    data = copy.deepcopy(GOOD)
    data["lines"][0].update(fields)
    return data


class LoaderTest(unittest.TestCase):
    def test_good(self):
        invoice = loader.from_dict(GOOD)
        self.assertEqual(invoice.number, "INV-0001")
        self.assertEqual(invoice.customer.name, "Test Customer")
        self.assertEqual(len(invoice.lines), 1)
        self.assertEqual(invoice.lines[0].tax, "std")

    def test_defaults(self):
        data = copy.deepcopy(GOOD)
        del data["lines"][0]["tax"]
        line = loader.from_dict(data).lines[0]
        self.assertEqual(line.tax, "std")
        self.assertFalse(line.discount)

    def test_missing_fields(self):
        data = copy.deepcopy(GOOD)
        del data["currency"]
        with self.assertRaisesRegex(ValueError, "missing currency"):
            loader.from_dict(data)

    def test_no_lines(self):
        with self.assertRaisesRegex(ValueError, "at least one line"):
            loader.from_dict(changed(lines=[]))

    def test_bad_values(self):
        cases = [
            (changed(date="2026/01/02"), "YYYY-MM-DD"),
            (changed(currency="eur"), "three capital letters"),
            (with_line(tax="vat"), "unknown tax code"),
            (with_line(quantity="-1"), "must not be negative"),
            (with_line(unit_price=1.5), "line 1: unit_price"),
            (with_line(discount="120"), "line 1: discount"),
        ]
        for data, message in cases:
            with self.assertRaisesRegex(ValueError, message):
                loader.from_dict(data)


if __name__ == "__main__":
    unittest.main()
