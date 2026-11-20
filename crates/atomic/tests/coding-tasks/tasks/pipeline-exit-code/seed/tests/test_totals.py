import unittest
from decimal import Decimal

from load import report_lines
from pipeline.rates import to_eur
from pipeline.totals import compute_totals

CUSTOMERS = [{"customer_id": "C1", "name": "North Mill", "region": "north"},
             {"customer_id": "C2", "name": "Quay", "region": "south"}]


def order(order_id, customer, amount, currency):
    return {"order_id": order_id, "customer_id": customer, "date": "2026-01-02", "amount": amount, "currency": currency}


class Rates(unittest.TestCase):
    def test_to_eur(self):
        self.assertEqual(to_eur("10", "EUR"), Decimal("10.00"))
        self.assertEqual(to_eur("10.05", "USD"), Decimal("9.25"))
        self.assertEqual(to_eur("100", "SEK"), Decimal("8.70"))
        with self.assertRaises(KeyError):
            to_eur("1", "XYZ")


class Totals(unittest.TestCase):
    def test_totals_per_customer(self):
        rows = compute_totals([order("1", "C2", "5", "EUR"), order("2", "C1", "10", "GBP"),
                               order("3", "C2", "1.50", "EUR")], CUSTOMERS)
        self.assertEqual(rows, [
            {"customer_id": "C1", "name": "North Mill", "orders": "1", "total_eur": "11.70"},
            {"customer_id": "C2", "name": "Quay", "orders": "2", "total_eur": "6.50"},
        ])

    def test_an_unknown_currency_is_logged_and_left_out(self):
        with self.assertLogs("pipeline.totals", "ERROR") as logged:
            rows = compute_totals([order("1", "C1", "5", "EUR"), order("2", "C1", "9", "XYZ")], CUSTOMERS)
        self.assertEqual(rows[0]["total_eur"], "5.00")
        self.assertIn("order 2: no rate for XYZ", logged.output[0])

    def test_report(self):
        lines = report_lines([{"customer_id": "C1", "name": "North Mill", "orders": "1", "total_eur": "11.70"}])
        self.assertEqual(lines[0], "Nightly totals (EUR)")
        self.assertEqual(lines[-1], "Grand total: 11.70")


if __name__ == "__main__":
    unittest.main()
