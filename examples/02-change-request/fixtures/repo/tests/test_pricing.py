# SPDX-License-Identifier: Apache-2.0
import unittest
from pricing import total_cents

class PricingTests(unittest.TestCase):
    def test_one_unit(self):
        self.assertEqual(total_cents(125, 1), 125)
