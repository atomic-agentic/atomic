import io
import json
import os
import random
import subprocess
import sys
import tempfile
import unittest
from decimal import ROUND_HALF_UP, Decimal

from invoicing import cli

RATES = {"std": Decimal("20"), "low": Decimal("12.5"), "reduced": Decimal("5.5"), "zero": Decimal("0")}
CENT = Decimal("0.01")


def half_up(value):
    return value.quantize(CENT, rounding=ROUND_HALF_UP)


def reference(data):
    """The README's money rules, worked out exactly."""
    lines, by_code = [], {}
    for line in data["lines"]:
        discount = Decimal(line.get("discount", "0"))
        amount = half_up(Decimal(line["quantity"]) * Decimal(line["unit_price"]) * (100 - discount) / 100)
        lines.append(amount)
        code = line.get("tax", "std")
        by_code[code] = by_code.get(code, Decimal(0)) + amount
    taxes = {code: half_up(by_code[code] * RATES[code] / 100) for code in RATES if code in by_code}
    subtotal = sum(lines, Decimal(0))
    total = subtotal + sum(taxes.values(), Decimal(0))
    return {
        "number": data["number"],
        "lines": [f"{a:.2f}" for a in lines],
        "subtotal": f"{subtotal:.2f}",
        "taxes": {code: f"{a:.2f}" for code, a in taxes.items()},
        "total": f"{total:.2f}",
    }


def invoice(number, lines):
    return {
        "number": number,
        "date": "2026-10-07",
        "currency": "EUR",
        "customer": {"id": "C-900", "name": "Synthetic Customer"},
        "lines": [
            dict({"sku": f"S-{i}", "description": f"Item {i}"}, **line) for i, line in enumerate(lines, 1)
        ],
    }


class Hidden(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.dir.cleanup()

    def run_cli(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        code = cli.main(list(argv), out=out, err=err)
        return code, out.getvalue(), err.getvalue()

    def totals(self, data):
        path = os.path.join(self.dir.name, data["number"] + ".json")
        with open(path, "w", encoding="utf-8") as f:
            json.dump(data, f)
        code, out, err = self.run_cli("totals", path)
        self.assertEqual(code, 0, err)
        return json.loads(out)

    def test_example(self):
        code, out, err = self.run_cli("totals", "examples/inv-2207.json")
        self.assertEqual(code, 0, err)
        self.assertEqual(json.loads(out), {
            "number": "INV-2207",
            "lines": ["12.35", "2.68", "108.38", "9.35", "20.00", "5.00", "5.00", "18.90", "2.81"],
            "subtotal": "184.47",
            "taxes": {"std": "33.11", "reduced": "1.04"},
            "total": "218.62",
        })

    def test_example_printed(self):
        result = subprocess.run([sys.executable, "-B", "-m", "invoicing", "render", "examples/inv-2207.json"],
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertIn("GLUE-PVA   Wood glue, 500 ml              1      2.675              2.68", lines)
        self.assertIn("SCR-4X30   Screws 4x30, per 100           7      1.335              9.35", lines)
        footer = [line.split() for line in lines if line.startswith(" " * 40)]
        self.assertEqual(footer, [
            ["Subtotal", "184.47"],
            ["Tax", "std", "20%", "33.11"],
            ["Tax", "reduced", "5.5%", "1.04"],
            ["Total", "EUR", "218.62"],
        ])

    def test_half_up(self):
        data = invoice("INV-H1", [
            {"quantity": "1", "unit_price": "2.675"},
            {"quantity": "1", "unit_price": "1.005"},
            {"quantity": "2", "unit_price": "0.0025"},
            {"quantity": "2", "unit_price": "1.0025"},
            {"quantity": "1", "unit_price": "0.125", "tax": "low"},
            {"quantity": "1", "unit_price": "1.00", "tax": "reduced"},
            {"quantity": "10", "unit_price": "0.0045", "tax": "reduced"},
        ])
        self.assertEqual(self.totals(data), {
            "number": "INV-H1",
            "lines": ["2.68", "1.01", "0.01", "2.01", "0.13", "1.00", "0.05"],
            "subtotal": "6.89",
            "taxes": {"std": "1.14", "low": "0.02", "reduced": "0.06"},
            "total": "8.11",
        })

    def test_zero_quantity_and_full_discount(self):
        data = invoice("INV-Z1", [
            {"quantity": "0", "unit_price": "99.99"},
            {"quantity": "3", "unit_price": "14.20", "discount": "100"},
            {"quantity": "1", "unit_price": "0.30", "discount": "50", "tax": "zero"},
        ])
        self.assertEqual(self.totals(data), {
            "number": "INV-Z1",
            "lines": ["0.00", "0.00", "0.15"],
            "subtotal": "0.15",
            "taxes": {"std": "0.00", "zero": "0.00"},
            "total": "0.15",
        })

    def test_many_lines_add_up(self):
        data = invoice("INV-M1", [{"quantity": "1", "unit_price": "0.333"} for _ in range(40)]
                       + [{"quantity": "3", "unit_price": "0.6665", "tax": "reduced"} for _ in range(9)])
        self.assertEqual(self.totals(data), {
            "number": "INV-M1",
            "lines": ["0.33"] * 40 + ["2.00"] * 9,
            "subtotal": "31.20",
            "taxes": {"std": "2.64", "reduced": "0.99"},
            "total": "34.83",
        })

    def test_random_invoices(self):
        rng = random.Random(2207)
        quantities = ["0", "1", "2", "3", "7", "12", "1.5", "0.25", "2.75", "250"]
        discounts = ["0", "0", "0", "5", "10", "12.5", "15", "33.3", "50", "100"]
        for n in range(120):
            lines = []
            for _ in range(rng.randint(1, 14)):
                places = rng.choice([2, 2, 3, 4])
                price = Decimal(rng.randint(0, 40000)) / (Decimal(10) ** places)
                lines.append({
                    "quantity": rng.choice(quantities),
                    "unit_price": f"{price:.{places}f}",
                    "discount": rng.choice(discounts),
                    "tax": rng.choice(list(RATES)),
                })
            data = invoice(f"INV-R{n}", lines)
            got = self.totals(data)
            self.assertEqual(got, reference(data), json.dumps(data))
            lines_sum = sum((Decimal(a) for a in got["lines"]), Decimal(0))
            self.assertEqual(Decimal(got["subtotal"]), lines_sum)
            self.assertEqual(Decimal(got["total"]),
                             Decimal(got["subtotal"]) + sum((Decimal(a) for a in got["taxes"].values()), Decimal(0)))


if __name__ == "__main__":
    unittest.main()
