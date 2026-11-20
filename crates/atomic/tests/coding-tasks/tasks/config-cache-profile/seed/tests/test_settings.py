import os
import unittest
from decimal import Decimal
from unittest import mock

from pricer.cache import clear_all
from pricer.settings import SettingsError, load_settings, profile_names

CONFIG = "config"


class SettingsTest(unittest.TestCase):
    def setUp(self):
        # Each test sees the files as they are, not what an earlier test cached.
        clear_all()

    def test_profiles(self):
        self.assertEqual(profile_names(CONFIG), ["eu", "jp", "us"])

    def test_us(self):
        rules = load_settings(CONFIG, profile="us")
        self.assertEqual(rules.currency, "USD")
        self.assertEqual(rules.tax_rate, Decimal("7.25"))
        self.assertEqual(rules.bulk_threshold, 10)

    def test_eu(self):
        rules = load_settings(CONFIG, profile="eu")
        self.assertEqual(rules.currency, "EUR")
        self.assertEqual(rules.fx_rate, Decimal("0.92"))
        self.assertEqual(rules.bulk_discount_pct, Decimal("4"))

    def test_jp(self):
        rules = load_settings(CONFIG, profile="jp")
        self.assertEqual((rules.currency, rules.decimals, rules.bulk_threshold), ("JPY", 0, 20))

    def test_environment_override(self):
        with mock.patch.dict(os.environ, {"PRICER_EU_TAX_RATE": "19"}):
            self.assertEqual(load_settings(CONFIG, profile="eu").tax_rate, Decimal("19"))

    def test_unknown_profile(self):
        with self.assertRaises(SettingsError):
            load_settings(CONFIG, profile="mars")
