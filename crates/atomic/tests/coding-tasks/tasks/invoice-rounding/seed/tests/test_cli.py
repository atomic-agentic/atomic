import io
import json
import os
import tempfile
import unittest

from invoicing import cli

SIMPLE = {
    "number": "INV-0002",
    "date": "2026-02-03",
    "currency": "GBP",
    "customer": {"id": "C-2", "name": "Small Shop"},
    "lines": [
        {"sku": "A-1", "description": "Pine board", "quantity": "2", "unit_price": "10.00", "tax": "std"},
        {"sku": "B-2", "description": "Delivery", "quantity": "1", "unit_price": "5", "tax": "zero"},
    ],
}


class CliTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.dir.name, "inv.json")
        with open(self.path, "w", encoding="utf-8") as f:
            json.dump(SIMPLE, f)

    def tearDown(self):
        self.dir.cleanup()

    def run_cli(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        code = cli.main(list(argv), out=out, err=err)
        return code, out.getvalue(), err.getvalue()

    def test_totals(self):
        code, out, _ = self.run_cli("totals", self.path)
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out), {
            "number": "INV-0002",
            "lines": ["20.00", "5.00"],
            "subtotal": "25.00",
            "taxes": {"std": "4.00", "zero": "0.00"},
            "total": "29.00",
        })

    def test_render(self):
        code, out, _ = self.run_cli("render", self.path)
        self.assertEqual(code, 0)
        lines = out.splitlines()
        self.assertEqual(lines[0], "INVOICE INV-0002" + " " * 46 + "2026-02-03")
        self.assertEqual(lines[1], "Customer: Small Shop (C-2)")
        self.assertIn("A-1        Pine board                     2      10.00             20.00", lines)
        self.assertEqual(lines[-1].split(), ["Total", "GBP", "29.00"])

    def test_check(self):
        code, out, _ = self.run_cli("check", self.path)
        self.assertEqual((code, out), (0, f"{self.path}: ok\n"))
        bad = os.path.join(self.dir.name, "bad.json")
        with open(bad, "w", encoding="utf-8") as f:
            f.write("{")
        code, _, err = self.run_cli("check", self.path, bad)
        self.assertEqual(code, 1)
        self.assertIn("is not JSON", err)

    def test_usage(self):
        self.assertEqual(self.run_cli()[0], 2)
        self.assertEqual(self.run_cli("render")[0], 2)
        self.assertEqual(self.run_cli("print", self.path)[0], 2)
        self.assertEqual(self.run_cli("--help")[0], 0)


if __name__ == "__main__":
    unittest.main()
