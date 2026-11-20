import unittest

from pipeline import schema


def order(**fields):
    row = {"order_id": "7", "customer_id": "C1", "date": "2026-03-01", "amount": "10.50", "currency": "EUR"}
    row.update(fields)
    return row


class OrderRules(unittest.TestCase):
    def problems(self, row, seen=None):
        return schema.order_problems(row, {"C1", "C2"}, set() if seen is None else seen)

    def test_a_good_order(self):
        self.assertEqual(self.problems(order()), [])
        self.assertEqual(self.problems(order(amount="0")), [])
        self.assertEqual(self.problems(order(amount="3.5")), [])

    def test_ids(self):
        self.assertEqual(len(self.problems(order(order_id="0"))), 1)
        self.assertEqual(len(self.problems(order(order_id="x7"))), 1)
        seen = set()
        self.assertEqual(self.problems(order(), seen), [])
        self.assertIn("duplicate", self.problems(order(), seen)[0])

    def test_customer(self):
        self.assertIn("unknown customer C3", self.problems(order(customer_id="C3"))[0])

    def test_dates(self):
        for text in ("2026-02-30", "2026-13-01", "26-03-01", "2026-3-1", ""):
            self.assertEqual(len(self.problems(order(date=text))), 1, text)
        self.assertTrue(schema.valid_date("2028-02-29"))

    def test_amounts(self):
        for text in ("-1", "1.234", "1e3", "", "1,50", ".5"):
            self.assertEqual(len(self.problems(order(amount=text))), 1, text)

    def test_currency(self):
        for text in ("eur", "EURO", "E1R", ""):
            self.assertEqual(len(self.problems(order(currency=text))), 1, text)

    def test_every_problem_is_listed(self):
        self.assertEqual(len(self.problems(order(customer_id="C9", date="x", amount="-2", currency="?"))), 4)


class CustomerRules(unittest.TestCase):
    def test_customer_rows(self):
        self.assertEqual(schema.customer_problems({"customer_id": "C1", "name": "A", "region": "north"}), [])
        self.assertEqual(len(schema.customer_problems({"customer_id": "C1", "name": "", "region": ""})), 1)


if __name__ == "__main__":
    unittest.main()
