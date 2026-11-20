import unittest
from decimal import Decimal

from pricer.cache import clear_all
from pricer.money import format_amount, parse_price, percent_of, round_to
from pricer.catalog import CatalogError, load_catalog
from pricer.quote import Job, JobError, format_quote, parse_job, price_job

CONFIG = "config"


class QuoteTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.catalog = load_catalog("data/catalog.csv")

    def setUp(self):
        clear_all()

    def line(self, profile, sku, quantity):
        return format_quote(price_job(Job(profile, sku, quantity), self.catalog, CONFIG))

    def test_us(self):
        self.assertEqual(self.line("us", "WIDGET-S", 3), "us WIDGET-S x3: net 28.65 USD, tax 2.08 USD, total 30.73 USD")

    def test_us_bulk(self):
        self.assertEqual(self.line("us", "HUB-4", 12),
                         "us HUB-4 x12: net 467.40 USD, tax 33.89 USD, total 501.29 USD (bulk -5%)")

    def test_eu(self):
        self.assertEqual(self.line("eu", "WIDGET-S", 3), "eu WIDGET-S x3: net 26.36 EUR, tax 5.54 EUR, total 31.90 EUR")

    def test_jp_has_no_decimals(self):
        self.assertEqual(self.line("jp", "LAMP", 2), "jp LAMP x2: net 5,678 JPY, tax 568 JPY, total 6,246 JPY")

    def test_parse_job(self):
        self.assertEqual(parse_job("EU widget-s 3\n"), Job("eu", "WIDGET-S", 3))
        for bad in ("eu WIDGET-S", "eu WIDGET-S three", "eu WIDGET-S 0", "eu WIDGET-S 3 4"):
            with self.assertRaises(JobError):
                parse_job(bad)

    def test_unknown_sku(self):
        with self.assertRaises(CatalogError):
            self.line("us", "NOPE", 1)


class MoneyTest(unittest.TestCase):
    def test_parse_price(self):
        self.assertEqual(parse_price(" 9.5 "), Decimal("9.50"))
        self.assertEqual(parse_price("41"), Decimal("41.00"))
        for bad in ("", "abc", "-1", "1.005", "NaN"):
            with self.assertRaises(ValueError):
                parse_price(bad)

    def test_round_half_up(self):
        self.assertEqual(round_to(Decimal("2.675"), 2), Decimal("2.68"))
        self.assertEqual(round_to(Decimal("2.665"), 2), Decimal("2.67"))
        self.assertEqual(round_to(Decimal("1445.5"), 0), Decimal("1446"))

    def test_percent_of(self):
        self.assertEqual(percent_of(Decimal("200"), Decimal("7.25")), Decimal("14.5"))

    def test_format(self):
        self.assertEqual(format_amount(Decimal("1234.5"), 2, "EUR"), "1,234.50 EUR")
        self.assertEqual(format_amount(Decimal("5677"), 0, "JPY"), "5,677 JPY")
