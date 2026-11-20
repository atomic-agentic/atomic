"""Subtotal, taxes and total of an invoice."""

from . import money, pricing, taxes
from .model import Totals


def compute_totals(invoice):
    """The amounts printed on an invoice: each line's, the subtotal, each tax and the total.

    Follows the money rules in README.md: taxes and the subtotal are worked out from the rounded line
    amounts, so the printed lines and footer always add up.
    """
    amounts = []
    subtotal = money.zero()
    by_code = {}
    for line in invoice.lines:
        amount = pricing.line_amount(line)
        amounts.append(amount)
        subtotal += amount
        by_code[line.tax] = by_code.get(line.tax, money.zero()) + amount
    tax_amounts = []
    for code in taxes.in_order(by_code):
        tax = money.round_cents(money.percent_of(by_code[code], taxes.rate_for(code)))
        tax_amounts.append((code, tax))
    total = subtotal + sum((amount for _, amount in tax_amounts), money.zero())
    return Totals(lines=tuple(amounts), subtotal=subtotal, taxes=tuple(tax_amounts), total=total)


def as_dict(invoice, totals):
    """The totals as plain strings, for JSON output."""
    return {
        "number": invoice.number,
        "lines": [money.plain(amount) for amount in totals.lines],
        "subtotal": money.plain(totals.subtotal),
        "taxes": {code: money.plain(amount) for code, amount in totals.taxes},
        "total": money.plain(totals.total),
    }
